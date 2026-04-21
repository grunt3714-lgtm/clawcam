//! VISCA-driven pan/tilt auto-tracking with velocity prediction and variable motor speed.
//!
//! Architecture (three cooperating pieces):
//!
//! 1. **Observation** — the monitor loop calls `update()` every inference cycle
//!    (~3 Hz). We pick a target track, compute its bbox-center velocity via a
//!    low-pass-filtered finite difference, and publish a snapshot to a
//!    `tokio::sync::watch` channel.
//!
//! 2. **Steering task** — a background tokio task ticks at ~10 Hz, reads the
//!    latest observation, extrapolates the target's position forward by a
//!    look-ahead horizon (covers inference + motor latency), and drives the
//!    camera with VISCA bursts. Running faster than inference means direction
//!    changes propagate within ~100 ms instead of waiting for the next YOLO
//!    result ~340 ms later.
//!
//! 3. **VISCA worker** — serializes outgoing PTZ HTTP requests to the
//!    in-process `src/ptz.rs` server on `127.0.0.1:8091`, the same control
//!    path the web UI uses.
//!
//! Each outgoing drive command carries a *variable* pan/tilt speed proportional
//! to how far off-center the predicted target is — big offsets move the motor
//! fast, small offsets creep. This is the single biggest smoothness lever,
//! eliminating the bang-bang feel of fixed-speed commands.
//!
//! Enabled only when `CLAWCAM_PTZ_TRACK=1`, so non-PTZ deployments (pi-cam
//! etc.) never touch any of this.

use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::detect::tracker::TrackedObject;

pub struct PtzTracker {
    /// Selected target across update() calls. Sticky — we keep following the
    /// same `track_id` until it vanishes, then fall back to the longest-lived.
    locked_track_id: Option<u64>,
    /// Monotonic timestamp of the most recent non-empty target, for idle recenter logic.
    last_seen_track: Option<Instant>,
    recenter_after: Option<Duration>,
    /// Last observed bbox center and timestamp — used to derive pixel/second velocity.
    last_center: Option<(f32, f32)>,
    last_obs_time: Option<Instant>,
    /// Low-pass-filtered velocity in pixels/second of the tracked bbox center.
    velocity: (f32, f32),
    /// Channel to the steering task.
    obs_tx: watch::Sender<Observation>,
}

