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

/// Which side of the call Joya translates for. Each direction is an independent
/// pipeline (capture → STT → Gemma → TTS → playback), and **both can run at
/// once** so the other party's speech is translated for you *and* your speech is
/// translated for them simultaneously. Color-coded in the overlay: `relay` is
/// gold, `self` is teal.
///
/// Languages are framed from *your* point of view: `languages.source` is the
/// language you speak, `languages.target` is the language the other party speaks.
/// `self` translates `source → target` (you → them); `relay` translates the
/// swapped pair, `target → source` (them → you).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Capture the other party's voice from an output/monitor device (your
    /// headphones), translate it into your language, and play it to your
    /// headphones so *you* hear it. Flow: **headphones → STT → Gemma → TTS →
    /// headphones**.
    #[serde(rename = "relay")]
    #[default]
    Relay,
    /// Capture your own voice from a microphone, translate it into the other
    /// party's language, and play it into the virtual mic sink so *they* hear it
    /// (and, when `output.monitor_self`, on your own headphones too). Flow:
    /// **microphone → STT → Gemma → TTS → mic (+ headphones when monitoring)**.
    ///
    /// Watch for feedback: if your microphone can hear your playback device, the
    /// translated speech gets re-captured and Joya chases its own tail. Use
    /// headphones, or route playback to a device the mic can't hear.
    #[serde(rename = "self")]
    SelfMode,
}

/// Audio routing. Joya can run one or two directions at once: `relay` (capture
/// the other party from your headphones, play the translation to your
/// headphones) and `self` (capture your microphone, play the translation to the
/// call). Each is independently enabled, so dual mode is just both `enabled:
/// true`. When both run, the two pipelines feed the same overlay, color-coded by
/// direction.
///
/// Output sinks/playback devices live in [`OutputConfig`], shared by both
/// directions — there's no per-direction sink setup. A single virtual mic sink
/// feeds the call (for `self`), and a single playback device plays to your
/// headphones (for `relay`, and for `self` when `monitor_self`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AudioConfig {
    /// Relay direction: headphones → STT → Gemma → TTS → headphones. Enabled by
    /// default — this is the original Joya flow.
    #[serde(default = "default_relay")]
    pub relay: RelayConfig,
    /// Self direction: microphone → STT → Gemma → TTS → mic (+ headphones when
    /// `output.monitor_self`). Disabled by default.
    #[serde(default, rename = "self")]
    pub self_mode: SelfConfig,
    /// Shared output routing (mic sink + playback device) for both directions.
    #[serde(default)]
    pub output: OutputConfig,
}

/// Relay-direction capture: the other party's voice coming through your
/// headphones (or an output monitor). The translation plays to
/// `output.playback_device` — see [`OutputConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RelayConfig {
    /// Enable this direction. `false` skips spawning its pipeline.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Where the other party's voice is captured from. For loop-free `relay`,
    /// set this to `call_remote` — the isolation sink created by
    /// `scripts/setup-virtual-mic.sh`, whose monitor cpal exposes under the sink
    /// node name (no `.monitor` suffix) — so `relay` hears only the other party
    /// and never Joya's own TTS. `null` uses the default output's monitor (your
    /// headphones), which also captures Joya's TTS and will loop. Run
    /// `list-devices` for names/ids.
    #[serde(default)]
    pub capture_device: Option<String>,
}

/// Self-direction capture: your microphone. The translation plays to
/// `output.mic_sink` (the call) and, when `output.monitor_self`, to
/// `output.playback_device` too — see [`OutputConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SelfConfig {
    /// Enable this direction. `false` (the default) skips spawning its pipeline.
    #[serde(default)]
    pub enabled: bool,
    /// Microphone to capture your voice from. `null` uses the default input
    /// device. Run `list-devices` for names.
    #[serde(default)]
    pub capture_device: Option<String>,
}

/// Shared output routing for translations, independent of which direction
/// produced them. Both directions draw from this one config — there's no
/// per-direction sink/playback setup — so a single virtual mic sink and a single
/// local playback device serve the whole app, letting both the other party (via
/// the call) and yourself (via your headphones) hear translations.
///
/// Routing by direction:
/// - `self` (you → other party): plays to `mic_sink` (the call), and, when
///   `monitor_self` is set, also to `playback_device` so you hear your own
///   outgoing translation.
/// - `relay` (other party → you): plays to `playback_device` (your headphones).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutputConfig {
    /// Virtual null-sink to feed translations into the call, e.g. `joya_mic`. The
    /// sink's monitor is what you select as the microphone in your call app so the
    /// other party hears the `self` direction's translation. `null` disables
    /// feeding the call — the `self` direction will have no audible output unless
    /// `monitor_self` is set.
    #[serde(default)]
    pub mic_sink: Option<String>,
    /// Local output device (your headphones/speakers) for translations you hear:
    /// the `relay` direction always, and the `self` direction when `monitor_self`
    /// is set. `null` uses the default output device.
    #[serde(default)]
    pub playback_device: Option<String>,
    /// Also play the `self` direction's outgoing translation on `playback_device`
    /// so you hear what's being sent to the call. Use headphones to avoid the mic
    /// re-capturing the playback (feedback).
    #[serde(default = "default_true")]
    pub monitor_self: bool,
}

/// Source/target languages for translation, framed from *your* point of view.
/// `source` is the language you speak (the `self` direction's input); `target`
/// is the language the other party speaks (the `relay` direction's input). The
/// `self` direction translates `source → target`; `relay` translates the swapped
/// pair, `target → source`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LanguageConfig {
    /// The language *you* speak — the `self` direction's input. `null` lets the
    /// model auto-detect your speech. Note: when `null`, the `relay` direction
    /// can't know your language to translate *into*, so it falls back to the
    /// default target language — set `source` explicitly for two-way use.
    #[serde(default)]
    pub source: Option<String>,
    /// The language the *other party* speaks — the `relay` direction's input,
    /// and the language the `self` direction translates into to speak to them.
    #[serde(default = "default_target")]
    pub target: String,
}

impl LanguageConfig {
    /// The reverse direction: your language and the other party's swap places.
    /// Used by the `relay` direction so it translates the other party's speech
    /// (their language) into your language — the mirror of `self`.
    pub fn swapped(&self) -> LanguageConfig {
        LanguageConfig {
            source: Some(self.target.clone()),
            target: self.source.clone().unwrap_or_else(default_target),
        }
    }
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
    /// TTS voice id. The Voxtral TTS API requires a voice on every request
    /// (there is no server-side default). When left empty, Joya auto-selects
    /// the first id returned by the provider's `/audio/voices` endpoint on
    /// the first utterance and caches it. Set this to pin a specific voice.
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
            relay: default_relay(),
            self_mode: SelfConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capture_device: None,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mic_sink: None,
            playback_device: None,
            monitor_self: true,
        }
    }
}

fn default_relay() -> RelayConfig {
    RelayConfig::default()
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
