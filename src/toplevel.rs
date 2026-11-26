//! Capture apps that do not speak XSMP or D-Bus by watching Wayland toplevels.
//!
//! `ext_foreign_toplevel_list_v1` is a staged, compositor-neutral protocol: it
//! streams handles for open windows with an `app_id`, a title and an identifier.
//! We map the app id to a `.desktop` file, take its `Exec=` line, and feed the
//! result into the same registration path XSMP uses. `closed` events drop the
//! live registration; the unit stays enabled so the app returns at next login.
//!
//! This is a coverage layer, not a replacement for XSMP: it cannot see argv,
//! cwd or environment, and it cannot ask an app to save its own state.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{Event as HandleEvent, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{
        Event as ListEvent, ExtForeignToplevelListV1, EVT_TOPLEVEL_OPCODE,
    },
};

use crate::service::Session;
use crate::wait;

#[derive(Default)]
struct Window {
    app_id: String,
    identifier: String,
    owner: Option<String>,
}

struct State {
    session: Session,
    windows: HashMap<ObjectId, Window>,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut State,
        _registry: &WlRegistry,
        _event: wl_registry::Event,
        _: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl State {
    fn register(&mut self, id: ObjectId) {
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        if window.owner.is_some() || window.app_id.is_empty() || window.identifier.is_empty() {
            return;
        }
        let owner = format!("toplevel:{}", window.identifier);
        let id_for_log = window.identifier.clone();
        let app_id = window.app_id.clone();
        let argv = match argv_from_desktop(&app_id) {
            Ok(argv) => argv,
            Err(err) => {
                eprintln!("linger: no toplevel recipe for {app_id}: {err:#}");
                return;
            }
        };
        let cwd = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let session = env::var("XDG_SESSION_ID").unwrap_or_default();
        match self
            .session
            .register_inferred(&app_id, &argv, &cwd, &session, &owner)
        {
            Ok(_) => {
                window.owner = Some(owner);
                println!("linger: toplevel {id_for_log} became {app_id}");
            }
            Err(err) => eprintln!("linger: toplevel {app_id} registration failed: {err:#}"),
        }
    }

    fn close(&mut self, id: ObjectId) {
        if let Some(mut window) = self.windows.remove(&id) {
            if let Some(owner) = window.owner.take() {
                let dropped = self.session.drop_live_owner(&owner);
                if dropped > 0 {
                    println!(
                        "linger: toplevel {} closed; dropped {dropped} live app(s)",
                        window.identifier
                    );
                }
            }
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);

    fn event(
        state: &mut State,
        _manager: &ExtForeignToplevelListV1,
        event: ListEvent,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ListEvent::Toplevel { toplevel } => {
                state.windows.insert(toplevel.id(), Window::default());
            }
            ListEvent::Finished => {
                eprintln!("linger: Wayland stopped sending toplevel events");
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut State,
        handle: &ExtForeignToplevelHandleV1,
        event: HandleEvent,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        match event {
            HandleEvent::AppId { app_id } => {
                if let Some(window) = state.windows.get_mut(&id) {
                    window.app_id = app_id;
                }
            }
            HandleEvent::Title { title } => {
                let _ = title;
            }
            HandleEvent::Identifier { identifier } => {
                if let Some(window) = state.windows.get_mut(&id) {
                    window.identifier = identifier;
                }
            }
            HandleEvent::Done => state.register(id),
            HandleEvent::Closed => state.close(id),
            _ => {}
        }
    }
}

/// Watch toplevels in a dedicated thread. Failure here never takes down the
/// XSMP server or the D-Bus service.
pub fn start(session: Session) -> Result<()> {
    thread::Builder::new()
        .name("linger-toplevel".into())
        .spawn(move || {
            if let Err(err) = watch(session) {
                eprintln!("linger: toplevel watcher: {err:#}");
            }
        })?;
    println!("linger: watching Wayland toplevels");
    Ok(())
}

fn watch(session: Session) -> Result<()> {
    if env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!("WAYLAND_DISPLAY is not set; skipping toplevel watcher");
    }
    // systemd --user can start us before the compositor creates its socket.
    wait::wait_for_display(60)?;

    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).context("initializing the Wayland registry")?;
    let _manager: ExtForeignToplevelListV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .context("binding ext_foreign_toplevel_list_v1")?;

    let mut state = State {
        session,
        windows: HashMap::new(),
    };
    loop {
        queue
            .blocking_dispatch(&mut state)
            .context("dispatching Wayland events")?;
    }
}

/// Directories searched for `.desktop` files, most specific first.
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = env::var_os("XDG_DATA_HOME") {
        dirs.push(Path::new(&home).join("applications"));
    } else if let Some(home) = env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".local/share/applications"));
    }
    let data_dirs =
        env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in env::split_paths(&data_dirs) {
        dirs.push(dir.join("applications"));
    }
    dirs
}

/// Read `Exec=` from the `[Desktop Entry]` group of an app id's desktop file.
fn desktop_exec(app_id: &str) -> Result<String> {
    for dir in application_dirs() {
        for candidate in [app_id, &app_id.replace('.', "-")] {
            let path = dir.join(format!("{candidate}.desktop"));
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let mut in_group = false;
            for line in raw.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_group = line == "[Desktop Entry]";
                    continue;
                }
                if !in_group {
                    continue;
                }
                if line.starts_with("Hidden=true") || line.starts_with("NoDisplay=true") {
                    bail!("desktop entry {candidate} is hidden");
                }
                if let Some(exec) = line.strip_prefix("Exec=") {
                    if exec.trim().is_empty() {
                        bail!("desktop entry {candidate} has an empty Exec");
                    }
                    return Ok(exec.to_string());
                }
            }
        }
    }
    bail!("no desktop entry found for {app_id}")
}

/// Drop desktop field codes (`%u`, `%F`, ...) while keeping literal `%%`.
fn strip_field_codes(exec: &str) -> String {
    let mut out = String::new();
    let mut chars = exec.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        if chars.next() == Some('%') {
            out.push('%');
        }
    }
    out
}

/// Split an Exec value into argv, honouring quotes and backslash escapes.
fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in exec.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// systemd prefers absolute paths; resolve bare Exec names via PATH.
fn absolute_command(command: &str) -> String {
    if command.starts_with('/') {
        return command.to_string();
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(command);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    command.to_string()
}

/// Best-effort restart recipe inferred from a Wayland app id.
fn argv_from_desktop(app_id: &str) -> Result<Vec<String>> {
    let exec = strip_field_codes(&desktop_exec(app_id)?);
    let mut argv = split_exec(&exec);
    if argv.is_empty() {
        bail!("desktop entry for {app_id} produced no argv");
    }
    argv[0] = absolute_command(&argv[0]);
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_desktop_field_codes() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox ");
        assert_eq!(strip_field_codes("app %F --flag %%s"), "app  --flag %s");
    }

    #[test]
    fn splits_quoted_exec_values() {
        assert_eq!(
            split_exec("/usr/bin/kitty --app 'dev tools'"),
            vec!["/usr/bin/kitty", "--app", "dev tools"]
        );
    }

    #[test]
    fn keeps_absolute_commands() {
        assert_eq!(absolute_command("/usr/bin/firefox"), "/usr/bin/firefox");
    }
}
