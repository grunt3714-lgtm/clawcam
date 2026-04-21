use anyhow::Result;
use serde::Serialize;
use std::net::IpAddr;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub class: String,
    pub class_id: u32,
    pub score: f32,
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackInfo {
    pub track_id: u64,
    pub class: String,
    pub duration_secs: f64,
    pub movement_px: f32,
    pub is_stationary: bool,
    pub bbox: [u32; 4],
}

/// Per-inference bbox sample tagged to a position in the assembled clip.
/// `t` is playback seconds from the start of the clip (clip is assembled at
/// a fixed 10 FPS, so `t = frame_index / 10.0`).
#[derive(Debug, Clone, Serialize)]
pub struct ClipPredSample {
    pub frame_index: usize,
    pub t: f64,
    pub boxes: Vec<Detection>,
}

#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub ts: String,
    pub epoch: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub detail: String,
    pub source: String,
    pub host: String,
    pub image: String,
    pub predictions: Vec<Detection>,

    // --- temporal / tracking fields (backward-compatible, omitted when None) ---

    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_phase: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Vec<TrackInfo>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_duration_secs: Option<f64>,

    /// Base64-encoded MP4 clip (sent on "end" phase only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,

    /// Pre-detection JPEG frames as base64 (sent on "start" phase only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_frames: Option<Vec<String>>,

    /// Per-inference bbox samples indexed into the assembled clip (sent on
    /// "end" phase only). Used by the UI to draw animated overlays synced to
    /// video playback time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_predictions: Option<Vec<ClipPredSample>>,
}

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn send(
    url: &str,
    token: Option<&str>,
    payload: &WebhookPayload,
) -> Result<()> {
    // Reject plaintext HTTP when a bearer token is configured,
    // unless the target is a private/local network address (RFC1918, loopback).
    if token.is_some() && url.starts_with("http://") && !is_private_url(url) {
        anyhow::bail!(
            "refusing to send bearer token over plaintext HTTP — use https:// for webhook URL"
        );
    }

    let client = reqwest::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()?;
    let mut req = client.post(url).json(payload);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        tracing::warn!("webhook returned {}: {}", resp.status(), resp.text().await.unwrap_or_default());
    } else {
        tracing::info!("webhook delivered successfully");
    }
    Ok(())
}

/// Check if a URL points to a private/local network address (safe for plaintext HTTP).
fn is_private_url(url: &str) -> bool {
    let host = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.split(':').next())
        .unwrap_or("");
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback(),
        };
    }
    host.ends_with(".local")
}
