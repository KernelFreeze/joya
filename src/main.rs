use clap::{Parser, Subcommand};
use etcetera::{AppStrategy, AppStrategyArgs, choose_app_strategy};
use gpui::{
    App, AppContext, Bounds, DisplayId, Pixels, Styled, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point, px, rgba, size,
};
use gpui_component::Root;
use gpui_platform::application;
use uuid::Uuid;

use crate::app::JoyaApp;
use crate::screen_overlay::ScreenOverlay;

mod app;
mod audio;
mod config;
mod pipeline;
mod playback;
mod screen_capture;
mod screen_overlay;
mod screen_translate;
mod stt;
mod translate;
mod tts;

#[derive(Parser)]
#[command(version, about = "Real-time voice-to-voice translation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the JSON schema for the config file to stdout
    Schema,
    /// List available audio input and output devices
    ListDevices,
}

fn run_app(cx: &mut App) {
    // Default to `info` so capture/VAD/pipeline logs are visible without forcing
    // the user to set RUST_LOG. `RUST_LOG` still wins when set, so `RUST_LOG=debug`
    // or `RUST_LOG=joya=trace` works for deeper digging.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    gpui_component::init(cx);

    let strategy = choose_app_strategy(AppStrategyArgs {
        top_level_domain: "dev".to_string(),
        author: "CelesteLove".to_string(),
        app_name: "Joya".to_string(),
    })
    .unwrap();

    let config_dir = strategy.config_dir();
    let config_path = config_dir.join("config.yaml");

    let config = if config_path.exists() {
        config::Config::load(&config_path).expect("failed to load config")
    } else {
        let config = config::Config::default();
        std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
        config
            .write(&config_path)
            .expect("failed to write default config");
        config
    };

    // The screen-translation overlay is a separate, full-screen click-through
    // window. Clone what it needs before `config` is moved into the panel window.
    let overlay_cfg = config.overlay.clone();
    let cerebras_cfg = config.cerebras.clone();

    let size = size(px(520.), px(720.0));
    let bounds = Bounds::centered(None, size, cx);

    cx.open_window(
        WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "joya".to_string(),
                layer: Layer::Overlay,
                anchor: Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                margin: Some((px(12.), px(12.), px(12.), px(0.))),
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                ..Default::default()
            }),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some("joya".to_string()),
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| JoyaApp::new(config, window, cx));
            cx.new(|cx| Root::new(view, window, cx).bg(rgba(0x00000000)))
        },
    )
    .expect("window should open");

    if overlay_cfg.enabled {
        // gpui hasn't enumerated Wayland outputs yet at startup (`displays()` is
        // empty), so the overlay's monitor can't be resolved/pinned synchronously.
        // Defer until the event loop has populated the display list.
        cx.spawn(async move |cx| {
            for _ in 0..40 {
                if cx.update(|cx| !cx.displays().is_empty()) {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            }
            cx.update(|cx| open_overlay_window(overlay_cfg, cerebras_cfg, cx));
        })
        .detach();
    }
}

/// Opens the full-screen, click-through overlay window. Called after gpui has
/// enumerated displays so the monitor can be pinned (see `resolve_overlay_monitor`).
fn open_overlay_window(
    overlay_cfg: config::OverlayConfig,
    cerebras_cfg: config::CerebrasConfig,
    cx: &mut App,
) {
    // Pin the overlay to one monitor and capture that same monitor, so the
    // screenshot and the floating text line up. `None` falls back to the
    // compositor's default placement at the panel size (degraded, but works).
    let (display_id, window_bounds, output_name) = resolve_overlay_monitor(&overlay_cfg, cx)
        .map(|(id, bounds, name)| (id, Some(WindowBounds::Windowed(bounds)), name))
        .unwrap_or((None, None, None));

    cx.open_window(
        WindowOptions {
            titlebar: None,
            window_bounds,
            display_id,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "joya-overlay".to_string(),
                layer: Layer::Overlay,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                keyboard_interactivity: KeyboardInteractivity::None,
                pointer_passthrough: true,
                ..Default::default()
            }),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some("joya".to_string()),
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        },
        // Host the overlay view directly, *not* via gpui-component's `Root`:
        // `Root::render` paints an opaque themed background over the whole window,
        // which would gray out the screen. The overlay must be fully transparent
        // except for its text boxes.
        |_window, cx| cx.new(|cx| ScreenOverlay::new(overlay_cfg, cerebras_cfg, output_name, cx)),
    )
    .expect("overlay window should open");
}

/// Pick the monitor for the overlay: the gpui display (for window placement +
/// size) and the libwayshot output name (for capture), correlated by logical
/// position so the overlay window and its screenshots always cover the same
/// screen. Honors `overlay.output` by name, else uses the primary display.
fn resolve_overlay_monitor(
    cfg: &config::OverlayConfig,
    cx: &App,
) -> Option<(Option<DisplayId>, Bounds<Pixels>, Option<String>)> {
    let outputs = match screen_capture::list_outputs() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("overlay: could not enumerate outputs: {e}");
            return None;
        }
    };

    // Target output: by configured name, else the first one.
    let target = match &cfg.output {
        Some(name) => outputs.iter().find(|o| &o.name == name),
        None => outputs.first(),
    }?;

    // Correlate with the gpui display. gpui's Wayland backend reports every
    // display at origin (0,0), so position is useless for matching — use the
    // output name (gpui exposes it only as `uuid() = v5(name)`), falling back to
    // a unique size match.
    let want_uuid = Uuid::new_v5(&Uuid::NAMESPACE_DNS, target.name.as_bytes());
    let displays = cx.displays();
    let display = displays
        .iter()
        .find(|d| d.uuid().ok() == Some(want_uuid))
        .or_else(|| {
            displays.iter().find(|d| {
                let s = d.bounds().size;
                f32::from(s.width) as u32 == target.width
                    && f32::from(s.height) as u32 == target.height
            })
        });
    let display_id = display.map(|d| d.id());
    let bounds = display.map(|d| d.bounds()).unwrap_or_else(|| Bounds {
        origin: point(px(target.x as f32), px(target.y as f32)),
        size: size(px(target.width as f32), px(target.height as f32)),
    });

    tracing::info!(
        "overlay: target output {:?} ({},{} {}x{}); window bounds {:?}; display_id {:?}",
        target.name,
        target.x,
        target.y,
        target.width,
        target.height,
        bounds,
        display_id,
    );

    Some((display_id, bounds, Some(target.name.clone())))
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Schema) => {
            let schema = schemars::schema_for!(config::Config);
            println!("{}", serde_json::to_string_pretty(&schema).unwrap());
        }
        Some(Commands::ListDevices) => audio::list_audio_devices(),
        None => application().run(run_app),
    }
}
