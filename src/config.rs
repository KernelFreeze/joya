use std::env::var;
use std::io;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level Joya configuration, persisted as YAML under the platform config dir.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub languages: LanguageConfig,
    #[serde(default)]
    pub mistral: MistralConfig,
    #[serde(default)]
    pub cerebras: CerebrasConfig,
}

/// Audio routing. Joya listens to one output/monitor device and plays the
/// synthesized translation back to a virtual mic sink (so the other party hears
/// it) and, optionally, to your own speakers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AudioConfig {
    /// Output/monitor device to listen to (the other party's audio). Run with
    /// `list-devices` to see names. `null` uses the default output device.
    #[serde(default)]
    pub capture_device: Option<String>,
    /// Virtual null-sink to play the translation into, e.g. `joya_mic`. The
    /// sink's monitor is what you select as the microphone in your call app.
    /// `null` disables feeding audio back to the call.
    #[serde(default)]
    pub mic_sink: Option<String>,
    /// Also play the translation on your own speakers so you can hear it.
    #[serde(default = "default_true")]
    pub monitor_self: bool,
    /// Device used for self-monitoring. `null` uses the default output device.
    #[serde(default)]
    pub playback_device: Option<String>,
}

/// Source/target languages for the translation step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LanguageConfig {
    /// Spoken language of the other party. `null` lets the model auto-detect.
    #[serde(default)]
    pub source: Option<String>,
    /// Language to translate into and speak back.
    #[serde(default = "default_target")]
    pub target: String,
}

/// Mistral Voxtral endpoints (realtime STT over WebSocket + TTS over REST).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MistralConfig {
    /// API key. Defaults to the `MISTRAL_API_KEY` env var.
    #[serde(default = "default_mistral_key")]
    pub api_key: String,
    /// REST base URL (TTS lives here; the realtime WS URL is derived from it).
    #[serde(default = "default_mistral_base")]
    pub base_url: String,
    /// Voxtral realtime transcription model.
    #[serde(default = "default_stt_model")]
    pub stt_model: String,
    /// Realtime transcription target latency, milliseconds.
    #[serde(default = "default_stt_delay_ms")]
    pub stt_target_delay_ms: u32,
    /// Voxtral TTS model.
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    /// TTS voice id. Empty lets the API pick its default. Discover ids via the
    /// provider's `/audio/voices` endpoint.
    #[serde(default)]
    pub tts_voice: String,
}

/// Cerebras (OpenAI-compatible) endpoint for the Gemma 4 translation step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CerebrasConfig {
    /// API key. Defaults to the `CEREBRAS_API_KEY` env var.
    #[serde(default = "default_cerebras_key")]
    pub api_key: String,
    /// OpenAI-compatible base URL.
    #[serde(default = "default_cerebras_base")]
    pub base_url: String,
    /// Translation model.
    #[serde(default = "default_cerebras_model")]
    pub model: String,
    /// Gemma 4 reasoning effort: `none`, `low`, `medium`, or `high`.
    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
}

fn default_true() -> bool {
    true
}
fn default_target() -> String {
    "English".into()
}
fn default_mistral_key() -> String {
    var("MISTRAL_API_KEY").unwrap_or_default()
}
fn default_mistral_base() -> String {
    "https://api.mistral.ai/v1".into()
}
fn default_stt_model() -> String {
    "voxtral-mini-transcribe-realtime-2602".into()
}
fn default_stt_delay_ms() -> u32 {
    480
}
fn default_tts_model() -> String {
    "voxtral-mini-tts-latest".into()
}
fn default_cerebras_key() -> String {
    var("CEREBRAS_API_KEY").unwrap_or_default()
}
fn default_cerebras_base() -> String {
    "https://api.cerebras.ai/v1".into()
}
fn default_cerebras_model() -> String {
    "gemma-4-31b".into()
}
fn default_reasoning_effort() -> String {
    "low".into()
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            capture_device: None,
            mic_sink: None,
            monitor_self: true,
            playback_device: None,
        }
    }
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            source: None,
            target: default_target(),
        }
    }
}

impl Default for MistralConfig {
    fn default() -> Self {
        Self {
            api_key: default_mistral_key(),
            base_url: default_mistral_base(),
            stt_model: default_stt_model(),
            stt_target_delay_ms: default_stt_delay_ms(),
            tts_model: default_tts_model(),
            tts_voice: String::new(),
        }
    }
}

impl Default for CerebrasConfig {
    fn default() -> Self {
        Self {
            api_key: default_cerebras_key(),
            base_url: default_cerebras_base(),
            model: default_cerebras_model(),
            reasoning_effort: default_reasoning_effort(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio: AudioConfig::default(),
            languages: LanguageConfig::default(),
            mistral: MistralConfig::default(),
            cerebras: CerebrasConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let contents = std::fs::read_to_string(path)?;
        yaml_serde::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), io::Error> {
        let contents = yaml_serde::to_string(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, contents)
    }
}
