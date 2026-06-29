//! On-screen text translation via Cerebras Gemma 4 (multimodal), tool-call only.
//!
//! Translation is **incremental** to avoid jitter: instead of recreating every
//! overlay each frame, each call is told which overlays are currently shown and
//! returns a single forced `update_overlays` tool call with `add` (newly-appeared
//! foreign text) and `keep` (the ids of currently-shown overlays whose source
//! text is *still visible*). The caller drops everything not kept — so a big
//! scene change, where the model keeps nothing, clears the stale overlays
//! automatically. Reporting what's still present is far more reliable for a small
//! model than reasoning about what disappeared. Existing overlays are never
//! repositioned, so stable text stays put. We parse only the tool arguments (a
//! fixed schema is more reliable than free-form output), with a lenient fallback
//! that strips ```json fences and surrounding prose.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::CerebrasConfig;

/// A live overlay: stable `id`, normalized [0,1] top-left and width, and the
/// translated text. Shared by the render view and the per-frame model context.
/// (Height isn't kept — the rendered box sizes to the text vertically.)
#[derive(Debug, Clone)]
pub struct Overlay {
    pub id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub text: String,
}

/// The per-frame update the model returns: new overlays to add, and the ids of
/// currently-shown overlays whose source text is *still visible* (everything not
/// kept is dropped by the caller).
#[derive(Debug, Default)]
pub struct OverlayUpdate {
    pub add: Vec<NewOverlay>,
    pub keep: Vec<u64>,
}

/// A newly-appeared overlay. No id yet — the caller assigns one on receipt.
#[derive(Debug, Clone, Deserialize)]
pub struct NewOverlay {
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub w: f32,
    #[serde(default)]
    pub h: f32,
    pub text: String,
}

pub struct ScreenTranslator {
    client: reqwest::Client,
    config: CerebrasConfig,
    system_prompt: String,
}

impl ScreenTranslator {
    pub fn new(config: CerebrasConfig, language: &str) -> Self {
        let system_prompt = format!(
            "You maintain a set of live translation overlays drawn on top of the user's screen. \
             The user reads {language}. On each call you get a fresh screenshot and the list of \
             overlays currently shown. Find on-screen text that is NOT already in {language} and \
             translate it into {language}, then call the `update_overlays` tool:\n\
             - `add`: one entry per foreign text run that is visible now but does NOT already have \
             an overlay in the current list. Give x,y (the top-left corner) and w,h (the size) as \
             normalized floats in [0,1] relative to the image, plus the translated `text`.\n\
             - `keep`: the ids of overlays in the current list whose original (untranslated) text \
             is STILL visible on screen, in roughly the same place. Any current overlay you do not \
             list in `keep` will be removed, so if an overlay's source text is gone simply leave \
             its id out. If the whole screen changed, keep none.\n\
             Do NOT re-add text that already has an overlay, and do NOT move existing overlays — \
             leave stable text where it is.\n\
             Ignore operating-system and application chrome: window title bars, the clock, the \
             taskbar/dock, the system tray, battery/Wi-Fi/network/volume indicators, the browser \
             address bar, tabs and toolbar, menu bars, scrollbars, and UI buttons or icons. \
             Translate only document and content text. Do not write any prose."
        );
        Self {
            client: reqwest::Client::new(),
            config,
            system_prompt,
        }
    }

    /// Ask the model for the delta against the currently-shown `current` overlays.
    pub async fn diff(&self, image_data_uri: &str, current: &[Overlay]) -> Result<OverlayUpdate> {
        // Cache layout: the static system prompt + tool schema lead, so their
        // tokens form a stable prefix Cerebras can cache across ticks. The dynamic
        // `describe` list and the always-changing image come last (uncacheable).
        // `prompt_cache_key` routes every overlay request to the same prefix cache.
        let body = json!({
            "model": self.config.model,
            "messages": [
                { "role": "system", "content": self.system_prompt },
                { "role": "user", "content": [
                    { "type": "image_url", "image_url": { "url": image_data_uri } },
                    { "type": "text", "text": describe(current) }
                ]}
            ],
            "tools": [ tool_schema() ],
            "tool_choice": { "type": "function", "function": { "name": "update_overlays" } },
            "prompt_cache_key": "joya-overlay",
            "reasoning_effort": self.config.reasoning_effort,
            "stream": false,
        });

        let url = format!(
            "{}/chat/completions",
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
            anyhow::bail!("screen translation failed ({status}): {detail}");
        }

        let parsed: ChatResponse = resp.json().await?;
        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .context("model returned no choices")?;
        extract_update(&message)
    }
}

