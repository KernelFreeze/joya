//! Audio capture + voice-activity detection.
//!
//! Adapted from cuteview's `transcribe.rs`: cpal input stream → mono downmix →
//! rubato resample to 16 kHz → Silero VAD. Instead of running a local STT model,
//! we emit PCM16 speech frames (gated by the VAD) and an end-of-utterance marker
//! that downstream streams to Voxtral Realtime.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::spawn;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use flume::{Receiver, Sender};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Fft, FixedSync, Resampler};
use tracing::{error, info};
use voice_activity_detector::VoiceActivityDetector;

const VAD_CHUNK_SIZE: usize = 512;
const SPEECH_THRESHOLD: f32 = 0.5;
// 32 chunks * 512 samples / 16000 Hz ≈ 1s of silence before end-of-utterance.
const SILENCE_CHUNKS_FOR_RESET: usize = 32;
// Pre-speech chunks replayed when speech starts, so the first words aren't clipped.
const LOOKBACK_CHUNKS: usize = 8;

/// One unit of VAD-gated audio handed to the STT stream.
pub enum Segment {
    /// A 16 kHz mono PCM16 frame of detected speech.
    Speech(Vec<i16>),
    /// Silence followed a speech run — the utterance is complete.
    EndOfUtterance,
}

pub struct CaptureHandle {
    /// Speech frames + end-of-utterance markers.
    pub segments: Receiver<Segment>,
    /// When true, incoming samples are dropped before the VAD worker. The worker
    /// thread stays alive so unpausing is instantaneous. Exposed for a future
    /// pause hotkey.
    #[allow(dead_code)]
    pub paused: Arc<AtomicBool>,
}

fn make_resampler(native_sample_rate: u32) -> Option<Fft<f32>> {
    if native_sample_rate != 16000 {
        Some(
            Fft::<f32>::new(native_sample_rate as usize, 16000, 1024, 1, 1, FixedSync::Input)
                .expect("failed to create resampler"),
        )
    } else {
        None
    }
}

fn make_vad() -> VoiceActivityDetector {
    VoiceActivityDetector::builder()
        .sample_rate(16000i64)
        .chunk_size(VAD_CHUNK_SIZE)
        .build()
        .expect("failed to build VAD")
}

fn resample_into(
    resampler: &mut Option<Fft<f32>>,
    raw_buffer: &mut VecDeque<f32>,
    resampled_buffer: &mut VecDeque<f32>,
) {
    if let Some(resampler) = resampler.as_mut() {
        let input_needed = resampler.input_frames_next();
        while raw_buffer.len() >= input_needed {
            let input_vec: Vec<f32> = raw_buffer.drain(..input_needed).collect();
            let output_frames = resampler.output_frames_next();
            let mut output_vec = vec![0.0f32; output_frames];

            let input_adapter =
                SequentialSlice::new(&input_vec, 1, input_needed).expect("input adapter");
            let mut output_adapter =
                SequentialSlice::new_mut(&mut output_vec, 1, output_frames).expect("output adapter");

            match resampler.process_into_buffer(&input_adapter, &mut output_adapter, None) {
                Ok((_consumed, produced)) => resampled_buffer.extend(&output_vec[..produced]),
                Err(e) => error!("Resampling error: {e}"),
            }
        }
    } else {
        resampled_buffer.extend(raw_buffer.drain(..));
    }
}

fn to_pcm16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

fn vad_worker(native_sample_rate: u32, audio_rx: Receiver<Vec<f32>>, seg_tx: Sender<Segment>) {
    let mut vad = make_vad();
    let mut resampler = make_resampler(native_sample_rate);

    let mut raw_buffer: VecDeque<f32> = VecDeque::new();
    let mut resampled_buffer: VecDeque<f32> = VecDeque::new();
    let mut is_speaking = false;
    let mut silence_counter: usize = 0;
    let mut lookback: VecDeque<Vec<f32>> = VecDeque::with_capacity(LOOKBACK_CHUNKS);

    while let Ok(chunk) = audio_rx.recv() {
        raw_buffer.extend(chunk.iter());
        resample_into(&mut resampler, &mut raw_buffer, &mut resampled_buffer);

        while resampled_buffer.len() >= VAD_CHUNK_SIZE {
            let vad_chunk: Vec<f32> = resampled_buffer.drain(..VAD_CHUNK_SIZE).collect();
            let speech_prob = vad.predict(vad_chunk.iter().copied());

            if speech_prob >= SPEECH_THRESHOLD {
                if !is_speaking {
                    is_speaking = true;
                    info!("Speech started ({} lookback chunks)", lookback.len());
                    for buffered in lookback.drain(..) {
                        if seg_tx.send(Segment::Speech(to_pcm16(&buffered))).is_err() {
                            return;
                        }
                    }
                }
                silence_counter = 0;
                if seg_tx.send(Segment::Speech(to_pcm16(&vad_chunk))).is_err() {
                    return;
                }
            } else if is_speaking {
                silence_counter += 1;
                if seg_tx.send(Segment::Speech(to_pcm16(&vad_chunk))).is_err() {
                    return;
                }
                if silence_counter >= SILENCE_CHUNKS_FOR_RESET {
                    info!("Speech ended, flushing utterance");
                    if seg_tx.send(Segment::EndOfUtterance).is_err() {
                        return;
                    }
                    vad.reset();
                    is_speaking = false;
                    silence_counter = 0;
                    lookback.clear();
                }
            } else {
                if lookback.len() >= LOOKBACK_CHUNKS {
                    lookback.pop_front();
                }
                lookback.push_back(vad_chunk);
            }
        }
    }
    info!("Audio channel closed, VAD worker exiting");
}

#[allow(deprecated)]
fn start_capture(device: cpal::Device) -> anyhow::Result<CaptureHandle> {
    let config = device.default_input_config()?;
    let native_sample_rate = config.sample_rate();
    let channels = config.channels() as usize;

    info!("Capture: {:?}, rate={native_sample_rate}, ch={channels}", device.name());

    let (audio_tx, audio_rx) = flume::bounded::<Vec<f32>>(64);
    let (seg_tx, seg_rx) = flume::unbounded::<Segment>();
    spawn(move || vad_worker(native_sample_rate, audio_rx, seg_tx));

    let paused = Arc::new(AtomicBool::new(false));
    let paused_cb = paused.clone();

    let stream = device.build_input_stream(
        config.config(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            if paused_cb.load(Ordering::Relaxed) {
                return;
            }
            let mono: Vec<f32> = if channels == 1 {
                data.to_vec()
            } else {
                data.chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                    .collect()
            };
            let _ = audio_tx.try_send(mono);
        },
        move |err| error!("Audio stream error: {err}"),
        None,
    )?;

    stream.play()?;
    std::mem::forget(stream);

    Ok(CaptureHandle { segments: seg_rx, paused })
}

/// Start capturing the device named `device_name`, or the default output device
/// when `None`. Output (monitor) devices are searched alongside inputs so a
/// headphone/speaker monitor can be captured directly.
#[allow(deprecated)]
pub fn capture(device_name: Option<&str>) -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .input_devices()?
            .chain(host.output_devices()?)
            .find(|d| d.name().map_or(false, |n| n == name))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Audio device '{name}' not found. Run with `list-devices` to see options."
                )
            })?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No default output device found"))?,
    };
    start_capture(device)
}

#[allow(deprecated)]
pub fn list_audio_devices() {
    let host = cpal::default_host();

    println!("Input devices:");
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                println!("  {name}");
            }
        }
    }

    println!("\nOutput devices:");
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                println!("  {name}");
            }
        }
    }
}
