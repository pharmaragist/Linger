# linger

XSMP-style app persistence for Wayland, backed by `systemd --user`.

`linger` keeps your apps alive across crashes, logouts and reboots. Apps that
opt in register their exact restart recipe over a small D-Bus channel; linger
turns that into normal `systemd --user` units. No compositor-specific IPC, no
X11, no polling.

## Why this shape

KDE Plasma solves session restore with `ksmserver`:

- Apps connect over XSMP and volunteer `SmRestartCommand`, `SmUserID`, etc.
- `ksmserver` saves those commands on logout (`saveCurrentSession`) and replays
  them on next login. Geometry is handled separately by KWin.
- There is no polling: apps push state, the manager stores it, logind/session
  events trigger the save.

Wayland has no native XSMP equivalent. Plasma Wayland still relies on XSMP
through Xwayland, which only helps cooperating KDE apps. Hyprland, sway, niri
and friends have no common protocol for "list my apps and how to restart them".

So `linger` publishes a compositor-agnostic channel on the **user D-Bus bus**:

```
app ──Register(app_id, argv, cwd, session)──▶ linger.service
                                                │
                                                ├──▶ config.toml
                                                └──▶ linger-*.service units
                                                          │
                                                          ▼
                                                  systemd --user
                                                          ▲
                                                          │
logind ──SessionRemoved / PrepareForShutdown──▶ linger.service
                                                (writes session.toml)
```

Like KDE, this is opt-in. Apps must call `Register`, or you add them manually
with `linger add`. Unlike KDE, the recipe is exact because the app supplies
argv and cwd itself — nothing is guessed from `/proc` or window classes.

## Install

```bash
cd ~/Projects/Linger
./install.sh                       # installs to ~/.local/bin/linger
PREFIX=/usr/local ./install.sh     # system-wide (needs root)
./install.sh --init                # install and run `linger init`
```

Or manually:

```bash
cargo build --release
install -Dm755 target/release/linger ~/.local/bin/linger
```

Make sure the install directory is in your `PATH`.

## Quick start

Run this from inside one of your Wayland sessions:

```bash
linger init
```

That creates `~/.config/linger/`, snapshots the Wayland environment, enables
lingering via logind, and installs/starts `linger.service` on the user bus.

You can still add keep-alive apps by hand:

```bash
linger add kitty /usr/bin/kitty --app dev
linger add lafd ~/Projects/linux-audio-focus-daemon/target/release/lafd \
  --no-wait-for-display --env RUST_LOG=info
linger up
```

Check everything:

```bash
linger list
linger status kitty
linger doctor
```

## Commands

| Command | What it does |
|---|---|
| `linger init` | One-time setup: dirs, env snapshot, `enable-linger`, install `linger.service` |
| `linger add <name> <cmd> [args...]` | Add a manual keep-alive app, write its unit, enable and start it |
| `linger remove <name>` | Remove it from config, stop, disable, delete unit |
| `linger list` | Show apps with active/enabled state and session |
| `linger sync` | Regenerate all units, `daemon-reload`, enable them |
| `linger up` | `sync` + start (or restart) everything |
| `linger down` | Stop and disable everything (keeps config) |
| `linger status <name>` | Show unit state from D-Bus properties + journal hint |
| `linger env [--session ID]` | Re-capture `WAYLAND_DISPLAY` / runtime dir / D-Bus address |
| `linger save` | Ask the service to write `session.toml` now |
| `linger restore` | Start every configured app that is not active |
| `linger doctor` | Verify systemd, lingering, Wayland env and `linger.service` |
| `linger serve` | Run the D-Bus service in the foreground (normally run by systemd) |
| `linger wait-for-display` | Internal `ExecStartPre` helper; alias `wait-for-x` kept for old units |

## App registration API

Service: `org.linger.Session1`  
Object: `/org/linger/Session1`  
Interface: `org.linger.Session1`

```xml
<method name="Register">
  <arg name="app_id"  type="s" direction="in" />
  <arg name="argv"    type="as" direction="in" />
  <arg name="cwd"     type="s" direction="in" />
  <arg name="session" type="s" direction="in" />
  <arg name="token"   type="s" direction="out" />
</method>
<method name="Unregister">
  <arg name="token" type="s" direction="in" />
</method>
<method name="List">
  <arg name="rows" type="a(sssas)" direction="out" />
</method>
<method name="SaveSession" />
<method name="RestoreSession" />
```

The caller's unique D-Bus name is recorded as the owner. If that bus name
disappears, linger drops the *live* registration but keeps the app as a
keep-alive target. App death is an event (`NameOwnerChanged`), not a poll.

Try it manually:

