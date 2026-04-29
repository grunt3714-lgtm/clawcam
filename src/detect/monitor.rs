use anyhow::{Context, Result};
use base64::Engine;
use gstreamer::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::detect::event::{EventDecision, EventManager, UpdateReason};
use crate::detect::frame_buffer::{FrameBuffer, TimestampedFrame};
use crate::detect::orientation::OrientationWatch;
use crate::detect::pipeline;
use crate::detect::ptz_track::PtzTracker;
use crate::detect::tracker::ObjectTracker;
use crate::detect::yolo::YoloDetector;
use crate::webhook::{self, ClipPredSample, Detection, TrackInfo, WebhookPayload};

// We don't videorate-throttle in GStreamer (negotiation is fragile), so this
// is the effective YOLO inference cadence. Override with CLAWCAM_INFERENCE_INTERVAL_MS.
const DEFAULT_INFERENCE_INTERVAL_MS: u64 = 100;
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

    let inference_interval = Duration::from_millis(
        std::env::var("CLAWCAM_INFERENCE_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_INFERENCE_INTERVAL_MS),
    );
    info!("inference interval: {} ms", inference_interval.as_millis());

    let mut detector = YoloDetector::load(&model_path)?;
    info!("YOLO model loaded");

    // Optional PTZ auto-tracker (UVC). Disabled unless CLAWCAM_PTZ_TRACK=1.
    let mut ptz_tracker = PtzTracker::from_env();

    // Orientation watchdog — fires a one-shot webhook per track when the bbox
    // aspect indicates the person isn't upright for `CLAWCAM_UPRIGHT_CONFIRM_MS`.
    // Opt-in via `CLAWCAM_UPRIGHT_CHECK=1`.
    let mut orientation_watch = OrientationWatch::from_env();
    if orientation_watch.enabled() {
        info!("orientation watchdog enabled");
    }

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

    // Install shutdown handler. The main loop polls this flag each iteration;
    // on SIGTERM/SIGINT we break out and flush an end-phase webhook for any
    // event that's still active, so the app doesn't leave it stuck as "active"
    // with no clip when systemd restarts the service mid-event.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("failed to install SIGTERM handler: {e}");
                    return;
                }
            };
            let mut intr = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("failed to install SIGINT handler: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => info!("SIGTERM received, initiating shutdown"),
                _ = intr.recv() => info!("SIGINT received, initiating shutdown"),
            }
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    // Pipeline supervisor — watches the GStreamer bus for ERROR/EOS and triggers
    // shutdown so the main loop's end-phase cleanup runs and systemd respawns us.
    // Why: rtspclientsink does not auto-reconnect when MediaMTX restarts; the
    // sink errors out and the pipeline goes silent while detection keeps running,
    // making clawcam look healthy from the outside while no frames reach MediaMTX.
    {
        let bus = gst_pipeline
            .bus()
            .context("pipeline bus unavailable")?;
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Some(msg) =
                    bus.timed_pop(gstreamer::ClockTime::from_mseconds(500))
                else {
                    continue;
                };
                match msg.view() {
                    gstreamer::MessageView::Error(e) => {
                        let src = e
                            .src()
                            .map(|o| o.name().to_string())
                            .unwrap_or_else(|| "?".to_string());
                        warn!(
                            "pipeline bus ERROR from {src}: {} (debug: {:?}) — triggering shutdown for respawn",
                            e.error(),
                            e.debug()
                        );
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                    gstreamer::MessageView::Eos(_) => {
                        warn!("pipeline bus EOS — triggering shutdown for respawn");
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    let telemetry_url = std::env::var("CLAWCAM_TELEMETRY_URL").ok();
    let telemetry_token = webhook_token_owned.clone();

    // Adaptive monitoring components
    let mut frame_buffer = FrameBuffer::new(FRAME_BUFFER_CAPACITY);
    let mut tracker = ObjectTracker::new();
    let mut event_mgr = EventManager::new();
    let mut event_id: Option<String> = None;
    let mut clip_frames: Vec<TimestampedFrame> = Vec::new();
    let mut clip_preds: Vec<ClipPredSample> = Vec::new();

    // Rolling inference timing, logged every N cycles so users can tune
    // CLAWCAM_YOLO_INPUT_SIZE / CLAWCAM_INFERENCE_INTERVAL_MS.
    let mut infer_ms_sum: u128 = 0;
    let mut infer_count: u32 = 0;
    const INFER_LOG_EVERY: u32 = 40;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Pull a frame and then drain any backlog, keeping only the newest.
        // The GStreamer mpsc channel fills while inference runs, and while
        // the PTZ motor is mid-burst the backlog contains stale frames that
        // would make the tracker overshoot (acting on where the subject was,
        // not where it is). Always inference on what's current.
        let mut frame = match frame_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                warn!("no frame received in 5s, pipeline may be stalled");
                continue;
            }
            Err(_) => break,
        };
        let mut dropped = 0u32;
        while let Ok(newer) = frame_rx.try_recv() {
            frame = newer;
            dropped += 1;
        }
        if dropped > 0 {
            tracing::debug!("dropped {dropped} stale frames before inference");
        }

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
        let infer_start = std::time::Instant::now();
        let detections = match detector.detect(&frame.data, frame.width, frame.height) {
            Ok(d) => d,
            Err(e) => {
                warn!("inference failed: {e}");
                continue;
            }
        };
        infer_ms_sum += infer_start.elapsed().as_millis();
        infer_count += 1;
        if infer_count >= INFER_LOG_EVERY {
            let avg = infer_ms_sum as f64 / infer_count as f64;
            info!("inference: avg {:.1} ms over last {} frames", avg, infer_count);
            infer_ms_sum = 0;
            infer_count = 0;
        }

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

        // Steer the camera to keep the most persistent subject centered.
        if let Some(pt) = ptz_tracker.as_mut() {
            pt.update(&tracks, frame.width, frame.height);
        }

        // Orientation watchdog: one-shot alert per track when the person's
        // bbox aspect indicates they're not upright (fallen / lying down /
        // sideways relative to expected mount). Evaluates every cycle but
        // debounces internally — only returns track IDs crossing the threshold.
        for track_id in orientation_watch.evaluate(&tracks) {
            let track = tracks.iter().find(|t| t.track_id == track_id);
            let bbox_str = track
                .map(|t| format!("{}x{}", t.bbox.right - t.bbox.left, t.bbox.bottom - t.bbox.top))
                .unwrap_or_default();
            info!("orientation alert: track#{track_id} not upright (bbox {bbox_str})");
            let eid = event_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            fire_webhook(
                webhook_url,
                webhook_token,
                &hostname,
                &detections,
                &jpeg,
                "ai_orientation_unusual",
                "update",
                &eid,
                track.into_iter().cloned().collect::<Vec<_>>().as_slice(),
                None,
                None,
                None,
                None,
            );
        }

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

        tokio::time::sleep(inference_interval).await;
    }

    // If we're shutting down mid-event, synthesize an end-phase webhook so the
    // app can close the event out with whatever clip we've managed to buffer
    // (even a partial clip is better than a permanently-"active" stranded row).
    if let Some(eid) = event_id.take() {
        let dur_secs = event_mgr
            .event_start()
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        warn!(
            "shutdown with active event {eid}: flushing end-phase webhook ({:.1}s, {} frames)",
            dur_secs,
            clip_frames.len()
        );

        let clip_b64 = if clip_frames.len() > 10 {
            match assemble_clip(&clip_frames, &frame_dir).await {
                Ok(data) => Some(data),
                Err(e) => {
                    warn!("shutdown clip assembly failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let last_jpeg = frame_buffer
            .recent(1)
            .first()
            .map(|f| f.jpeg.clone())
            .unwrap_or_default();
        let last_tracks = tracker.active_tracks().to_vec();

        let now = chrono::Utc::now();
        let payload = WebhookPayload {
            ts: now.format("%b %d %H:%M:%S").to_string(),
            epoch: now.timestamp(),
            event_type: "motion".to_string(),
            detail: "ai_event_shutdown_flush".to_string(),
            source: "clawcam".to_string(),
            host: hostname.clone(),
            image: b64(&last_jpeg),
            predictions: Vec::new(),
            event_id: Some(eid.clone()),
            event_phase: Some("end".to_string()),
            tracks: Some(build_track_info(&last_tracks)),
            event_duration_secs: Some(dur_secs),
            clip: clip_b64,
            pre_frames: None,
            clip_predictions: Some(std::mem::take(&mut clip_preds)),
        };

        match tokio::time::timeout(
            Duration::from_secs(10),
            webhook::send(webhook_url, webhook_token, &payload),
        )
        .await
        {
            Ok(Ok(())) => info!("shutdown end webhook delivered for {eid}"),
            Ok(Err(e)) => warn!("shutdown end webhook failed: {e}"),
            Err(_) => warn!("shutdown end webhook timed out after 10s"),
        }
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

/// Assemble JPEG frames into an MP4 clip using GStreamer, return as base64.
/// No external ffmpeg dependency — reuses the gst-plugins that are already
/// present (x264enc + mp4mux + multifilesrc + h264parse).
async fn assemble_clip(
    frames: &[TimestampedFrame],
    working_dir: &std::path::Path,
) -> Result<String> {
    let tmp_dir = tempfile::tempdir_in(working_dir)
        .or_else(|_| tempfile::tempdir())
        .context("failed to create temp dir")?;

    // Write frames as numbered JPEGs — multifilesrc ingests them in sequence.
    for (i, frame) in frames.iter().enumerate() {
        let path = tmp_dir.path().join(format!("frame_{i:04}.jpg"));
        std::fs::write(&path, &frame.jpeg)?;
    }

    let frame_pattern = tmp_dir.path().join("frame_%04d.jpg");
    let clip_path = tmp_dir.path().join("clip.mp4");
    let stop_index = frames.len().saturating_sub(1);

    let tmp_path = tmp_dir.path().to_path_buf();
    let clip_path_for_thread = clip_path.clone();
    // GStreamer's blocking bus wait doesn't play nicely with tokio's executor;
    // run the whole pipeline on a dedicated blocking thread.
    tokio::task::spawn_blocking(move || -> Result<()> {
        use gstreamer as gst;
        use gstreamer::prelude::*;

        gst::init().ok();
        let desc = format!(
            "multifilesrc location=\"{}\" start-index=0 stop-index={} caps=image/jpeg,framerate=10/1 ! \
             jpegdec ! videoconvert ! video/x-raw,format=I420 ! \
             x264enc speed-preset=ultrafast tune=zerolatency bframes=0 key-int-max=30 ! \
             h264parse config-interval=-1 ! mp4mux faststart=true ! \
             filesink location=\"{}\"",
            frame_pattern.display(),
            stop_index,
            clip_path_for_thread.display(),
        );
        let pipeline = gst::parse::launch(&desc).context("assemble_clip parse_launch")?;
        pipeline.set_state(gst::State::Playing).context("assemble_clip start")?;

        let bus = pipeline.bus().context("assemble_clip no bus")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let result = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Err(anyhow::anyhow!("assemble_clip: GStreamer pipeline timed out"));
            }
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(remaining.as_millis() as u64))
            else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Eos(_) => break Ok(()),
                gst::MessageView::Error(e) => {
                    break Err(anyhow::anyhow!(
                        "assemble_clip gstreamer error: {}",
                        e.error()
                    ));
                }
                _ => {}
            }
        };
        let _ = pipeline.set_state(gst::State::Null);
        result.map(|_| ())?;
        let _ = tmp_path; // keep tempdir alive for the duration of the pipeline
        Ok(())
    })
    .await
    .context("assemble_clip join")??;

    let clip_bytes = tokio::fs::read(&clip_path).await?;
    Ok(b64(&clip_bytes))
}
