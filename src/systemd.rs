//! Thin wrappers around `systemctl --user` / `loginctl`.
//!
//! This is the only layer that knows how linger talks to systemd. Swapping it
//! for a direct zbus connection to `org.freedesktop.systemd1` later only
//! requires changing this module.

use anyhow::{bail, Context, Result};
use std::process::{Command, Output, Stdio};

fn command(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program} (is systemd installed?)"))
}

fn systemctl(args: &[&str]) -> Result<Output> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    command("systemctl", &full)
}

fn ensure(output: Output, what: &str) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    let detail = if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail
    };
    bail!("{what}: {detail}")
}

fn mutate(args: &[&str], what: &str) -> Result<()> {
    let output = ensure(systemctl(args)?, what)?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !text.is_empty() {
        for line in text.lines() {
            println!("  {line}");
        }
    }
    Ok(())
}

pub fn daemon_reload() -> Result<()> {
    ensure(systemctl(&["daemon-reload"])?, "systemctl --user daemon-reload")?;
    Ok(())
}

pub fn enable(unit: &str) -> Result<()> {
    mutate(&["enable", unit], &format!("enabling {unit}"))
}

pub fn disable(unit: &str) -> Result<()> {
    mutate(&["disable", unit], &format!("disabling {unit}"))
}

pub fn start(unit: &str) -> Result<()> {
    mutate(&["start", unit], &format!("starting {unit}"))
}

pub fn stop(unit: &str) -> Result<()> {
    mutate(&["stop", unit], &format!("stopping {unit}"))
}

pub fn restart(unit: &str) -> Result<()> {
    mutate(&["restart", unit], &format!("restarting {unit}"))
}

pub fn is_active(unit: &str) -> bool {
    systemctl(&["is-active", unit])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn is_enabled(unit: &str) -> bool {
    systemctl(&["is-enabled", unit])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `loginctl enable-linger <user>` — keeps the user manager alive after logout.
pub fn enable_linger(user: &str) -> Result<()> {
    let output = command("loginctl", &["enable-linger", user])?;
    ensure(output, &format!("loginctl enable-linger {user}"))?;
    Ok(())
}

pub fn lingering_enabled(user: &str) -> Result<bool> {
    let output = command("loginctl", &["show-user", user, "-p", "Linger", "--value"])?;
    let output = ensure(output, "querying linger state")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "yes")
}

/// True when the systemd user manager answered at all.
pub fn manager_ok() -> bool {
    systemctl(&["is-system-running"])
        .map(|out| {
            let text = String::from_utf8_lossy(&out.stdout);
            let state = text.trim();
            state == "running" || state == "degraded" || out.status.success()
        })
        .unwrap_or(false)
}

/// Stream `systemctl status` straight to the terminal (pager disabled).
pub fn status(unit: &str) -> Result<()> {
    let status = Command::new("systemctl")
        .args(["--user", "status", unit, "--no-pager", "-l"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run systemctl --user status")?;
    if !status.success() {
        bail!("systemctl status exited with {status}");
    }
    Ok(())
}
