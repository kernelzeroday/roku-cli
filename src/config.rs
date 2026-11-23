use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    pub roku_ip: Option<String>,
}

impl Config {
    fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("no config dir")?
            .join("roku-cli");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("config.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if path.exists() {
            Ok(serde_json::from_str(&std::fs::read_to_string(&path)?)?)
        } else {
            Ok(Config::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
