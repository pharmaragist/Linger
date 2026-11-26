//! Minimal XSMP session manager for Wayland.
//!
//! XSMP is layered on ICE. This module implements the small part of ICE and
//! XSMP that cooperating clients (Qt/KDE, GTK, xterm, ...) actually use:
//! byte-order negotiation, connection/protocol setup, RegisterClient,
//! SetProperties, SaveYourself and SaveYourselfDone. The RestartCommand and
//! CurrentDirectory volunteered by the client are turned into the same
//! systemd unit that `linger add` produces. No X11 connection is involved.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths::Paths;
use crate::service::Session;
use crate::{service, systemd};

const ICE_BYTE_ORDER: u8 = 1;
/// ICE byte-order values are numeric, unlike X11 core's 'l'/'B' bytes.
const ICE_LSB_FIRST: u8 = 0;
const ICE_MSB_FIRST: u8 = 1;
const ICE_CONNECTION_SETUP: u8 = 2;
const ICE_CONNECTION_REPLY: u8 = 6;
const ICE_PROTOCOL_SETUP: u8 = 7;
const ICE_PROTOCOL_REPLY: u8 = 8;
const ICE_PING: u8 = 9;
const ICE_PING_REPLY: u8 = 10;
const ICE_WANT_TO_CLOSE: u8 = 11;
const ICE_NO_CLOSE: u8 = 12;

const SM_REGISTER_CLIENT: u8 = 1;
const SM_REGISTER_CLIENT_REPLY: u8 = 2;
const SM_SAVE_YOURSELF: u8 = 3;
const SM_SAVE_YOURSELF_REQUEST: u8 = 4;
const SM_INTERACT_REQUEST: u8 = 5;
const SM_INTERACT: u8 = 6;
const SM_INTERACT_DONE: u8 = 7;
const SM_SAVE_YOURSELF_DONE: u8 = 8;
const SM_CONNECTION_CLOSED: u8 = 11;
const SM_SET_PROPERTIES: u8 = 12;
const SM_DELETE_PROPERTIES: u8 = 13;
const SM_GET_PROPERTIES: u8 = 14;
const SM_GET_PROPERTIES_REPLY: u8 = 15;
const SM_SAVE_YOURSELF_PHASE2_REQUEST: u8 = 16;
const SM_SAVE_YOURSELF_PHASE2: u8 = 17;
const SM_SAVE_COMPLETE: u8 = 18;

/// XSMP is the first subprotocol ICE registers in this process.
const OUR_OPCODE: u8 = 1;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

static XSMP: OnceLock<Arc<Manager>> = OnceLock::new();

#[derive(Default, Clone)]
struct Properties {
    program: String,
    argv: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    restart_never: bool,
}

impl Properties {
    fn merge(&mut self, other: Properties) {
        if !other.program.is_empty() {
            self.program = other.program;
        }
        if !other.argv.is_empty() {
            self.argv = other.argv;
        }
        if !other.cwd.is_empty() {
            self.cwd = other.cwd;
        }
        self.restart_never |= other.restart_never;
        for (key, value) in other.env {
            if let Some(slot) = self.env.iter_mut().find(|(name, _)| name == &key) {
                slot.1 = value;
            } else {
                self.env.push((key, value));
            }
        }
    }

    fn env(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Property {
    name: String,
    #[allow(dead_code)]
    kind: String,
    values: Vec<Vec<u8>>,
}

struct ClientHandle {
    writer: Arc<Mutex<UnixStream>>,
}

struct Manager {
    session: Session,
    paths: Paths,
    clients: Mutex<HashMap<u64, ClientHandle>>,
    next_conn: AtomicU64,
    next_seq: AtomicU64,
}

/// Path of the listening socket. Kept under XDG_RUNTIME_DIR with 0600 perms.
pub fn socket_path(paths: &Paths) -> Result<PathBuf> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let uid = systemd::current_uid().unwrap_or(1000);
    let _ = paths;
    Ok(PathBuf::from(runtime).join(format!("linger-xsmp-{uid}.sock")))
}

/// Value to put in `SESSION_MANAGER`.
pub fn env_value(paths: &Paths) -> Result<String> {
    let host = fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into()))
        .trim()
        .to_string();
    let host = if host.is_empty() {
        "localhost".to_string()
    } else {
        host
    };
    Ok(format!("local/{host}:{}", socket_path(paths)?.display()))
}

