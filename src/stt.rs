//! Voxtral Realtime speech-to-text over WebSocket.
//!
//! - URL: `wss://<host>/v1/audio/transcriptions/realtime?model=<model>`, auth via
//!   `Authorization: Bearer` on the upgrade request.
//! - `session.update` configures the audio format (pcm_s16le @ 16 kHz).
//! - `input_audio.append` carries base64 PCM16; `input_audio.flush` +
//!   `input_audio.end` finalize the utterance.
//! - Responses: `transcription.text.delta` (incremental) and `transcription.done`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flume::{Receiver, Sender};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
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
// Small safety window for the provider to apply `session.update`. In normal use
// this happens while idle, so it does not clip speech or add perceived latency.
const SESSION_SETTLE_MS: u64 = 75;

type RealtimeWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = SplitSink<RealtimeWs, Message>;
type WsRead = SplitStream<RealtimeWs>;

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

async fn open_session(config: &MistralConfig) -> anyhow::Result<(WsWrite, WsRead)> {
    let url = realtime_url(&config.base_url, &config.stt_model);
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.api_key).parse()?,
    );

    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, read) = ws.split();

    let session_update = json!({
        "type": "session.update",
        "session": {
            "audio_format": { "encoding": "pcm_s16le", "sample_rate": 16000 },
            "target_streaming_delay_ms": config.stt_target_delay_ms,
        }
    })
    .to_string();
    write.send(Message::Text(session_update.into())).await?;
    tokio::time::sleep(Duration::from_millis(SESSION_SETTLE_MS)).await;
    info!("Realtime STT session opened and configured");

    Ok((write, read))
}

async fn replace_session(
    config: &MistralConfig,
    write: &mut WsWrite,
    read: &mut WsRead,
) -> anyhow::Result<()> {
    let (new_write, new_read) = open_session(config).await?;
    *write = new_write;
    *read = new_read;
    Ok(())
}

/// Runs the realtime STT loop until the segment channel closes. Each utterance
/// gets its own WebSocket session, but the next session is opened while idle so
/// it is ready before the first speech frames arrive.
pub async fn run(config: MistralConfig, seg_rx: Receiver<Segment>, events: Sender<SttEvent>) {
    loop {
        if let Err(e) = transcribe_utterance(&config, &seg_rx, &events).await {
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
    events: &Sender<SttEvent>,
) -> anyhow::Result<()> {
    let (mut write, mut read) = open_session(config).await?;
    info!("Realtime STT session ready; waiting for speech");

    // Block until the first speech frame of the next utterance, while still
    // watching the idle WebSocket so we can reopen it if the provider closes an
    // unused session before the other person speaks.
    let first_frame = loop {
        tokio::select! {
            seg = seg_rx.recv_async() => {
                match seg {
                    Ok(Segment::Speech(frame)) => break frame,
                    Ok(Segment::EndOfUtterance) => continue,
                    Err(_) => return Ok(()),
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if is_error_event(text.as_str()) {
                            warn!("Realtime STT provider error while idle: {text}; reopening session");
                            replace_session(config, &mut write, &mut read).await?;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            if is_error_event(text) {
                                warn!("Realtime STT provider error while idle: {text}; reopening session");
                                replace_session(config, &mut write, &mut read).await?;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        warn!("Realtime STT session closed while idle; reopening session");
                        replace_session(config, &mut write, &mut read).await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!("Realtime STT idle session read failed: {e}; reopening session");
                        replace_session(config, &mut write, &mut read).await?;
                    }
                }
            }
        }
    };

    if let Err(e) = write
        .send(Message::Text(append_message(&first_frame).into()))
        .await
    {
        warn!("Realtime STT session was not writable at speech start: {e}; reopening session");
        replace_session(config, &mut write, &mut read).await?;
        write
            .send(Message::Text(append_message(&first_frame).into()))
            .await?;
    }

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

fn is_error_event(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|payload| {
            payload
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("error")
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
