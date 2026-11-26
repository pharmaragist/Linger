//! XSMP-style session management over the user D-Bus bus.
//!
//! Apps register themselves with their exact restart recipe (argv, cwd,
//! session); linger persists it as a systemd user unit. Death is noticed via
//! `NameOwnerChanged`, and session end via logind's `SessionRemoved` /
//! `PrepareForShutdown` signals. There is no polling and no compositor API.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use zbus::fdo::RequestNameFlags;
use zbus::interface;
use zbus::message::Header;
use zbus::zvariant::OwnedObjectPath;

use crate::config::{validate_name, App, Config};
use crate::paths::Paths;
use crate::systemd;
use crate::unit;

pub const BUS_NAME: &str = "org.linger.Session1";
pub const OBJECT_PATH: &str = "/org/linger/Session1";
const MANIFEST_FILE: &str = "session.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub app_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub session: String,
    pub owner: String,
}

#[derive(Debug, Default)]
pub struct Registry {
    /// Currently live registrations, keyed by `<app_id>@<unique bus name>`.
    live: HashMap<String, Registration>,
    /// Everything seen since the service started. Owners can disappear while
    /// the keep-alive unit remains, so this is what gets written at session end.
    known: HashMap<String, Registration>,
}

impl Registry {
    fn insert(&mut self, registration: Registration) -> String {
        let token = format!("{}@{}", registration.app_id, registration.owner);
        self.known.insert(token.clone(), registration.clone());
        self.live.insert(token.clone(), registration);
        token
    }

    fn drop_owner(&mut self, owner: &str) -> usize {
        let before = self.live.len();
        self.live.retain(|_, app| app.owner != owner);
        before - self.live.len()
    }

    fn drop_session(&mut self, session: &str) -> usize {
        let before = self.live.len();
        self.live.retain(|_, app| app.session != session);
        before - self.live.len()
    }

    fn list(&self) -> Vec<(String, String, String, Vec<String>)> {
        let mut rows: Vec<_> = self
            .live
            .values()
            .map(|app| {
                (
                    app.app_id.clone(),
                    app.session.clone(),
                    app.owner.clone(),
                    app.argv.clone(),
                )
            })
            .collect();
        rows.sort();
        rows
    }

