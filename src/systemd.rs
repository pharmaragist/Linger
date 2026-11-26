//! All systemd / logind interaction goes through D-Bus via zbus.
//!
//! No subprocesses, no parsing of `systemctl` text. The user manager lives on
//! the session bus; logind lives on the system bus.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::MetadataExt;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const SYSTEMD_DEST: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const LOGIN1_USER_IFACE: &str = "org.freedesktop.login1.User";
const REPLACE_MODE: &str = "replace";

fn user_bus() -> Result<Connection> {
    Connection::session().context("connecting to the user D-Bus session bus")
}

fn system_bus() -> Result<Connection> {
    Connection::system().context("connecting to the system D-Bus bus")
}

fn manager<'a>(conn: &'a Connection) -> Result<Proxy<'a>> {
    Proxy::new(conn, SYSTEMD_DEST, SYSTEMD_PATH, MANAGER_IFACE)
        .context("connecting to the systemd manager interface")
}

fn login1<'a>(conn: &'a Connection) -> Result<Proxy<'a>> {
    Proxy::new(conn, LOGIN1_DEST, LOGIN1_PATH, LOGIN1_MANAGER_IFACE)
        .context("connecting to the logind manager interface")
}

fn current_uid() -> Result<u32> {
    fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .context("reading our uid from /proc/self")
}

fn print_changes(changes: &[(String, String, String)]) {
    for (kind, path, destination) in changes {
        match kind.as_str() {
            "symlink" => println!("  Created symlink {destination} -> {path}"),
            "unlink" => println!("  Removed symlink {destination}"),
            other => println!("  {other}: {path} -> {destination}"),
        }
    }
}

pub fn daemon_reload() -> Result<()> {
    let conn = user_bus()?;
    manager(&conn)?
        .call::<_, _, ()>("Reload", &())
        .context("systemd Reload() failed")?;
    Ok(())
}

pub fn enable(unit: &str) -> Result<()> {
    let conn = user_bus()?;
    let files = vec![unit.to_string()];
    let (_, changes): (bool, Vec<(String, String, String)>) = manager(&conn)?
        .call("EnableUnitFiles", &(files, false, true))
        .with_context(|| format!("EnableUnitFiles({unit}) failed"))?;
    print_changes(&changes);
    Ok(())
}

pub fn disable(unit: &str) -> Result<()> {
    let conn = user_bus()?;
    let files = vec![unit.to_string()];
    let changes: Vec<(String, String, String)> = manager(&conn)?
        .call("DisableUnitFiles", &(files, false))
        .with_context(|| format!("DisableUnitFiles({unit}) failed"))?;
    print_changes(&changes);
    Ok(())
}

fn unit_job(method: &str, unit: &str) -> Result<()> {
    let conn = user_bus()?;
    let _job: OwnedObjectPath = manager(&conn)?
        .call(method, &(unit.to_string(), REPLACE_MODE))
        .with_context(|| format!("{method}({unit}) failed"))?;
    Ok(())
}

pub fn start(unit: &str) -> Result<()> {
    unit_job("StartUnit", unit)
}

pub fn stop(unit: &str) -> Result<()> {
    unit_job("StopUnit", unit)
}

pub fn restart(unit: &str) -> Result<()> {
    unit_job("RestartUnit", unit)
}

fn get_unit_path(mgr: &Proxy<'_>, unit: &str) -> Result<OwnedObjectPath> {
    mgr.call("GetUnit", &(unit.to_string(),))
        .with_context(|| format!("GetUnit({unit}) failed"))
}

pub fn is_active(unit: &str) -> bool {
    (|| -> Result<bool> {
        let conn = user_bus()?;
        let mgr = manager(&conn)?;
        let path = get_unit_path(&mgr, unit)?;
        let proxy = Proxy::new(&conn, SYSTEMD_DEST, path.as_str(), UNIT_IFACE)?;
        let state: String = proxy.get_property("ActiveState")?;
        Ok(state == "active" || state == "reloading")
    })()
    .unwrap_or(false)
}

pub fn is_enabled(unit: &str) -> bool {
    (|| -> Result<bool> {
        let conn = user_bus()?;
        let state: String = manager(&conn)?
            .call("GetUnitFileState", &(unit.to_string(),))
            .with_context(|| format!("GetUnitFileState({unit}) failed"))?;
        Ok(matches!(
            state.as_str(),
            "enabled" | "enabled-runtime" | "static" | "generated" | "indirect"
        ))
    })()
    .unwrap_or(false)
}

/// True when the systemd user manager answered at all.
pub fn manager_ok() -> bool {
    (|| -> Result<bool> {
        let conn = user_bus()?;
        let state: String = manager(&conn)?.get_property("SystemState")?;
        Ok(state == "running" || state == "degraded")
    })()
    .unwrap_or(false)
}

/// Render the useful part of `systemctl status` from unit properties.
pub fn status(unit: &str) -> Result<()> {
    let conn = user_bus()?;
    let mgr = manager(&conn)?;
    let path = get_unit_path(&mgr, unit)?;
    let proxy = Proxy::new(&conn, SYSTEMD_DEST, path.as_str(), UNIT_IFACE)?;

    let description: String = proxy
        .get_property("Description")
        .unwrap_or_else(|_| unit.to_string());
    println!("{unit} - {description}");

    for (label, property) in [
        ("     Loaded", "LoadState"),
        ("     Active", "ActiveState"),
        ("       Sub", "SubState"),
        ("File state", "UnitFileState"),
        ("    Result", "Result"),
    ] {
        let value: String = proxy
            .get_property(property)
            .unwrap_or_else(|_| "n/a".to_string());
        println!("{label}: {value}");
    }

    let main_pid: u32 = proxy.get_property("MainPID").unwrap_or(0);
    let restarts: u32 = proxy.get_property("NRestarts").unwrap_or(0);
    let exit_code: i32 = proxy.get_property("ExecMainStatus").unwrap_or(0);
    println!("  Main PID: {main_pid}");
    println!("  Restarts: {restarts}");
    println!(" Exit code: {exit_code}");
    println!();
    println!("Recent logs: journalctl --user -u {unit} --no-pager -n 20");
    Ok(())
}

/// `loginctl enable-linger` equivalent: SetUserLinger on the system bus.
pub fn enable_linger() -> Result<()> {
    let conn = system_bus()?;
    login1(&conn)?
        .call::<_, _, ()>("SetUserLinger", &(current_uid()?, true, false))
        .context("SetUserLinger failed (polkit may require authentication)")?;
    Ok(())
}

pub fn lingering_enabled() -> Result<bool> {
    let conn = system_bus()?;
    let mgr = login1(&conn)?;
    let path: OwnedObjectPath = mgr
        .call("GetUser", &(current_uid()?,))
        .context("GetUser failed")?;
    let user = Proxy::new(&conn, LOGIN1_DEST, path.as_str(), LOGIN1_USER_IFACE)
        .context("opening the logind user interface")?;
    let linger: bool = user.get_property("Linger")?;
    Ok(linger)
}
