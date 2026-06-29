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
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use flume::{Receiver, Sender};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Fft, FixedSync, Resampler};
use tracing::{error, info, warn};
use voice_activity_detector::VoiceActivityDetector;

use crate::config::Direction;

const VAD_CHUNK_SIZE: usize = 512;
const SPEECH_THRESHOLD: f32 = 0.5;
// 32 chunks * 512 samples / 16000 Hz ≈ 1s of silence before end-of-utterance.
const SILENCE_CHUNKS_FOR_RESET: usize = 32;
// Pre-speech chunks replayed when speech starts, so the first words aren't clipped.
const LOOKBACK_CHUNKS: usize = 8;
// Log the input level this often so you can see whether samples are arriving
// and whether the mic is live but quiet (below VAD threshold) vs. truly silent.
const LEVEL_LOG_INTERVAL_SECS: f64 = 0.5;
// If the RMS stays below this for a while, warn — a totally dead input (no
// samples, or a muted/loopback device) reads ~0.0.
const DEAD_LEVEL: f32 = 0.002;
// Below this but above DEAD: samples are arriving but the mic may be muted or
// the wrong device. The VAD threshold is 0.5; if peak input never gets close,
// speech will never trigger.
const QUIET_LEVEL: f32 = 0.02;

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

/// Short lowercase label for log lines, mirroring the UI's direction pill.
fn direction_tag(direction: Direction) -> &'static str {
    match direction {
        Direction::Relay => "relay",
        Direction::SelfMode => "self",
    }
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

fn vad_worker(
    direction: Direction,
    native_sample_rate: u32,
    audio_rx: Receiver<Vec<f32>>,
    seg_tx: Sender<Segment>,
) {
    let mut vad = make_vad();
    let mut resampler = make_resampler(native_sample_rate);

    let mut raw_buffer: VecDeque<f32> = VecDeque::new();
    let mut resampled_buffer: VecDeque<f32> = VecDeque::new();
    let mut is_speaking = false;
    let mut silence_counter: usize = 0;
    let mut lookback: VecDeque<Vec<f32>> = VecDeque::with_capacity(LOOKBACK_CHUNKS);

    // Input-level metering. We accumulate sum-of-squares and peak over a window
    // and log every LEVEL_LOG_INTERVAL_SECS. This is the fastest way to tell
    // "no samples at all" (dead device/wrong default) from "samples but quiet"
    // (muted mic, wrong source) from "loud enough but VAD not firing"
    // (threshold too high, model issue). Tagged with the direction so dual-mode
    // logs read unambiguously.
    let mut last_level_log = Instant::now();
    let mut sum_sq: f64 = 0.0;
    let mut peak: f32 = 0.0;
    let mut sample_count: u64 = 0;
    let mut quiet_warned = false;
    let mut dead_warned = false;

    while let Ok(chunk) = audio_rx.recv() {
        // Accumulate level stats before the VAD consumes the samples.
        for &s in &chunk {
            let abs = s.abs();
            sum_sq += (s as f64) * (s as f64);
            if abs > peak {
                peak = abs;
            }
        }
        sample_count += chunk.len() as u64;

        if last_level_log.elapsed().as_secs_f64() >= LEVEL_LOG_INTERVAL_SECS {
            let rms = if sample_count > 0 {
                (sum_sq / sample_count as f64).sqrt() as f32
            } else {
                0.0
            };
            // Drop the peak back down so it reflects this window, not all time.
            // (We keep it simple: report the window peak then reset.)
            let window_peak = peak;
            let tag = direction_tag(direction);
            if sample_count == 0 {
                if !dead_warned {
                    warn!(
                        "[{tag}] no audio samples in {LEVEL_LOG_INTERVAL_SECS:.1}s — \
                         capture device is producing no data (wrong device? paused? closed?)"
                    );
                    dead_warned = true;
                }
            } else if window_peak < DEAD_LEVEL {
                if !quiet_warned {
                    warn!(
                        "[{tag}] input appears dead: rms={rms:.5} peak={window_peak:.5} \
                         (expected speech to exceed ~0.1). Muted mic, wrong default device, \
                         or a loopback/monitor with nothing playing?"
                    );
                    quiet_warned = true;
                }
            } else {
                info!(
                    "[{tag}] input level: rms={rms:.4} peak={window_peak:.4}{}",
                    if window_peak < QUIET_LEVEL { " (quiet — below typical speech)" } else { "" },
                );
                quiet_warned = false;
                dead_warned = false;
            }
            last_level_log = Instant::now();
            sum_sq = 0.0;
            peak = 0.0;
            sample_count = 0;
        }

        raw_buffer.extend(chunk.iter());
        resample_into(&mut resampler, &mut raw_buffer, &mut resampled_buffer);

        while resampled_buffer.len() >= VAD_CHUNK_SIZE {
            let vad_chunk: Vec<f32> = resampled_buffer.drain(..VAD_CHUNK_SIZE).collect();
            let speech_prob = vad.predict(vad_chunk.iter().copied());

            if speech_prob >= SPEECH_THRESHOLD {
                if !is_speaking {
                    is_speaking = true;
                    info!(
                        "[{tag}] Speech started ({} lookback chunks)",
                        lookback.len(),
                        tag = direction_tag(direction),
                    );
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
                    info!("[{tag}] Speech ended, flushing utterance",
                        tag = direction_tag(direction));
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
    info!("[{tag}] Audio channel closed, VAD worker exiting", tag = direction_tag(direction));
}

#[allow(deprecated)]
fn start_capture(
    direction: Direction,
    device: cpal::Device,
) -> anyhow::Result<CaptureHandle> {
    let config = device.default_input_config()?;
    let native_sample_rate = config.sample_rate();
    let channels = config.channels() as usize;

    let tag = direction_tag(direction);
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let device_id = device.id().map(|id| id.1).unwrap_or_default();
    info!(
        "[{tag}] Capture device: '{device_name}'{} rate={native_sample_rate}, ch={channels} \
         (direction={direction:?})",
        if device_id.is_empty() || device_id == device_name {
            String::new()
        } else {
            format!(" [id={device_id}]")
        },
    );

    let (audio_tx, audio_rx) = flume::bounded::<Vec<f32>>(64);
    let (seg_tx, seg_rx) = flume::unbounded::<Segment>();
    spawn(move || vad_worker(direction, native_sample_rate, audio_rx, seg_tx));

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
        move |err| error!("[{tag}] Audio stream error: {err}", tag = direction_tag(direction)),
        None,
    )?;

    stream.play()?;
    std::mem::forget(stream);

    Ok(CaptureHandle { segments: seg_rx, paused })
}

/// Start capturing the device named `device_name`, or the default device for
/// the active `direction` when `None` — the default output (your headphones) in
/// `relay` mode, the default input (your microphone) in `self` mode. Output
/// (monitor) devices are searched alongside inputs so a headphone/speaker
/// monitor can be captured directly in `relay` mode, and a real microphone in
/// `self` mode.
///
/// `device_name` is matched against both the human-readable name (cpal's
/// `Device::name`, which on PipeWire is the `node.nick` and is often duplicated
/// across several nodes on the same card — e.g. every Scarlett Solo input shows
/// up as "Scarlett Solo USB") *and* the unique device id (cpal's `Device::id`,
/// the underlying PipeWire `node.name` like
/// `alsa_input.usb-Focusrite_...HiFi__Mic1__source`). When several devices share
/// a friendly name, set `capture_device` to the id printed by `list-devices` to
/// pick the exact one.
#[allow(deprecated)]
pub fn capture(device_name: Option<&str>, direction: Direction) -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let tag = direction_tag(direction);
    let device = match device_name {
        Some(name) => {
            info!("[{tag}] Resolving device '{name}'…");
            host.input_devices()?
                .chain(host.output_devices()?)
                .find(|d| device_matches(d, name))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Audio device '{name}' not found. Run with `list-devices` to see options \
                         (match is against either the friendly name or the unique id)."
                    )
                })?
        }
        None => match direction {
            Direction::Relay => {
                // The "input" config of an output device is its monitor — what's
                // playing out of the headphones.
                let dev = host
                    .default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("No default output device found"))?;
                info!(
                    "[{tag}] No capture_device set; using default OUTPUT device \
                     (headphone monitor) for relay capture."
                );
                dev
            }
            Direction::SelfMode => {
                let dev = host
                    .default_input_device()
                    .ok_or_else(|| anyhow::anyhow!("No default input device found"))?;
                info!(
                    "[{tag}] No capture_device set; using default INPUT device \
                     (microphone) for self capture."
                );
                dev
            }
        },
    };
    start_capture(direction, device)
}