/// Snapshot the monitor loop publishes every inference cycle. The steering
/// task reads this plus its own timestamp to extrapolate the current target
/// position in between inferences.
#[derive(Clone, Copy, Debug)]
struct Observation {
    /// `None` means no target in frame right now (stop tracking).
    center: Option<(f32, f32)>,
    velocity: (f32, f32),
    frame_w: u32,
    frame_h: u32,
    observed_at: Instant,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            center: None,
            velocity: (0.0, 0.0),
            frame_w: 0,
            frame_h: 0,
            observed_at: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Home reserved for future re-center-on-idle wiring
enum Command {
    /// VISCA drive burst with per-axis direction (-1/0/+1) and VISCA speed bytes.
    Burst {
        pan: i32,
        tilt: i32,
        pan_speed: u8,
        tilt_speed: u8,
        duration_ms: u64,
    },
    Stop,
    Home,
}

struct SteeringConfig {
    deadzone: f32,
    /// Seconds ahead to predict — should roughly cover inference + motor latency.
    lookahead_s: f32,
    /// VISCA speed byte range for pan (0x01..=0x18). Scaled linearly with offset magnitude.
    pan_speed_min: u8,
    pan_speed_max: u8,
    /// VISCA speed byte range for tilt (0x01..=0x14).
    tilt_speed_min: u8,
    tilt_speed_max: u8,
    /// Each burst tells the motor to keep going for this long. Must exceed the
    /// steering tick so we never leave a gap where the server's auto-stop fires.
    drive_duration_ms: u64,
    /// How often to refresh the motor even if direction is unchanged (keep-alive).
    refresh_interval: Duration,
    /// How often the steering task evaluates and potentially updates the motor.
    tick: Duration,
}

impl PtzTracker {
    pub fn from_env() -> Option<Self> {
        if std::env::var("CLAWCAM_PTZ_TRACK").ok().as_deref() != Some("1") {
            return None;
        }

        let endpoint = std::env::var("CLAWCAM_PTZ_HTTP")
            .unwrap_or_else(|_| "http://127.0.0.1:8091/ptz".to_string());
        let token = std::env::var("CLAWCAM_PTZ_TOKEN").ok();
        let deadzone = env_float("CLAWCAM_PTZ_DEADZONE_PCT", 8.0).clamp(0.0, 50.0) / 100.0;
        let lookahead_ms = env_int("CLAWCAM_PTZ_LOOKAHEAD_MS", 450).clamp(0, 2000) as f32;
        let pan_speed_min = env_int("CLAWCAM_PTZ_PAN_SPEED_MIN", 3).clamp(1, 24) as u8;
        let pan_speed_max = env_int("CLAWCAM_PTZ_PAN_SPEED_MAX", 20).clamp(1, 24) as u8;
        let tilt_speed_min = env_int("CLAWCAM_PTZ_TILT_SPEED_MIN", 3).clamp(1, 20) as u8;
        let tilt_speed_max = env_int("CLAWCAM_PTZ_TILT_SPEED_MAX", 16).clamp(1, 20) as u8;
        let drive_duration_ms = env_int("CLAWCAM_PTZ_DRIVE_MS", 600).clamp(200, 10_000) as u64;
        let refresh_interval =
            Duration::from_millis(env_int("CLAWCAM_PTZ_REFRESH_MS", 300).clamp(50, 10_000) as u64);
        let tick =
            Duration::from_millis(env_int("CLAWCAM_PTZ_TICK_MS", 100).clamp(20, 2000) as u64);
        let recenter_after = std::env::var("CLAWCAM_PTZ_RECENTER_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs);

        info!(
            "PTZ tracking enabled (VISCA via {endpoint}): \
             deadzone={:.0}% lookahead={lookahead_ms:.0}ms \
             pan_speed=[{pan_speed_min},{pan_speed_max}] tilt_speed=[{tilt_speed_min},{tilt_speed_max}] \
             drive={drive_duration_ms}ms refresh={}ms tick={}ms",
            deadzone * 100.0,
            refresh_interval.as_millis(),
            tick.as_millis(),
        );

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(8);
        {
            let endpoint = endpoint.clone();
            let token = token.clone();
            tokio::spawn(async move {
                command_worker(endpoint, token, cmd_rx).await;
            });
        }

        let cfg = SteeringConfig {
            deadzone,
            lookahead_s: lookahead_ms / 1000.0,
            pan_speed_min,
            pan_speed_max,
            tilt_speed_min,
            tilt_speed_max,
            drive_duration_ms,
            refresh_interval,
            tick,
        };

        let (obs_tx, obs_rx) = watch::channel(Observation::default());
        {
            let cmd_tx = cmd_tx.clone();
            tokio::spawn(async move {
                steering_task(cfg, obs_rx, cmd_tx).await;
            });
        }

        Some(Self {
            locked_track_id: None,
            last_seen_track: None,
            recenter_after,
            last_center: None,
            last_obs_time: None,
            velocity: (0.0, 0.0),
            obs_tx,
        })
    }

    /// Called by the monitor loop once per inference cycle. Picks the target,
    /// updates the velocity estimate, and publishes a fresh observation.
    pub fn update(&mut self, tracks: &[TrackedObject], frame_w: u32, frame_h: u32) {
        let now = Instant::now();
        let target = self.select_target(tracks).cloned();

        let center = target.as_ref().map(|t| {
            (
                (t.bbox.left + t.bbox.right) as f32 / 2.0,
                (t.bbox.top + t.bbox.bottom) as f32 / 2.0,
            )
        });

        // Update velocity estimate (low-pass filtered finite difference).
        if let (Some(c), Some(prev_c), Some(prev_t)) = (center, self.last_center, self.last_obs_time) {
            let dt = now.duration_since(prev_t).as_secs_f32();
            if dt > 0.01 && dt < 1.0 {
                let vx = (c.0 - prev_c.0) / dt;
                let vy = (c.1 - prev_c.1) / dt;
                // Exponential moving average to smooth noisy bbox jitter.
                let alpha = 0.55;
                self.velocity.0 = alpha * vx + (1.0 - alpha) * self.velocity.0;
                self.velocity.1 = alpha * vy + (1.0 - alpha) * self.velocity.1;
            }
        } else {
            // No history, or target just re-acquired — no reliable velocity.
            self.velocity = (0.0, 0.0);
        }
        self.last_center = center;
        self.last_obs_time = Some(now);

        if target.is_some() {
            self.last_seen_track = Some(now);
            self.locked_track_id = target.as_ref().map(|t| t.track_id);
        } else {
            self.locked_track_id = None;
        }

        let obs = Observation {
            center,
            velocity: self.velocity,
            frame_w,
            frame_h,
            observed_at: now,
        };
        let _ = self.obs_tx.send(obs);

        // Idle-return-to-center logic is decoupled from steering tick.
        if target.is_none() {
            self.maybe_recenter();
        }
    }

    fn select_target<'a>(&self, tracks: &'a [TrackedObject]) -> Option<&'a TrackedObject> {
        if let Some(id) = self.locked_track_id {
            if let Some(t) = tracks.iter().find(|t| t.track_id == id) {
                return Some(t);
            }
        }
        tracks.iter().max_by_key(|t| t.duration().as_millis())
    }

