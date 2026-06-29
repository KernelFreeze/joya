//! Full-screen, click-through translation overlay (its own layer-shell window).
//!
//! A background thread screenshots the screen every `interval_ms` and asks Gemma
//! 4 for an *incremental* update (see [`crate::screen_translate`]): which overlays
//! to add and which to remove, given the set already shown. The thread owns that
//! persistent set, applies the delta, and sends the full set here over a flume
//! channel. Keeping stable text in place across frames is what stops the overlay
//! from jittering. The view paints each overlay at its normalized position scaled
//! by the window's own logical size. Wholly independent of the voice pipeline.

use std::thread;
use std::time::Duration;

use gpui::{Context, FontWeight, Window, div, prelude::*, px, rgba};
use tracing::{error, info};

use crate::config::{CerebrasConfig, OverlayConfig};
use crate::screen_capture::{self, ScreenCapturer};
use crate::screen_translate::{Overlay, OverlayUpdate, ScreenTranslator};

/// Safety cap so overlays can't grow without bound.
const MAX_OVERLAYS: usize = 100;

/// How many consecutive frames an overlay may go unconfirmed (not in the model's
/// `keep` list) before it's dropped. A grace of 2 tolerates a single model
/// omission without flicker; a full scene change (empty `keep`) clears at once.
const MAX_MISSES: u8 = 2;

/// A live overlay plus how many consecutive frames it has gone unconfirmed.
struct Tracked {
    overlay: Overlay,
    misses: u8,
}

enum OverlayEvent {
    Overlays(Vec<Overlay>),
    Error(String),
    /// Momentarily hide all overlays so the screenshot doesn't capture them — see
    /// the feedback-loop note on `spawn_loop`.
    Hide,
    Show,
}

pub struct ScreenOverlay {
    font_px: f32,
    overlays: Vec<Overlay>,
    last_error: Option<String>,
    /// When true the view draws nothing, so the next capture sees a clean screen.
    hidden: bool,
    /// Set when we just hid: render registers an `on_next_frame` callback to tell
    /// the capture thread (via `ready_tx`) that the hidden frame is on screen.
    pending_present: bool,
    ready_tx: flume::Sender<()>,
}

impl ScreenOverlay {
    /// `output` is the resolved Wayland output name the overlay window is pinned
    /// to (see `main::resolve_overlay_monitor`), so the screenshots cover exactly
    /// the same screen as the overlay.
    pub fn new(
        overlay: OverlayConfig,
        cerebras: CerebrasConfig,
        output: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = flume::unbounded::<OverlayEvent>();
        // Handshake: the capture thread blocks on `ready_rx` until render confirms
        // (via `on_next_frame`) that the hidden frame is on screen.
        let (ready_tx, ready_rx) = flume::unbounded::<()>();
        spawn_loop(overlay.clone(), cerebras, output, tx, ready_rx);

        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            font_px: overlay.font_px,
            overlays: Vec::new(),
            last_error: None,
            hidden: false,
            pending_present: false,
            ready_tx,
        }
    }

    fn handle_event(&mut self, event: OverlayEvent, cx: &mut Context<Self>) {
        match event {
            OverlayEvent::Overlays(o) => {
                self.overlays = o;
                self.last_error = None;
            }
            OverlayEvent::Error(e) => self.last_error = Some(e),
            OverlayEvent::Hide => {
                self.hidden = true;
                self.pending_present = true;
            }
            OverlayEvent::Show => self.hidden = false,
        }
        cx.notify();
    }
}

