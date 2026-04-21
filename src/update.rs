//! `clawcam update` — pull the latest release from GitHub and replace the
//! currently-running binary, or the binary on a remote device.
//!
//! Local update: downloads `clawcam-<platform>.tar.gz` from the GitHub
//! release, extracts it, and atomically renames over `std::env::current_exe()`.
//! Linux permits replacing a running executable via rename, so the swap is
//! safe even if the binary is currently being executed.
//!
//! Remote update: SSHes to the device, shells out to `curl | tar` on the
//! device itself (avoids scping large tarballs), `sudo install`s the new
//! binary to `/usr/local/bin/clawcam`, and restarts the `clawcam` service.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tempfile::NamedTempFile;

use crate::device::{Device, DeviceRegistry};
use crate::ssh::session;

const REPO: &str = "grunt3714-lgtm/clawcam";

/// Replace the currently-running clawcam binary with the latest (or pinned) release.
pub async fn update_local(version: Option<&str>) -> Result<()> {
    let tag = resolve_tag(version).await?;
    let platform = local_platform()?;
    let url = asset_url(&tag, &platform);

    println!("fetching clawcam {tag} for {platform}...");
    let tmp_tarball = NamedTempFile::new()?;
    download(&url, tmp_tarball.path()).await?;

    let extract_dir = tempfile::tempdir()?;
    extract_tarball(tmp_tarball.path(), extract_dir.path())?;

    let artifact = format!("clawcam-{platform}");
    let extracted = extract_dir.path().join(&artifact);
    if !extracted.exists() {
        bail!("release tarball did not contain expected binary '{artifact}'");
    }

    let current = std::env::current_exe().context("could not locate current binary")?;
    let staged = current.with_extension("new");
    std::fs::copy(&extracted, &staged)
        .with_context(|| format!("writing staged binary to {}", staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, &current).with_context(|| {
        format!(
            "could not replace {} (try running with sudo if installed system-wide)",
            current.display()
        )
    })?;

    println!("updated local clawcam to {tag}");
    Ok(())
}

/// Update the clawcam binary on a registered remote device.
pub async fn update_remote(dev: &Device, version: Option<&str>) -> Result<()> {
    let tag = resolve_tag(version).await?;
    let platform = remote_platform(dev).await?;
    let url = asset_url(&tag, &platform);

    println!("[{}] updating to {tag} for {platform}...", dev.name);

    // The heredoc runs on the device. `set -e` + trap ensures we clean up
    // the tempdir even on failure. `sudo systemctl restart` is only attempted
    // if the service is installed — so this also works for devices that don't
    // run the monitor.
    let script = format!(
        "set -euo pipefail\n\
         tmp=$(mktemp -d)\n\
         trap 'rm -rf \"$tmp\"' EXIT\n\
         curl -fsSL '{url}' | tar xz -C \"$tmp\"\n\
         sudo install -m 0755 \"$tmp/clawcam-{platform}\" /usr/local/bin/clawcam\n\
         if systemctl is-enabled --quiet clawcam 2>/dev/null; then\n\
           sudo systemctl restart clawcam\n\
           echo 'service restarted'\n\
         fi\n\
         /usr/local/bin/clawcam --version || true\n"
    );

    let out = session::run_cmd(dev, &script).await?;
    let trimmed = out.trim();
    if !trimmed.is_empty() {
        println!("[{}] {trimmed}", dev.name);
    }
    println!("[{}] ok", dev.name);
    Ok(())
}

/// Update every device in the registry. Failures on one device don't block others.
pub async fn update_all(version: Option<&str>) -> Result<()> {
    let reg = DeviceRegistry::load()?;
    let devs: Vec<Device> = reg.list().into_iter().cloned().collect();
    if devs.is_empty() {
        println!("no devices registered");
        return Ok(());
    }
    let tag = resolve_tag(version).await?;
    let mut failed: Vec<String> = Vec::new();
    for dev in &devs {
        if let Err(e) = update_remote(dev, Some(&tag)).await {
            eprintln!("[{}] FAILED: {e:#}", dev.name);
            failed.push(dev.name.clone());
        }
    }
    if !failed.is_empty() {
        bail!("{} device(s) failed: {}", failed.len(), failed.join(", "));
    }
    Ok(())
}

async fn resolve_tag(version: Option<&str>) -> Result<String> {
    if let Some(v) = version {
        return Ok(v.to_string());
    }
    let client = reqwest::Client::builder()
        .user_agent("clawcam-updater")
        .build()?;
    let resp = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .send()
        .await
        .context("GitHub API request failed")?
        .error_for_status()
        .context("GitHub API returned error status")?;
    let body: serde_json::Value = resp.json().await.context("parsing GitHub response")?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .context("no tag_name in GitHub release response")
}

fn asset_url(tag: &str, platform: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/clawcam-{platform}.tar.gz")
}

async fn download(url: &str, out: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("clawcam-updater")
        .build()?;
    let bytes = client
        .get(url)
        .send()
        .await
        .context("download request failed")?
        .error_for_status()
        .context("download HTTP error (is this release+platform published?)")?
        .bytes()
        .await?;
    std::fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

fn extract_tarball(tarball: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .args([
            "xzf",
            tarball.to_str().context("non-UTF8 tarball path")?,
            "-C",
            dest.to_str().context("non-UTF8 dest path")?,
        ])
        .status()
        .context("failed to run tar — is it installed?")?;
    if !status.success() {
        bail!("tar extract failed: {status}");
    }
    Ok(())
}

fn local_platform() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => Ok("linux-amd64".into()),
        ("linux", "aarch64") => Ok("linux-arm64".into()),
        ("linux", "arm") => Ok("linux-armv7".into()),
        ("macos", "aarch64") => Ok("darwin-arm64".into()),
        ("macos", "x86_64") => Ok("darwin-amd64".into()),
        _ => bail!("unsupported local platform: {os}-{arch}"),
    }
}

async fn remote_platform(dev: &Device) -> Result<String> {
    let out = session::run_cmd(dev, "uname -s -m").await?;
    let trimmed = out.trim();
    let mut parts = trimmed.split_whitespace();
    let os_raw = parts.next().unwrap_or("").to_ascii_lowercase();
    let arch_raw = parts.next().unwrap_or("").to_ascii_lowercase();
    let os = match os_raw.as_str() {
        "linux" => "linux",
        "darwin" => "darwin",
        _ => bail!("unsupported remote OS: {os_raw}"),
    };
    let arch = match arch_raw.as_str() {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        "armv7l" | "armv6l" => "armv7",
        _ => bail!("unsupported remote arch: {arch_raw}"),
    };
    Ok(format!("{os}-{arch}"))
}