    fn snapshot(&self) -> Vec<Registration> {
        let mut rows: Vec<_> = self.known.values().cloned().collect();
        rows.sort_by(|a, b| {
            (&a.app_id, &a.session, &a.owner).cmp(&(&b.app_id, &b.session, &b.owner))
        });
        rows
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SessionManifest {
    #[serde(default)]
    registrations: Vec<Registration>,
}

pub struct Session {
    registry: Arc<Mutex<Registry>>,
    paths: Paths,
}

impl Session {
    fn new(registry: Arc<Mutex<Registry>>, paths: Paths) -> Self {
        Self { registry, paths }
    }

    fn register_inner(
        &self,
        app_id: &str,
        argv: &[String],
        cwd: &str,
        session: &str,
        owner: &str,
    ) -> Result<String> {
        if app_id.trim().is_empty() {
            bail!("app_id must not be empty");
        }
        if argv.is_empty() {
            bail!("argv must not be empty");
        }
        if cwd.is_empty() {
            bail!("cwd must not be empty");
        }

        let name = config_name(app_id, session)?;
        let session_value = if session.is_empty() {
            None
        } else {
            Some(session.to_string())
        };

        // Exact registrations override the launch recipe in config. Restart
        // policy and extra env stay under the user's control.
        let mut config = Config::load(&self.paths.config_file)?;
        let command = argv[0].clone();
        let args = argv[1..].to_vec();
        let existing = config
            .apps
            .iter()
            .position(|app| {
                app.app_id.as_deref() == Some(app_id)
                    && app.session.as_deref() == session_value.as_deref()
            })
            .or_else(|| config.apps.iter().position(|app| app.name == name));
        if let Some(pos) = existing {
            let app = &mut config.apps[pos];
            app.command = command;
            app.args = args;
            app.working_dir = Some(cwd.to_string());
            app.session = session_value.clone();
            app.app_id = Some(app_id.to_string());
        } else {
            config.apps.push(App {
                name: name.clone(),
                app_id: Some(app_id.to_string()),
                command,
                args,
                restart: Default::default(),
                restart_sec: 3,
                wait_for_display: true,
                env: Vec::new(),
                working_dir: Some(cwd.to_string()),
                session: session_value.clone(),
                after: Vec::new(),
            })
        }
        config.save(&self.paths.config_file)?;

        let app = config.find(&name).context("registered app disappeared")?;
        let env_file = self.paths.env_file_for(session_value.as_deref());
        let binary = std::env::current_exe().context("finding linger executable")?;
        let content = unit::render(app, &env_file, &binary);
        let path = self.paths.unit_path(&name);
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;

        let unit_name = Paths::unit_name(&name);
        on_worker_thread(move || {
            systemd::daemon_reload()?;
            systemd::enable(&unit_name)
        })?;

        let registration = Registration {
            app_id: app_id.to_string(),
            argv: argv.to_vec(),
            cwd: cwd.to_string(),
            session: session.to_string(),
            owner: owner.to_string(),
        };
        let token = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
            .insert(registration);

        println!(
            "linger: registered {app_id} (session {}, unit {name})",
            if session.is_empty() {
                "default"
            } else {
                session
            }
        );
        Ok(token)
    }

    fn save_manifest(&self) -> Result<()> {
        let registrations = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
            .snapshot();
        let manifest = SessionManifest { registrations };
        let path = self.paths.config_dir.join(MANIFEST_FILE);
        fs::write(&path, toml::to_string_pretty(&manifest)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[interface(name = "org.linger.Session1")]
impl Session {
    /// Register an app with its exact restart recipe. Returns an unregister token.
    fn register(
        &self,
        app_id: String,
        argv: Vec<String>,
        cwd: String,
        session: String,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        let owner = header
            .sender()
            .map(|sender| sender.to_string())
            .unwrap_or_default();
        self.register_inner(&app_id, &argv, &cwd, &session, &owner)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    /// Drop a live registration. The unit stays enabled as a keep-alive target.
    fn unregister(&self, token: String) -> zbus::fdo::Result<()> {
        self.registry
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("registry lock poisoned".into()))?
            .live
            .remove(&token);
        Ok(())
    }

    /// Live registrations as (app_id, session, owner, argv).
    fn list(&self) -> Vec<(String, String, String, Vec<String>)> {
        self.registry
            .lock()
            .map(|registry| registry.list())
            .unwrap_or_default()
    }

    /// Write all known registrations to ~/.config/linger/session.toml.
    fn save_session(&self) -> zbus::fdo::Result<()> {
        self.save_manifest()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    /// Start every configured app that is not active.
    fn restore_session(&self) -> zbus::fdo::Result<()> {
        let config = Config::load(&self.paths.config_file)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        let units: Vec<String> = config
            .apps
            .into_iter()
            .map(|app| Paths::unit_name(&app.name))
            .collect();
        on_worker_thread(move || {
            for unit in units {
                if !systemd::is_active(&unit) {
                    systemd::start(&unit)?;
                }
            }
            Ok(())
        })
        .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }
}

/// Run the D-Bus service until the machine shuts down.
pub fn serve() -> Result<()> {
    let paths = Paths::discover()?;
    fs::create_dir_all(&paths.config_dir)?;
    fs::create_dir_all(&paths.unit_dir)?;
    if !paths.env_file.exists() {
        fs::write(
            &paths.env_file,
            "# Run `linger env` from a Wayland session.\n",
        )?;
    }

    let registry = Arc::new(Mutex::new(Registry::default()));
    let (tx, rx) = mpsc::channel::<()>();

    let conn = zbus::blocking::Connection::session().context("connecting to the user bus")?;
    let reply = conn
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .context("requesting the linger bus name")?;
    if !matches!(
        reply,
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner
    ) {
        bail!("another linger service already owns {BUS_NAME} ({reply})");
    }
    conn.object_server()
        .at(OBJECT_PATH, Session::new(registry.clone(), paths.clone()))
        .context("registering the session interface")?;

    println!("linger: serving {BUS_NAME} at {OBJECT_PATH}");
    spawn_owner_watcher(registry.clone());
    spawn_shutdown_watcher(registry.clone(), paths.clone(), tx.clone());
    spawn_session_watcher(registry, paths);

    let _ = rx.recv();
    Ok(())
}

fn spawn_owner_watcher(registry: Arc<Mutex<Registry>>) {
    thread::spawn(move || {
        let run = || -> Result<()> {
            let conn = zbus::blocking::Connection::session()?;
            let proxy = zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            )?;
            for signal in proxy.receive_signal("NameOwnerChanged")? {
                let (_, old_owner, new_owner): (String, String, String) =
                    signal.body().deserialize()?;
                if new_owner.is_empty() && !old_owner.is_empty() {
                    let dropped = registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
                        .drop_owner(&old_owner);
                    if dropped > 0 {
                        println!(
                            "linger: {old_owner} exited; {dropped} live registration(s) dropped"
                        );
                    }
                }
            }
            Ok(())
        };
        if let Err(err) = run() {
            eprintln!("linger: NameOwnerChanged watcher: {err}");
        }
    });
}

fn spawn_shutdown_watcher(registry: Arc<Mutex<Registry>>, paths: Paths, tx: mpsc::Sender<()>) {
    thread::spawn(move || {
        let run = || -> Result<()> {
            let conn = zbus::blocking::Connection::system().context("connecting to logind")?;
            let proxy = zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )?;
            for _ in proxy
                .receive_signal("PrepareForShutdown")
                .context("subscribing to PrepareForShutdown")?
            {
                save_snapshot(&registry, &paths);
                let _ = tx.send(());
                break;
            }
            Ok(())
        };
        if let Err(err) = run() {
            eprintln!("linger: shutdown watcher: {err}");
        }
    });
}

fn spawn_session_watcher(registry: Arc<Mutex<Registry>>, paths: Paths) {
    thread::spawn(move || {
        let run = || -> Result<()> {
            let conn = zbus::blocking::Connection::system().context("connecting to logind")?;
            let proxy = zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )?;
            for signal in proxy
                .receive_signal("SessionRemoved")
                .context("subscribing to SessionRemoved")?
            {
                let (session, _path): (String, OwnedObjectPath) = signal.body().deserialize()?;
                let dropped = registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
                    .drop_session(&session);
                save_snapshot(&registry, &paths);
                if dropped > 0 {
                    println!("linger: session {session} ended; saved {dropped} dropped app(s)");
                }
            }
            Ok(())
        };
        if let Err(err) = run() {
            eprintln!("linger: session watcher: {err}");
        }
    });
}

fn save_snapshot(registry: &Arc<Mutex<Registry>>, paths: &Paths) {
    let session = Session::new(registry.clone(), paths.clone());
    if let Err(err) = session.save_manifest() {
        eprintln!("linger: final save failed: {err}");
    }
}

fn call(method: &str) -> Result<()> {
    let conn = zbus::blocking::Connection::session().context("connecting to the user bus")?;
    let proxy = zbus::blocking::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, BUS_NAME)
        .with_context(|| format!("connecting to {BUS_NAME}"))?;
    proxy
        .call::<_, _, ()>(method, &())
        .with_context(|| format!("{method}() failed"))?;
    Ok(())
}

pub fn save_via_dbus() -> Result<()> {
    call("SaveSession")
}

pub fn restore_via_dbus() -> Result<()> {
    call("RestoreSession")
}

/// Keep blocking zbus calls out of a D-Bus method handler's executor thread.
fn on_worker_thread<T, F>(task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    thread::spawn(task)
        .join()
        .map_err(|_| anyhow::anyhow!("linger worker thread panicked"))?
}

fn config_name(app_id: &str, session: &str) -> Result<String> {
    let base = safe_component(app_id, "app");
    let suffix = safe_component(session, "");
    let name = if suffix.is_empty() {
        base
    } else {
        format!("{base}-{suffix}")
    };
    validate_name(&name)?;
    Ok(name)
}

fn safe_component(value: &str, empty: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        empty.to_string()
    } else {
        out.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_names_include_session() {
        assert_eq!(config_name("kitty", "").unwrap(), "kitty");
        assert_eq!(config_name("kitty", "3").unwrap(), "kitty-3");
        assert_eq!(
            config_name("org.example.App", "3").unwrap(),
            "org-example-App-3"
        );
        assert_eq!(config_name("kitty", "a/b").unwrap(), "kitty-a-b");
        assert_eq!(config_name("!!!", "3").unwrap(), "app-3");
    }

    #[test]
    fn registry_keeps_known_after_owner_exit() {
        let mut registry = Registry::default();
        let token = registry.insert(Registration {
            app_id: "kitty".into(),
            argv: vec!["/usr/bin/kitty".into()],
            cwd: "/home/u".into(),
            session: "3".into(),
            owner: ":1.5".into(),
        });
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.drop_owner(":1.5"), 1);
        assert!(registry.list().is_empty());
        assert_eq!(registry.snapshot().len(), 1);
        registry.live.remove(&token);
    }
}
