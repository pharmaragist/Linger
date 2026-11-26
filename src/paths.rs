use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub env_file: PathBuf,
    pub unit_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let xdg = dirs::config_dir().context("could not determine XDG config directory")?;
        let config_dir = xdg.join("linger");
        let unit_dir = xdg.join("systemd/user");
        let config_file = config_dir.join("config.toml");
        let env_file = config_dir.join("env.conf");
        Ok(Self {
            config_dir,
            config_file,
            env_file,
            unit_dir,
        })
    }

    pub fn unit_path(&self, name: &str) -> PathBuf {
        self.unit_dir.join(format!("linger-{name}.service"))
    }

    pub fn unit_name(name: &str) -> String {
        format!("linger-{name}.service")
    }
}