/// Path of the environment.d fragment that gives systemd-launched apps the
/// `SESSION_MANAGER` value.
pub fn environment_fragment_path(paths: &Paths) -> Result<PathBuf> {
    let base = paths
        .config_dir
        .parent()
        .context("no config parent directory")?;
    Ok(base.join("environment.d/90-linger.conf"))
}

/// Install an environment.d fragment so systemd-started apps find the XSMP
/// server. Graphical sessions launched outside systemd still need the value
/// exported from their login environment; `linger init` prints the line.
pub fn install_environment(paths: &Paths) -> Result<PathBuf> {
    let file = environment_fragment_path(paths)?;
    fs::create_dir_all(file.parent().context("no environment.d parent")?)?;
    let value = env_value(paths)?;
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    fs::write(&file, format!("SESSION_MANAGER=\"{escaped}\"\n"))?;
    Ok(file)
}

/// Ask every connected XSMP client to save its current state.
pub fn checkpoint() {
    let Some(manager) = XSMP.get() else {
        return;
    };
    let ids: Vec<u64> = manager
        .clients
        .lock()
        .map(|clients| clients.keys().copied().collect())
        .unwrap_or_default();
    for id in ids {
        let msg = save_yourself(1, 0, 0, 0);
        if let Err(err) = manager.send(id, &msg) {
            eprintln!("linger: XSMP checkpoint failed for client {id}: {err}");
        }
    }
}

/// Start the listener and hand accepted connections to per-client threads.
pub fn start(session: Session, paths: Paths) -> Result<()> {
    let path = socket_path(&paths)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding XSMP socket {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    let manager = Arc::new(Manager {
        session,
        paths,
        clients: Mutex::new(HashMap::new()),
        next_conn: AtomicU64::new(1),
        next_seq: AtomicU64::new(0),
    });
    let _ = XSMP.set(manager.clone());
    let value = env_value(&manager.paths)?;

    thread::Builder::new()
        .name("linger-xsmp-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let manager = manager.clone();
                        thread::spawn(move || {
                            if let Err(err) = manager.handle(stream) {
                                eprintln!("linger: XSMP client: {err:#}");
                            }
                        });
                    }
                    Err(err) => eprintln!("linger: XSMP accept failed: {err}"),
                }
            }
        })?;

    println!("linger: XSMP listening at {}", path.display());
    println!("linger: set SESSION_MANAGER={value}");
    Ok(())
}

impl Manager {
    fn handle(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let mut reader = stream;

        // ICE requires both sides to announce byte order immediately.
        reader.write_all(&ice_message(0, ICE_BYTE_ORDER, ICE_LSB_FIRST, 0, &[]))?;
        reader.flush()?;
        let first =
            read_first_message(&mut reader).context("reading ICE byte order from XSMP client")?;
        if first[0] != 0 || first[1] != ICE_BYTE_ORDER {
            bail!("expected ICE byte order message");
        }
        let swap = match first[2] {
            ICE_LSB_FIRST => false,
            ICE_MSB_FIRST => true,
            other => bail!("unknown ICE byte order 0x{other:02x}"),
        };
        // One writer per connection: checkpoints and this thread share it, so
        // ICE messages can never interleave on the socket.
        let writer = Arc::new(Mutex::new(reader.try_clone()?));

        let mut opcode = 0u8;
        let mut client_id = String::new();
        let mut registered = false;
        let mut props = Properties::default();
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);

