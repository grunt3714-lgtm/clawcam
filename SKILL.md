---
name: clawcam
description: AI-powered camera monitoring for Raspberry Pi — YOLOv8 detection with object tracking, adaptive event lifecycle, video clip recording, pre-detection frames, structured predictions, webhooks, TTS, and mic capture.
metadata: {"clawdbot":{"emoji":"📷","requires":{"bins":["clawcam"]}}}
---

# clawcam Skill

A specialized tool for managing AI-powered camera devices on Raspberry Pi via `clawcam`. Register devices, deploy the on-device YOLO detection monitor with adaptive object tracking and event lifecycle management, capture snapshots or video clips, speak through the device speaker, and listen via the device microphone.

## Overview

`clawcam` SSHes into your Raspberry Pi, deploys a monitor binary with a YOLOv8n ONNX model, and pushes detection events directly to your webhook endpoint. GStreamer handles camera capture from any connected source — Pi Camera Module, USB webcam, or conference camera.

The monitor tracks objects across frames with persistent IDs, manages event lifecycles (start → update → complete), and records video clips automatically. Each event webhook includes a 1080p snapshot, structured AI predictions, tracking data (duration, movement, stationary status), and on event completion, an MP4 clip assembled from buffered frames.

## Requirements

- **Hardware:** Raspberry Pi (3B+/4/5) with any supported camera:
  - Pi Camera Module (via libcamera)
  - USB webcam (via V4L2)
  - USB conference camera (via V4L2)
- **Network:** SSH key access to `pi@<device_ip>` (or custom user).
- **Host Software:** `clawcam` binary installed and available in the system PATH.
- **Host Software:** Cross-compiled `clawcam` binary for the target Pi architecture (aarch64 or armv7).
- **Host Software:** YOLOv8n ONNX model at `models/yolov8n.onnx`.

## Usage

### 0. Register a Device
Add a Pi camera to the device registry before using any other commands.
```bash
clawcam device add barn-cam 192.168.1.50
clawcam device add garage-cam 192.168.1.51 --port 2222 --user admin
clawcam device list
clawcam device remove barn-cam
```

### 1. Deploy with Webhook
Deploy the on-device monitor that pushes detection events directly to your endpoint.
```bash
clawcam setup barn-cam \
  --webhook http://your-host:8080/hooks/clawcam \
  --webhook-token YOUR_TOKEN
```

Setup auto-detects the connected camera (libcamera or V4L2), installs GStreamer and dependencies, uploads the binary and YOLO model, and creates a systemd service for boot persistence.

### 2. Check Device Status
Monitor the health of the detection pipeline, camera, model, and system resources.
```bash
clawcam status barn-cam
```
*Note: Add `--json` for machine-readable output.*

### 3. Speak Through Device
Send a text-to-speech message through the device's speaker.
```bash
clawcam speak barn-cam "Hello, this is a security notice"
```
*Optional Flags:*
- `--volume <1-100>`: Speaker volume (default 80).

### 4. Listen to Device Mic
Capture audio from the device microphone.
```bash
clawcam listen barn-cam --dur 10 --out recording.wav
```

### 5. Capture Media
Capture snapshots or clips from the camera.

**Snapshot:**
```bash
clawcam snap barn-cam --out shot.jpg
```

**Clip:**
```bash
clawcam clip barn-cam --dur 10 --out clip.mp4
```

### 6. Pan / Tilt / Zoom (VISCA conference cameras)

PTZ drives VISCA commands over the on-device serial port (configured via `CLAWCAM_PTZ_SERIAL` on the Pi, exposed as an HTTP server on port 8091). This is the same control path as the clawcam-app web UI — any motion that works in the browser works here.

> Heads-up: many cheap USB webcams advertise UVC PTZ descriptors but don't have physical motors. `clawcam ptz` does **not** use `v4l2-ctl` — it only drives real VISCA-capable conference cams connected to a serial (USB-to-RS-232 adapter) interface.

```bash
# Return the camera to its home/center position
clawcam ptz pond-cam center

# Stop all motion immediately
clawcam ptz pond-cam stop

# Direction burst, each axis is -1, 0, or +1. Auto-stops after --duration ms.
# pan:  -1 = left, +1 = right
# tilt: -1 = down, +1 = up
# zoom: -1 = wide, +1 = tele
clawcam ptz pond-cam nudge --pan +1 --duration 500
clawcam ptz pond-cam nudge --tilt -1
clawcam ptz pond-cam nudge --zoom +1 --duration 200
```

Auto-tracking (object-follow) runs inside the on-device monitor and POSTs the same direction bursts to `http://127.0.0.1:8091/ptz`. Configure via `CLAWCAM_PTZ_*` env — see below.

### 7. Teardown
Stop the on-device monitor and clean up.
```bash
clawcam teardown barn-cam
```

### 8. Update

Pull the latest release from GitHub and replace the binary. Works for the local install or any registered device.

```bash
# Update the local clawcam binary
clawcam update

# Update a specific device (SSHes, installs to /usr/local/bin, restarts service)
clawcam update pond-cam

# Update every registered device
clawcam update --all

# Pin a specific version (applies to any variant above)
clawcam update --version v0.4.1
clawcam update pond-cam --version v0.4.1
```

## Webhook Payload

Each event produces up to 3 webhooks across its lifecycle. All fields after `predictions` are optional and backward-compatible.

### Initial alert (`event_phase: "start"`)

