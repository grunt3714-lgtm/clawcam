mod cmd;
mod device;
mod detect;
mod media;
mod ssh;
mod webhook;

use clap::Parser;
use cmd::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("clawcam=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    cmd::run(cli).await
}