        loop {
            let Some(msg) = read_message(&mut reader, swap)? else {
                break;
            };
            let major = msg[0];
            let minor = msg[1];
            let payload = &msg[8..];

            if major == 0 {
                match minor {
                    ICE_CONNECTION_SETUP => {
                        let index = connection_version_index(&msg, swap)?;
                        send(
                            &writer,
                            &ice_message(
                                0,
                                ICE_CONNECTION_REPLY,
                                index,
                                0,
                                &strings("linger", "0.1.0"),
                            ),
                        )?;
                    }
                    ICE_PROTOCOL_SETUP => {
                        let setup = protocol_setup(&msg, swap)?;
                        if setup.name != "XSMP" {
                            bail!("unsupported ICE protocol {}", setup.name);
                        }
                        opcode = setup.opcode;
                        let reply = ice_message(
                            0,
                            ICE_PROTOCOL_REPLY,
                            setup.version_index,
                            OUR_OPCODE,
                            &strings("linger", "0.1.0"),
                        );
                        send(&writer, &reply)?;
                        self.clients.lock().map_err(lock_err)?.insert(
                            conn_id,
                            ClientHandle {
                                writer: writer.clone(),
                            },
                        );
                    }
                    ICE_PING => send(&writer, &ice_message(0, ICE_PING_REPLY, 0, 0, &[]))?,
                    ICE_WANT_TO_CLOSE => send(&writer, &ice_message(0, ICE_NO_CLOSE, 0, 0, &[]))?,
                    _ => {}
                }
                continue;
            }

            if opcode == 0 || major != opcode {
                continue;
            }

            match minor {
                SM_REGISTER_CLIENT => {
                    let mut rd = Reader::new(payload, swap);
                    let previous_id = decode(&rd.array8()?);
                    let _ = previous_id;
                    client_id = self.new_client_id();
                    let mut reply = Vec::new();
                    put_array8(&mut reply, client_id.as_bytes());
                    send(
                        &writer,
                        &ice_message(OUR_OPCODE, SM_REGISTER_CLIENT_REPLY, 0, 0, &reply),
                    )?;
                    // Required by XSMP for a first registration; harmless on
                    // restart and gives us the restart command again.
                    send(&writer, &save_yourself(1, 0, 0, 0))?;
                    registered = true;
                }
                SM_SET_PROPERTIES => {
                    if registered {
                        props.merge(parse_properties(payload, swap)?);
                        // Capture the volunteered recipe now. Logout checkpoints
                        // can arrive after apps have already disconnected.
                        if let Err(err) = self.persist(&client_id, &props) {
                            eprintln!("linger: XSMP could not persist {client_id}: {err:#}");
                        }
                    }
                }
                SM_DELETE_PROPERTIES => {}
                SM_GET_PROPERTIES => {
                    let mut reply = Vec::new();
                    put_list_array8(&mut reply, &[]);
                    send(
                        &writer,
                        &ice_message(OUR_OPCODE, SM_GET_PROPERTIES_REPLY, 0, 0, &reply),
                    )?;
                }
                SM_SAVE_YOURSELF_PHASE2_REQUEST => {
                    send(
                        &writer,
                        &ice_message(OUR_OPCODE, SM_SAVE_YOURSELF_PHASE2, 0, 0, &[]),
                    )?;
                }
                SM_INTERACT_REQUEST => {
                    send(&writer, &ice_message(OUR_OPCODE, SM_INTERACT, 0, 0, &[]))?;
                }
                SM_INTERACT_DONE => {}
                SM_SAVE_YOURSELF_REQUEST => {
                    if payload.len() >= 4 {
                        send(
                            &writer,
                            &save_yourself(payload[0], payload[1], payload[2], payload[3]),
                        )?;
                    }
                }
                SM_SAVE_YOURSELF_DONE => {
                    if registered {
                        if let Err(err) = self.persist(&client_id, &props) {
                            eprintln!("linger: XSMP could not persist {client_id}: {err:#}");
                        }
                    }
                    send(
                        &writer,
                        &ice_message(OUR_OPCODE, SM_SAVE_COMPLETE, 0, 0, &[]),
                    )?;
                }
                SM_CONNECTION_CLOSED => break,
                _ => {}
            }
        }

        self.clients.lock().map_err(lock_err)?.remove(&conn_id);
        if !client_id.is_empty() {
            self.session.drop_live_owner(&format!("xsmp:{client_id}"));
        }
        Ok(())
    }

    fn persist(&self, client_id: &str, props: &Properties) -> Result<()> {
        let argv = if props.argv.is_empty() && !props.program.is_empty() {
            vec![props.program.clone()]
        } else {
            props.argv.clone()
        };
        if argv.is_empty() {
            return Ok(());
        }
        if props.restart_never {
            return Ok(());
        }
        let program = if props.program.is_empty() {
            argv[0].clone()
        } else {
            props.program.clone()
        };
        let app_id = Path::new(&program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone());
        let cwd = if props.cwd.is_empty() {
            std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
        } else {
            props.cwd.clone()
        };
        let session = props
            .env("XDG_SESSION_ID")
            .map(str::to_string)
            .or_else(|| session_from_env_file(&self.paths.env_file))
            .unwrap_or_default();
        let registered = self.session.register_inner(
            &app_id,
            &argv,
            &cwd,
            &session,
            &format!("xsmp:{client_id}"),
        );
        service::save_snapshot(&self.session)?;
        registered.context("registering XSMP client")?;
        Ok(())
    }

    fn send(&self, id: u64, msg: &[u8]) -> Result<()> {
        let writer = self
            .clients
            .lock()
            .map_err(lock_err)?
            .get(&id)
            .map(|client| client.writer.clone())
            .context("XSMP client is gone")?;
        send(&writer, msg)
    }

    fn new_client_id(&self) -> String {
        new_client_id(&self.next_seq)
    }
}

