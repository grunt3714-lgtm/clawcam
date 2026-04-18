use anyhow::Result;
use tracing::info;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_teardown(dev: &Device) -> Result<()> {
    info!("tearing down clawcam on {} ({})", dev.name, dev.host);

    session::run_cmd(dev, "\
        sudo systemctl stop clawcam 2>/dev/null; \
        sudo systemctl disable clawcam 2>/dev/null; \
        sudo rm -f /etc/systemd/system/clawcam.service; \
        sudo systemctl daemon-reload; \
        sudo rm -f /usr/local/bin/clawcam; \
        sudo rm -rf /usr/local/share/clawcam"
    ).await?;

    println!("clawcam removed from {}", dev.name);
    Ok(())
}
