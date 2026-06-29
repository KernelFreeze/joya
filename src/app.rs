use std::collections::HashMap;

use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, ScrollHandle, Window, div, prelude::*, px,
    rgba,
};
use gpui_component::ActiveTheme;
use tracing::{info, warn};

use crate::audio::{self, CaptureHandle};
use crate::config::{Config, Direction, LanguageConfig};
use crate::pipeline::{self, PipelineConfig, Stage, UiEvent};
use crate::playback::Player;

struct Entry {
    direction: Direction,
    source: String,
    translated: String,
}

/// Per-direction live state (partial transcript + current stage).
#[derive(Default)]
struct DirectionState {
    source_partial: String,
    stage: Stage,
    last_error: Option<String>,
}

impl DirectionState {
    fn stage_label(&self) -> &'static str {
        match self.stage {
            Stage::Listening => "listening",
            Stage::Transcribing => "transcribing",
            Stage::Translating => "translating",
            Stage::Speaking => "speaking",
        }
    }
}

pub struct JoyaApp {
    config: Config,
    entries: Vec<Entry>,
    /// One slot per active direction. Keyed by `Direction` so the receiver loop
    /// can update the right side without scanning.
    states: HashMap<Direction, DirectionState>,
    /// Which directions actually spawned a pipeline, in spawn order (so the
    /// header lists them deterministically).
    active_directions: Vec<Direction>,
    #[allow(dead_code)]
    captures: Vec<CaptureHandle>,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl JoyaApp {
    pub fn new(config: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (ui_tx, ui_rx) = flume::unbounded::<UiEvent>();

        let mut captures = Vec::new();
        let mut active_directions = Vec::new();

        // Spawn one pipeline per enabled direction. Each gets its own capture
        // device and its own Player (opened here, on the main thread, because
        // cpal streams are not `Send`).
        for direction in [Direction::Relay, Direction::SelfMode] {
            let pipeline_cfg = match build_pipeline(&config, direction) {
                Ok(Some(cfg)) => cfg,
                Ok(None) => continue,
                Err(msg) => {
                    warn!("Refused to start {direction:?}: {msg}");
                    let _ = ui_tx.send(UiEvent::Error {
                        direction,
                        message: msg,
                    });
                    continue;
                }
            };
            match audio::capture(pipeline_cfg.capture_device.as_deref(), direction) {
                Ok(handle) => {
                    pipeline::spawn(
                        PipelineConfig {
                            direction,
                            mistral: config.mistral.clone(),
                            cerebras: config.cerebras.clone(),
                            languages: pipeline_cfg.languages,
                            player: pipeline_cfg.player,
                        },
                        handle.segments.clone(),
                        ui_tx.clone(),
                    );
                    captures.push(handle);
                    active_directions.push(direction);
                }
                Err(e) => {
                    warn!("Failed to start {direction:?} capture: {e}");
                    let _ = ui_tx.send(UiEvent::Error {
                        direction,
                        message: format!("capture: {e}"),
                    });
                }
            }
        }

        if active_directions.is_empty() {
            warn!(
                "No audio directions enabled — enable `audio.relay.enabled` or `audio.self.enabled`"
            );
        }

        cx.spawn(async move |this, cx| {
            while let Ok(event) = ui_rx.recv_async().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let mut states = HashMap::new();
        for d in &active_directions {
            states.insert(*d, DirectionState::default());
        }

        Self {
            config,
            entries: Vec::new(),
            states,
            active_directions,
            captures,
            scroll_handle: ScrollHandle::new(),
            focus_handle,
        }
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::SourcePartial { direction, text } => {
                if let Some(s) = self.states.get_mut(&direction) {
                    s.source_partial = text;
                }
            }
            UiEvent::SourceFinal { direction, text } => {
                if let Some(s) = self.states.get_mut(&direction) {
                    s.source_partial = text;
                }
            }
            UiEvent::Translation {
                direction,
                source,
                translated,
            } => {
                info!("[{direction:?}] Translated: {source} -> {translated}");
                self.entries.push(Entry {
                    direction,
                    source,
                    translated,
                });
                if let Some(s) = self.states.get_mut(&direction) {
                    s.source_partial.clear();
                    s.last_error = None;
                }
            }
            UiEvent::Stage { direction, stage } => {
                if let Some(s) = self.states.get_mut(&direction) {
                    s.stage = stage;
                }
            }
            UiEvent::Error { direction, message } => {
                if let Some(s) = self.states.get_mut(&direction) {
                    s.last_error = Some(message);
                }
            }
        }
        self.auto_scroll_to_bottom();
        cx.notify();
    }

    fn auto_scroll_to_bottom(&self) {
        let offset = self.scroll_handle.offset();
        let max_offset = self.scroll_handle.max_offset();
        if (offset.y + max_offset.y).abs() < px(80.0) {
            self.scroll_handle.scroll_to_bottom();
        }
    }
}

/// Resolved per-direction settings needed to spawn a pipeline.
struct ResolvedPipeline {
    capture_device: Option<String>,
    languages: LanguageConfig,
    player: Player,
}

/// Builds the per-direction pieces (capture device name, languages, Player).
///
/// Returns:
/// - `Ok(None)` if the direction is disabled — skip silently.
/// - `Ok(Some(_))` if it's safe to proceed to capture.
/// - `Err(msg)` if the direction is enabled but its config is unsafe to run —
///   the caller surfaces this as a UI error instead of starting the pipeline.
///
/// The Player is opened here on the calling thread.
fn build_pipeline(
    config: &Config,
    direction: Direction,
) -> Result<Option<ResolvedPipeline>, String> {
    // Languages are framed from your point of view: `source` = you, `target` =
    // the other party. `self` translates source→target (you→them); `relay`
    // translates the swapped pair (them→you). Both draw output sinks from the
    // shared `audio.output` config — routing lives in `Player`, not here.
    match direction {
        Direction::Relay => {
            let r = &config.audio.relay;
            if !r.enabled {
                return Ok(None);
            }
            // Guard: with no explicit capture device, relay falls back to the
            // default output's monitor — which also hears Joya's own TTS (played
            // to `output.playback_device`), so it would re-translate itself in
            // an infinite loop. Require an explicit, loop-free capture source.
            if r.capture_device.is_none() {
                return Err(
                    "relay.capture_device is not set. With no capture device, relay captures \
                     the default output's monitor, which also picks up Joya's own TTS and \
                     re-translates it in a loop. Run `scripts/setup-virtual-mic.sh` to \
                     create the isolation sink, then set `relay.capture_device: \
                     call_remote.monitor` so relay hears only the other party."
                        .into(),
                );
            }
            let player = Player::new(Direction::Relay, &config.audio.output);
            Ok(Some(ResolvedPipeline {
                capture_device: r.capture_device.clone(),
                languages: config.languages.swapped(),
                player,
            }))
        }
        Direction::SelfMode => {
            let s = &config.audio.self_mode;
            if !s.enabled {
                return Ok(None);
            }
            let player = Player::new(Direction::SelfMode, &config.audio.output);
            Ok(Some(ResolvedPipeline {
                capture_device: s.capture_device.clone(),
                languages: config.languages.clone(),
                player,
            }))
        }
    }
}

/// The accent color and its translucent fill counterpart for one direction.
/// `relay` is gold, `self` is teal. The accent paints the direction badge, the
/// translated text itself, and the entry/partial borders; the fill tints the
/// background band behind each translation.
struct DirectionPalette {
    accent: gpui::Rgba,
    fill: gpui::Rgba,
}

impl DirectionPalette {
    fn for_direction(direction: Direction) -> Self {
        match direction {
            Direction::Relay => Self {
                accent: rgba(0xFFD700FF),
                fill: rgba(0xFFD70030),
            },
            Direction::SelfMode => Self {
                accent: rgba(0x2DD4BFFF),
                fill: rgba(0x2DD4BF30),
            },
        }
    }

    fn label(direction: Direction) -> &'static str {
        match direction {
            Direction::Relay => "relay",
            Direction::SelfMode => "self",
        }
    }
}

impl Focusable for JoyaApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for JoyaApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = cx.theme().foreground;