```json
{
  "ts": "Apr 17 14:30:45",
  "epoch": 1776437445,
  "type": "motion",
  "detail": "ai_detected",
  "source": "clawcam",
  "host": "192.168.1.50",
  "image": "<base64 1080p JPEG>",
  "predictions": [
    { "class": "person", "class_id": 0, "score": 0.87, "left": 120, "top": 80, "right": 320, "bottom": 430 }
  ],
  "event_id": "a3db45af-f756-43c2-8dea-aa30276903a5",
  "event_phase": "start",
  "tracks": [
    { "track_id": 1, "class": "person", "duration_secs": 0.0, "movement_px": 0.0, "is_stationary": true, "bbox": [120, 80, 320, 430] }
  ],
  "pre_frames": ["<base64 JPEG>", "<base64 JPEG>", "<base64 JPEG>"]
}
```

### Tracking update (`event_phase: "update"`)

Sent for prolonged events (>3s) or new arrivals. `detail` is `"ai_tracking_update"` or `"ai_new_arrival"`.

### Final report (`event_phase: "end"`)

Sent 3s after all objects leave. Includes an MP4 clip.

```json
{
  "detail": "ai_event_complete",
  "event_phase": "end",
  "event_id": "a3db45af-...",
  "event_duration_secs": 18.1,
  "clip": "<base64 MP4>"
}
```
```

## Detection Classes

Uses the full COCO 80-class set via YOLOv8n. Key classes for monitoring:

| Class | ID |
|-------|----|
| person | 0 |
| bicycle | 1 |
| car | 2 |
| motorcycle | 3 |
| bus | 5 |
| truck | 7 |
| bird | 14 |
| cat | 15 |
| dog | 16 |
| backpack | 24 |
| suitcase | 28 |

Default confidence threshold: 0.6. Default IOU threshold: 0.45.

## Environment Variables

The on-device monitor reads (set via `/etc/clawcam.env`):

| Variable | Description | Default |
|----------|-------------|---------|
| `CLAWCAM_CAMERA_SOURCE` | GStreamer source element | `v4l2src` |
| `CLAWCAM_MODEL_PATH` | Path to ONNX model | `/usr/local/share/clawcam/yolov8n.onnx` |
| `CLAWCAM_WEBHOOK` | Webhook URL | *(required)* |
| `CLAWCAM_WEBHOOK_TOKEN` | Bearer token for webhook auth | *(optional)* |
| `CLAWCAM_CONF_THRESHOLD` | Detection confidence threshold (0.1–1.0) | `0.6` |
| `CLAWCAM_INFERENCE_INTERVAL_MS` | Sleep between YOLO inferences. Lower = more FPS, more CPU | `100` |
| `CLAWCAM_YOLO_INPUT_SIZE` | YOLO input dimensions (multiples of 32). 320/416 are big Pi CPU speedups | `640` |

### Auto-tracking (UVC cameras only)

Set these in `/etc/clawcam.env` to make the camera follow the most persistent detection:

| Variable | Description | Default |
|----------|-------------|---------|
| `CLAWCAM_PTZ_TRACK` | `1` to enable auto-tracking | *(disabled)* |
| `CLAWCAM_PTZ_HTTP` | VISCA server endpoint the tracker POSTs to | `http://127.0.0.1:8091/ptz` |
| `CLAWCAM_PTZ_TOKEN` | Bearer token, if the VISCA server requires one | *(optional)* |
| `CLAWCAM_PTZ_DEADZONE_PCT` | % of half-frame where camera won't move | `10` |
| `CLAWCAM_PTZ_GAIN` | Scales burst duration with offset magnitude | `1.0` |
| `CLAWCAM_PTZ_MIN_DURATION_MS` | Shortest motion burst issued when outside the deadzone | `120` |
| `CLAWCAM_PTZ_MAX_DURATION_MS` | Longest motion burst (when target is near the frame edge) | `700` |
| `CLAWCAM_PTZ_PAN_INVERT` | `1` to flip pan direction (e.g. upside-down mount) | *(no)* |
| `CLAWCAM_PTZ_TILT_INVERT` | `1` to flip tilt direction | *(no)* |
| `CLAWCAM_PTZ_RECENTER_SEC` | Seconds of no tracks before issuing a home() (0/unset = never) | *(disabled)* |

## Troubleshooting

- **No camera detected:** Ensure a Pi Camera Module or USB camera is connected. Check with `ls /dev/video*` or `libcamera-hello --list-cameras`.
- **SSH failures:** Verify your SSH keys are authorized on the Pi (`ssh-copy-id pi@<ip>`).
- **Service not starting:** Check logs with `journalctl -u clawcam` on the Pi.
- **Low detection accuracy:** Try a larger YOLO model (yolov8s.onnx) at the cost of inference speed.
- **Slow inference:** Out of the box YOLOv8n runs around 3–5 FPS on Pi 4 CPU. To go faster, drop `CLAWCAM_YOLO_INPUT_SIZE` (try 416 or 320 — roughly quadratic speedup) and/or lower `CLAWCAM_INFERENCE_INTERVAL_MS`. Check actual inference latency in the journal: the monitor logs `inference: avg N.N ms over last 40 frames` periodically.
- **PTZ commands fail with "permission denied":** The SSH user needs access to the v4l2 device. On Debian/Raspberry Pi OS, add the user to the `video` group: `sudo usermod -aG video <user>` then re-login.
- **PTZ moves in the wrong direction:** UVC sign conventions vary by camera. Flip with `CLAWCAM_PTZ_PAN_INVERT=1` and/or `CLAWCAM_PTZ_TILT_INVERT=1` in `/etc/clawcam.env` for auto-tracking. For manual `clawcam ptz ... nudge`, pass the opposite sign.
- **Empty predictions:** If `predictions` is empty, nothing exceeded the 0.4 confidence threshold in the frame.
