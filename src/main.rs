mod cli;
mod config;
mod envfile;
mod paths;
mod systemd;
mod unit;
mod wait;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::{validate_env_entry, validate_name, App, Config, RestartPolicy};
use paths::Paths;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as ProcessCommand;

fn main() {
    if let Err(err) = run() {
        eprintln!("linger: error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;

    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Add {
            name,
            command,
            args,
            restart,
            delay,
            wait_for_x,
            envs,
            after,
        } => cmd_add(
            &paths, name, command, args, restart, delay, wait_for_x, envs, after,
        ),
        Command::Remove { name } => cmd_remove(&paths, &name),
        Command::List => cmd_list(&paths),
        Command::Sync => cmd_sync(&paths),
        Command::Up => cmd_up(&paths),
        Command::Down => cmd_down(&paths),
        Command::Status { name } => cmd_status(&paths, &name),
        Command::Env => cmd_env(&paths),
        Command::Doctor => cmd_doctor(&paths),
        Command::WaitForX { display, timeout } => wait::wait_for_x(display.as_deref(), timeout),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn cmd_init(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.config_dir)?;
    fs::create_dir_all(&paths.unit_dir)?;
    println!("config: {}", paths.config_file.display());
    println!("units:  {}", paths.unit_dir.display());

    if paths.config_file.exists() {
        println!("config already exists, leaving it alone");
    } else {
        Config::default().save(&paths.config_file)?;
        println!("created empty config");
    }

    if paths.env_file.exists() {
        println!("env file already exists (run `linger env` to refresh)");
    } else {
        let vars = envfile::capture();
        envfile::write(&paths.env_file, &vars)?;
        println!("captured {} session variables into {}", vars.len(), paths.env_file.display());
    }

    let user = current_user()?;
    if systemd::lingering_enabled(&user)? {
        println!("lingering already enabled for {user}");
    } else {
        systemd::enable_linger(&user)?;
        println!("enabled lingering for {user}");
    }

    println!("done. add apps with `linger add <name> <command>` then `linger up`");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    paths: &Paths,
    name: String,
    command: String,
    args: Vec<String>,
    restart: RestartPolicy,
    delay: u64,
    wait_for_x: bool,
    envs: Vec<String>,
    after: Vec<String>,
) -> Result<()> {
    validate_name(&name)?;
    for entry in &envs {
        validate_env_entry(entry)?;
    }

    let mut cfg = Config::load(&paths.config_file)?;
    if cfg.find(&name).is_some() {
        bail!("app '{name}' already exists; remove it first");
    }

    let resolved = resolve_command(&command);
    cfg.apps.push(App {
        name: name.clone(),
        command: resolved,
        args,
        restart,
        restart_sec: delay,
        wait_for_x,
        env: envs,
        after,
    });
    cfg.save(&paths.config_file)?;
    println!("added '{name}' to {}", paths.config_file.display());

    cmd_sync(paths)?;
    let unit = Paths::unit_name(&name);
    systemd::start(&unit)?;
    println!("'{name}' is enabled and started");
    Ok(())
}

fn cmd_remove(paths: &Paths, name: &str) -> Result<()> {
    let mut cfg = Config::load(&paths.config_file)?;
    let Some(pos) = cfg.apps.iter().position(|app| app.name == name) else {
        bail!("no app named '{name}'");
    };
    cfg.apps.remove(pos);
    cfg.save(&paths.config_file)?;

    let unit = Paths::unit_name(name);
    if systemd::is_active(&unit) {
        systemd::stop(&unit)?;
    }
    if systemd::is_enabled(&unit) {
        systemd::disable(&unit)?;
    }
    let path = paths.unit_path(name);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    systemd::daemon_reload()?;
    println!("removed '{name}'");
    Ok(())
}

fn cmd_list(paths: &Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    if cfg.apps.is_empty() {
        println!("no apps configured yet. try `linger add <name> <command>`");
        return Ok(());
    }
    println!("{:<20} {:<10} {:<11} {}", "NAME", "ACTIVE", "ENABLED", "COMMAND");
    for app in &cfg.apps {
        let unit = Paths::unit_name(&app.name);
        let active = if systemd::is_active(&unit) { "active" } else { "inactive" };
        let enabled = if systemd::is_enabled(&unit) { "enabled" } else { "disabled" };
        println!(
            "{:<20} {:<10} {:<11} {} {}",
            app.name, active, enabled, app.command, app.args.join(" ")
        );
    }
    Ok(())
}

fn cmd_sync(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.unit_dir)?;
    if !paths.env_file.exists() {
        let vars = envfile::capture();
        envfile::write(&paths.env_file, &vars)?;
        println!("captured session env into {}", paths.env_file.display());
    }
    let cfg = Config::load(&paths.config_file)?;
    let linger_bin = std::env::current_exe().context("finding linger executable")?;

    for app in &cfg.apps {
        let content = unit::render(app, &paths.env_file, &linger_bin);
        let path = paths.unit_path(&app.name);
        fs::write(&path, content)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }

    systemd::daemon_reload()?;
    for app in &cfg.apps {
        systemd::enable(&Paths::unit_name(&app.name))?;
    }
    println!("synced {} app(s)", cfg.apps.len());
    Ok(())
}

fn cmd_up(paths: &Paths) -> Result<()> {
    cmd_sync(paths)?;
    let cfg = Config::load(&paths.config_file)?;
    for app in &cfg.apps {
        let unit = Paths::unit_name(&app.name);
        if systemd::is_active(&unit) {
            systemd::restart(&unit)?;
        } else {
            systemd::start(&unit)?;
        }
    }
    println!("all {} app(s) up", cfg.apps.len());
    Ok(())
}

fn cmd_down(paths: &Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    for app in &cfg.apps {
        let unit = Paths::unit_name(&app.name);
        if systemd::is_active(&unit) {
            systemd::stop(&unit)?;
        }
        if systemd::is_enabled(&unit) {
            systemd::disable(&unit)?;
        }
        println!("'{}' stopped and disabled", app.name);
    }
    Ok(())
}

fn cmd_status(paths: &Paths, name: &str) -> Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    if cfg.find(name).is_none() {
        bail!("no app named '{name}'");
    }
    systemd::status(&Paths::unit_name(name))
}

