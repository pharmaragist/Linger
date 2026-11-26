mod cli;
mod config;
mod envfile;
mod paths;
mod running;
mod service;
mod systemd;
mod unit;
mod wait;
mod xsmp;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::{validate_env_entry, validate_name, App, Config, RestartPolicy};
use paths::Paths;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
            wait_for_display,
            envs,
            after,
        } => cmd_add(
            &paths,
            name,
            command,
            args,
            restart,
            delay,
            wait_for_display,
            envs,
            after,
        ),
        Command::Remove { name } => cmd_remove(&paths, &name),
        Command::List => cmd_list(&paths),
        Command::Sync => cmd_sync(&paths),
        Command::Up => cmd_up(&paths),
        Command::Down => cmd_down(&paths),
        Command::Status { name } => cmd_status(&paths, &name),
        Command::Env { session } => cmd_env(&paths, session.as_deref()),
        Command::Doctor => cmd_doctor(&paths),
        Command::Serve => service::serve(),
        Command::Save => service::save_via_dbus(),
        Command::Restore => service::restore_via_dbus(),
        Command::WaitForDisplay { timeout } => wait::wait_for_display(timeout),
    }
}

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

    let mut vars = envfile::capture();
    let snapshots = write_env_snapshots(paths, None, &mut vars)?;
    println!(
        "captured {snapshots} session env snapshot(s) (global{} session-specific)",
        if snapshots > 1 { " +" } else { "" }
    );
    let env_file = xsmp::install_environment(paths)?;
    println!("XSMP environment fragment: {}", env_file.display());

    let user = current_user()?;
    if systemd::lingering_enabled()? {
        println!("lingering already enabled for {user}");
    } else {
        systemd::enable_linger()?;
        println!("enabled lingering for {user}");
    }

    install_service_unit(paths)?;

    println!();
    println!(
        "done. `linger.service` runs {} and the XSMP server on {}.",
        service::BUS_NAME,
        xsmp::socket_path(paths)?.display()
    );
    println!("apps that support XSMP need SESSION_MANAGER in their environment:");
    println!("  export SESSION_MANAGER='{}'", xsmp::env_value(paths)?);
    println!("put that line in your login shell or compositor environment.");
    println!("non-XSMP apps can still be pinned with `linger add <name> <command>`");
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
    wait_for_display: bool,
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
        app_id: None,
        command: resolved,
        args,
        restart,
        restart_sec: delay,
        wait_for_display,
        env: envs,
        working_dir: None,
        session: None,
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
        println!("no apps configured yet. register from an app or use `linger add`");
        return Ok(());
    }
    println!(
        "{:<20} {:<10} {:<11} {:<8} {}",
        "NAME", "ACTIVE", "ENABLED", "SESSION", "COMMAND"
    );
    for app in &cfg.apps {
        let unit = Paths::unit_name(&app.name);
        let active = if systemd::is_active(&unit) {
            "active"
        } else {
            "inactive"
        };
        let enabled = if systemd::is_enabled(&unit) {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "{:<20} {:<10} {:<11} {:<8} {} {}",
            app.name,
            active,
            enabled,
            app.session.as_deref().unwrap_or("-"),
            app.command,
            app.args.join(" ")
        );
    }
    Ok(())
}

