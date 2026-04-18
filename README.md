<div align="center">

<img src="logo.svg" alt="clawcam" width="480"/>


**AI-powered camera monitoring for Raspberry Pi**

[![Release](https://img.shields.io/github/v/release/grunt3714-lgtm/clawcam?color=%23e94560&style=flat-square)](https://github.com/grunt3714-lgtm/clawcam/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange?style=flat-square)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Raspberry_Pi-c51a4a?style=flat-square&logo=raspberrypi&logoColor=white)](https://www.raspberrypi.com/)
[![YOLO](https://img.shields.io/badge/model-YOLOv8n-blueviolet?style=flat-square)](https://github.com/ultralytics/ultralytics)

*Any camera. On-device YOLO. Object tracking. Adaptive events. Video clips. No cloud.*

```mermaid
graph LR
    PI["Raspberry Pi<br/><i>YOLOv8 + GStreamer</i>"] -->|"webhook POST<br/>image + tracks + clips"| HOOK["your API /<br/>OpenClaw hook"]

    style PI fill:#1a1a2e,stroke:#e94560,color:#eee
    style HOOK fill:#533483,stroke:#e94560,color:#eee
```

</div>

---

### 🤖 Install via AI Agent

Send this to your AI agent:

> Read https://raw.githubusercontent.com/grunt3714-lgtm/clawcam/master/SKILL.md and install the clawcam skill. Run the installer:
> ```
> curl -fsSL https://raw.githubusercontent.com/grunt3714-lgtm/clawcam/master/skill-install.sh | bash
> ```

<details>
<summary>👤 Manual install</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/grunt3714-lgtm/clawcam/master/install.sh | bash
```

</details>

---

`clawcam` SSHes into your Raspberry Pi, deploys a monitor binary with a YOLOv8n model, and pushes detection events directly to your webhook. GStreamer captures frames from any connected camera — **Pi Camera Module**, **USB webcam**, or **conference camera**. YOLO runs inference on-device with adaptive object tracking, intelligent event management, and automatic video clip recording.

### Highlights

- **Camera-agnostic** — Pi Camera Module (libcamera), USB webcams, conference cams (V4L2)
- **On-device YOLO** — YOLOv8n via ONNX Runtime, no cloud inference
- **Object tracking** — IoU-based frame-to-frame tracking with persistent track IDs, duration, and movement vectors
- **Adaptive events** — intelligent lifecycle: initial alert → tracking updates → final report with clip. No spam for stationary objects, escalates for new arrivals
- **Video clips** — automatic MP4 recording from a rolling frame buffer with 2s pre-detection context, assembled on event completion
- **Pre-detection frames** — 3 JPEG frames from before the detection are included in the initial alert
- **80 detection classes** — full COCO set (person, car, dog, cat, bicycle, etc.)
- **Configurable threshold** — `CLAWCAM_CONF_THRESHOLD` env var (default 0.6)
- **Device registry** — name your cameras, manage them by friendly name
- **Zero device setup** — `clawcam setup <name>` handles everything over SSH
- **Secure by default** — SSH host key verification, secrets in env files, HTTPS enforcement for public webhooks
- **Cloud-free** — all detection and tracking runs locally, events stay on your network

## Install

```sh
git clone https://github.com/grunt3714-lgtm/clawcam.git
cd clawcam
cargo build --release
```

Cross-compile for Raspberry Pi:

```sh
# Pi 4/5 (64-bit)
cargo build --release --target aarch64-unknown-linux-gnu

# Pi 3/Zero 2 (32-bit)
cargo build --release --target armv7-unknown-linux-gnueabihf
```

Download the YOLO model:

```sh
mkdir -p models
# YOLOv8n — fast, good for Pi
wget -O models/yolov8n.onnx https://github.com/ultralytics/assets/releases/download/v8.2.0/yolov8n.onnx
```

Requires: Rust 2024 edition, SSH key access to `pi@<device>`, GStreamer dev libraries on host for compilation.

## Quick start

```sh
# Register your Pi camera
clawcam device add barn-cam 192.168.1.50

# Deploy monitor with webhook — Pi pushes events directly
clawcam setup barn-cam \
  --webhook http://your-host:8080/hooks/clawcam \
  --webhook-token YOUR_TOKEN
```

That's it. The Pi will POST detection events with a 1080p snapshot and YOLO predictions to your endpoint.

## Webhook payload

Each event produces up to 3 webhooks across its lifecycle:

### Initial alert (`event_phase: "start"`)

Fired when objects are first detected. Includes pre-detection frames for context.

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

Sent for prolonged events (>3s) or new object arrivals. Rate-limited to every 10s.

```json
{
  "detail": "ai_tracking_update",
  "event_phase": "update",
  "event_id": "a3db45af-...",
  "tracks": [
    { "track_id": 1, "class": "person", "duration_secs": 10.4, "movement_px": 245.3, "is_stationary": false, "bbox": [200, 90, 400, 440] }
  ],
  "event_duration_secs": 10.4
}
```

`detail` is `"ai_new_arrival"` when a new object appears during an existing event.

### Final report (`event_phase: "end"`)

Sent 3s after all objects leave the frame. Includes an MP4 clip assembled from buffered frames.

```json
{
  "detail": "ai_event_complete",
  "event_phase": "end",
  "event_id": "a3db45af-...",
  "event_duration_secs": 18.1,
  "clip": "<base64 MP4>"
}
```

## Commands

| Command | Description |
|---------|-------------|
| `device add NAME HOST` | Register a new Pi camera |
| `device list` | List all registered devices |
| `device remove NAME` | Remove a device from the registry |
| `setup NAME` | Deploy monitor + YOLO model (with `--webhook`) |
| `status NAME` | Check device health, camera, model, resources |
| `snap NAME` | Capture a JPEG snapshot |
| `clip NAME` | Record a short MP4 clip |
| `speak NAME MSG` | Play TTS through the device speaker |
| `listen NAME` | Record audio from the device microphone |
| `teardown NAME` | Stop the monitor and clean up |

### `device add`

```
$ clawcam device add barn-cam 192.168.1.50
added device 'barn-cam' at 192.168.1.50:22

$ clawcam device list
NAME             HOST                 PORT   USER
barn-cam         192.168.1.50         22     pi
garage-cam       192.168.1.51         22     pi
```

### `setup NAME`

```
$ clawcam setup barn-cam --webhook http://your-host:8080/events
setting up barn-cam (192.168.1.50)
installing system dependencies...
detecting camera...
detected camera source: v4l2src device=/dev/video0
deploying clawcam binary...
deployed: clawcam 0.3.0
deploying YOLO model...
creating systemd service...
setup complete — clawcam is active on barn-cam
```

Flags:
- `--webhook URL` — Pi POSTs events directly to this URL
- `--webhook-token TOKEN` — Bearer token for webhook auth
- `--user` — SSH user (default: pi)

### `snap NAME` / `clip NAME`

```sh
clawcam snap barn-cam --out shot.jpg
clawcam clip barn-cam --dur 10 --out clip.mp4
```

## How it works

```mermaid
graph TD
    subgraph pi ["Raspberry Pi"]
        CAM["Camera<br/><i>Pi CSI / USB / V4L2</i>"] -->|frames| GST["GStreamer Pipeline"]
        GST -->|"RGB 1280x720"| YOLO["YOLOv8n<br/><b>ONNX Runtime</b>"]
        GST -->|"JPEG"| BUF["Frame Buffer<br/><i>3s rolling</i>"]
        YOLO -->|detections| TRK["Object Tracker<br/><i>IoU matching</i>"]
        TRK -->|tracks| EVT["Event Manager<br/><i>state machine</i>"]
        BUF -->|"pre-frames + clip"| EVT
        EVT -->|"start / update / end"| HOOK
    end

    subgraph host ["Host"]
        HOOK["webhook endpoint"] -->|"JSON + clip"| AGENT["OpenClaw / your API"]
    end

    style pi fill:#1a1a2e,stroke:#e94560,color:#eee
    style host fill:#0d1117,stroke:#0f3460,color:#eee
    style YOLO fill:#533483,stroke:#e94560,color:#eee
    style TRK fill:#16213e,stroke:#0f3460,color:#eee
    style EVT fill:#16213e,stroke:#0f3460,color:#eee
    style BUF fill:#16213e,stroke:#0f3460,color:#eee
    style HOOK fill:#533483,stroke:#e94560,color:#eee
    style AGENT fill:#533483,stroke:#0f3460,color:#eee
```

1. **GStreamer** captures frames from the connected camera (auto-detected)
2. Frames are split: RGB for inference, JPEG into a **3-second rolling buffer**
3. **YOLOv8n** runs inference via ONNX Runtime (~2 FPS on Pi 4)
4. **Object tracker** matches detections across frames using IoU, assigns persistent track IDs, measures duration and movement
5. **Event manager** decides what to report based on the state machine:
   - **Initial alert** — first detection, includes 3 pre-detection frames
   - **Tracking update** — objects persist >3s or new arrival (rate-limited to 10s)
   - **Final report** — all objects gone for 3s, assembles MP4 clip from buffered frames via ffmpeg
6. Stationary objects already reported are suppressed to avoid spam

### Supported cameras

| Camera Type | GStreamer Source | Auto-detected |
|-------------|----------------|---------------|
| Pi Camera Module (v1/v2/v3) | `libcamerasrc` | Yes |
| USB webcam | `v4l2src device=/dev/video0` | Yes |
| USB conference camera | `v4l2src device=/dev/video0` | Yes |
| Network/RTSP camera | `rtspsrc location=rtsp://...` | No (set `CLAWCAM_CAMERA_SOURCE`) |

### Detection classes (COCO)

Full 80-class COCO set. Most relevant for monitoring:

| Class | ID | Class | ID |
|-------|----|-------|----|
| person | 0 | car | 2 |
| bicycle | 1 | motorcycle | 3 |
| bus | 5 | truck | 7 |
| bird | 14 | cat | 15 |
| dog | 16 | backpack | 24 |

## On-device architecture

| Component | Role |
|-----------|------|
| **GStreamer** | Camera capture, frame scaling, JPEG encoding |
| **ONNX Runtime** | YOLOv8n inference on CPU |
| **Frame buffer** | 3s rolling JPEG ring buffer for pre/post-detection context |
| **Object tracker** | IoU-based frame-to-frame matching, track IDs, movement |
| **Event manager** | State machine: Idle → Active → Cooldown → Complete |
| **ffmpeg** | Assembles buffered JPEG frames into MP4 clips |
| **systemd** | Service management, boot persistence, auto-restart |

### File layout on device

| Path | Description |
|------|-------------|
| `/usr/local/bin/clawcam` | Monitor binary |
| `/usr/local/share/clawcam/yolov8n.onnx` | YOLO model |
| `/etc/systemd/system/clawcam.service` | systemd unit |
| `/etc/clawcam.env` | Secrets (webhook token, mode 0600) |
| `/var/log/clawcam.log` | Monitor log |

## License

MIT
