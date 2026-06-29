//! Orchestrates the translation pipeline on a dedicated tokio thread.
//!
//! Capture/VAD runs on its own std threads (see `audio`). Here we run the async
//! stages — realtime STT, Cerebras translation, Voxtral TTS — and feed the
//! result to playback. STT and the per-utterance consumer share one
//! current-thread runtime via `join!` so the non-`Send` `Player` can live here.
//!
//! One pipeline per enabled audio direction. When both `relay` and `self` are
//! enabled (dual mode), two independent threads run side by side, each
//! capturing its own device and driving its own STT/translate/TTS chain. Both
//! feed the same UI channel; every event is tagged with its [`Direction`] so the
//! overlay can color-code and interleave them.

use std::thread;

use flume::{Receiver, Sender};
use tracing::error;

use crate::audio::Segment;
use crate::config::{CerebrasConfig, Direction, LanguageConfig, MistralConfig};
use crate::playback::Player;
use crate::stt::{self, SttEvent};
use crate::translate::Translator;
use crate::tts::Tts;

/// Current pipeline stage, surfaced in the UI. Per-direction in dual mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Listening,
    Transcribing,
    Translating,
    Speaking,
}

/// Events sent from a pipeline to the GPUI app. Each carries the [`Direction`]
/// that produced it so the overlay can color-code and track per-direction state
/// (partial transcript, stage) independently.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// Live partial transcript of the current utterance.
    SourcePartial {
        direction: Direction,
        text: String,
    },
    /// Completed source transcript.
    SourceFinal {
        direction: Direction,
        text: String,
    },
    /// Completed translation.
    Translation {
        direction: Direction,
        source: String,
        translated: String,
    },
    Stage {
        direction: Direction,
        stage: Stage,
    },
    Error {
        direction: Direction,
        message: String,
    },
}

/// Everything one pipeline thread needs. Splitting this out from `Config` lets
/// dual mode spawn two pipelines with different devices/languages without either
/// holding a full copy of the other's settings.
pub struct PipelineConfig {
    pub direction: Direction,
    pub mistral: MistralConfig,
    pub cerebras: CerebrasConfig,
    pub languages: LanguageConfig,
    /// Output sinks for this direction, opened by the caller on the main thread
    /// (cpal streams are not `Send`), then handed in already alive.
    pub player: Player,
}

/// Spawns one pipeline thread for `config.direction`. Consumes VAD segments from
/// that direction's capture and drives the shared UI channel.
pub fn spawn(config: PipelineConfig, segments: Receiver<Segment>, ui_tx: Sender<UiEvent>) {
    let direction = config.direction;
    thread::Builder::new()
        .name(format!("joya-pipeline-{direction:?}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build pipeline runtime");
            rt.block_on(run(config, segments, ui_tx));
        })
        .expect("failed to spawn pipeline thread");
}

async fn run(config: PipelineConfig, segments: Receiver<Segment>, ui_tx: Sender<UiEvent>) {
    let direction = config.direction;
    let (stt_tx, stt_rx) = flume::unbounded::<SttEvent>();
    let stt_fut = stt::run(config.mistral.clone(), segments, stt_tx);
    let consume_fut = consume(config, stt_rx, ui_tx.clone());
    // Joined (not spawned) so `Player` need not be `Send`.
    futures::future::join(stt_fut, consume_fut).await;
    let _ = ui_tx.send(UiEvent::Stage {
        direction,
        stage: Stage::Listening,
    });
}

async fn consume(config: PipelineConfig, stt_rx: Receiver<SttEvent>, ui_tx: Sender<UiEvent>) {
    let direction = config.direction;
    let language = config.languages.target.clone();
    let translator = Translator::new(config.cerebras, &config.languages);
    let tts = Tts::new(config.mistral, language);
    let player = config.player;

    while let Ok(event) = stt_rx.recv_async().await {
        match event {
            SttEvent::Partial(text) => {
                let _ = ui_tx.send(UiEvent::Stage {
                    direction,
                    stage: Stage::Transcribing,
                });
                let _ = ui_tx.send(UiEvent::SourcePartial { direction, text });
            }
            SttEvent::Final(source) => {
                let _ = ui_tx.send(UiEvent::SourceFinal {
                    direction,
                    text: source.clone(),
                });
                let _ = ui_tx.send(UiEvent::Stage {
                    direction,
                    stage: Stage::Translating,
                });

                match translator.translate(&source).await {
                    Ok(translation) => {
                        let _ = ui_tx.send(UiEvent::Translation {
                            direction,
                            source,
                            translated: translation.translated.clone(),
                        });
                        let _ = ui_tx.send(UiEvent::Stage {
                            direction,
                            stage: Stage::Speaking,
                        });
                        match tts.synthesize(&translation.translated).await {
                            Ok(audio) => player.submit(&audio),
                            Err(e) => {
                                error!("TTS failed ({direction:?}): {e}");
                                let _ = ui_tx.send(UiEvent::Error {
                                    direction,
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        error!("Translation failed ({direction:?}): {e}");
                        let _ = ui_tx.send(UiEvent::Error {
                            direction,
                            message: e.to_string(),
                        });
                    }
                }

                let _ = ui_tx.send(UiEvent::Stage {
                    direction,
                    stage: Stage::Listening,
                });
            }
        }
    }
}