    fn maybe_recenter(&mut self) {
        let (Some(last), Some(after)) = (self.last_seen_track, self.recenter_after) else {
            return;
        };
        if last.elapsed() < after {
            return;
        }
        // We don't hold a direct cmd_tx handle; piggyback on the steering
        // task by publishing a "no-target" observation (which steers to stop)
        // and issuing the home command via a dedicated watch event isn't
        // currently modeled. For now the tracker stays stopped, which is
        // fine — the user can hit home on the web UI if they want recentered.
        info!("ptz: idle >{}s, motor remains stopped until next detection", after.as_secs());
        self.last_seen_track = None;
    }
}

/// Background task that continuously reads the latest observation, extrapolates
/// the target's predicted position, and issues VISCA bursts. Runs at `cfg.tick`
/// rate — faster than inference — so direction changes propagate quickly.
async fn steering_task(
    cfg: SteeringConfig,
    obs_rx: watch::Receiver<Observation>,
    cmd_tx: mpsc::Sender<Command>,
) {
    let mut current_motion: (i32, i32) = (0, 0);
    let mut current_speeds: (u8, u8) = (0, 0);
    let mut last_drive_at = Instant::now() - cfg.refresh_interval * 2;
    let mut interval = tokio::time::interval(cfg.tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        let obs = *obs_rx.borrow();

        // No target → ensure motor is stopped.
        let Some(center) = obs.center else {
            if current_motion != (0, 0) {
                let _ = cmd_tx.try_send(Command::Stop);
                current_motion = (0, 0);
            }
            continue;
        };
        if obs.frame_w == 0 || obs.frame_h == 0 {
            continue;
        }

        // Predict where the target will be `lookahead_s` from now, using last
        // bbox sample + velocity + time since sample. Offsets inference lag
        // and motor latency. Clamp the extrapolation dt to avoid runaway if
        // observations get stale (e.g. camera paused).
        let dt_since_obs = obs.observed_at.elapsed().as_secs_f32().min(1.5);
        let project = dt_since_obs + cfg.lookahead_s;
        let predicted_cx = center.0 + obs.velocity.0 * project;
        let predicted_cy = center.1 + obs.velocity.1 * project;

        let ox = (predicted_cx - obs.frame_w as f32 / 2.0) / (obs.frame_w as f32 / 2.0);
        let oy = (predicted_cy - obs.frame_h as f32 / 2.0) / (obs.frame_h as f32 / 2.0);

        let pan_active = ox.abs() >= cfg.deadzone;
        let tilt_active = oy.abs() >= cfg.deadzone;
        if !pan_active && !tilt_active {
            if current_motion != (0, 0) {
                let _ = cmd_tx.try_send(Command::Stop);
                current_motion = (0, 0);
            }
            continue;
        }

        // Direction bytes: VISCA server uses +pan = right, +tilt = up.
        // Image y grows downward → tilt = -oy.signum. Installation-level
        // mount flips (upside-down) are applied inside the VISCA server.
        let pan_dir = if pan_active { ox.signum() as i32 } else { 0 };
        let tilt_dir = if tilt_active { -oy.signum() as i32 } else { 0 };

        // P-controller: speed scales linearly with |offset|, bottoming out at
        // min_speed just past the deadzone and capping at max_speed at the edge.
        let pan_speed = if pan_active {
            scale_speed(ox.abs(), cfg.deadzone, cfg.pan_speed_min, cfg.pan_speed_max)
        } else {
            0
        };
        let tilt_speed = if tilt_active {
            scale_speed(oy.abs(), cfg.deadzone, cfg.tilt_speed_min, cfg.tilt_speed_max)
        } else {
            0
        };

        let desired = (pan_dir, tilt_dir);
        let speed_changed = (pan_speed, tilt_speed) != current_speeds;
        let direction_changed = desired != current_motion;
        let refresh_due = last_drive_at.elapsed() >= cfg.refresh_interval;

        if !direction_changed && !speed_changed && !refresh_due {
            continue;
        }

        let cmd = Command::Burst {
            pan: pan_dir,
            tilt: tilt_dir,
            pan_speed,
            tilt_speed,
            duration_ms: cfg.drive_duration_ms,
        };
        if cmd_tx.try_send(cmd).is_err() {
            // Worker is behind — skip this tick, next one will try again.
            continue;
        }
        if direction_changed {
            tracing::info!(
                "ptz: dir change ox={:.2} oy={:.2} v=({:.0},{:.0}) → pan={} tilt={} (speeds {}/{})",
                ox, oy, obs.velocity.0, obs.velocity.1, pan_dir, tilt_dir, pan_speed, tilt_speed,
            );
        }
        current_motion = desired;
        current_speeds = (pan_speed, tilt_speed);
        last_drive_at = Instant::now();
    }
}

