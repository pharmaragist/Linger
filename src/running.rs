//! Detect already-running apps without spawning helper processes.
//!
//! `linger up` and `RestoreSession` must not launch a second copy of an app
//! that is already on screen. GUI apps like Firefox hand off to the running
//! instance and exit, which looks like a clean exit rather than a failure.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub(crate) fn base_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Does `argv` describe the same app as the configured `command` + `args`?
fn argv_matches(command: &str, args: &[String], argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    if base_name(first) != base_name(command) {
        return false;
    }
    args.iter().all(|arg| argv.contains(arg))
}

/// True if a process matching the configured command is already running.
pub fn is_running(command: &str, args: &[String]) -> Result<bool> {
    let entries = fs::read_dir("/proc").context("reading /proc")?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let raw = match fs::read(entry.path().join("cmdline")) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let argv: Vec<String> = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        if argv_matches(command, args, &argv) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_command_basename() {
        let argv = vec!["/usr/lib64/firefox/firefox".to_string()];
        assert!(argv_matches("/usr/bin/firefox", &[], &argv));
    }

    #[test]
    fn matches_all_configured_args() {
        let argv = vec![
            "/usr/bin/kitty".to_string(),
            "--app".to_string(),
            "dev".to_string(),
        ];
        let args = vec!["--app".to_string(), "dev".to_string()];
        assert!(argv_matches("/usr/bin/kitty", &args, &argv));
        assert!(!argv_matches(
            "/usr/bin/kitty",
            &["--app".to_string(), "other".to_string()],
            &argv
        ));
    }

    #[test]
    fn does_not_match_other_apps() {
        let argv = vec!["/usr/bin/firefox".to_string()];
        assert!(!argv_matches("/usr/bin/kitty", &[], &argv));
    }
}
