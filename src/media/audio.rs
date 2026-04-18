use anyhow::Result;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_speak(dev: &Device, message: &str, volume: u8) -> Result<()> {
    // Use espeak/piper on the Pi for TTS
    let escaped = message.replace('\'', "'\\''");
    session::run_cmd(dev, &format!(
        "command -v piper >/dev/null 2>&1 && \
         echo '{escaped}' | piper --output_raw | aplay -r 22050 -f S16_LE -c 1 || \
         command -v espeak-ng >/dev/null 2>&1 && \
         espeak-ng -a {volume} '{escaped}' || \
         espeak -a {volume} '{escaped}'"
    )).await?;

    println!("spoke: {message}");
    Ok(())
}

pub async fn run_listen(dev: &Device, duration: u32, out: Option<&str>) -> Result<()> {
    let remote_path = "/tmp/clawcam_audio.wav";

    session::run_cmd(dev, &format!(
        "arecord -d {duration} -f S16_LE -r 16000 -c 1 {remote_path}"
    )).await?;

    let local_path = out.unwrap_or("recording.wav");
    session::scp_from(dev, remote_path, local_path).await?;
    session::run_cmd(dev, &format!("rm -f {remote_path}")).await?;

    println!("recording saved to {local_path} ({duration}s)");
    Ok(())
}
