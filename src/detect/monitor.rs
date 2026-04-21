use anyhow::{Context, Result};
use base64::Engine;
use gstreamer::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tracing::{info, warn};

use crate::detect::event::{EventDecision, EventManager, UpdateReason};
use crate::detect::frame_buffer::{FrameBuffer, TimestampedFrame};
use crate::detect::pipeline;
use crate::detect::tracker::ObjectTracker;
use crate::detect::yolo::YoloDetector;
use crate::webhook::{self, ClipPredSample, Detection, TrackInfo, WebhookPayload};

// We don't videorate-throttle in GStreamer (negotiation is fragile), so this
// is the effective YOLO inference cadence.
const INFERENCE_INTERVAL: Duration = Duration::from_millis(500);
const FRAME_BUFFER_CAPACITY: usize = 30; // ~3s at 10 FPS
const PRE_ROLL_FRAMES: usize = 20; // ~2s of pre-detection context for clips
const PRE_FRAMES_IN_ALERT: usize = 3; // frames to include in initial webhook
const MAX_CLIP_FRAMES: usize = 300; // ~30s cap
const STATIONARY_THRESHOLD_PX: f32 = 5.0;

pub async fn run_monitor(
    webhook_url: Option<&str>,
    webhook_token: Option<&str>,
    host: Option<&str>,
    log_path: Option<&str>,
) -> Result<()> {
    if let Some(path) = log_path {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .context("failed to open log file")?;
        let _ = tracing_subscriber::fmt()
            .with_writer(file)
            .with_ansi(false)
            .try_init();
    }

    let webhook_url_owned = match webhook_url {
        Some(u) => u.to_string(),
        None => std::env::var("CLAWCAM_WEBHOOK")
            .context("no webhook URL — pass --webhook or set CLAWCAM_WEBHOOK")?,
    };
    let webhook_url = webhook_url_owned.as_str();

    let webhook_token_owned = match webhook_token {
        Some(t) => Some(t.to_string()),
        None => std::env::var("CLAWCAM_WEBHOOK_TOKEN").ok(),
    };
    let webhook_token: Option<&str> = webhook_token_owned.as_deref();

    let camera_source =
        std::env::var("CLAWCAM_CAMERA_SOURCE").unwrap_or_else(|_| "v4l2src".to_string());
    let model_path = std::env::var("CLAWCAM_MODEL_PATH")
        .unwrap_or_else(|_| "/usr/local/share/clawcam/yolov8n.onnx".to_string());
    let hostname = host
        .map(String::from)
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());

    let frame_dir = ["/run/clawcam"]
        .iter()
        .map(std::path::PathBuf::from)
        .chain(dirs::runtime_dir().map(|d| d.join("clawcam")))
        .find(|d| {
            std::fs::create_dir_all(d).is_ok()
                && std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).is_ok()
        })
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let latest_frame_path = frame_dir.join("clawcam_latest.jpg");

    info!("starting monitor: camera={camera_source} model={model_path}");

    // Optional PTZ control server for motorized conference cams (VISCA over serial).
    // Spawned only when CLAWCAM_PTZ_SERIAL is set. Binds to CLAWCAM_PTZ_BIND
    // (default 0.0.0.0:8091) and writes VISCA commands to the serial device.
    if let Ok(serial) = std::env::var("CLAWCAM_PTZ_SERIAL") {
        let bind = std::env::var("CLAWCAM_PTZ_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8091".to_string());
        tokio::spawn(async move {
            if let Err(e) = crate::ptz::serve(bind, serial).await {
                warn!("PTZ server exited: {e:#}");
            }
        });
    }

    let mut detector = YoloDetector::load(&model_path)?;
    info!("YOLO model loaded");

    let stream_url_owned = std::env::var("CLAWCAM_STREAM_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let stream_url = stream_url_owned.as_deref();
    let stream_width = std::env::var("CLAWCAM_STREAM_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1280u32);
    let stream_height = std::env::var("CLAWCAM_STREAM_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(720u32);
    let stream_fps = std::env::var("CLAWCAM_STREAM_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20u32);
    let (frame_rx, gst_pipeline) =
        pipeline::create_pipeline(&camera_source, stream_width, stream_height, stream_fps, stream_url)?;
    gst_pipeline.set_state(gstreamer::State::Playing)?;
    info!("pipeline started");

    let telemetry_url = std::env::var("CLAWCAM_TELEMETRY_URL").ok();
    let telemetry_token = webhook_token_owned.clone();

    // Adaptive monitoring components
    let mut frame_buffer = FrameBuffer::new(FRAME_BUFFER_CAPACITY);
    let mut tracker = ObjectTracker::new();
    let mut event_mgr = EventManager::new();
    let mut event_id: Option<String> = None;
    let mut clip_frames: Vec<TimestampedFrame> = Vec::new();
    let mut clip_preds: Vec<ClipPredSample> = Vec::new();

    loop {
        let frame = match frame_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                warn!("no frame received in 5s, pipeline may be stalled");
                continue;
            }
            Err(_) => break,
        };

        // Grab JPEG for buffer and latest-frame file
        let jpeg = match pipeline::grab_jpeg(&gst_pipeline) {
            Ok(j) => j,
            Err(_) => continue,
        };

        // Write latest frame for _snap
        if std::fs::write(&latest_frame_path, &jpeg).is_ok() {
            std::fs::set_permissions(
                &latest_frame_path,
                std::fs::Permissions::from_mode(0o600),
            )
            .ok();
        }

        // Push to rolling buffer
        frame_buffer.push(jpeg.clone());

        // If we're in an active event, stash frames for clip assembly
        if event_mgr.is_recording() {
            clip_frames.push(TimestampedFrame {
                jpeg: jpeg.clone(),
                captured_at: std::time::Instant::now(),
                epoch: chrono::Utc::now().timestamp(),
            });
            // Hard cap to bound memory (~15 MB at 50KB/frame)
            if clip_frames.len() > MAX_CLIP_FRAMES {
                clip_frames.drain(..clip_frames.len() - MAX_CLIP_FRAMES);
            }
        }

        // Run inference
        let detections = match detector.detect(&frame.data, frame.width, frame.height) {
            Ok(d) => d,
            Err(e) => {
                warn!("inference failed: {e}");
                continue;
            }
        };

        // While recording, stash a bbox sample indexed to the clip frame just pushed.
        // Clip is assembled at a fixed 10 FPS (see `assemble_clip`), so playback
        // `t = frame_index / 10.0`.
        if event_mgr.is_recording() && !clip_frames.is_empty() {
            let idx = clip_frames.len() - 1;
            clip_preds.push(ClipPredSample {
                frame_index: idx,
                t: idx as f64 / 10.0,
                boxes: detections.clone(),
            });
        }

        // Update tracker
        let tracks = tracker.update(&detections);

        // Push telemetry (every inference cycle) so the web UI can draw live overlays.
        if let Some(url) = telemetry_url.as_deref() {
            fire_telemetry(url, telemetry_token.as_deref(), &hostname, frame.width, frame.height, &detections, &tracks);
        }
        let has_new = event_mgr
            .event_start()
            .map(|s| tracker.has_new_arrivals_since(s))
            .unwrap_or(false);

        // Evaluate what to do
        let decision = event_mgr.evaluate(&tracks, has_new);

        match decision {
            EventDecision::Quiet => {}

            EventDecision::InitialAlert { tracks: trks } => {
                let eid = uuid::Uuid::new_v4().to_string();
                event_id = Some(eid.clone());

                // Seed clip buffer with pre-roll frames
                clip_frames = frame_buffer.clone_recent(PRE_ROLL_FRAMES);
                // Pre-roll frames have no bbox samples (no inference was run on them).
                // Start fresh so per-frame bboxes align with the assembled clip.
                clip_preds.clear();

                // Build pre-detection frames for the webhook
                let pre = frame_buffer
                    .recent(PRE_FRAMES_IN_ALERT)
                    .iter()
                    .map(|f| b64(&f.jpeg))
                    .collect::<Vec<_>>();

                let summary = format_tracks(&trks);
                info!("event {eid}: initial alert — {summary}");

                fire_webhook(
                    webhook_url,
                    webhook_token,
                    &hostname,
                    &detections,
                    &jpeg,
                    "ai_detected",
                    "start",
                    &eid,
                    &trks,
                    None,
                    None,
                    Some(pre),
                    None,
                );
            }

            EventDecision::Update { tracks: trks, reason } => {
                if let Some(ref eid) = event_id {
                    let detail = match reason {
                        UpdateReason::NewArrival => "ai_new_arrival",
                        UpdateReason::Prolonged => "ai_tracking_update",
                    };
                    let dur = tracker.longest_duration().map(|d| d.as_secs_f64());
                    let summary = format_tracks(&trks);
                    info!("event {eid}: update ({detail}) — {summary}");

                    fire_webhook(
                        webhook_url,
                        webhook_token,
                        &hostname,
                        &detections,
                        &jpeg,
                        detail,
                        "update",
                        eid,
                        &trks,
                        dur,
                        None,
                        None,
                        None,
                    );
                }
            }

            EventDecision::Complete { total_duration, .. } => {
                if let Some(ref eid) = event_id {
                    let dur_secs = total_duration.as_secs_f64();
                    info!(
                        "event {eid}: complete — {:.1}s, assembling clip from {} frames",
                        dur_secs,
                        clip_frames.len()
                    );

                    // Assemble clip in background if we have enough frames
                    let clip_b64 = if clip_frames.len() > 10 {
                        match assemble_clip(&clip_frames, &frame_dir).await {
                            Ok(data) => {
                                info!("clip assembled: {}KB", data.len() / 1024);
                                Some(data)
                            }
                            Err(e) => {
                                warn!("clip assembly failed: {e}");
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Get the last known tracks (they may be empty since objects left)
                    let last_tracks = tracker.active_tracks().to_vec();

                    fire_webhook(
                        webhook_url,
                        webhook_token,
                        &hostname,
                        &detections,
                        &jpeg,
                        "ai_event_complete",
                        "end",
                        eid,
                        &last_tracks,
                        Some(dur_secs),
                        clip_b64.as_deref(),
                        None,
                        Some(std::mem::take(&mut clip_preds)),
                    );
                }

                event_id = None;
                clip_frames.clear();
                clip_preds.clear();
            }
        }

        tokio::time::sleep(INFERENCE_INTERVAL).await;
    }

    gst_pipeline.set_state(gstreamer::State::Null)?;
    info!("monitor stopped");
    Ok(())
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn format_tracks(
    tracks: &[crate::detect::tracker::TrackedObject],
) -> String {
    tracks
        .iter()
        .map(|t| {
            format!(
                "{}#{} ({:.0}%, {:.1}s)",
                t.class,
                t.track_id,
                t.score * 100.0,
                t.duration().as_secs_f64()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_track_info(
    tracks: &[crate::detect::tracker::TrackedObject],
) -> Vec<TrackInfo> {
    tracks
        .iter()
        .map(|t| TrackInfo {
            track_id: t.track_id,
            class: t.class.clone(),
            duration_secs: t.duration().as_secs_f64(),
            movement_px: t.movement(),
            is_stationary: t.is_stationary(STATIONARY_THRESHOLD_PX),
            bbox: [t.bbox.left, t.bbox.top, t.bbox.right, t.bbox.bottom],
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn fire_webhook(
    webhook_url: &str,
    webhook_token: Option<&str>,
    hostname: &str,
    detections: &[Detection],
    jpeg: &[u8],
    detail: &str,
    phase: &str,
    event_id: &str,
    tracks: &[crate::detect::tracker::TrackedObject],
    event_duration_secs: Option<f64>,
    clip: Option<&str>,
    pre_frames: Option<Vec<String>>,
    clip_predictions: Option<Vec<ClipPredSample>>,
) {
    let now = chrono::Utc::now();
    let payload = WebhookPayload {
        ts: now.format("%b %d %H:%M:%S").to_string(),
        epoch: now.timestamp(),
        event_type: "motion".to_string(),
        detail: detail.to_string(),
        source: "clawcam".to_string(),
        host: hostname.to_string(),
        image: b64(jpeg),
        predictions: detections.to_vec(),
        event_id: Some(event_id.to_string()),
        event_phase: Some(phase.to_string()),
        tracks: Some(build_track_info(tracks)),
        event_duration_secs,
        clip: clip.map(String::from),
        pre_frames,
        clip_predictions,
    };

    let url = webhook_url.to_string();
    let token = webhook_token.map(String::from);
    tokio::spawn(async move {
        if let Err(e) = webhook::send(&url, token.as_deref(), &payload).await {
            warn!("webhook send failed: {e}");
        }
    });
}

fn fire_telemetry(
    url: &str,
    token: Option<&str>,
    hostname: &str,
    width: u32,
    height: u32,
    detections: &[Detection],
    tracks: &[crate::detect::tracker::TrackedObject],
) {
    let body = serde_json::json!({
        "host": hostname,
        "epoch_ms": chrono::Utc::now().timestamp_millis(),
        "width": width,
        "height": height,
        "predictions": detections,
        "tracks": build_track_info(tracks),
    });
    let url = url.to_string();
    let tok = token.map(String::from);
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut req = client.post(&url).json(&body);
        if let Some(t) = tok {
            req = req.bearer_auth(t);
        }
        let _ = req.send().await;
    });
}

/// Assemble JPEG frames into an MP4 clip using ffmpeg, return as base64.
async fn assemble_clip(
    frames: &[TimestampedFrame],
    working_dir: &std::path::Path,
) -> Result<String> {
    let tmp_dir = tempfile::tempdir_in(working_dir)
        .or_else(|_| tempfile::tempdir())
        .context("failed to create temp dir")?;

    // Write frames as numbered JPEGs
    for (i, frame) in frames.iter().enumerate() {
        let path = tmp_dir.path().join(format!("frame_{i:04}.jpg"));
        std::fs::write(&path, &frame.jpeg)?;
    }

    let clip_path = tmp_dir.path().join("clip.mp4");

    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-framerate", "10",
            "-i", &format!("{}/frame_%04d.jpg", tmp_dir.path().display()),
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-crf", "28",
            "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
            clip_path.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run ffmpeg — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg failed: {stderr}");
    }

    let clip_bytes = tokio::fs::read(&clip_path).await?;
    Ok(b64(&clip_bytes))
}
