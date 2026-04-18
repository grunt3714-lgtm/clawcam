use anyhow::Result;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_clip(dev: &Device, duration: u32, out: Option<&str>) -> Result<()> {
    let remote_path = "/tmp/clawcam_clip.mp4";

    // Record a clip using GStreamer on the Pi
    // Try v4l2src first, then libcamerasrc
    session::run_cmd(dev, &format!(
        "timeout {timeout} gst-launch-1.0 -q v4l2src \
         ! videoconvert ! video/x-raw,width=1280,height=720,framerate=30/1 \
         ! x264enc tune=zerolatency bitrate=2000 \
         ! mp4mux ! filesink location={remote_path} 2>/dev/null || \
         timeout {timeout} gst-launch-1.0 -q libcamerasrc \
         ! video/x-raw,width=1280,height=720,framerate=30/1 \
         ! videoconvert ! x264enc tune=zerolatency bitrate=2000 \
         ! mp4mux ! filesink location={remote_path}",
        timeout = duration + 3,
    )).await?;

    let local_path = out.unwrap_or("clip.mp4");
    session::scp_from(dev, remote_path, local_path).await?;
    session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;

    println!("clip saved to {local_path} ({duration}s)");
    Ok(())
}