fn cmd_env(paths: &Paths) -> Result<()> {
    let vars = envfile::capture();
    envfile::write(&paths.env_file, &vars)?;
    for (key, value) in &vars {
        println!("{key}={value}");
    }
    println!("saved {} variables to {}", vars.len(), paths.env_file.display());
    println!("note: rerun `linger env` after starting a new X session");
    Ok(())
}

fn cmd_doctor(paths: &Paths) -> Result<()> {
    let mut problems = 0;

    if systemd::manager_ok() {
        println!("[ok]   systemd --user manager is running");
    } else {
        println!("[FAIL] systemd --user manager is not responding");
        problems += 1;
    }

    let user = current_user()?;
    match systemd::lingering_enabled(&user) {
        Ok(true) => println!("[ok]   lingering enabled for {user}"),
        Ok(false) => {
            println!("[FAIL] lingering disabled — apps will die on logout");
            println!("       fix with: loginctl enable-linger {user}");
            problems += 1;
        }
        Err(err) => {
            println!("[FAIL] could not query linger state: {err}");
            problems += 1;
        }
    }

    if paths.config_file.exists() {
        println!("[ok]   config at {}", paths.config_file.display());
    } else {
        println!("[FAIL] no config; run `linger init`");
        problems += 1;
    }

    if paths.env_file.exists() {
        println!("[ok]   env file at {}", paths.env_file.display());
    } else {
        println!("[FAIL] no env file; run `linger env`");
        problems += 1;
    }

    match std::env::var("DISPLAY") {
        Ok(display) => println!("[ok]   DISPLAY={display}"),
        Err(_) => {
            println!("[FAIL] DISPLAY is unset — run this from inside your X session");
            problems += 1;
        }
    }

    if problems == 0 {
        println!("all checks passed");
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn current_user() -> Result<String> {
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            return Ok(user);
        }
    }
    let output = ProcessCommand::new("id")
        .arg("-un")
        .output()
        .context("running `id -un`")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_command(input: &str) -> String {
    if input.contains('/') {
        return input.to_string();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(input);
        if is_executable(&candidate) {
            return candidate.to_string_lossy().into_owned();
        }
    }
    eprintln!("linger: warning: '{input}' not found in PATH; keeping it verbatim");
    input.to_string()
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
