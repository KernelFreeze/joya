//! Voxtral Realtime speech-to-text over WebSocket.
//!
//! Protocol mirrors the working TalkingMobs implementation:
//! - URL: `wss://<host>/v1/audio/transcriptions/realtime?model=<model>`, auth via
//!   `Authorization: Bearer` on the upgrade request.
//! - `session.update` configures the audio format (pcm_s16le @ 16 kHz).
//! - `input_audio.append` carries base64 PCM16; `input_audio.flush` +
//!   `input_audio.end` finalize the utterance.
//! - Responses: `transcription.text.delta` (incremental) and `transcription.done`.
//!
//! One WebSocket session per utterance, driven by the VAD's end-of-utterance marker.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flume::{Receiver, Sender};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{error, info, warn};

use crate::audio::Segment;
use crate::config::MistralConfig;

/// Transcription output from the realtime stream.
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// Live partial transcript for the in-flight utterance.
    Partial(String),
    /// Completed utterance, ready to translate.
    Final(String),
}

const FLUSH: &str = r#"{"type":"input_audio.flush"}"#;
const END: &str = r#"{"type":"input_audio.end"}"#;

fn realtime_url(base: &str, model: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/audio/transcriptions/realtime?model={model}")
}

fn append_message(frame: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(frame.len() * 2);
    for sample in frame {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    json!({ "type": "input_audio.append", "audio": BASE64.encode(&bytes) }).to_string()
}

/// Runs the realtime STT loop until the segment channel closes. Each utterance
/// opens its own WebSocket session.
pub async fn run(config: MistralConfig, seg_rx: Receiver<Segment>, events: Sender<SttEvent>) {
    loop {
        // Block until the first speech frame of the next utterance.
        let first = loop {
            match seg_rx.recv_async().await {
                Ok(Segment::Speech(frame)) => break frame,
                Ok(Segment::EndOfUtterance) => continue,
                Err(_) => return,
            }
        };

        if let Err(e) = transcribe_utterance(&config, &seg_rx, first, &events).await {
            error!("Realtime STT session failed: {e}");
            // Resync: drop any in-flight frames up to the next end-of-utterance.
            while let Ok(seg) = seg_rx.recv_async().await {
                if matches!(seg, Segment::EndOfUtterance) {
                    break;
                }
            }
        }
    }
}

async fn transcribe_utterance(
    config: &MistralConfig,
    seg_rx: &Receiver<Segment>,
    first_frame: Vec<i16>,
    events: &Sender<SttEvent>,
) -> anyhow::Result<()> {
    let url = realtime_url(&config.base_url, &config.stt_model);
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.api_key).parse()?,
    );

    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = ws.split();
    info!("Realtime STT session opened");

    let session_update = json!({
        "type": "session.update",
        "session": {
            "audio_format": { "encoding": "pcm_s16le", "sample_rate": 16000 },
            "target_streaming_delay_ms": config.stt_target_delay_ms,
        }
    })
    .to_string();
    write.send(Message::Text(session_update.into())).await?;
    write.send(Message::Text(append_message(&first_frame).into())).await?;

    let mut full = String::new();
    let mut input_done = false;

    loop {
        tokio::select! {
            seg = seg_rx.recv_async(), if !input_done => {
                match seg {
                    Ok(Segment::Speech(frame)) => {
                        write.send(Message::Text(append_message(&frame).into())).await?;
                    }
                    Ok(Segment::EndOfUtterance) | Err(_) => {
                        write.send(Message::Text(FLUSH.to_string().into())).await?;
                        write.send(Message::Text(END.to_string().into())).await?;
                        input_done = true;
                    }
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if handle_event(text.as_str(), &mut full, events) {
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            if handle_event(text, &mut full, events) {
                                return Ok(());
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // Stream closed without an explicit done — surface what we have.
                        if !full.is_empty() {
                            let _ = events.send(SttEvent::Final(std::mem::take(&mut full)));
                        }
                        return Ok(());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
}

/// Returns true when the utterance is finished (a `transcription.done` or error).
fn handle_event(raw: &str, full: &mut String, events: &Sender<SttEvent>) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("transcription.text.delta") => {
            if let Some(text) = payload.get("text").and_then(Value::as_str) {
                full.push_str(text);
                let _ = events.send(SttEvent::Partial(full.clone()));
            }
            false
        }
        Some("transcription.done") => {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| std::mem::take(full));
            if !text.trim().is_empty() {
                let _ = events.send(SttEvent::Final(text));
            }
            true
        }
        Some("error") => {
            warn!("Realtime STT provider error: {raw}");
            true
        }
        _ => false,
    }
}
