use anyhow::{Context, Result};
use base64::Engine;
use std::path::PathBuf;
use tracing::info;

use crate::device::Device;
use crate::ssh::session;

const REMOTE_BIN: &str = "/usr/local/bin/clawcam";
const REMOTE_MODEL: &str = "/usr/local/share/clawcam/yolov8n.onnx";
const SERVICE_NAME: &str = "clawcam";
const GITHUB_REPO: &str = "grunt3714-lgtm/clawcam";

pub async fn run_setup(
    dev: &Device,
    user: &str,
    webhook: &str,
    webhook_token: Option<&str>,
) -> Result<()> {
    info!("setting up {} ({})", dev.name, dev.host);

    // 1. Install system dependencies (runtime only — no -dev packages needed on device)
    info!("installing system dependencies...");
    session::run_cmd(dev, "\
        sudo apt-get update -qq && sudo apt-get install -y -qq \
         gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
         gstreamer1.0-plugins-bad gstreamer1.0-libav \
         v4l-utils libv4l-0"
    ).await.context("failed to install dependencies")?;

    // 2. Detect camera source
    info!("detecting camera...");
    let cam_source = detect_camera_source(dev).await?;
    info!("detected camera source: {cam_source}");

    // 3. Deploy clawcam binary
    info!("deploying clawcam binary...");
    let arch = session::run_cmd(dev, "uname -m").await?;
    let arch = arch.trim();
    session::run_cmd(dev, "sudo mkdir -p /usr/local/share/clawcam && rm -rf /tmp/clawcam /tmp/clawcam.service").await?;

    let local_bin = find_binary(arch);
    match local_bin {
        Ok(path) => {
            info!("uploading local binary from {path}");
            session::scp_to(dev, &path, "/tmp/clawcam").await?;
        }
        Err(_) => {
            info!("no local binary found, downloading from GitHub release...");
            download_binary_to_device(dev, arch).await?;
        }
    }
    session::run_cmd(dev, &format!(
        "sudo mv /tmp/clawcam {REMOTE_BIN} && sudo chmod +x {REMOTE_BIN}"
    )).await?;

    // 4. Deploy YOLO model
    info!("deploying YOLO model...");
    let local_model = find_model();
    match local_model {
        Some(path) => {
            info!("uploading local model from {}", path.display());
            session::scp_to(dev, &path.to_string_lossy(), "/tmp/yolov8n.onnx").await?;
        }
        None => {
            info!("no local model found, downloading from GitHub release...");
            download_model_to_device(dev).await?;
        }
    }
    session::run_cmd(dev, &format!(
        "sudo mv /tmp/yolov8n.onnx {REMOTE_MODEL}"
    )).await?;

    // 5. Verify deployment
    let version = session::run_cmd(dev, &format!("{REMOTE_BIN} --version")).await?;
    info!("deployed: {}", version.trim());

    // 6. Create systemd service
    info!("creating systemd service...");
    let token_flag = webhook_token
        .map(|t| format!("--webhook-token '{t}'"))
        .unwrap_or_default();
    let service = format!(
        r#"[Unit]
Description=ClawCam AI Detection Monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={user}
ExecStart={REMOTE_BIN} monitor \
    --webhook '{webhook}' \
    {token_flag} \
    --host '{host}' \
    --log-path /var/log/clawcam.log
Environment=CLAWCAM_CAMERA_SOURCE={cam_source}
Environment=CLAWCAM_MODEL_PATH={REMOTE_MODEL}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
"#,
        host = dev.host,
    );

    // Write service file via temp file + SCP
    let tmp_local = "/tmp/clawcam_service_tmp";
    std::fs::write(tmp_local, &service)?;
    info!("wrote local service file");
    session::scp_to(dev, tmp_local, "/tmp/clawcam.service").await?;
    std::fs::remove_file(tmp_local).ok();
    info!("SCP'd service file to device");
    session::run_cmd(dev, "sudo mv /tmp/clawcam.service /etc/systemd/system/clawcam.service").await?;
    info!("installed service file");

    // 7. Enable and start
    session::run_cmd(dev, &format!(
        "sudo systemctl daemon-reload && sudo systemctl enable {SERVICE_NAME} && sudo systemctl start {SERVICE_NAME}"
    )).await?;

    let status = session::run_cmd(dev, &format!(
        "systemctl is-active {SERVICE_NAME}"
    )).await?;

    if status.trim() == "active" {
        info!("clawcam is running on {}", dev.name);
        println!("setup complete — {} is active on {}", SERVICE_NAME, dev.name);
    } else {
        anyhow::bail!("service failed to start — check `journalctl -u {SERVICE_NAME}` on {}", dev.host);
    }

    Ok(())
}

