# linger

XSMP session manager and app persistence for Wayland, backed by `systemd --user`.

`linger` keeps your apps alive across crashes, logouts and reboots. It is a real
XSMP server: cooperating apps (Qt/KDE, GTK, ...) connect over ICE and volunteer
their restart command, just as they would with `ksmserver`. linger turns that
into normal `systemd --user` units. A small D-Bus channel is available for apps
that cannot use XSMP. No compositor-specific IPC, no X11 connection, no polling.

## Why this shape

KDE Plasma solves session restore with `ksmserver`:

- Apps connect over XSMP and volunteer `SmRestartCommand`, `SmUserID`, etc.
- `ksmserver` saves those commands on logout (`saveCurrentSession`) and replays
  them on next login. Geometry is handled separately by KWin.
- There is no polling: apps push state, the manager stores it, logind/session
  events trigger the save.

Wayland has no native session-management protocol. Plasma Wayland still relies
on XSMP through Xwayland, which only helps cooperating KDE apps that find a
`SESSION_MANAGER`. Hyprland, sway, niri and friends have no common protocol for
"list my apps and how to restart them".

`linger` therefore does two things:

1. It runs a real XSMP server on a private Unix socket, advertised through
   `SESSION_MANAGER` exactly like `ksmserver`. XSMP-capable apps register with
   it and send their own `RestartCommand`, `Program`, `CurrentDirectory`,
   `Environment` and `RestartStyleHint`.
2. It publishes a compositor-agnostic channel on the **user D-Bus bus** for apps
   that cannot use XSMP, plus `linger add` for manual pinning.

The two registration paths feed the same config and unit machinery (D-Bus path
shown below; XSMP arrives at the same `linger.service` over its ICE socket):

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

Like KDE, capture is opt-in: an app must speak XSMP, call `Register`, or be
added by hand. The recipe is exact because the app supplies argv and cwd
itself — nothing is guessed from `/proc` or window classes.

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

That creates `~/.config/linger/`, snapshots the Wayland environment, installs an
`environment.d` fragment, enables lingering via logind, and installs/starts
`linger.service` on the user bus.

For apps launched from your shell or compositor (rather than by systemd), copy
the `export SESSION_MANAGER=...` line printed by `linger init` into your login
shell or compositor environment so they can find the XSMP server. `linger
doctor` checks both the socket and `SESSION_MANAGER`.

You can still add keep-alive apps by hand:

```bash
linger add kitty /usr/bin/kitty --app dev
linger add --no-wait-for-display --env RUST_LOG=info lafd \
  ~/Projects/linux-audio-focus-daemon/target/release/lafd
linger up
```

Because app argv is trailing, linger flags must come before the app name.

Check everything:

```bash
linger list
linger status kitty
linger doctor
```

## Commands

| Command | What it does |
|---|---|
| `linger init` | One-time setup: dirs, env snapshot, `SESSION_MANAGER` fragment, `enable-linger`, install `linger.service` |
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
| `linger serve` | Run the D-Bus + XSMP service in the foreground (normally run by systemd) |
| `linger wait-for-display` | Internal `ExecStartPre` helper |

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

## XSMP

`linger serve` also listens on `$XDG_RUNTIME_DIR/linger-xsmp-$UID.sock` (mode
0600) and advertises it as `local/<host>:<path>` through `SESSION_MANAGER`.
That is the standard ICE discovery mechanism: any XSMP client that sees the
variable can connect without being modified or rebuilt.

The handshake is the real ICE protocol: byte-order negotiation,
`ConnectionSetup`/`Reply`, then `ProtocolSetup`/`Reply` for XSMP. Both little-
and big-endian clients are supported. After `RegisterClient`, linger sends the
required initial `SaveYourself` (local, no shutdown, no interaction) and reads
the properties the client sends back:

| Property | Use |
|---|---|
| `RestartCommand` (or `CloneCommand`) | Exact argv for the generated unit |
| `Program` | Fallback argv and app name |
| `CurrentDirectory` | Unit `WorkingDirectory=` |
| `Environment` | `XDG_SESSION_ID` to pick the env snapshot and name the unit |
| `RestartStyleHint` | `3` (`RestartNever`) opts the app out of persistence |

On `SaveYourselfDone`, linger writes the app to `config.toml`, renders and
enables its unit, and records it in `session.toml`. Killing the client (or
logind ending its session) drops the live registration but leaves the unit as a
keep-alive target. `linger save` asks connected XSMP clients to save again via
a local, non-shutdown `SaveYourself`.

`linger init` writes `~/.config/environment.d/90-linger.conf` so apps started by
`systemd --user` inherit the variable. Apps started by a compositor or shell
need it exported in their launch environment.

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
  clients. Like KDE, linger can only auto-capture apps that speak XSMP (or call
  `Register`, or are added by hand). Apps that ignore `SESSION_MANAGER` cannot
  be captured automatically.
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
  xsmp.rs      ICE/XSMP server, property parsing, client bookkeeping
  unit.rs      systemd unit renderer
  wait.rs      Wayland socket readiness helper
  main.rs      command dispatch
```

All systemd and logind interaction goes through `zbus`. There are no
`systemctl` / `loginctl` subprocesses and no parsed command output.

## Roadmap

- [x] Direct D-Bus control via `zbus` (systemd + logind, no subprocesses)
- [x] Real XSMP server over ICE (`SESSION_MANAGER` discovery)
- [x] D-Bus registration channel for non-XSMP apps
- [x] Event-driven session-end save via logind signals
- [ ] `linger edit` to open a single app's config block
- [ ] Per-app startup timeout / `SuccessExitStatus`
- [ ] Optional tmux-resurrect integration for terminal sessions
- [ ] `linger export` / `import` config

## License

GPL-3.0-only
