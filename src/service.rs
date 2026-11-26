//! XSMP-style session management over the user D-Bus bus.
//!
//! Apps register their exact restart recipe; linger persists it as a systemd
//! user unit. Death comes from `NameOwnerChanged`, session end from logind.
//! No polling, no compositor API.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use zbus::fdo::RequestNameFlags;
use zbus::interface;
use zbus::message::Header;
use zbus::zvariant::OwnedObjectPath;

use crate::config::{validate_name, App, Config};
use crate::paths::{safe_component, Paths};
use crate::{systemd, unit};

pub const BUS_NAME: &str = "org.linger.Session1";
pub const OBJECT_PATH: &str = "/org/linger/Session1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub app_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub session: String,
    pub owner: String,
}

#[derive(Debug, Default)]
struct Registry {
    /// Every registration seen since startup; survives app exit.
    entries: HashMap<String, Registration>,
    /// Tokens whose D-Bus owner is still alive.
    live: HashSet<String>,
}

impl Registry {
    fn insert(&mut self, registration: Registration) -> String {
        let token = format!("{}@{}", registration.app_id, registration.owner);
        self.entries.insert(token.clone(), registration);
        self.live.insert(token.clone());
        token
    }

    fn drop_live(&mut self, matches: impl Fn(&Registration) -> bool) -> usize {
        let tokens: Vec<String> = self
            .entries
            .iter()
            .filter(|(token, app)| self.live.contains(token.as_str()) && matches(app))
            .map(|(token, _)| token.clone())
            .collect();
        for token in &tokens {
            self.live.remove(token);
        }
        tokens.len()
    }

