use anyhow::Result;

use crate::device::Device;
use crate::ssh::session;

pub async fn run_status(dev: &Device, json: bool) -> Result<()> {
    let output = session::run_cmd(dev, r#"
        echo "SERVICE=$(systemctl is-active clawcam 2>/dev/null || echo 'not-installed')"
        echo "UPTIME=$(systemctl show clawcam --property=ActiveEnterTimestamp 2>/dev/null | cut -d= -f2)"
        echo "CAMERA=$(ls /dev/video* 2>/dev/null | head -1 || echo 'none')"
        echo "LIBCAM=$(which libcamera-hello 2>/dev/null && echo 'yes' || echo 'no')"
        echo "MODEL=$(test -f /usr/local/share/clawcam/yolov8n.onnx && echo 'present' || echo 'missing')"
        echo "DISK=$(df -h / | tail -1 | awk '{print $5}')"
        echo "MEM=$(free -m | awk '/Mem:/{printf "%d/%dMB", $3, $2}')"
        echo "TEMP=$(vcgencmd measure_temp 2>/dev/null | cut -d= -f2 || echo 'n/a')"
    "#).await?;

    if json {
        let mut map = serde_json::Map::new();
        map.insert("device".into(), serde_json::Value::String(dev.name.clone()));
        map.insert("host".into(), serde_json::Value::String(dev.host.clone()));
        for line in output.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.to_lowercase(), serde_json::Value::String(v.to_string()));
            }
        }
        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        println!("device: {} ({})", dev.name, dev.host);
        for line in output.lines() {
            if let Some((k, v)) = line.split_once('=') {
                println!("  {}: {}", k.to_lowercase(), v);
            }
        }
    }

    Ok(())
}
