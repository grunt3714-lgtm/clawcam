use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeviceRegistry {
    devices: BTreeMap<String, Device>,
    #[serde(skip)]
    path: PathBuf,
}

impl DeviceRegistry {
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("cannot determine config directory")?
            .join("clawcam");
        fs::create_dir_all(&config_dir)?;
        Ok(config_dir.join("devices.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        let mut registry = if path.exists() {
            let data = fs::read_to_string(&path)?;
            serde_json::from_str::<DeviceRegistry>(&data)
                .context("failed to parse device registry")?
        } else {
            DeviceRegistry::default()
        };
        registry.path = path;
        Ok(registry)
    }

    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, data)?;
        Ok(())
    }

    pub fn add(&mut self, name: &str, host: &str, port: u16, user: &str) -> Result<()> {
        if self.devices.contains_key(name) {
            bail!("device '{name}' already exists");
        }
        self.devices.insert(
            name.to_string(),
            Device {
                name: name.to_string(),
                host: host.to_string(),
                port,
                user: user.to_string(),
            },
        );
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.devices.remove(name).is_none() {
            bail!("device '{name}' not found");
        }
        self.save()
    }

    pub fn get(&self, name: &str) -> Result<Device> {
        self.devices
            .get(name)
            .cloned()
            .context(format!("device '{name}' not found — run `clawcam device add` first"))
    }

    pub fn list(&self) -> Vec<&Device> {
        self.devices.values().collect()
    }
}
