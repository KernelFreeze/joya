//! GPUI overlay app. Mirrors cuteview's entity shape: workers feed a flume
//! channel, a `cx.spawn` receiver loop updates state, and `Render` paints it.

use gpui::{
    App, Context, FocusHandle, Focusable, ScrollHandle, Window, div, prelude::*, px, rgba,
};
use gpui_component::ActiveTheme;
use tracing::info;

use crate::audio::{self, CaptureHandle};
use crate::config::Config;
use crate::pipeline::{self, Stage, UiEvent};

struct Entry {
    source: String,
    translated: String,
    reasoning: Option<String>,
}

pub struct JoyaApp {
    config: Config,
    entries: Vec<Entry>,
    source_partial: String,
    stage: Stage,
    last_error: Option<String>,
    #[allow(dead_code)]
    capture: CaptureHandle,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl JoyaApp {
    pub fn new(config: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let capture = audio::capture(config.audio.capture_device.as_deref())
            .expect("failed to start audio capture");

        let (ui_tx, ui_rx) = flume::unbounded::<UiEvent>();
        pipeline::spawn(config.clone(), capture.segments.clone(), ui_tx);

        cx.spawn(async move |this, cx| {
            while let Ok(event) = ui_rx.recv_async().await {
                if this.update(cx, |this, cx| this.handle_event(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            config,
            entries: Vec::new(),
            source_partial: String::new(),
            stage: Stage::Listening,
            last_error: None,
            capture,
            scroll_handle: ScrollHandle::new(),
            focus_handle,
        }
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::SourcePartial(text) | UiEvent::SourceFinal(text) => {
                self.source_partial = text;
            }
            UiEvent::Translation { source, translated, reasoning } => {
                info!("Translated: {source} -> {translated}");
                self.entries.push(Entry { source, translated, reasoning });
                self.source_partial.clear();
                self.last_error = None;
            }
            UiEvent::Stage(stage) => self.stage = stage,
            UiEvent::Error(e) => self.last_error = Some(e),
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

    fn stage_label(&self) -> &'static str {
        match self.stage {
            Stage::Listening => "listening",
            Stage::Transcribing => "transcribing",
            Stage::Translating => "translating",
            Stage::Speaking => "speaking",
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

        let header = div()
            .flex_none()
            .flex()
            .justify_between()
            .px_3()
            .py_1()
            .text_sm()
            .text_color(rgba(0xFFFFFFAA))
            .child(format!(
                "{} → {}",
                self.config.languages.source.as_deref().unwrap_or("auto"),
                self.config.languages.target
            ))
            .child(format!("{} · {}", self.stage_label(), self.config.cerebras.model));

        let mut list = div().flex().flex_col().justify_end().min_h_full().gap_3().p_4();

        for entry in &self.entries {
            let mut block = div().flex().flex_col().gap_1();
            block = block.child(
                div()
                    .text_sm()
                    .text_color(rgba(0xFFFFFF99))
                    .child(entry.source.clone()),
            );
            block = block.child(
                div()
                    .bg(rgba(0xFFD70030))
                    .rounded_md()
                    .px_3()
                    .py_2()
                    .child(entry.translated.clone()),
            );
            if let Some(reasoning) = &entry.reasoning {
                block = block.child(
                    div()
                        .text_xs()
                        .text_color(rgba(0xFFFFFF66))
                        .child(format!("reasoning: {reasoning}")),
                );
            }
            list = list.child(block);
        }

        if !self.source_partial.is_empty() {
            list = list.child(
                div()
                    .bg(rgba(0x3B82F640))
                    .rounded_md()
                    .px_2()
                    .py_1()
                    .child(self.source_partial.clone()),
            );
        }

        if let Some(error) = &self.last_error {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(rgba(0xFF6B6BDD))
                    .child(error.clone()),
            );
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
