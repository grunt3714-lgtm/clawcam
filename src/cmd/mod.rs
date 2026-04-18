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

    /// Run the on-device detection monitor (not for direct use)
    Monitor {
        /// Webhook URL
        #[arg(long)]
        webhook: String,
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
        Command::Monitor { webhook, webhook_token, host, log_path } => {
            crate::detect::monitor::run_monitor(&webhook, webhook_token.as_deref(), host.as_deref(), log_path.as_deref()).await
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