        // Header: languages on the left, per-direction status pills on the right.
        let mut status = div().flex().items_center().gap_3();
        for d in &self.active_directions {
            let palette = DirectionPalette::for_direction(*d);
            let label = DirectionPalette::label(*d);
            let stage = self.states.get(d).map(|s| s.stage_label()).unwrap_or("off");
            status = status.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(palette.accent).child(label))
                    .child(
                        div()
                            .text_color(rgba(0xFFFFFF66))
                            .child(format!("· {stage}")),
                    ),
            );
        }
        status = status.child(
            div()
                .text_color(rgba(0xFFFFFF66))
                .child(format!("· {}", self.config.cerebras.model)),
        );

        let header = div()
            .flex_none()
            .flex()
            .justify_between()
            .items_center()
            .px_3()
            .py_1()
            .text_sm()
            .text_color(rgba(0xFFFFFFAA))
            .gap_12()
            .child(format!(
                "{} → {}",
                self.config.languages.source.as_deref().unwrap_or("auto"),
                self.config.languages.target
            ))
            .child(status);

        let mut list = div()
            .flex()
            .flex_col()
            .justify_end()
            .min_h_full()
            .gap_3()
            .p_4();

        for entry in &self.entries {
            let palette = DirectionPalette::for_direction(entry.direction);
            let mut block = div().flex().flex_col().gap_1();

            // Header row: a solid accent badge identifying the direction, then
            // the original source transcript in a dim neutral tone.
            block = block.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(palette.accent)
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgba(0x15151EFF))
                            .child(DirectionPalette::label(entry.direction)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(0xFFFFFF80))
                            .child(entry.source.clone()),
                    ),
            );

            // The translation itself — large and bold, rendered in the
            // direction's accent color so `relay` (gold) and `self` (teal) are
            // distinguishable at a glance. The tinted band and accent left
            // border reinforce the per-direction color coding.
            block = block.child(
                div()
                    .bg(palette.fill)
                    .border_l_2()
                    .border_color(palette.accent)
                    .rounded_md()
                    .px_3()
                    .py_2()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(palette.accent)
                    .child(entry.translated.clone()),
            );
            list = list.child(block);
        }

        // One live partial per active direction, shown in its own color.
        for d in &self.active_directions {
            if let Some(s) = self.states.get(d) {
                if !s.source_partial.is_empty() {
                    let palette = DirectionPalette::for_direction(*d);
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .bg(palette.fill)
                            .border_l_2()
                            .border_color(palette.accent)
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(palette.accent)
                                    .child(DirectionPalette::label(*d)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(palette.accent)
                                    .child(s.source_partial.clone()),
                            ),
                    );
                }
                if let Some(err) = &s.last_error {
                    list = list.child(div().text_sm().text_color(rgba(0xFF6B6BDD)).child(format!(
                        "{}: {}",
                        DirectionPalette::label(*d),
                        err
                    )));
                }
            }
        }

        let scroll_region = div()
            .flex_1()
            .min_h_0()
            .id("joya-scroll")
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(list);

        div()
            .track_focus(&self.focus_handle)
            .bg(rgba(0x1E1E2EE6))
            .size_full()
            .shadow_lg()
            .text_xl()
            .text_color(fg)
            .flex()
            .flex_col()
            .child(header)
            .child(scroll_region)
            .into_any_element()
    }
}
