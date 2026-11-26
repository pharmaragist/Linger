use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl RestartPolicy {
    /// The value systemd's `Restart=` directive understands.
    pub fn as_systemd(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::OnFailure => "on-failure",
            Self::Never => "no",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "default_delay")]
    pub restart_sec: u64,
    #[serde(default = "default_true")]
    pub wait_for_x: bool,
    /// Extra `KEY=VALUE` entries for this app only.
    #[serde(default)]
    pub env: Vec<String>,
    /// Extra systemd units to order this app after.
    #[serde(default)]
    pub after: Vec<String>,
}

fn default_delay() -> u64 {
    3
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub apps: Vec<App>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
    }

    pub fn find(&self, name: &str) -> Option<&App> {
        self.apps.iter().find(|app| app.name == name)
    }

}

pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("invalid name '{name}': use letters, digits, '-' and '_' only");
    }
    Ok(())
}

pub fn validate_env_entry(entry: &str) -> Result<()> {
    let (key, _) = entry
        .split_once('=')
        .with_context(|| format!("env entry '{entry}' must look like KEY=VALUE"))?;
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("invalid env key '{key}' in '{entry}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(validate_name("my-app_2").is_ok());
        assert!(validate_name("bad name").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn systemd_restart_values() {
        assert_eq!(RestartPolicy::Always.as_systemd(), "always");
        assert_eq!(RestartPolicy::OnFailure.as_systemd(), "on-failure");
        assert_eq!(RestartPolicy::Never.as_systemd(), "no");
    }

    #[test]
    fn defaults_are_applied() {
        let cfg: Config = toml::from_str(
            r#"
            [[apps]]
            name = "xterm"
            command = "/usr/bin/xterm"
            "#,
        )
        .unwrap();
        let app = &cfg.apps[0];
        assert_eq!(app.restart, RestartPolicy::Always);
        assert_eq!(app.restart_sec, 3);
        assert!(app.wait_for_x);
    }

    #[test]
    fn kebab_case_restart_policy_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [[apps]]
            name = "xterm"
            command = "/usr/bin/xterm"
            restart = "on-failure"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.apps[0].restart, RestartPolicy::OnFailure);
    }
}
