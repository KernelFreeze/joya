//! Plays synthesized speech to one or two output devices: the virtual mic sink
//! (so the other party hears the translation) and, optionally, your own speakers.
//!
//! cuteview had no playback path, so this is new. Each output device gets a cpal
//! stream backed by a shared sample buffer; `submit` resamples the TTS audio to
//! the device rate and enqueues it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{error, info, warn};

use crate::tts::TtsAudio;

struct Sink {
    buffer: Arc<Mutex<VecDeque<f32>>>,
    sample_rate: u32,
    _stream: cpal::Stream,
}

/// Holds the output streams for the app lifetime. Not `Send` (cpal streams are
/// platform-bound), so it must live on the thread that created it.
pub struct Player {
    sinks: Vec<Sink>,
}

impl Player {
    /// Opens a sink for `mic_sink` (when set) and, when `monitor_self`, for
    /// `playback_device` (default output when `None`).
    pub fn new(
        mic_sink: Option<&str>,
        playback_device: Option<&str>,
        monitor_self: bool,
    ) -> Self {
        let host = cpal::default_host();
        let mut sinks = Vec::new();

        if let Some(name) = mic_sink {
            match open_sink(&host, Some(name)) {
                Ok(sink) => {
                    info!("Playback: virtual mic sink '{name}' @ {}Hz", sink.sample_rate);
                    sinks.push(sink);
                }
                Err(e) => error!("Failed to open mic sink '{name}': {e}"),
            }
        }

        if monitor_self {
            match open_sink(&host, playback_device) {
                Ok(sink) => {
                    info!("Playback: self-monitor @ {}Hz", sink.sample_rate);
                    sinks.push(sink);
                }
                Err(e) => error!("Failed to open self-monitor output: {e}"),
            }
        }

        if sinks.is_empty() {
            warn!("No playback outputs available — translations will not be audible");
        }

        Self { sinks }
    }

    /// Resamples `audio` to each sink's rate and enqueues it for playback.
    pub fn submit(&self, audio: &TtsAudio) {
        for sink in &self.sinks {
            let resampled = resample_linear(&audio.samples, audio.sample_rate, sink.sample_rate);
            if let Ok(mut buf) = sink.buffer.lock() {
                buf.extend(resampled);
            }
        }
    }
}

#[allow(deprecated)]
fn open_sink(host: &cpal::Host, name: Option<&str>) -> anyhow::Result<Sink> {
    let device = match name {
        Some(name) => host
            .output_devices()?
            .find(|d| d.name().map_or(false, |n| n == name))
            .ok_or_else(|| anyhow::anyhow!("output device '{name}' not found"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("no default output device"))?,
    };

    let supported = device.default_output_config()?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.config();

    let buffer = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let cb_buffer = buffer.clone();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut buf = cb_buffer.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                let sample = buf.pop_front().unwrap_or(0.0);
                for out in frame.iter_mut() {
                    *out = sample;
                }
            }
        },
        move |err| error!("Playback stream error: {err}"),
        None,
    )?;
    stream.play()?;

    Ok(Sink { buffer, sample_rate, _stream: stream })
}

/// Linear-interpolation resampler. Adequate for speech; avoids a stateful
/// streaming resampler for one-shot buffers.
fn resample_linear(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if input.is_empty() || in_rate == out_rate {
        return input.to_vec();
    }
    let ratio = out_rate as f64 / in_rate as f64;
    let out_len = (input.len() as f64 * ratio) as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input[idx.min(last)];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}