/// Detect what camera is available on the Pi using GStreamer.
async fn detect_camera_source(dev: &Device) -> Result<String> {
    // Check if libcamerasrc GStreamer element is available (Pi Camera Module)
    let gst_check = session::run_cmd(dev,
        "gst-inspect-1.0 libcamerasrc >/dev/null 2>&1 && echo 'libcamerasrc'"
    ).await;
    if let Ok(output) = &gst_check {
        if output.trim() == "libcamerasrc" {
            return Ok("libcamerasrc".to_string());
        }
    }

    // Check for V4L2 devices (USB webcams, conference cams)
    let v4l2 = session::run_cmd(dev,
        "gst-inspect-1.0 v4l2src >/dev/null 2>&1 && ls /dev/video0 2>/dev/null && echo 'v4l2src'"
    ).await;
    if let Ok(output) = &v4l2 {
        if output.contains("v4l2src") {
            return Ok("v4l2src".to_string());
        }
    }

    anyhow::bail!("no camera detected — ensure GStreamer and camera drivers are installed")
}

/// Find a local cross-compiled binary for the target architecture.
fn find_binary(arch: &str) -> Result<String> {
    let artifact = match arch {
        "aarch64" => "clawcam-pi-arm64",
        "armv7l" | "armv6l" => "clawcam-pi-armv7",
        "x86_64" => "clawcam-linux-amd64",
        _ => anyhow::bail!("unsupported architecture: {arch}"),
    };

    let target = match arch {
        "aarch64" => "aarch64-unknown-linux-gnu",
        "armv7l" | "armv6l" => "armv7-unknown-linux-gnueabihf",
        "x86_64" => "x86_64-unknown-linux-gnu",
        _ => unreachable!(),
    };

    // Check multiple locations for the binary
    let candidates = [
        format!("target/{target}/release/clawcam"),
        artifact.to_string(),
        format!("/tmp/{artifact}"),
    ];

    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return Ok(path.clone());
        }
    }

    anyhow::bail!("no local binary found for {arch}")
}

/// Download the binary directly to the device from GitHub releases.
async fn download_binary_to_device(dev: &Device, arch: &str) -> Result<()> {
    let artifact = match arch {
        "aarch64" => "clawcam-pi-arm64",
        "armv7l" | "armv6l" => "clawcam-pi-armv7",
        "x86_64" => "clawcam-linux-amd64",
        _ => anyhow::bail!("unsupported architecture: {arch}"),
    };

    let tarball = format!("{artifact}.tar.gz");
    let url = get_release_asset_url(&tarball).await?;

    session::run_cmd(dev, &format!(
        "curl -fsSL -L '{url}' | tar xz -C /tmp && mv /tmp/{artifact} /tmp/clawcam"
    )).await.context(format!("failed to download {artifact} from GitHub release"))?;

    Ok(())
}

/// Download the YOLO model directly to the device from GitHub releases.
async fn download_model_to_device(dev: &Device) -> Result<()> {
    let url = get_release_asset_url("yolov8n.onnx").await?;

    session::run_cmd(dev, &format!(
        "curl -fsSL -L '{url}' -o /tmp/yolov8n.onnx"
    )).await.context("failed to download YOLO model from GitHub release")?;

    Ok(())
}

/// Get the download URL for a release asset from the latest GitHub release.
async fn get_release_asset_url(asset_name: &str) -> Result<String> {
    let api_url = format!(
        "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
    );

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .get(&api_url)
        .header("User-Agent", "clawcam")
        .send()
        .await?
        .json()
        .await?;

    let tag = resp["tag_name"]
        .as_str()
        .context("could not find latest release")?;

    // Direct download URL pattern for GitHub releases
    let url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/{tag}/{asset_name}"
    );

    info!("release asset: {url}");
    Ok(url)
}

/// Find the YOLO model locally — check multiple locations.
fn find_model() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("models/yolov8n.onnx"),
        dirs::config_dir()
            .map(|d| d.join("openclaw/skills/clawcam/models/yolov8n.onnx"))
            .unwrap_or_default(),
        dirs::config_dir()
            .map(|d| d.join("clawcam/models/yolov8n.onnx"))
            .unwrap_or_default(),
        dirs::data_dir()
            .map(|d| d.join("clawcam/yolov8n.onnx"))
            .unwrap_or_default(),
    ];

    candidates.iter().find(|p| p.exists()).cloned()
}
