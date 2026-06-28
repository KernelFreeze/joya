use clap::{Parser, Subcommand};
use etcetera::{AppStrategy, AppStrategyArgs, choose_app_strategy};
use gpui::{
    App, AppContext, Bounds, Styled, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    px, rgba, size,
};
use gpui_component::Root;
use gpui_platform::application;

use crate::app::JoyaApp;

mod app;
mod audio;
mod config;
mod pipeline;
mod playback;
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
    tracing_subscriber::fmt::init();
    gpui_component::init(cx);

    let strategy = choose_app_strategy(AppStrategyArgs {
        top_level_domain: "dev".to_string(),
        author: "Celeste Love".to_string(),
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
        config.write(&config_path).expect("failed to write default config");
        config
    };

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
