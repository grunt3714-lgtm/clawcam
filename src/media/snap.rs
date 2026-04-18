use anyhow::Result;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_snap(dev: &Device, out: Option<&str>) -> Result<()> {
    let remote_path = "/tmp/clawcam_snap.jpg";

    // Priority 1: rpicam-apps (Pi 4/5 with Pi Camera Module — Pi OS default)
    let rpicam_result = session::run_cmd(dev, &format!(
        "rpicam-still -t 1000 -n -o {remote_path} --width 1920 --height 1080 --timeout 2000 --quality 90 2>&1"
    )).await;

    if rpicam_result.is_ok() {
        let local_path = out.unwrap_or("snapshot.jpg");
        session::scp_from(dev, remote_path, local_path).await?;
        session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;
        println!("snapshot saved to {local_path}(rpicam-still)");
        return Ok(());
    }

    // Priority 2: GStreamer with libcamerasrc (Pi Camera Module via libcamera on Ubuntu)
    let gst_libcam = session::run_cmd(dev, &format!(
        "timeout 10 gst-launch-1.0 -e libcamerasrc ! \
         video/x-raw,width=1920,height=1080 ! videoconvert \
         ! jpegenc quality=90 ! multifilesink location={remote_path} max-files=1 2>&1"
    )).await;

    if gst_libcam.is_ok() {
        let local_path = out.unwrap_or("snapshot.jpg");
        session::scp_from(dev, remote_path, local_path).await?;
        session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;
        println!("snapshot saved to {local_path}(libcamerasrc)");
        return Ok(());
    }

    // Priority 3: GStreamer with v4l2src (USB webcams, conference cams)
    let result = session::run_cmd(dev, &format!(
        "timeout 10 gst-launch-1.0 -e v4l2src num-buffers=1 ! \
         videoconvert ! videoscale ! video/x-raw,width=1920,height=1080 \
         ! jpegenc quality=90 ! multifilesink location={remote_path} max-files=1 2>&1"
    )).await;

    if result.is_ok() {
        let local_path = out.unwrap_or("snapshot.jpg");
        session::scp_from(dev, remote_path, local_path).await?;
        session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;
        println!("snapshot saved to {local_path}(v4l2src)");
        return Ok(());
    }

    // Check if there's an existing snapshot from the monitor
    let existing = session::run_cmd(dev, &format!(
        "test -f {remote_path} && echo 'found' || echo 'none'"
    )).await;

    if existing.as_deref().unwrap_or("") == "found" {
        let local_path = out.unwrap_or("snapshot.jpg");
        session::scp_from(dev, remote_path, local_path).await?;
        session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;
        println!("snapshot saved to {local_path}(existing)");
        return Ok(());
    }

    anyhow::bail!("failed to capture snapshot — no camera source available")
}
