use anyhow::{Context, Result};

use crate::device::Device;
use crate::ssh::session;
use crate::media::detect_source;

const LATEST_FRAME: &str = "/tmp/clawcam_latest.jpg";

/// Remote snap: SSHes into the device and runs `clawcam _snap` there.
pub async fn run_snap(dev: &Device, out: Option<&str>) -> Result<()> {
    let remote_path = "/tmp/clawcam_snap.jpg";

    session::run_cmd(dev, &format!(
        "clawcam _snap --out {remote_path}"
    )).await.context("snap failed on device — is clawcam installed?")?;

    let local_path = out.unwrap_or("snapshot.jpg");
    session::scp_from(dev, remote_path, local_path).await?;
    session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;

    println!("snapshot saved to {local_path}");
    Ok(())
}

/// On-device snap: if the monitor is running, read its latest frame.
/// Otherwise, open the camera directly.
pub fn run_snap_local(out: &str) -> Result<()> {
    // If the monitor is running, it writes /tmp/clawcam_latest.jpg every detection
    let latest = std::path::Path::new(LATEST_FRAME);
    if latest.exists() {
        let metadata = std::fs::metadata(latest)?;
        let age = metadata.modified()?.elapsed().unwrap_or_default();
        // Use the cached frame if it's less than 10 seconds old
        if age.as_secs() < 10 {
            std::fs::copy(latest, out)?;
            println!("{out}");
            return Ok(());
        }
    }

    // Monitor not running or frame too stale — open camera directly
    capture_fresh(out)
}

/// Open the camera via GStreamer and capture a single JPEG frame.
fn capture_fresh(out: &str) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    gst::init().context("failed to initialize GStreamer")?;

    let source = detect_source();

    let pipeline = gst::parse::launch(&format!(
        "{source} ! videoconvert ! videoscale ! \
         video/x-raw,width=1920,height=1080 ! jpegenc quality=90 ! \
         appsink name=sink emit-signals=true max-buffers=1 drop=true"
    ))
    .context("failed to create snap pipeline")?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow::anyhow!("pipeline cast failed"))?;

    let sink = pipeline
        .by_name("sink")
        .context("sink not found")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("appsink cast failed"))?;

    pipeline.set_state(gst::State::Playing)?;

    let sample = sink
        .pull_sample()
        .map_err(|_| anyhow::anyhow!("failed to capture frame — check camera connection"))?;
    let buffer = sample.buffer().context("no buffer")?;
    let map = buffer.map_readable()?;

    std::fs::write(out, map.as_slice())?;

    pipeline.set_state(gst::State::Null)?;
    println!("{out}");
    Ok(())
}