fn cmd_sync(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.unit_dir)?;
    if !paths.env_file.exists() {
        let mut vars = envfile::capture();
        write_env_snapshots(paths, None, &mut vars)?;
    }
    let cfg = Config::load(&paths.config_file)?;
    let linger_bin = std::env::current_exe().context("finding linger executable")?;

    for app in &cfg.apps {
        let env_file = paths.env_file_for(app.session.as_deref());
        let content = unit::render(app, &env_file, &linger_bin);
        let path = paths.unit_path(&app.name);
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
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
    let mut started = 0;
    let mut skipped = 0;
    for app in &cfg.apps {
        let unit = Paths::unit_name(&app.name);
        if systemd::is_active(&unit) {
            println!("'{}' is already active", app.name);
            skipped += 1;
            continue;
        }
        if running::is_running(&app.command, &app.args).unwrap_or(false) {
            println!("'{}' is already running; skipping", app.name);
            skipped += 1;
            continue;
        }
        systemd::start(&unit)?;
        started += 1;
    }
    println!("started {started} app(s), skipped {skipped}");
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

fn cmd_env(paths: &Paths, session: Option<&str>) -> Result<()> {
    let mut vars = envfile::capture();
    let snapshots = write_env_snapshots(paths, session, &mut vars)?;
    for (key, value) in &vars {
        println!("{key}={value}");
    }
    println!(
        "saved {snapshots} snapshot(s): {} and per-session files in {}",
        paths.env_file.display(),
        paths.config_dir.display()
    );
    if let Some(session) = session {
        println!(
            "session {session}: {}",
            paths.session_env_file(session).display()
        );
    }
    println!("rerun `linger env` whenever a Wayland session gets a new socket");
    Ok(())
}

fn write_env_snapshots(
    paths: &Paths,
    explicit_session: Option<&str>,
    vars: &mut BTreeMap<String, String>,
) -> Result<usize> {
    vars.insert("SESSION_MANAGER".to_string(), xsmp::env_value(paths)?);
    envfile::write(&paths.env_file, vars)?;
    let mut count = 1;
    let session = explicit_session
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("XDG_SESSION_ID")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    if let Some(session) = session {
        envfile::write(&paths.session_env_file(&session), vars)?;
        count += 1;
    }
    Ok(count)
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
    match systemd::lingering_enabled() {
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

    if systemd::is_active("linger.service") {
        println!("[ok]   linger.service is active on the user bus");
    } else {
        println!("[FAIL] linger.service is not active; run `linger init`");
        problems += 1;
    }

    if paths.config_file.exists() {
        println!("[ok]   config at {}", paths.config_file.display());
    } else {
        println!("[FAIL] no config; run `linger init`");
        problems += 1;
    }

    match fs::read_to_string(&paths.env_file) {
        Ok(raw) if raw.contains("WAYLAND_DISPLAY=") => {
            println!("[ok]   env file at {}", paths.env_file.display())
        }
        _ => {
            println!(
                "[FAIL] env file has no WAYLAND_DISPLAY; run `linger env` in a Wayland session"
            );
            problems += 1;
        }
    }

    let socket = xsmp::socket_path(paths)?;
    if socket.exists() {
        println!("[ok]   XSMP socket at {}", socket.display());
    } else {
        println!("[FAIL] XSMP socket missing; restart `linger.service`");
        problems += 1;
    }

    let export_line = xsmp::env_value(paths)?;
    let in_shell = std::env::var("SESSION_MANAGER")
        .map(|value| value.contains(&socket.display().to_string()))
        .unwrap_or(false);
    let in_fragment = xsmp::environment_fragment_path(paths)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|raw| raw.contains(&socket.display().to_string()))
        .unwrap_or(false);
    if in_shell {
        println!("[ok]   SESSION_MANAGER points at linger");
    } else if in_fragment {
        println!("[ok]   SESSION_MANAGER configured for systemd-launched apps");
        println!("       export SESSION_MANAGER='{export_line}' for shell-launched apps");
    } else {
        println!("[FAIL] SESSION_MANAGER is not set to linger's XSMP socket");
        println!("       export SESSION_MANAGER='{export_line}'");
        problems += 1;
    }

    for key in [
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        match std::env::var(key) {
            Ok(value) if !value.is_empty() => println!("[ok]   {key}={value}"),
            _ => {
                println!("[FAIL] {key} is unset — run this from inside your Wayland session");
                problems += 1;
            }
        }
    }

    if problems == 0 {
        println!("all checks passed");
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

fn install_service_unit(paths: &Paths) -> Result<()> {
    let binary = std::env::current_exe().context("finding linger executable")?;
    let unit = format!(
        "# Generated by linger. Do not edit.\n\
         [Unit]\n\
         Description=linger session manager (XSMP + D-Bus)\n\n\
         [Service]\n\
         Type=simple\n\
         Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/%U/bus\n\
         ExecStart={} serve\n\
         Restart=always\n\
         RestartSec=5\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        binary.display()
    );
    let path = paths.unit_dir.join("linger.service");
    fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
    systemd::daemon_reload()?;
    if !systemd::is_enabled("linger.service") {
        systemd::enable("linger.service")?;
    }
    if systemd::is_active("linger.service") {
        systemd::restart("linger.service")?;
    } else {
        systemd::start("linger.service")?;
    }
    println!("service unit installed: {}", path.display());
    Ok(())
}

fn current_user() -> Result<String> {
    let uid = systemd::current_uid()?;
    let passwd = fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next().unwrap_or("");
        if fields.nth(1).and_then(|value| value.parse::<u32>().ok()) == Some(uid) {
            return Ok(name.to_string());
        }
    }
    bail!("could not resolve uid {uid} to a username")
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
