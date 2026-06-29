//! Screen capture for the translation overlay, via wlr-screencopy (libwayshot).
//!
//! Independent of gpui: libwayshot opens its own Wayland connection, so capture
//! never touches the UI's surfaces. A single output (monitor) is captured — the
//! one the overlay window is pinned to — selected by Wayland output name so the
//! screenshot and the overlay always cover the same screen.

use anyhow::{Context, Result};
use base64::Engine;
use image::DynamicImage;
use libwayshot::WayshotConnection;

/// One output's identity and logical geometry, used by `main` to correlate a
/// libwayshot output with the matching gpui display.
pub struct OutputGeometry {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Enumerate all Wayland outputs (name + logical position/size). Opens a
/// short-lived connection; the result is owned/`Send` so `main` can use it.
pub fn list_outputs() -> Result<Vec<OutputGeometry>> {
    let conn = WayshotConnection::new().context("open wlr-screencopy connection")?;
    Ok(conn
        .get_all_outputs()
        .iter()
        .map(|o| {
            let region = o.logical_region.inner;
            OutputGeometry {
                name: o.name.clone(),
                x: region.position.x,
                y: region.position.y,
                width: region.size.width,
                height: region.size.height,
            }
        })
        .collect())
}

pub struct ScreenCapturer {
    conn: WayshotConnection,
    /// Output name to capture; `None` falls back to the first output.
    output: Option<String>,
}

impl ScreenCapturer {
    pub fn new(output: Option<String>) -> Result<Self> {
        let conn = WayshotConnection::new().context("open wlr-screencopy connection")?;
        Ok(Self { conn, output })
    }

    /// Capture the configured output (no cursor) as an image.
    pub fn capture(&self) -> Result<DynamicImage> {
        let outputs = self.conn.get_all_outputs();
        let output = match &self.output {
            Some(name) => outputs
                .iter()
                .find(|o| &o.name == name)
                .with_context(|| format!("output {name:?} not found"))?,
            None => outputs.first().context("no Wayland outputs to capture")?,
        };
        self.conn
            .screenshot_single_output(output, false)
            .context("wlr-screencopy capture failed")
    }
}

/// Encode an image as JPEG and wrap it as a base64 data URI for the chat API.
pub fn to_data_uri(image: &DynamicImage) -> Result<String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image
        .to_rgb8()
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .context("JPEG encode")?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    Ok(format!("data:image/jpeg;base64,{b64}"))
}
