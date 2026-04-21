//! PTZ control endpoint for motorized conference cameras over VISCA (RS-232/RS-485).
//!
//! Binds a tiny HTTP server that accepts `POST /ptz` with JSON body and issues
//! VISCA byte sequences to the configured serial port. Used by ClawHub's web UI
//! to drive the camera without needing to SSH into the device.
//!
//! Body:
//!   {
//!     "pan":  -1 | 0 | +1,      // start pan left/right (direction only)
//!     "tilt": -1 | 0 | +1,      // start tilt up/down
//!     "zoom": -1 | 0 | +1,      // start zoom wide/tele
//!     "home": true,             // return to home position
//!     "stop": true,              // stop all motion immediately
//!     "duration_ms": 300,       // auto-send stop after N ms (default 300)
//!     "address": 1              // VISCA cam address (default 1)
//!   }

use anyhow::Result;
use std::io::Write as _;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

pub async fn serve(bind: String, serial: String) -> Result<()> {
    // Configure serial port: 9600 8N1, raw.
    let status = std::process::Command::new("stty")
        .args([
            "-F", &serial, "9600", "cs8", "-cstopb", "-parenb", "raw", "-echo",
        ])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => warn!("stty on {serial} exited {s}"),
        Err(e) => warn!("stty on {serial} failed: {e}"),
    }

    let listener = TcpListener::bind(&bind).await?;
    info!("PTZ control endpoint listening on {bind} → {serial}");

    let token = std::env::var("CLAWCAM_PTZ_TOKEN").ok();

    loop {
        let (sock, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("ptz accept: {e:#}");
                continue;
            }
        };
        let serial = serial.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, &serial, token.as_deref()).await {
                warn!("ptz handler: {e:#}");
            }
        });
    }
}

async fn handle(mut sock: TcpStream, serial: &str, token: Option<&str>) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut total = 0usize;
    let (headers, body) = loop {
        if total == buf.len() {
            return respond(&mut sock, 400, r#"{"error":"request too large"}"#).await;
        }
        let n = sock.read(&mut buf[total..]).await?;
        if n == 0 {
            break (String::new(), String::new());
        }
        total += n;
        if let Some(p) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&buf[..p]).unwrap_or("").to_string();
            let content_len = content_length(&headers);
            let body_start = p + 4;
            if total - body_start >= content_len {
                let end = body_start + content_len;
                let body = String::from_utf8_lossy(&buf[body_start..end]).to_string();
                break (headers, body);
            }
        }
    };

    let method_path = headers.lines().next().unwrap_or("");
    if !method_path.starts_with("POST /ptz") && !method_path.starts_with("POST /ptz ") {
        return respond(&mut sock, 404, r#"{"error":"not found"}"#).await;
    }
    if let Some(t) = token {
        let auth = headers
            .lines()
            .find_map(|l| l.strip_prefix("Authorization:").or_else(|| l.strip_prefix("authorization:")))
            .unwrap_or("")
            .trim();
        let presented = auth.strip_prefix("Bearer ").unwrap_or("");
        if presented != t {
            return respond(&mut sock, 401, r#"{"error":"unauthorized"}"#).await;
        }
    }

    let cmd: PtzCmd = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!(r#"{{"error":"bad body: {e}"}}"#);
            return respond(&mut sock, 400, &msg).await;
        }
    };

    let addr_byte = 0x80 | (cmd.address & 0x0F);
    let primary = build_visca(addr_byte, &cmd);
    if let Err(e) = write_serial(serial, &primary) {
        let msg = format!(r#"{{"error":"serial write: {e}"}}"#);
        return respond(&mut sock, 502, &msg).await;
    }

    // For nudges (pan/tilt/zoom direction), auto-stop after duration_ms so a
    // click produces a short motion rather than running forever.
    let has_motion = cmd.pan != 0 || cmd.tilt != 0 || cmd.zoom != 0;
    if has_motion && !cmd.home && !cmd.stop && cmd.duration_ms > 0 {
        tokio::time::sleep(Duration::from_millis(cmd.duration_ms)).await;
        let stop = stop_all_visca(addr_byte, cmd.pan != 0 || cmd.tilt != 0, cmd.zoom != 0);
        let _ = write_serial(serial, &stop);
    }

    respond(&mut sock, 200, r#"{"ok":true}"#).await
}

fn content_length(headers: &str) -> usize {
    for line in headers.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

async fn respond(sock: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(out.as_bytes()).await?;
    sock.shutdown().await.ok();
    Ok(())
}

#[derive(serde::Deserialize, Debug)]
struct PtzCmd {
    #[serde(default)]
    pan: i32,
    #[serde(default)]
    tilt: i32,
    #[serde(default)]
    zoom: i32,
    #[serde(default)]
    home: bool,
    #[serde(default)]
    stop: bool,
    #[serde(default = "default_duration")]
    duration_ms: u64,
    #[serde(default = "default_address")]
    address: u8,
}
fn default_duration() -> u64 { 300 }
fn default_address() -> u8 { 1 }

// VISCA byte sequences. See: https://www.epiphan.com/userguides/LUMiO12x/Content/UserGuides/PTZ/3-operation/VISCAcommands.htm
fn build_visca(addr: u8, cmd: &PtzCmd) -> Vec<u8> {
    if cmd.home {
        return vec![addr, 0x01, 0x06, 0x04, 0xFF];
    }
    if cmd.stop {
        return stop_all_visca(addr, true, true);
    }
    if cmd.zoom != 0 {
        // 0x2p = Tele variable speed 0-7; 0x3p = Wide variable speed 0-7.
        let p = if cmd.zoom > 0 { 0x26 } else { 0x36 }; // speed 6
        return vec![addr, 0x01, 0x04, 0x07, p, 0xFF];
    }
    // Pan/Tilt Drive: pan speed 01-18, tilt speed 01-14.
    let pan_dir: u8 = match cmd.pan.signum() {
        -1 => 0x01, // left
        1 => 0x02,  // right
        _ => 0x03,  // stop
    };
    let tilt_dir: u8 = match cmd.tilt.signum() {
        -1 => 0x02, // down
        1 => 0x01,  // up
        _ => 0x03,  // stop
    };
    vec![addr, 0x01, 0x06, 0x01, 0x10, 0x10, pan_dir, tilt_dir, 0xFF]
}

fn stop_all_visca(addr: u8, pan_tilt: bool, zoom: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    if pan_tilt {
        v.extend_from_slice(&[addr, 0x01, 0x06, 0x01, 0x03, 0x03, 0x03, 0x03, 0xFF]);
    }
    if zoom {
        v.extend_from_slice(&[addr, 0x01, 0x04, 0x07, 0x00, 0xFF]);
    }
    v
}

fn write_serial(path: &str, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    Ok(())
}
