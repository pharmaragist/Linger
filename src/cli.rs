use crate::config::RestartPolicy;
use clap::{ArgAction, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "linger",
    version,
    about = "Keep your apps alive across logouts, reboots and crashes with systemd --user"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Enable lingering, create config dirs and capture the session environment
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
        /// Skip waiting for X to be ready before starting
        #[arg(long = "no-wait-for-x", action = ArgAction::SetFalse, default_value_t = true)]
        wait_for_x: bool,
        /// Extra env entry, e.g. RUST_LOG=debug (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// Extra systemd unit to order after (repeatable)
        #[arg(long)]
        after: Vec<String>,
    },

    /// Remove an app and its systemd unit
    Remove {
        name: String,
    },

    /// List configured apps with their systemd state
    List,

    /// Regenerate and enable all unit files from config
    Sync,

    /// Sync, then start every configured app
    Up,

    /// Stop and disable every configured app
    Down,

    /// Show systemd status for one app
    Status {
        name: String,
    },

    /// Re-capture DISPLAY / XAUTHORITY / DBUS from the current session
    Env,

    /// Check that lingering, systemd and X are all wired up
    Doctor,

    /// Internal helper invoked by generated units
    #[command(hide = true)]
    WaitForX {
        #[arg(long)]
        display: Option<String>,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
}
