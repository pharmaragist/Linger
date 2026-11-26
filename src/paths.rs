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

    /// Environment snapshot for one logind session. Apps can refresh it with
    /// `linger env --session <id>` without disturbing other sessions.
    pub fn session_env_file(&self, session: &str) -> PathBuf {
        let mut name: String = session
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        while name.contains("--") {
            name = name.replace("--", "-");
        }
        let name = name.trim_matches('-');
        if name.is_empty() {
            self.config_dir.join("env.session.conf")
        } else {
            self.config_dir.join(format!("env.{name}.conf"))
        }
    }

    /// Per-session env files win when they exist; otherwise fall back to the
    /// global snapshot taken by `linger env`.
    pub fn env_file_for(&self, session: Option<&str>) -> PathBuf {
        match session.filter(|value| !value.is_empty()) {
            Some(session) => {
                let candidate = self.session_env_file(session);
                if candidate.exists() {
                    candidate
                } else {
                    self.env_file.clone()
                }
            }
            None => self.env_file.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_env_names_are_safe() {
        let paths = Paths {
            config_dir: PathBuf::from("/cfg"),
            config_file: PathBuf::from("/cfg/config.toml"),
            env_file: PathBuf::from("/cfg/env.conf"),
            unit_dir: PathBuf::from("/units"),
        };
        assert_eq!(
            paths.session_env_file("3"),
            PathBuf::from("/cfg/env.3.conf")
        );
        assert_eq!(
            paths.session_env_file("a/b:c"),
            PathBuf::from("/cfg/env.a-b-c.conf")
        );
        assert_eq!(
            paths.session_env_file("!!!"),
            PathBuf::from("/cfg/env.session.conf")
        );
    }
}
