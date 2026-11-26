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

#[derive(Subcommand)]
pub enum Command {
    /// Enable lingering and install the D-Bus session service
    Init,

    /// Add an app to keep alive
    Add {
        /// Short name used for the systemd unit (letters, digits, '-' and '_')
        name: String,
        /// Executable to run
        command: String,
        /// Arguments passed to the executable
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
            alias = "no-wait-for-x",
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
    #[command(hide = true, alias = "wait-for-x")]
    WaitForDisplay {
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
}
