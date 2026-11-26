use crate::config::RestartPolicy;
use clap::{ArgAction, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "linger",
    version,
    about = "XSMP-style app persistence for Wayland, backed by systemd --user"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Enable lingering and install the D-Bus session service
    Init,

    /// Add an app to keep alive
    Add {
        /// Short name used for the systemd unit (letters, digits, '-' and '_')
        name: String,
        /// Executable to run
        command: String,
        /// Arguments passed to the executable; linger flags must precede NAME
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long, value_enum, default_value = "always")]
        restart: RestartPolicy,
        /// Seconds before a restart
        #[arg(long, default_value_t = 3)]
        delay: u64,
        /// Skip waiting for the Wayland socket before starting
        #[arg(
            long = "no-wait-for-display",
            action = ArgAction::SetFalse,
            default_value_t = true
        )]
        wait_for_display: bool,
        /// Extra env entry, e.g. RUST_LOG=debug (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// Extra systemd unit to order after (repeatable)
        #[arg(long)]
        after: Vec<String>,
    },

    /// Remove an app and its systemd unit
    Remove { name: String },

    /// List configured apps with their systemd state
    List,

    /// Regenerate and enable all unit files from config
    Sync,

    /// Sync, then start every configured app
    Up,

    /// Stop and disable every configured app
    Down,

    /// Show systemd status for one app
    Status { name: String },

    /// Re-capture WAYLAND_DISPLAY / XDG_RUNTIME_DIR / DBUS from this session
    Env {
        /// Save this snapshot under a logind session ID as well
        #[arg(long)]
        session: Option<String>,
    },

    /// Check that lingering, systemd and Wayland are all wired up
    Doctor,

    /// Run the D-Bus registration service in the foreground
    Serve,

    /// Write the current registration manifest
    Save,

    /// Start every configured app that is not active
    Restore,

    /// Internal helper invoked by generated units
    #[command(hide = true)]
    WaitForDisplay {
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_flags() {
        let cli = Cli::try_parse_from([
            "linger",
            "add",
            "--no-wait-for-display",
            "--env",
            "RUST_LOG=debug",
            "kitty",
            "/usr/bin/kitty",
            "--app",
            "dev",
        ])
        .unwrap();
        match cli.command {
            Command::Add {
                wait_for_display,
                args,
                envs,
                ..
            } => {
                assert!(!wait_for_display);
                assert_eq!(args, ["--app", "dev"]);
                assert_eq!(envs, ["RUST_LOG=debug"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_env_session() {
        let cli = Cli::try_parse_from(["linger", "env", "--session", "3"]).unwrap();
        match cli.command {
            Command::Env { session } => assert_eq!(session.as_deref(), Some("3")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn no_x11_aliases() {
        assert!(Cli::try_parse_from(["linger", "wait-for-x"]).is_err());
    }
}