/// Convert a normalized offset magnitude in [deadzone, 1.0] to a VISCA speed
/// byte in [min, max]. Just past the deadzone → min; at frame edge → max.
fn scale_speed(abs_offset: f32, deadzone: f32, min: u8, max: u8) -> u8 {
    if max <= min {
        return min.max(1);
    }
    let range_hi = (1.0 - deadzone).max(0.001);
    let t = ((abs_offset - deadzone).max(0.0) / range_hi).min(1.0);
    let lo = min as f32;
    let hi = max as f32;
    let v = lo + t * (hi - lo);
    v.round().clamp(1.0, 24.0) as u8
}

fn env_float(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

fn env_int(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

async fn command_worker(endpoint: String, token: Option<String>, mut rx: mpsc::Receiver<Command>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("ptz worker: could not build http client: {e:#}");
            return;
        }
    };

    while let Some(cmd) = rx.recv().await {
        let body = match cmd {
            Command::Burst { pan, tilt, pan_speed, tilt_speed, duration_ms } => serde_json::json!({
                "pan": pan,
                "tilt": tilt,
                "zoom": 0,
                "duration_ms": duration_ms,
                "pan_speed": pan_speed,
                "tilt_speed": tilt_speed,
            }),
            Command::Stop => serde_json::json!({ "stop": true }),
            Command::Home => serde_json::json!({ "home": true }),
        };

        let mut req = client.post(&endpoint).json(&body);
        if let Some(t) = token.as_deref() {
            req = req.bearer_auth(t);
        }
        if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) {
            warn!("ptz worker: POST {endpoint} failed: {e}");
        }
    }
}
