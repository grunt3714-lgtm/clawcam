use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::device::Device;

/// Run a command on a remote device via SSH.
pub async fn run_cmd(dev: &Device, cmd: &str) -> Result<String> {
    let output = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=no",
            "-o", "ConnectTimeout=10",
            "-p", &dev.port.to_string(),
            &format!("{}@{}", dev.user, dev.host),
            cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to execute ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ssh command failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Copy a local file to a remote device via SCP.
pub async fn scp_to(dev: &Device, local: &str, remote: &str) -> Result<()> {
    let status = Command::new("scp")
        .args([
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=no",
            "-P", &dev.port.to_string(),
            local,
            &format!("{}@{}:{}", dev.user, dev.host, remote),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("failed to execute scp")?;

    if !status.success() {
        anyhow::bail!("scp failed");
    }
    Ok(())
}

/// Copy a remote file to a local path via SCP.
pub async fn scp_from(dev: &Device, remote: &str, local: &str) -> Result<()> {
    let status = Command::new("scp")
        .args([
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=no",
            "-P", &dev.port.to_string(),
            &format!("{}@{}:{}", dev.user, dev.host, remote),
            local,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("failed to execute scp")?;

    if !status.success() {
        anyhow::bail!("scp failed");
    }
    Ok(())
}
