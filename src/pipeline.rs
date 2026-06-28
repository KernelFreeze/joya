//! Orchestrates the translation pipeline on a dedicated tokio thread.
//!
//! Capture/VAD runs on its own std threads (see `audio`). Here we run the async
//! stages — realtime STT, Cerebras translation, Voxtral TTS — and feed the
//! result to playback. STT and the per-utterance consumer share one
//! current-thread runtime via `join!` so the non-`Send` `Player` can live here.

use std::thread;

use flume::{Receiver, Sender};
use tracing::error;

use crate::audio::Segment;
use crate::config::Config;
use crate::playback::Player;
use crate::stt::{self, SttEvent};
use crate::translate::Translator;
use crate::tts::Tts;

/// Current pipeline stage, surfaced in the UI.
#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Listening,
    Transcribing,
    Translating,
    Speaking,
}

/// Events sent from the pipeline to the GPUI app.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// Live partial transcript of the current utterance.
    SourcePartial(String),
    /// Completed source transcript.
    SourceFinal(String),
    /// Completed translation (with optional Gemma reasoning).
    Translation {
        source: String,
        translated: String,
        reasoning: Option<String>,
    },
    Stage(Stage),
    Error(String),
}

/// Spawns the pipeline thread. Consumes VAD segments and drives the UI channel.
pub fn spawn(config: Config, segments: Receiver<Segment>, ui_tx: Sender<UiEvent>) {
    thread::Builder::new()
        .name("joya-pipeline".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build pipeline runtime");
            rt.block_on(run(config, segments, ui_tx));
        })
        .expect("failed to spawn pipeline thread");
}

async fn run(config: Config, segments: Receiver<Segment>, ui_tx: Sender<UiEvent>) {
    let (stt_tx, stt_rx) = flume::unbounded::<SttEvent>();
    let stt_fut = stt::run(config.mistral.clone(), segments, stt_tx);
    let consume_fut = consume(config, stt_rx, ui_tx);
    // Joined (not spawned) so `Player` need not be `Send`.
    futures::future::join(stt_fut, consume_fut).await;
}

async fn consume(config: Config, stt_rx: Receiver<SttEvent>, ui_tx: Sender<UiEvent>) {
    let translator = Translator::new(config.cerebras.clone(), &config.languages);
    let tts = Tts::new(config.mistral.clone());
    let player = Player::new(
        config.audio.mic_sink.as_deref(),
        config.audio.playback_device.as_deref(),
        config.audio.monitor_self,
    );

    while let Ok(event) = stt_rx.recv_async().await {
        match event {
            SttEvent::Partial(text) => {
                let _ = ui_tx.send(UiEvent::Stage(Stage::Transcribing));
                let _ = ui_tx.send(UiEvent::SourcePartial(text));
            }
            SttEvent::Final(source) => {
                let _ = ui_tx.send(UiEvent::SourceFinal(source.clone()));
                let _ = ui_tx.send(UiEvent::Stage(Stage::Translating));

                match translator.translate(&source).await {
                    Ok(translation) => {
                        let _ = ui_tx.send(UiEvent::Translation {
                            source,
                            translated: translation.translated.clone(),
                            reasoning: translation.reasoning,
                        });
                        let _ = ui_tx.send(UiEvent::Stage(Stage::Speaking));
                        match tts.synthesize(&translation.translated).await {
                            Ok(audio) => player.submit(&audio),
                            Err(e) => {
                                error!("TTS failed: {e}");
                                let _ = ui_tx.send(UiEvent::Error(e.to_string()));
                            }
                        }
                    }
                    Err(e) => {
                        error!("Translation failed: {e}");
                        let _ = ui_tx.send(UiEvent::Error(e.to_string()));
                    }
                }

                let _ = ui_tx.send(UiEvent::Stage(Stage::Listening));
            }
        }
    }
}
