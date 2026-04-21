use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::device::DeviceRegistry;

#[derive(Parser)]
#[command(name = "clawcam", version, about = "AI-powered camera monitoring for Raspberry Pi")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage camera devices
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },

    /// Deploy clawcam to a registered device
    Setup {
        /// Device name from registry
        name: String,
        /// Webhook URL for detection events
        #[arg(long)]
        webhook: String,
        /// Bearer token for webhook auth
        #[arg(long)]
        webhook_token: Option<String>,
        /// SSH user (default: pi)
        #[arg(long, default_value = "pi")]
        user: String,
    },

    /// Check device status
    Status {
        /// Device name from registry
        name: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Capture a JPEG snapshot
    Snap {
        /// Device name from registry
        name: String,
        /// Output file path
        #[arg(long, short)]
        out: Option<String>,
    },

    /// Record a video clip
    Clip {
        /// Device name from registry
        name: String,
        /// Duration in seconds
        #[arg(long, default_value = "10")]
        dur: u32,
        /// Output file path
        #[arg(long, short)]
        out: Option<String>,
    },

    /// Speak text through device speaker
    Speak {
        /// Device name from registry
        name: String,
        /// Text to speak
        message: String,
        /// Volume 1-100
        #[arg(long, default_value = "80")]
        volume: u8,
    },

    /// Record audio from device microphone
    Listen {
        /// Device name from registry
        name: String,
        /// Duration in seconds
        #[arg(long, default_value = "10")]
        dur: u32,
        /// Output file path
        #[arg(long, short)]
        out: Option<String>,
    },

    /// Stop and remove clawcam from a device
    Teardown {
        /// Device name from registry
        name: String,
    },

    /// Update clawcam to the latest GitHub release
    Update {
        /// Device to update. Omit to update the local binary.
        name: Option<String>,
        /// Update every registered device (cannot be combined with a device name).
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Specific release tag (default: latest).
        #[arg(long)]
        version: Option<String>,
    },

    /// Pan/tilt/zoom control via the device's VISCA HTTP endpoint (port 8091).
    /// Same path the clawcam-app web UI uses; requires the on-device VISCA
    /// server (set CLAWCAM_PTZ_SERIAL in the systemd unit) connected to a
    /// real conference-cam motor over RS-232/RS-485.
    Ptz {
        /// Device name from registry
        name: String,
        /// Port for the on-device VISCA HTTP server (default 8091)
        #[arg(long, default_value = "8091")]
        port: u16,
        #[command(subcommand)]
        action: PtzCliAction,
    },

    /// On-device: capture a JPEG snapshot (used internally by remote snap)
    #[command(name = "_snap", hide = true)]
    SnapLocal {
        /// Output file path
        #[arg(long)]
        out: String,
    },

    /// On-device: record a video clip (used internally by remote clip)
    #[command(name = "_clip", hide = true)]
    ClipLocal {
        /// Duration in seconds
        #[arg(long, default_value = "10")]
        dur: u32,
        /// Output file path
        #[arg(long)]
        out: String,
    },

    /// Run the on-device detection monitor (not for direct use)
    Monitor {
        /// Webhook URL (or set CLAWCAM_WEBHOOK env var)
        #[arg(long)]
        webhook: Option<String>,
        /// Bearer token for webhook auth
        #[arg(long)]
        webhook_token: Option<String>,
        /// Device hostname for event payloads
        #[arg(long)]
        host: Option<String>,
        /// Log file path
        #[arg(long)]
        log_path: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PtzCliAction {
    /// Return the camera to its home/center position.
    Center,
    /// Stop all motion immediately.
    Stop,
    /// Send a direction burst. Each axis is -1 (left/down/wide), 0 (no motion),
    /// or +1 (right/up/tele). The camera auto-stops after --duration ms.
    Nudge {
        /// Pan direction: -1 (left), 0, +1 (right)
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        pan: i32,
        /// Tilt direction: -1 (down), 0, +1 (up)
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        tilt: i32,
        /// Zoom direction: -1 (wide), 0, +1 (tele)
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        zoom: i32,
        /// Duration of the burst in milliseconds before auto-stop (default 300)
        #[arg(long, default_value = "300")]
        duration: u64,
    },
}

#[derive(Subcommand)]
pub enum DeviceAction {
    /// Register a new device
    Add {
        /// Friendly name for the device
        name: String,
        /// IP address or hostname
        host: String,
        /// SSH port
        #[arg(long, default_value = "22")]
        port: u16,
        /// SSH user
        #[arg(long, default_value = "pi")]
        user: String,
    },
    /// List all registered devices
    List,
    /// Remove a device from the registry
    Remove {
        /// Device name
        name: String,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Device { action } => run_device(action).await,
        Command::Setup { name, webhook, webhook_token, user } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::ssh::setup::run_setup(&dev, &user, &webhook, webhook_token.as_deref()).await
        }
        Command::Status { name, json } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::ssh::status::run_status(&dev, json).await
        }
        Command::Snap { name, out } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::media::snap::run_snap(&dev, out.as_deref()).await
        }
        Command::Clip { name, dur, out } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::media::clip::run_clip(&dev, dur, out.as_deref()).await
        }
        Command::Speak { name, message, volume } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::media::audio::run_speak(&dev, &message, volume).await
        }
        Command::Listen { name, dur, out } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::media::audio::run_listen(&dev, dur, out.as_deref()).await
        }
        Command::Teardown { name } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            crate::ssh::teardown::run_teardown(&dev).await
        }
        Command::Update { name, all, version } => {
            if all {
                crate::update::update_all(version.as_deref()).await
            } else if let Some(n) = name {
                let registry = DeviceRegistry::load()?;
                let dev = registry.get(&n)?;
                crate::update::update_remote(&dev, version.as_deref()).await
            } else {
                crate::update::update_local(version.as_deref()).await
            }
        }
        Command::Ptz { name, port, action } => {
            let registry = DeviceRegistry::load()?;
            let dev = registry.get(&name)?;
            let action = match action {
                PtzCliAction::Center => crate::media::ptz::PtzAction::Center,
                PtzCliAction::Stop => crate::media::ptz::PtzAction::Stop,
                PtzCliAction::Nudge { pan, tilt, zoom, duration } => {
                    crate::media::ptz::PtzAction::Nudge {
                        pan,
                        tilt,
                        zoom,
                        duration_ms: duration,
                    }
                }
            };
            crate::media::ptz::run_ptz(&dev, port, action).await
        }
        Command::SnapLocal { out } => {
            crate::media::snap::run_snap_local(&out)
        }
        Command::ClipLocal { dur, out } => {
            crate::media::clip::run_clip_local(dur, &out)
        }
        Command::Monitor { webhook, webhook_token, host, log_path } => {
            crate::detect::monitor::run_monitor(webhook.as_deref(), webhook_token.as_deref(), host.as_deref(), log_path.as_deref()).await
        }
    }
}

async fn run_device(action: DeviceAction) -> Result<()> {
    let mut registry = DeviceRegistry::load()?;
    match action {
        DeviceAction::Add { name, host, port, user } => {
            registry.add(&name, &host, port, &user)?;
            println!("added device '{name}' at {host}:{port}");
        }
        DeviceAction::List => {
            let devices = registry.list();
            if devices.is_empty() {
                println!("no devices registered");
            } else {
                println!("{:<16} {:<20} {:<6} {}", "NAME", "HOST", "PORT", "USER");
                for d in devices {
                    println!("{:<16} {:<20} {:<6} {}", d.name, d.host, d.port, d.user);
                }
            }
        }
        DeviceAction::Remove { name } => {
            registry.remove(&name)?;
            println!("removed device '{name}'");
        }
    }
    Ok(())
}