/// The frame context: the overlays currently on screen, so the model can tell
/// what's new and what's gone without us re-sending positions to translate.
fn describe(current: &[Overlay]) -> String {
    if current.is_empty() {
        return "No overlays are currently shown. Add overlays for any foreign text you see."
            .into();
    }
    let mut s = String::from("Overlays currently shown (id: text @ x,y):\n");
    for o in current {
        s.push_str(&format!(
            "#{}: {:?} @ {:.2},{:.2}\n",
            o.id, o.text, o.x, o.y
        ));
    }
    s.push_str(
        "Add overlays only for foreign text not already in this list; remove the ids whose text \
         is gone.",
    );
    s
}

fn tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "update_overlays",
            "description": "Add new translation overlays and confirm which existing ones are still visible.",
            "parameters": {
                "type": "object",
                "properties": {
                    "add": {
                        "type": "array",
                        "description": "Foreign text runs that just appeared and have no overlay yet.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x": { "type": "number", "description": "Left edge, 0..1 of image width." },
                                "y": { "type": "number", "description": "Top edge, 0..1 of image height." },
                                "w": { "type": "number", "description": "Width, 0..1 of image width." },
                                "h": { "type": "number", "description": "Height, 0..1 of image height." },
                                "text": { "type": "string", "description": "The translated text." }
                            },
                            "required": ["x", "y", "text"]
                        }
                    },
                    "keep": {
                        "type": "array",
                        "description": "Ids of currently-shown overlays whose source text is still visible. Any id not listed is removed.",
                        "items": { "type": "integer" }
                    }
                },
                "required": ["add", "keep"]
            }
        }
    })
}

/// Prefer the structured tool call; fall back to lenient JSON in `content`. Fails
/// (rather than returning an empty update) when nothing parses, so the caller
/// leaves the existing overlays untouched instead of wiping them on a glitch.
fn extract_update(message: &ChoiceMessage) -> Result<OverlayUpdate> {
    let raw = message
        .tool_calls
        .first()
        .map(|c| c.function.arguments.clone())
        .or_else(|| message.content.clone())
        .context("model returned neither a tool call nor content")?;
    let parsed =
        parse_update(&raw).with_context(|| format!("could not parse model output: {raw}"))?;
    Ok(OverlayUpdate {
        add: parsed
            .add
            .into_iter()
            .filter(|n| !n.text.trim().is_empty())
            .map(clamp)
            .collect(),
        keep: parsed.keep.iter().filter_map(value_to_id).collect(),
    })
}

/// Lenient parse: handle ```json fences / prose by extracting the outermost JSON
/// object before deserializing.
fn parse_update(raw: &str) -> Option<RawUpdate> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    serde_json::from_str(raw.get(start..=end)?).ok()
}

/// Accept ids whether the model emits them as numbers or strings.
fn value_to_id(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn clamp(mut n: NewOverlay) -> NewOverlay {
    n.x = n.x.clamp(0.0, 1.0);
    n.y = n.y.clamp(0.0, 1.0);
    n.w = n.w.clamp(0.0, 1.0);
    n.h = n.h.clamp(0.0, 1.0);
    n
}

#[derive(Deserialize)]
struct RawUpdate {
    #[serde(default)]
    add: Vec<NewOverlay>,
    #[serde(default)]
    keep: Vec<Value>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Deserialize)]
struct ToolCall {
    function: ToolCallFunction,
}

#[derive(Deserialize)]
struct ToolCallFunction {
    #[serde(default)]
    arguments: String,
}
