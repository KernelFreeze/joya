//! Translation via Cerebras Gemma 4 (OpenAI-compatible chat completions).
//!
//! One non-streaming request per utterance with `reasoning_effort` enabled, so
//! the round trip stays fast while still exercising Gemma 4's reasoning.

use serde::Deserialize;
use serde_json::json;

use crate::config::{CerebrasConfig, LanguageConfig};

/// A completed translation.
#[derive(Debug, Clone)]
pub struct Translation {
    pub translated: String,
}

pub struct Translator {
    client: reqwest::Client,
    config: CerebrasConfig,
    system_prompt: String,
}

impl Translator {
    pub fn new(config: CerebrasConfig, languages: &LanguageConfig) -> Self {
        let from = match &languages.source {
            Some(src) => format!("from {src} "),
            None => String::new(),
        };
        let system_prompt = format!(
            "You are a professional real-time interpreter. Translate the user's message {from}\
             into {target}. Output only the translation — no quotes, transliteration, notes, or \
             explanations. Preserve tone and meaning; keep it natural and concise for speech.",
            from = from,
            target = languages.target,
        );
        Self { client: reqwest::Client::new(), config, system_prompt }
    }

    pub async fn translate(&self, text: &str) -> anyhow::Result<Translation> {
        let body = json!({
            "model": self.config.model,
            "messages": [
                { "role": "system", "content": self.system_prompt },
                { "role": "user", "content": text },
            ],
            "reasoning_effort": self.config.reasoning_effort,
            "stream": false,
        });

        let url = format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'));
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
            anyhow::bail!("Cerebras translation failed ({status}): {detail}");
        }

        let parsed: ChatResponse = resp.json().await?;
        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("Cerebras returned no choices"))?;

        Ok(Translation {
            translated: message.content.unwrap_or_default().trim().to_string(),
        })
    }
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
}