/// Capture + incremental-inference loop on a dedicated thread with its own
/// current-thread tokio runtime — the same shape as `pipeline::spawn`, kept
/// separate so the overlay never blocks (or is blocked by) the voice pipeline.
/// Owns the persistent overlay set so positions stay stable across frames.
///
/// Critically, the overlay is hidden for the instant of each screenshot. Otherwise
/// the capture would include our own translation boxes, and the model — seeing its
/// previous English overlays still "on screen" — would keep confirming them and
/// never let them clear (and the boxes occlude the original text it needs to read).
fn spawn_loop(
    overlay: OverlayConfig,
    cerebras: CerebrasConfig,
    output: Option<String>,
    tx: flume::Sender<OverlayEvent>,
    ready_rx: flume::Receiver<()>,
) {
    thread::Builder::new()
        .name("joya-overlay".into())
        .spawn(move || {
            let capturer = match ScreenCapturer::new(output) {
                Ok(c) => c,
                Err(e) => {
                    error!("overlay capture init failed: {e}");
                    let _ = tx.send(OverlayEvent::Error(format!("capture init: {e}")));
                    return;
                }
            };
            let translator = ScreenTranslator::new(cerebras, &overlay.language);
            let interval = Duration::from_millis(overlay.interval_ms.max(250));

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build overlay runtime");

            rt.block_on(async move {
                let mut tracked: Vec<Tracked> = Vec::new();
                let mut next_id: u64 = 1;
                loop {
                    let current: Vec<Overlay> = tracked.iter().map(|t| t.overlay.clone()).collect();

                    // Hide our overlays, wait for that clean frame to reach the
                    // screen, capture it, then show them again. The handshake keeps
                    // the hidden window to ~one frame; the timeout is a fallback in
                    // case the view isn't redrawing.
                    let _ = tx.send(OverlayEvent::Hide);
                    let _ = ready_rx.recv_timeout(Duration::from_millis(250));
                    let captured = capturer.capture();
                    let _ = tx.send(OverlayEvent::Show);

                    let result = async {
                        let image = captured?;
                        let uri = screen_capture::to_data_uri(&image)?;
                        translator.diff(&uri, &current).await
                    }
                    .await;

                    match result {
                        Ok(update) => {
                            apply_update(&mut tracked, &mut next_id, update);
                            let shown = tracked.iter().map(|t| t.overlay.clone()).collect();
                            let _ = tx.send(OverlayEvent::Overlays(shown));
                        }
                        // On API/parse failure leave the overlays as they are — a
                        // transient glitch must not wipe a good set.
                        Err(e) => {
                            error!("overlay tick failed: {e}");
                            let _ = tx.send(OverlayEvent::Error(e.to_string()));
                        }
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        })
        .expect("failed to spawn overlay thread");
}

/// Apply the model's update: drop overlays it no longer confirms (`keep`), then
/// append the new ones with fresh ids. Confirmed overlays are left untouched so
/// their positions don't jitter.
fn apply_update(tracked: &mut Vec<Tracked>, next_id: &mut u64, update: OverlayUpdate) {
    let before = tracked.len();
    if update.keep.is_empty() {
        // The model confirmed none of the existing overlays — a full scene change.
        // Clear at once so stale text doesn't linger.
        tracked.clear();
    } else {
        for t in tracked.iter_mut() {
            t.misses = if update.keep.contains(&t.overlay.id) {
                0
            } else {
                t.misses + 1
            };
        }
        tracked.retain(|t| t.misses < MAX_MISSES);
    }
    let removed = before - tracked.len();

    let mut added = 0;
    for n in update.add {
        // Skip exact-duplicate translations (the same text already shown).
        let dup = tracked
            .iter()
            .any(|t| t.overlay.text.trim().to_lowercase() == n.text.trim().to_lowercase());
        if dup {
            continue;
        }
        tracked.push(Tracked {
            overlay: Overlay {
                id: *next_id,
                x: n.x,
                y: n.y,
                w: n.w,
                text: n.text,
            },
            misses: 0,
        });
        *next_id += 1;
        added += 1;
    }
    if tracked.len() > MAX_OVERLAYS {
        let drain = tracked.len() - MAX_OVERLAYS;
        tracked.drain(0..drain);
    }
    info!("overlay: {} shown (+{added} -{removed})", tracked.len());
}

impl Render for ScreenOverlay {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // We were just hidden for a capture: once this (empty) frame is on screen,
        // let the capture thread know it can shoot.
        if self.pending_present {
            self.pending_present = false;
            let ready = self.ready_tx.clone();
            window.on_next_frame(move |_, _| {
                let _ = ready.send(());
            });
        }

        let size = window.viewport_size();
        let w = f32::from(size.width);
        let h = f32::from(size.height);

        let mut root = div().size_full().relative();

        // While hidden, draw nothing so the screenshot is clean.
        if self.hidden {
            return root;
        }

        for o in &self.overlays {
            root = root.child(
                div()
                    .absolute()
                    .left(px(o.x * w))
                    .top(px(o.y * h))
                    // Hug the detected text width when the model gave one, but never
                    // run past the right edge of the screen.
                    .max_w(px(if o.w > 0.01 { o.w } else { 1.0 - o.x }.max(0.05) * w))
                    .bg(rgba(0x000000CC))
                    .rounded_md()
                    .px_2()
                    .py_0p5()
                    .text_size(px(self.font_px))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgba(0xFFFFFFFF))
                    .child(o.text.clone()),
            );
        }

        if let Some(err) = &self.last_error {
            root = root.child(
                div()
                    .absolute()
                    .left(px(8.0))
                    .bottom(px(8.0))
                    .bg(rgba(0x000000CC))
                    .rounded_md()
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .text_color(rgba(0xFF6B6BDD))
                    .child(format!("overlay: {err}")),
            );
        }

        root
    }
}
