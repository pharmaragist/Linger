use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Poll until the X display accepts connections, or `timeout_secs` elapses.
pub fn wait_for_x(display: Option<&str>, timeout_secs: u64) -> Result<()> {
    let display = display
        .map(str::to_string)
        .or_else(|| std::env::var("DISPLAY").ok())
        .unwrap_or_else(|| ":0".to_string());

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if x_ready(&display) {
            println!("linger: display {display} is ready");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("display {display} did not become ready within {timeout_secs}s");
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn x_ready(display: &str) -> bool {
    socket_for(display)
        .map(|path| path.exists())
        .unwrap_or(false)
        && probe_with_xdpyinfo(display)
}

/// Prefer a real protocol probe; fall back to socket presence if xdpyinfo is absent.
fn probe_with_xdpyinfo(display: &str) -> bool {
    match Command::new("xdpyinfo")
        .arg("-display")
        .arg(display)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn socket_for(display: &str) -> Option<PathBuf> {
    let number = display.strip_prefix(':')?;
    let number = number.split('.').next()?;
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_display_socket() {
        assert_eq!(
            socket_for(":0"),
            Some(PathBuf::from("/tmp/.X11-unix/X0"))
        );
        assert_eq!(
            socket_for(":12.0"),
            Some(PathBuf::from("/tmp/.X11-unix/X12"))
        );
        assert_eq!(socket_for("remote:0"), None);
    }
}
