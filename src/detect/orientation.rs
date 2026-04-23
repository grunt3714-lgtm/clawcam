//! Fires a webhook when a tracked person's bbox aspect ratio indicates they
//! are not upright (lying down, fallen, or in a posture inconsistent with the
//! expected one). Debounced so a brief bend-over doesn't trigger.
//!
//! Enabled only when `CLAWCAM_UPRIGHT_CHECK=1`. Each track fires at most once
//! per appearance; a departure + return re-arms it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::detect::tracker::TrackedObject;

pub struct OrientationConfig {
    pub enabled: bool,
    /// Anything with `height/width` below this (i.e. wider than tall relative to this ratio)
    /// is considered "not upright". For a normal standing person on an upright-mounted camera,
    /// height/width is typically >1.5, so a cutoff of 1.0 flags clear landscape bboxes.
    pub min_height_to_width: f32,
    /// If true, the camera is mounted sideways so an "upright" person produces a
    /// landscape bbox — the comparison is inverted.
    pub upright_is_landscape: bool,
    /// How long a track must stay in the "not upright" state before we fire.
    pub confirm_after: Duration,
}

impl OrientationConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("CLAWCAM_UPRIGHT_CHECK").ok().as_deref() == Some("1");
        let min_height_to_width = std::env::var("CLAWCAM_UPRIGHT_MIN_H_TO_W")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0_f32);
        let upright_is_landscape =
            std::env::var("CLAWCAM_UPRIGHT_IS_LANDSCAPE").ok().as_deref() == Some("1");
        let confirm_after = Duration::from_millis(
            std::env::var("CLAWCAM_UPRIGHT_CONFIRM_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3_000_u64),
        );
        Self {
            enabled,
            min_height_to_width,
            upright_is_landscape,
            confirm_after,
        }
    }
}

pub struct OrientationWatch {
    cfg: OrientationConfig,
    /// Per-track: when we first observed a bad posture in the current streak.
    /// Cleared when the track recovers an upright pose or disappears.
    first_bad: HashMap<u64, Instant>,
    /// Track IDs we've already fired for. Cleared on track departure.
    fired: HashMap<u64, ()>,
}

impl OrientationWatch {
    pub fn from_env() -> Self {
        Self {
            cfg: OrientationConfig::from_env(),
            first_bad: HashMap::new(),
            fired: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Returns track IDs that just crossed the "not upright" threshold on this call.
    /// Call once per inference cycle with the current track set.
    pub fn evaluate(&mut self, tracks: &[TrackedObject]) -> Vec<u64> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let now = Instant::now();
        let mut newly_confirmed = Vec::new();

        let live_ids: Vec<u64> = tracks.iter().map(|t| t.track_id).collect();
        // Drop state for tracks that have disappeared (re-arm on return).
        self.first_bad.retain(|id, _| live_ids.contains(id));
        self.fired.retain(|id, _| live_ids.contains(id));

        for track in tracks {
            // Only check `person` — other classes don't have a well-defined upright pose.
            if track.class != "person" {
                self.first_bad.remove(&track.track_id);
                continue;
            }
            if self.fired.contains_key(&track.track_id) {
                continue;
            }

            let w = track.bbox.right.saturating_sub(track.bbox.left) as f32;
            let h = track.bbox.bottom.saturating_sub(track.bbox.top) as f32;
            if w < 1.0 || h < 1.0 {
                continue;
            }
            let ratio = if self.cfg.upright_is_landscape { w / h } else { h / w };
            let upright = ratio >= self.cfg.min_height_to_width;

            if upright {
                self.first_bad.remove(&track.track_id);
                continue;
            }
            let first = *self
                .first_bad
                .entry(track.track_id)
                .or_insert(now);
            if now.duration_since(first) >= self.cfg.confirm_after {
                self.fired.insert(track.track_id, ());
                newly_confirmed.push(track.track_id);
            }
        }

        newly_confirmed
    }
}