fn new_client_id(counter: &AtomicU64) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let seq = counter.fetch_add(1, Ordering::Relaxed) % 10_000;
    format!(
        "11{:08X}{:013}1{:010}{:04}",
        0x7F000001u32, millis, pid, seq
    )
}

fn send(writer: &Arc<Mutex<UnixStream>>, msg: &[u8]) -> Result<()> {
    let mut stream = writer.lock().map_err(lock_err)?;
    stream.write_all(msg)?;
    stream.flush()?;
    Ok(())
}

fn lock_err<T>(_: std::sync::PoisonError<T>) -> anyhow::Error {
    anyhow::anyhow!("XSMP state lock poisoned")
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn session_from_env_file(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("XDG_SESSION_ID=") {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn save_yourself(save_type: u8, shutdown: u8, interact: u8, fast: u8) -> Vec<u8> {
    ice_message(
        OUR_OPCODE,
        SM_SAVE_YOURSELF,
        0,
        0,
        &[save_type, shutdown, interact, fast, 0, 0, 0, 0],
    )
}

fn strings(first: &str, second: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_string(&mut out, first);
    put_string(&mut out, second);
    out
}

fn pad_len(len: usize, align: usize) -> usize {
    (align - (len % align)) % align
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len() + pad_len(2 + bytes.len(), 4), 0);
}

fn put_array8(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len() + pad_len(4 + bytes.len(), 8), 0);
}

fn put_list_array8(out: &mut Vec<u8>, values: &[Vec<u8>]) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for value in values {
        put_array8(out, value);
    }
}

/// Build an ICE message little-endian. `extra[0]` and `extra[1]` occupy the
/// two header bytes that individual message types use for their own fields.
fn ice_message(major: u8, minor: u8, extra0: u8, extra1: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![major, minor, extra0, extra1, 0, 0, 0, 0];
    out.extend_from_slice(payload);
    let remainder = pad_len(out.len(), 8);
    out.resize(out.len() + remainder, 0);
    // ICE's length field counts 8-byte units *after* the 8-byte header.
    let words = ((out.len() - 8) / 8) as u32;
    out[4..8].copy_from_slice(&words.to_le_bytes());
    out
}

fn read_u16(bytes: &[u8], swap: bool) -> u16 {
    let array: [u8; 2] = bytes.try_into().unwrap();
    if swap {
        u16::from_be_bytes(array)
    } else {
        u16::from_le_bytes(array)
    }
}

fn read_u32(bytes: &[u8], swap: bool) -> u32 {
    let array: [u8; 4] = bytes.try_into().unwrap();
    if swap {
        u32::from_be_bytes(array)
    } else {
        u32::from_le_bytes(array)
    }
}

fn read_first_message(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut head = [0u8; 8];
    stream.read_exact(&mut head)?;
    let swap = head[2] == ICE_MSB_FIRST;
    let words = read_u32(&head[4..8], swap) as usize;
    // ByteOrder has no payload after the 8-byte header.
    if words != 0 {
        bail!("invalid ICE byte order length {words}");
    }
    Ok(head.to_vec())
}

fn read_message(stream: &mut UnixStream, swap: bool) -> Result<Option<Vec<u8>>> {
    let mut head = [0u8; 8];
    loop {
        match stream.read(&mut head[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        }
    }
    stream.read_exact(&mut head[1..])?;
    let words = read_u32(&head[4..8], swap) as usize;
    if words * 8 > MAX_MESSAGE {
        bail!("invalid ICE message length {words}");
    }
    // Length is data words after the 8-byte header, so total is 8 + words * 8.
    let mut msg = head.to_vec();
    msg.resize(8 + words * 8, 0);
    stream.read_exact(&mut msg[8..])?;
    Ok(Some(msg))
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    swap: bool,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], swap: bool) -> Self {
        Self { data, pos: 0, swap }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos + len;
        let slice = self
            .data
            .get(self.pos..end)
            .context("truncated XSMP message")?;
        self.pos = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(read_u16(self.take(2)?, self.swap))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(read_u32(self.take(4)?, self.swap))
    }

    fn array8(&mut self) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        if len > MAX_MESSAGE {
            bail!("XSMP array too large");
        }
        let bytes = self.take(len)?.to_vec();
        self.take(pad_len(4 + len, 8))?;
        Ok(bytes)
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?.to_vec();
        self.take(pad_len(2 + len, 4))?;
        Ok(decode(&bytes))
    }

    fn list_array8(&mut self) -> Result<Vec<Vec<u8>>> {
        let count = self.u32()? as usize;
        let _unused = self.u32()?;
        if count > 10_000 {
            bail!("too many XSMP values");
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.array8()?);
        }
        Ok(out)
    }

    fn property(&mut self) -> Result<Property> {
        Ok(Property {
            name: decode(&self.array8()?),
            kind: decode(&self.array8()?),
            values: self.list_array8()?,
        })
    }
}