    fn list(&self) -> Vec<(String, String, String, Vec<String>)> {
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .filter(|(token, _)| self.live.contains(token.as_str()))
            .map(|(_, app)| {
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
        let mut rows: Vec<_> = self.entries.values().cloned().collect();
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
    fn register_inner(
        &self,
        app_id: &str,
        argv: &[String],
        cwd: &str,
        session: &str,
        owner: &str,
    ) -> Result<String> {
        if app_id.trim().is_empty() || argv.is_empty() || cwd.is_empty() {
            bail!("app_id, argv and cwd must not be empty");
        }

        let session_value = (!session.is_empty()).then(|| session.to_string());
        let desired_name = config_name(app_id, session)?;
        let mut config = Config::load(&self.paths.config_file)?;

        let existing = config
            .apps
            .iter()
            .position(|app| {
                app.app_id.as_deref() == Some(app_id)
                    && app.session.as_deref() == session_value.as_deref()
            })
            .or_else(|| config.apps.iter().position(|app| app.name == desired_name));
        let name = existing
            .map(|pos| config.apps[pos].name.clone())
            .unwrap_or(desired_name);

        let command = argv[0].clone();
        let args = argv[1..].to_vec();
        match existing {
            Some(pos) => {
                let app = &mut config.apps[pos];
                app.app_id = Some(app_id.into());
                app.command = command;
                app.args = args;
                app.working_dir = Some(cwd.into());
                app.session = session_value.clone();
            }
            None => config.apps.push(App {
                name: name.clone(),
                app_id: Some(app_id.into()),
                command,
                args,
                restart: Default::default(),
                restart_sec: 3,
                wait_for_display: true,
                env: Vec::new(),
                working_dir: Some(cwd.into()),
                session: session_value.clone(),
                after: Vec::new(),
            }),
        }
        config.save(&self.paths.config_file)?;

        let app = config.find(&name).context("registered app disappeared")?;
        let env_file = self.paths.env_file_for(session_value.as_deref());
        let binary = std::env::current_exe().context("finding linger executable")?;
        fs::write(
            self.paths.unit_path(&name),
            unit::render(app, &env_file, &binary),
        )
        .context("writing unit file")?;

        let unit_name = Paths::unit_name(&name);
        on_worker_thread(move || {
            systemd::daemon_reload()?;
            systemd::enable(&unit_name)
        })?;

        let registration = Registration {
            app_id: app_id.into(),
            argv: argv.to_vec(),
            cwd: cwd.into(),
            session: session.into(),
            owner: owner.into(),
        };
        let token = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
            .insert(registration);
        println!(
            "linger: registered {app_id} (session {}, unit {name})",
            session_value.as_deref().unwrap_or("default")
        );
        Ok(token)
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
        let owner = header.sender().map(ToString::to_string).unwrap_or_default();
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
        save_snapshot(&self.registry, &self.paths)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    /// Start every configured app that is not active.
    fn restore_session(&self) -> zbus::fdo::Result<()> {
        let units: Vec<String> = Config::load(&self.paths.config_file)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?
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
    conn.request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .context("another linger service already owns the user bus name")?;
    conn.object_server()
        .at(
            OBJECT_PATH,
            Session {
                registry: registry.clone(),
                paths: paths.clone(),
            },
        )
        .context("registering the session interface")?;

    println!("linger: serving {BUS_NAME} at {OBJECT_PATH}");
    spawn_owner_watcher(registry.clone());
    spawn_shutdown_watcher(registry.clone(), paths.clone(), tx);
    spawn_session_watcher(registry, paths);

    rx.recv().context("waiting for shutdown signal")?;
    Ok(())
}

fn watch<F>(label: &'static str, task: F)
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    thread::spawn(move || {
        if let Err(err) = task() {
            eprintln!("linger: {label}: {err}");
        }
    });
}

fn logind_proxy<'a>(conn: &'a zbus::blocking::Connection) -> Result<zbus::blocking::Proxy<'a>> {
    zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .context("opening the logind manager interface")
}

fn spawn_owner_watcher(registry: Arc<Mutex<Registry>>) {
    watch("NameOwnerChanged watcher", move || {
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
                    .drop_live(|app| app.owner == old_owner);
                if dropped > 0 {
                    println!("linger: {old_owner} exited; dropped {dropped} live app(s)");
                }
            }
        }
        Ok(())
    });
}

fn spawn_shutdown_watcher(registry: Arc<Mutex<Registry>>, paths: Paths, tx: mpsc::Sender<()>) {
    watch("shutdown watcher", move || {
        let conn = zbus::blocking::Connection::system()?;
        let proxy = logind_proxy(&conn)?;
        for _ in proxy.receive_signal("PrepareForShutdown")? {
            save_snapshot(&registry, &paths)?;
            let _ = tx.send(());
            break;
        }
        Ok(())
    });
}

fn spawn_session_watcher(registry: Arc<Mutex<Registry>>, paths: Paths) {
    watch("session watcher", move || {
        let conn = zbus::blocking::Connection::system()?;
        let proxy = logind_proxy(&conn)?;
        for signal in proxy.receive_signal("SessionRemoved")? {
            let (session, _path): (String, OwnedObjectPath) = signal.body().deserialize()?;
            let dropped = registry
                .lock()
                .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
                .drop_live(|app| app.session == session);
            save_snapshot(&registry, &paths)?;
            if dropped > 0 {
                println!("linger: session {session} ended; saved {dropped} dropped app(s)");
            }
        }
        Ok(())
    });
}

fn save_snapshot(registry: &Arc<Mutex<Registry>>, paths: &Paths) -> Result<()> {
    let registrations = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?
        .snapshot();
    let path = paths.config_dir.join("session.toml");
    fs::write(
        &path,
        toml::to_string_pretty(&SessionManifest { registrations })?,
    )
    .with_context(|| format!("writing {}", path.display()))
}

fn call(method: &str) -> Result<()> {
    let conn = zbus::blocking::Connection::session().context("connecting to the user bus")?;
    zbus::blocking::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, BUS_NAME)?
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

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(owner: &str, session: &str) -> Registration {
        Registration {
            app_id: "kitty".into(),
            argv: vec!["/usr/bin/kitty".into()],
            cwd: "/home/u".into(),
            session: session.into(),
            owner: owner.into(),
        }
    }

    #[test]
    fn config_names_include_session() {
        assert_eq!(config_name("kitty", "").unwrap(), "kitty");
        assert_eq!(config_name("kitty", "3").unwrap(), "kitty-3");
        assert_eq!(
            config_name("org.example.App", "3").unwrap(),
            "org-example-App-3"
        );
        assert_eq!(config_name("!!!", "3").unwrap(), "app-3");
    }

    #[test]
    fn registry_keeps_known_after_owner_exit() {
        let mut registry = Registry::default();
        let token = registry.insert(registration(":1.5", "3"));
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.drop_live(|app| app.owner == ":1.5"), 1);
        assert!(registry.list().is_empty());
        assert_eq!(registry.snapshot().len(), 1);
        assert!(!registry.live.remove(&token));
    }

    #[test]
    fn registry_drops_only_matching_session() {
        let mut registry = Registry::default();
        registry.insert(registration(":1.5", "3"));
        registry.insert(registration(":1.6", "4"));
        assert_eq!(registry.drop_live(|app| app.session == "3"), 1);
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.snapshot().len(), 2);
    }
}
