use anyhow::Result;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_snap(dev: &Device, out: Option<&str>) -> Result<()> {
    let remote_path = "/tmp/clawcam_snap.jpg";

    // Use GStreamer on the Pi to grab a single JPEG frame
    session::run_cmd(dev, &format!(
        "gst-launch-1.0 -e $(cat /etc/systemd/system/clawcam.service 2>/dev/null | \
         grep CLAWCAM_CAMERA_SOURCE | sed 's/.*=//' || echo 'v4l2src') \
         ! videoconvert ! videoscale ! video/x-raw,width=1920,height=1080 \
         ! jpegenc quality=90 ! multifilesink location={remote_path} max-files=1"
    )).await?;

    // Simpler approach: use the clawcam binary on-device if available
    let result = session::run_cmd(dev, &format!(
        "timeout 5 gst-launch-1.0 -q v4l2src num-buffers=1 \
         ! videoconvert ! jpegenc ! filesink location={remote_path} 2>/dev/null || \
         timeout 5 gst-launch-1.0 -q libcamerasrc ! \
         video/x-raw,width=1920,height=1080 ! videoconvert \
         ! jpegenc ! multifilesink location={remote_path} max-files=1 2>/dev/null"
    )).await;

    if result.is_err() {
        // Fallback: check if there's an existing snapshot from the monitor
        session::run_cmd(dev, &format!(
            "test -f {remote_path} || echo 'no snapshot available'"
        )).await?;
    }

    let local_path = out.unwrap_or("snapshot.jpg");
    session::scp_from(dev, remote_path, local_path).await?;
    session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;

    println!("snapshot saved to {local_path}");
    Ok(())
}