```bash
gdbus call --session \
  --dest org.linger.Session1 \
  --object-path /org/linger/Session1 \
  --method org.linger.Session1.Register \
  "'kitty'" "['/usr/bin/kitty', '--app', 'dev']" "'/home/you'" "'3'"
```

`app_id` is a free-form identifier such as `kitty` or `org.gnome.Nautilus`;
dots are sanitized for the systemd unit name. `session` should normally be
`$XDG_SESSION_ID`. It is used to:

- name the unit (`linger-kitty-3.service`) so two Wayland sessions can run the
  same app without collisions;
- select the matching env snapshot (`env.3.conf`) when one exists;
- drop live registrations when logind emits `SessionRemoved`.

`SaveSession` writes `~/.config/linger/session.toml` with every registration
seen since the service started. It is called automatically on logind
`SessionRemoved` and `PrepareForShutdown`.

## Config

Lives at `~/.config/linger/config.toml` (see `config.example.toml`):

```toml
[[apps]]
name = "kitty"
command = "/usr/bin/kitty"
args = ["--app", "dev"]
restart = "always"      # always | on-failure | never
restart_sec = 3
wait_for_display = true
working_dir = "/home/you"
session = "3"
env = ["RUST_LOG=debug"]
after = ["display-manager.service"]

[[apps]]
name = "lafd"
command = "/home/you/Projects/linux-audio-focus-daemon/target/release/lafd"
restart = "always"
wait_for_display = false
env = ["RUST_LOG=info"]
```

`wait_for_x = true` in old configs is still accepted as an alias.

After editing by hand, run `linger up` (or `linger sync` to avoid starting
apps immediately).

Session-wide variables go in `~/.config/linger/env.conf`, generated by
`linger env`. With multiple Wayland sessions, run:

```bash
linger env --session "$XDG_SESSION_ID"
```

from each session. That writes a per-session file (for example `env.3.conf`)
without disturbing the others. Units belonging to that session prefer their
own file and fall back to the global one.

## Generated unit

```ini
# Generated by linger. Edit ~/.config/linger/config.toml, then run `linger sync`.
[Unit]
Description=linger: kitty
After=display-manager.service

[Service]
Type=simple
EnvironmentFile=/home/you/.config/linger/env.3.conf
Environment="LINGER_SESSION=3"
WorkingDirectory=/home/you
ExecStartPre=/home/you/.local/bin/linger wait-for-display
ExecStart=/usr/bin/kitty --app dev
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
```

## Failure modes

| Event | Behaviour |
|---|---|
| App crashes | `Restart=always` / `on-failure` brings it back after `RestartSec` |
| App exits its bus name | Live registration drops; the unit remains enabled |
| You log out | Lingering keeps `systemd --user` (and the units) alive |
| Session ends | logind `SessionRemoved` triggers a final `session.toml` save |
| Reboot | Units start with the user manager and wait for the Wayland socket |
| Compositor restarts | Apps are independent of the compositor; no IPC coupling |

## Caveats

- There is no Wayland protocol that lets a daemon discover and replay arbitrary
  clients. Like KDE, linger can only auto-capture apps that opt in.
- Window positions, workspaces and application-internal state (browser tabs,
  editor undo history) are not restored. KDE handles geometry separately via
  KWin; linger deliberately does not.
- `linger env` must be refreshed when a session gets a new Wayland socket or
  D-Bus address; otherwise `wait-for-display` will eventually time out.
- `Register` writes and enables a unit immediately. Running `linger serve`
  outside the installed user unit may fail if that unit path is not in
  systemd's search path.

## Layout

```
src/
  cli.rs       clap command definitions
  config.rs    TOML config model + validation
  envfile.rs   capture and render systemd EnvironmentFile (Wayland only)
  paths.rs     config/unit/env paths, including per-session env files
  service.rs   org.linger.Session1 D-Bus service + logind event watchers
  systemd.rs   zbus clients for systemd (user bus) + logind (system bus)
  unit.rs      systemd unit renderer
  wait.rs      Wayland socket readiness helper
  main.rs      command dispatch
```

All systemd and logind interaction goes through `zbus`. There are no
`systemctl` / `loginctl` subprocesses and no parsed command output.

## Roadmap

- [x] Direct D-Bus control via `zbus` (systemd + logind, no subprocesses)
- [x] XSMP-style registration channel on the user bus
- [x] Event-driven session-end save via logind signals
- [ ] `linger edit` to open a single app's config block
- [ ] Per-app startup timeout / `SuccessExitStatus`
- [ ] Optional tmux-resurrect integration for terminal sessions
- [ ] `linger export` / `import` config

## License

GPL-3.0-only
