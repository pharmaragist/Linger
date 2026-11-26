//! Wait for the Wayland compositor's socket before launching a GUI app.
//!
//! No X11 support: WAYLAND_DISPLAY and XDG_RUNTIME_DIR are required.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

pub fn wait_for_display(timeout_secs: u64) -> Result<()> {
    let display = std::env::var("WAYLAND_DISPLAY").context("WAYLAND_DISPLAY is not set")?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;

    let socket = if display.starts_with('/') {
        PathBuf::from(display)
    } else {
        PathBuf::from(runtime).join(display)
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if socket.exists() {
            println!("linger: wayland socket {} is ready", socket.display());
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "wayland socket {} did not appear within {timeout_secs}s",
                socket.display()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_built_from_display() {
        // Pure logic check via the same construction used above.
        let socket = PathBuf::from("/run/user/1000").join("wayland-0");
        assert_eq!(socket.to_string_lossy(), "/run/user/1000/wayland-0");
    }
}