fn parse_properties(body: &[u8], swap: bool) -> Result<Properties> {
    let mut rd = Reader::new(body, swap);
    let count = rd.u32()? as usize;
    let _unused = rd.u32()?;
    if count > 10_000 {
        bail!("too many XSMP properties");
    }
    let mut out = Properties::default();
    for _ in 0..count {
        let property = rd.property()?;
        let values: Vec<String> = property.values.iter().map(|value| decode(value)).collect();
        match property.name.as_str() {
            "Program" => {
                if let Some(value) = values.first() {
                    out.program = value.clone();
                }
            }
            "RestartCommand" | "CloneCommand" => {
                if !values.is_empty() {
                    out.argv = values;
                }
            }
            "CurrentDirectory" => {
                if let Some(value) = values.first() {
                    out.cwd = value.clone();
                }
            }
            "RestartStyleHint" => {
                out.restart_never =
                    values.first().and_then(|v| v.as_bytes().first()).copied() == Some(3);
            }
            "Environment" => {
                let mut index = 0;
                while index + 1 < values.len() {
                    out.env
                        .push((values[index].clone(), values[index + 1].clone()));
                    index += 2;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

struct ProtocolSetup {
    name: String,
    opcode: u8,
    version_index: u8,
}

fn connection_version_index(msg: &[u8], swap: bool) -> Result<u8> {
    let count = msg[2] as usize;
    let auth_count = msg[3] as usize;
    // iceConnectionSetupMsg: 8-byte header, then mustAuthenticate + unused[7].
    let mut rd = Reader::new(&msg[16..], swap);
    let _vendor = rd.string()?;
    let _release = rd.string()?;
    for _ in 0..auth_count {
        let _ = rd.string()?;
    }
    for index in 0..count {
        let major = rd.u16()?;
        let minor = rd.u16()?;
        if major == 1 && minor == 0 {
            return Ok(index as u8);
        }
    }
    bail!("client does not support ICE 1.0")
}

fn protocol_setup(msg: &[u8], swap: bool) -> Result<ProtocolSetup> {
    // iceProtocolSetupMsg: 8-byte header where byte 2 is the opcode the client
    // wants us to use for *its* messages, then versionCount + authCount + unused[6].
    let his_opcode = msg[2];
    let version_count = msg[8] as usize;
    let auth_count = msg[9] as usize;
    let mut rd = Reader::new(&msg[16..], swap);
    let name = rd.string()?;
    let _vendor = rd.string()?;
    let _release = rd.string()?;
    for _ in 0..auth_count {
        let _ = rd.string()?;
    }
    for index in 0..version_count {
        let major = rd.u16()?;
        let minor = rd.u16()?;
        if major == 1 && minor == 0 {
            return Ok(ProtocolSetup {
                name,
                opcode: his_opcode,
                version_index: index as u8,
            });
        }
    }
    bail!("client does not support XSMP 1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ice_length_is_in_eight_byte_words() {
        let msg = ice_message(1, 2, 0, 0, &[1, 2, 3]);
        assert_eq!(msg.len() % 8, 0);
        assert_eq!(read_u32(&msg[4..8], false), ((msg.len() - 8) / 8) as u32);
        assert_eq!(msg[8..11], [1, 2, 3]);
    }

    #[test]
    fn empty_ice_messages_have_zero_length() {
        let ping = ice_message(0, ICE_PING, 0, 0, &[]);
        assert_eq!(ping.len(), 8);
        assert_eq!(read_u32(&ping[4..8], false), 0);

        let byte_order = ice_message(0, ICE_BYTE_ORDER, ICE_LSB_FIRST, 0, &[]);
        assert_eq!(byte_order[2], ICE_LSB_FIRST);
        assert_eq!(read_u32(&byte_order[4..8], false), 0);

        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&ping).unwrap();
        let msg = read_message(&mut reader, false).unwrap().unwrap();
        assert_eq!(msg, ping);

        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&byte_order).unwrap();
        let first = read_first_message(&mut reader).unwrap();
        assert_eq!(first[2], ICE_LSB_FIRST);
    }

    #[test]
    fn array8_round_trip() {
        let mut buf = Vec::new();
        put_array8(&mut buf, b"kitty");
        let mut rd = Reader::new(&buf, false);
        assert_eq!(rd.array8().unwrap(), b"kitty");
        assert_eq!(rd.pos, buf.len());
    }

    #[test]
    fn big_endian_array8_round_trip() {
        let bytes = [
            0, 0, 0, 5, b'k', b'i', b't', b't', b'y', 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut rd = Reader::new(&bytes, true);
        assert_eq!(rd.array8().unwrap(), b"kitty");
    }

    #[test]
    fn parses_properties() {
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        put_array8(&mut body, b"RestartCommand");
        put_array8(&mut body, b"LISTofARRAY8");
        put_list_array8(
            &mut body,
            &[
                b"/usr/bin/kitty".to_vec(),
                b"--app".to_vec(),
                b"dev".to_vec(),
            ],
        );
        put_array8(&mut body, b"CurrentDirectory");
        put_array8(&mut body, b"ARRAY8");
        put_list_array8(&mut body, &[b"/home/u".to_vec()]);

        let props = parse_properties(&body, false).unwrap();
        assert_eq!(props.argv, ["/usr/bin/kitty", "--app", "dev"]);
        assert_eq!(props.cwd, "/home/u");
    }

    #[test]
    fn merge_honors_restart_never() {
        let mut props = Properties::default();
        props.merge(Properties {
            restart_never: true,
            ..Properties::default()
        });
        assert!(props.restart_never);
    }

    #[test]
    fn checkpoint_save_is_local_and_not_shutdown() {
        let msg = save_yourself(1, 0, 0, 0);
        assert_eq!(msg[0], OUR_OPCODE);
        assert_eq!(msg[1], SM_SAVE_YOURSELF);
        assert_eq!(msg[8..12], [1, 0, 0, 0]);
    }

    #[test]
    fn parses_ice_connection_setup_with_prefix() {
        let mut payload = vec![0u8; 8]; // mustAuthenticate + unused[7]
        put_string(&mut payload, "libICE");
        put_string(&mut payload, "1.1.1");
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        let mut msg = vec![0u8; 8];
        msg[2] = 1; // versionCount
        msg[3] = 0; // authCount
        msg.extend_from_slice(&payload);
        let words = ((msg.len() - 8) / 8) as u32;
        msg[4..8].copy_from_slice(&words.to_le_bytes());
        assert_eq!(connection_version_index(&msg, false).unwrap(), 0);
    }

    #[test]
    fn parses_ice_protocol_setup_with_prefix() {
        let mut payload = vec![1u8, 0]; // versionCount, authCount
        payload.resize(8, 0); // unused[6]
        put_string(&mut payload, "XSMP");
        put_string(&mut payload, "libSM");
        put_string(&mut payload, "1.2.6");
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        let mut msg = vec![0u8; 8];
        msg[2] = 7; // the opcode this client will use for XSMP messages
        msg[3] = 0; // mustAuthenticate
        msg.extend_from_slice(&payload);
        let words = ((msg.len() - 8) / 8) as u32;
        msg[4..8].copy_from_slice(&words.to_le_bytes());
        let setup = protocol_setup(&msg, false).unwrap();
        assert_eq!(setup.name, "XSMP");
        assert_eq!(setup.opcode, 7);
        assert_eq!(setup.version_index, 0);
    }

    #[test]
    fn client_id_has_xsmp_shape() {
        let counter = AtomicU64::new(0);
        let id = new_client_id(&counter);
        assert!(id.starts_with("11"));
        assert_eq!(id.len(), 2 + 8 + 13 + 1 + 10 + 4);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn installs_environment_fragment() {
        let base = std::env::temp_dir().join(format!("linger-xsmp-env-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let config_dir = base.join("linger");
        let paths = Paths {
            config_file: config_dir.join("config.toml"),
            env_file: config_dir.join("env.conf"),
            unit_dir: base.join("systemd/user"),
            config_dir,
        };
        let file = install_environment(&paths).unwrap();
        let raw = fs::read_to_string(&file).unwrap();
        assert!(raw.starts_with("SESSION_MANAGER=\""));
        assert!(raw.contains("local/"));
        assert!(raw.contains("linger-xsmp-"));
        fs::remove_dir_all(&base).ok();
    }
}
