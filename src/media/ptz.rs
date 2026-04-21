//! Host-side PTZ control for VISCA conference cameras.
//!
//! Talks to each device's in-process VISCA HTTP server (`src/ptz.rs`, default
//! port 8091) over the network. The server translates our directional burst
//! commands into VISCA byte sequences on its configured serial device
//! (typically `/dev/ttyUSB0`), then auto-stops after `duration_ms`.
//!
//! This is the same control path the clawcam-app web UI uses, so any motion
//! that works in the browser works here and vice-versa. We used to drive
//! `v4l2-ctl` directly, but many UVC webcams advertise PTZ descriptors whose
//! motors aren't actually wired — VISCA over real serial is what physically
//! moves conference-cam motors.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::device::Device;

const DEFAULT_PORT: u16 = 8091;
const DEFAULT_DURATION_MS: u64 = 300;

#[derive(Debug, Clone)]
pub enum PtzAction {
    /// Return the camera to its home position.
    Center,
    /// Stop all motion immediately.
    Stop,
    /// Burst motion: pan/tilt/zoom direction in {-1, 0, +1}, auto-stops after duration_ms.
    Nudge {
        pan: i32,
        tilt: i32,
        zoom: i32,
        duration_ms: u64,
    },
}

pub async fn run_ptz(dev: &Device, port: u16, action: PtzAction) -> Result<()> {
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let url = format!("http://{}:{port}/ptz", dev.host);
    let token = std::env::var("CLAWCAM_PTZ_TOKEN").ok();

    let body = match action {
        PtzAction::Center => serde_json::json!({ "home": true }),
        PtzAction::Stop => serde_json::json!({ "stop": true }),
        PtzAction::Nudge {
            pan,
            tilt,
            zoom,
            duration_ms,
        } => {
            for (name, v) in [("pan", pan), ("tilt", tilt), ("zoom", zoom)] {
                if !(-1..=1).contains(&v) {
                    anyhow::bail!("{name} must be -1, 0, or +1 (got {v})");
                }
            }
            let duration_ms = if duration_ms == 0 {
                DEFAULT_DURATION_MS
            } else {
                duration_ms.min(10_000)
            };
            serde_json::json!({
                "pan": pan,
                "tilt": tilt,
                "zoom": zoom,
                "duration_ms": duration_ms,
            })
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut req = client.post(&url).json(&body);
    if let Some(t) = token.as_deref() {
        req = req.bearer_auth(t);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("could not reach PTZ endpoint {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("PTZ endpoint returned {status}: {text}");
    }
    println!("{}", text.trim());
    Ok(())
}
