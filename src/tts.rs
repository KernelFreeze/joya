//! Text-to-speech via Mistral Voxtral (`POST /audio/speech`).
//!
//! We request WAV so the output is self-describing (sample rate + format come
//! from the RIFF header), then decode it to mono f32 for cpal playback. The
//! provider returns either raw audio bytes or JSON with a base64 `audio_data`
//! field; both are handled.

use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;

use crate::config::MistralConfig;

/// Decoded speech ready for playback.
pub struct TtsAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

pub struct Tts {
    client: reqwest::Client,
    config: MistralConfig,
    /// Language this pipeline speaks into (its target language), used to pick a
    /// voice from `config.tts_voices`. Voxtral voices are language-specific.
    language: String,
    /// Lazily discovered voice id, cached after the first `/audio/voices` lookup
    /// when no configured voice applies. The Voxtral TTS API has no default
    /// voice: a `voice` must be sent on every request, so we pick the first
    /// available one and remember it rather than re-querying per utterance.
    resolved_voice: OnceLock<Option<String>>,
}

impl Tts {
    pub fn new(config: MistralConfig, language: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            language,
            resolved_voice: OnceLock::new(),
        }
    }

    /// Returns the voice id to send on the next request. A per-language voice in
    /// `tts_voices` wins, then the `tts_voice` default; otherwise the first id
    /// from `/audio/voices` is fetched once and cached. `None` means discovery
    /// failed (the caller gets the API's own error message, the most actionable
    /// one).
    async fn resolve_voice(&self) -> Option<String> {
        if let Some(voice) = self.config.tts_voices.get(&self.language) {
            let voice = voice.trim();
            if !voice.is_empty() {
                return Some(voice.to_owned());
            }
        }

        let configured = self.config.tts_voice.trim();
        if !configured.is_empty() {
            return Some(configured.to_owned());
        }

        if let Some(cached) = self.resolved_voice.get() {
            return cached.clone();
        }

        let url = format!(
            "{}/audio/voices",
            self.config.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let value: serde_json::Value = resp.json().await.ok()?;
        let first = value
            .get("items")
            .and_then(|items| items.get(0))
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        // `set` may race another caller; whichever wins, the result is the same.
        let _ = self.resolved_voice.set(first.clone());
        first
    }

    pub async fn synthesize(&self, text: &str) -> anyhow::Result<TtsAudio> {
        // The Voxtral TTS API requires a voice on every request (there is no
        // server-side default). Resolve one from config or auto-discover it.
        let voice = self.resolve_voice().await.ok_or_else(|| {
            anyhow::anyhow!(
                "No TTS voice configured and none could be auto-discovered. \
                 Set `mistral.tts_voice`, or `mistral.tts_voices.{}`, to a voice \
                 id (list them with GET `{}/audio/voices`).",
                self.language,
                self.config.base_url.trim_end_matches('/')
            )
        })?;

        let body = json!({
            "model": self.config.tts_model,
            "input": text,
            "response_format": "wav",
            "voice": voice,
        });

        let url = format!(
            "{}/audio/speech",
            self.config.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("Voxtral TTS failed ({status}): {detail}");
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = resp.bytes().await?;

        let audio = if content_type.contains("json") || starts_with_json(&body) {
            let value: serde_json::Value = serde_json::from_slice(&body)?;
            let encoded = value
                .get("audio_data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("TTS JSON response missing audio_data"))?;
            BASE64.decode(encoded)?
        } else {
            body.to_vec()
        };

        decode_wav(&audio)
    }
}

fn starts_with_json(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .map(|b| *b == b'{')
        .unwrap_or(false)
}

/// Minimal RIFF/WAVE decoder → mono f32. Supports PCM16 and IEEE float32.
fn decode_wav(bytes: &[u8]) -> anyhow::Result<TtsAudio> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("TTS response is not a WAV file");
    }

    let mut pos = 12;
    let mut format_tag = 1u16;
    let mut channels = 1u16;
    let mut sample_rate = 24000u32;
    let mut bits = 16u16;
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());
        match id {
            b"fmt " if body_end - body_start >= 16 => {
                let f = &bytes[body_start..body_end];
                format_tag = u16::from_le_bytes([f[0], f[1]]);
                channels = u16::from_le_bytes([f[2], f[3]]).max(1);
                sample_rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                bits = u16::from_le_bytes([f[14], f[15]]);
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }
        // Chunks are word-aligned (padded to even size).
        pos = body_start + size + (size & 1);
    }

    let data = data.ok_or_else(|| anyhow::anyhow!("WAV missing data chunk"))?;
    let ch = channels as usize;

    let interleaved: Vec<f32> = match (format_tag, bits) {
        (1, 16) => data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect(),
        (3, 32) => data
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        (fmt, bits) => anyhow::bail!("Unsupported WAV format tag {fmt} / {bits} bits"),
    };

    let samples = if ch == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    };

    Ok(TtsAudio {
        samples,
        sample_rate,
    })
}