/// True if `device`'s friendly name *or* its unique id equals `name`. Both are
/// printed by `list_audio_devices` so the user can target a specific node when
/// several share the same friendly name.
#[allow(deprecated)]
fn device_matches(device: &cpal::Device, name: &str) -> bool {
    if device.name().map(|n| n == name).unwrap_or(false) {
        return true;
    }
    // `Device::id` is the unique node name on PipeWire (e.g.
    // `alsa_input.usb-Focusrite_...HiFi__Mic1__source`).
    device.id().map(|id| id.1 == name).unwrap_or(false)
}

#[allow(deprecated)]
pub fn list_audio_devices() {
    let host = cpal::default_host();
    let default_input = host.default_input_device().and_then(|d| d.name().ok());
    let default_output = host.default_output_device().and_then(|d| d.name().ok());

    println!(
        "Input devices{}",
        default_input.as_deref().map(|n| format!(" (default: {n})")).unwrap_or_default()
    );
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                let marker = if default_input.as_deref() == Some(name.as_str()) {
                    "  <- default"
                } else {
                    ""
                };
                let id = device.id().map(|id| id.1).unwrap_or_default();
                println!("  {name}{marker}");
                if !id.is_empty() && id != name {
                    println!("    id: {id}");
                }
            }
        }
    }

    println!(
        "\nOutput devices{}",
        default_output.as_deref().map(|n| format!(" (default: {n})")).unwrap_or_default()
    );
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                let marker = if default_output.as_deref() == Some(name.as_str()) {
                    "  <- default"
                } else {
                    ""
                };
                let id = device.id().map(|id| id.1).unwrap_or_default();
                println!("  {name}{marker}");
                if !id.is_empty() && id != name {
                    println!("    id: {id}");
                }
            }
        }
    }

    println!("\nIn `relay` mode, `capture_device: null` uses the default OUTPUT (headphone monitor).");
    println!("In `self` mode,   `capture_device: null` uses the default INPUT  (microphone).");
    println!("When several devices share a name (common on PipeWire — e.g. multiple Scarlett");
    println!("inputs all read \"Scarlett Solo USB\"), set `capture_device` to the `id:` line above");
    println!("to target the exact node.");
}
