use anyhow::{Context, Result};
use tracing::info;

use crate::device::Device;
use crate::ssh::session;

const REMOTE_BIN: &str = "/usr/local/bin/clawcam";
const REMOTE_MODEL: &str = "/usr/local/share/clawcam/yolov8n.onnx";
const SERVICE_NAME: &str = "clawcam";

pub async fn run_setup(
    dev: &Device,
    user: &str,
    webhook: &str,
    webhook_token: Option<&str>,
) -> Result<()> {
    info!("setting up {} ({})", dev.name, dev.host);

    // 1. Install system dependencies
    info!("installing system dependencies...");
    session::run_cmd(dev, &format!(
        "sudo apt-get update -qq && sudo apt-get install -y -qq \
         gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
         gstreamer1.0-plugins-bad gstreamer1.0-libav \
         libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
         v4l-utils libv4l-dev"
    )).await.context("failed to install dependencies")?;

    // 2. Detect camera source
    info!("detecting camera...");
    let cam_source = detect_camera_source(dev).await?;
    info!("detected camera source: {cam_source}");

    // 3. Upload clawcam binary
    info!("deploying clawcam binary...");
    let arch = session::run_cmd(dev, "uname -m").await?;
    let arch = arch.trim();
    let local_bin = find_binary(arch)?;
    session::run_cmd(dev, "sudo mkdir -p /usr/local/share/clawcam").await?;
    session::scp_to(dev, &local_bin, "/tmp/clawcam").await?;
    session::run_cmd(dev, &format!(
        "sudo mv /tmp/clawcam {REMOTE_BIN} && sudo chmod +x {REMOTE_BIN}"
    )).await?;

    // 4. Upload YOLO model
    info!("deploying YOLO model...");
    session::scp_to(dev, "models/yolov8n.onnx", "/tmp/yolov8n.onnx").await
        .context("failed to upload model — ensure models/yolov8n.onnx exists")?;
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

    let escaped = service.replace('\'', "'\\''");
    session::run_cmd(dev, &format!(
        "echo '{escaped}' | sudo tee /etc/systemd/system/{SERVICE_NAME}.service > /dev/null"
    )).await?;

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

/// Detect what camera is available on the Pi.
/// Returns a GStreamer source element string.
async fn detect_camera_source(dev: &Device) -> Result<String> {
    // Check for libcamera (Pi Camera Module)
    let libcam = session::run_cmd(dev, "which libcamera-hello 2>/dev/null && libcamera-hello --list-cameras 2>&1 | head -5").await;
    if let Ok(output) = &libcam {
        if output.contains("Available cameras") && !output.contains(": 0 cameras") {
            return Ok("libcamerasrc".to_string());
        }
    }

    // Check for V4L2 devices (USB webcams, conference cams)
    let v4l2 = session::run_cmd(dev, "ls /dev/video* 2>/dev/null | head -1").await;
    if let Ok(output) = &v4l2 {
        let dev_path = output.trim();
        if !dev_path.is_empty() {
            return Ok(format!("v4l2src device={dev_path}"));
        }
    }

    anyhow::bail!("no camera detected on device — connect a Pi Camera Module or USB camera")
}

/// Find the cross-compiled binary for the target architecture.
fn find_binary(arch: &str) -> Result<String> {
    let target = match arch {
        "aarch64" => "aarch64-unknown-linux-gnu",
        "armv7l" | "armv6l" => "armv7-unknown-linux-gnueabihf",
        "x86_64" => "x86_64-unknown-linux-gnu",
        _ => anyhow::bail!("unsupported architecture: {arch}"),
    };
    let path = format!("target/{target}/release/clawcam");
    if !std::path::Path::new(&path).exists() {
        anyhow::bail!(
            "binary not found at {path} — cross-compile first:\n  \
             cargo build --release --target {target}"
        );
    }
    Ok(path)
}
