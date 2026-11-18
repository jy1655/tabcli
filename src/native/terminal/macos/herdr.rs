//! Herdr: a terminal of an already running Herdr Core (`hc`), reached through the core's own
//! local socket with newline JSON-RPC 2.0 (protocol generation 1). Bridge never starts,
//! restarts or stops a core, and never addresses a terminal it did not create.
//!
//! Every action opens one connection, checks `hello` (generation, the methods it uses and,
//! for an existing surface, the recorded permanent core id) and runs on that same
//! connection, so a core replaced between the identity check and the action is refused.
//! The handle names the address (`t7`, one terminal forever on its core), the socket, the
//! core id, the creation time and the incarnation of the program the core started:
//! Bridge's launch host, which waits for a binding that names its own address and PID
//! before the provider can start. A terminal whose reply was lost therefore starts nothing.
//!
//! Creation passes `no_parent` and `no_notify` (a terminal started from inside Herdr would
//! otherwise end with the caller's terminal and type notes into it), `keep: false` and
//! `rm: false`. Prompts go through `terminal.send` without `force`, which refuses a blocked
//! program; screen reads ask for no history, which could scroll a full-screen program.
//! Status words (OSC 7501) are never request evidence. The socket is the one `hc` uses:
//! `HC_SOCKET`, else `socket` in the config, else `<session>.sock` in the runtime or state
//! directory of `HC_SESSION` (herdr-core 0.1.21 `paths.rs`).
use super::OwnedSurfaceClose;
use super::ownership::{SYSTEM_READS, verified_macos_terminal_owner_with};
use super::{MacTerminalAdapter, MacosOpenMode};
use crate::native::private_to_user::owned_and_not_writable_by_others;
use crate::native::session::Store;
use crate::native::session::close::budget::close_remaining;
use crate::native::terminal::ownership::{
    self, NativeSessionOwner, OwnerGroups, OwnerStop, OwnerStopIntent, ProvenShell,
};
use crate::native::terminal::{
    CloseOutcome, HerdrSurface, RetainedLaunchSurface, TerminalKind, TerminalSendFailure,
    TerminalSendResult, TerminalSession,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
        io::AsRawFd,
        net::UnixStream,
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const HOST_COMMAND: &str = TerminalKind::Herdr.registration().host_command;
const GENERATION: u64 = 1;
const REQUIRED_METHODS: [&str; 6] = [
    "terminal.run",
    "terminal.get",
    "terminal.kill",
    "terminal.rm",
    "terminal.send",
    "terminal.read",
];
/// The labels Bridge gives its terminals: the session, and the launch attempt.
const SESSION_LABEL: &str = "tabcli-session";
const LAUNCH_LABEL: &str = "tabcli-launch";
/// Every terminal also gets a random local name, `tabcli-` and 24 hexadecimal digits (31 of
/// the 32 characters hc allows): a terminal whose creation was not confirmed is read by that
/// name on its own core. `terminal.list` is not used for it, because hc's list also asks
/// every configured remote machine.
const NAME_PREFIX: &str = "tabcli-";
const NAME_ENTROPY: usize = 24;
/// Refusals that herdr-core's `terminal_run` gives before the terminal exists. Anything
/// else (`unavailable`, `internal`, an unknown kind) can come after creation, for example
/// when saving the registry times out or fails.
const RUN_REFUSED_BEFORE_CREATE: [&str; 10] = [
    "invalid_params",
    "name_taken",
    "invalid_template",
    "not_found",
    "machine_unknown",
    "config",
    "spawn_failed",
    "usage",
    "unknown_method",
    "unsupported_generation",
];
/// macOS `sockaddr_un.sun_path` holds 104 bytes including the terminating NUL.
const MAX_SOCKET_PATH: usize = 103;
const MAX_REPLY: usize = 8 * 1024 * 1024;
const MAX_SEND_TEXT: usize = 1024 * 1024;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const CALLER_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
const MAX_ANCESTRY: usize = 64;
/// Error kinds with which the core refuses a send before it types anything.
const SEND_REFUSED_BEFORE_INPUT: [&str; 11] = [
    "blocked",
    "status_mismatch",
    "not_found",
    "finished",
    "invalid_params",
    "config",
    "machine_unknown",
    "needs_target",
    "usage",
    "unknown_method",
    "unsupported_generation",
];

/// One call's failure, by what it proves about the request.
#[derive(Debug)]
pub(in crate::native::terminal) enum CallError {
    /// Nothing of the request reached the core.
    NotSent(anyhow::Error),
    /// The request was written, but no valid reply arrived: the core may have acted.
    Unanswered(anyhow::Error),
    /// The core answered with an error of a stable kind.
    Refused { kind: String, message: String },
}

impl CallError {
    fn into_error(self, method: &str) -> anyhow::Error {
        match self {
            Self::NotSent(error) => error.context(format!("Herdr {method} was not sent")),
            Self::Unanswered(error) => {
                error.context(format!("Herdr {method} was sent but not answered"))
            }
            Self::Refused { kind, message } => {
                anyhow!("Herdr refused {method} ({kind}): {message}")
            }
        }
    }
}

/// One connection to a core: one request, then its one reply line.
pub(in crate::native::terminal) trait Connection {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, CallError>;
}

/// What the adapter reads from this process and the system.
pub(in crate::native::terminal) trait Core {
    /// The socket that `hc` would use from this environment.
    fn endpoint(&self) -> Result<PathBuf>;
    /// A connection to a running core at exactly `socket`; never starts a core.
    fn connect(&self, socket: &Path, deadline: Instant) -> Result<Box<dyn Connection + '_>>;
    fn executable(&self) -> Result<PathBuf>;
    /// The `HC_TERMINAL_ID` of this process.
    fn caller_terminal(&self) -> Option<String>;
    fn current_pid(&self) -> u32;
    /// The parent of a live process; `None` when there is no such process.
    fn parent_pid(&self, pid: u32) -> Result<Option<u32>>;
    /// The birth of a live process; `None` when there is no such process.
    fn process_start(&self, pid: u32) -> Result<Option<(u64, u64)>>;
    fn launch_nonce(&self) -> Result<String>;
    fn pause(&self, deadline: Instant) {
        thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(in crate::native::terminal) struct Installed;

impl Core for Installed {
    fn endpoint(&self) -> Result<PathBuf> {
        resolve_socket(&EndpointInputs::from_env(), &read_config_socket)
    }

    fn connect(&self, socket: &Path, deadline: Instant) -> Result<Box<dyn Connection + '_>> {
        Ok(Box::new(SocketConnection::open(socket, deadline)?))
    }

    fn executable(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed to resolve Agent Bridge executable")
    }

    fn caller_terminal(&self) -> Option<String> {
        std::env::var("HC_TERMINAL_ID")
            .ok()
            .filter(|value| !value.is_empty())
    }

    fn current_pid(&self) -> u32 {
        std::process::id()
    }

    fn parent_pid(&self, pid: u32) -> Result<Option<u32>> {
        Ok(super::process::macos_process_info(pid)?.map(|info| info.parent_pid))
    }

    fn process_start(&self, pid: u32) -> Result<Option<(u64, u64)>> {
        super::process::macos_process_start(pid)
    }

    fn launch_nonce(&self) -> Result<String> {
        let mut bytes = [0_u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

/// The environment that decides where `hc` looks for its core.
#[derive(Clone, Debug, Default)]
pub(super) struct EndpointInputs {
    pub(super) hc_socket: Option<OsString>,
    pub(super) hc_session: Option<String>,
    pub(super) home: Option<OsString>,
    pub(super) xdg_state_home: Option<OsString>,
    pub(super) xdg_config_home: Option<OsString>,
    pub(super) xdg_runtime_dir: Option<OsString>,
    /// `/run/user/<uid>` when that directory exists.
    pub(super) run_user: Option<PathBuf>,
}

impl EndpointInputs {
    fn from_env() -> Self {
        let run_user = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        Self {
            hc_socket: std::env::var_os("HC_SOCKET"),
            hc_session: std::env::var("HC_SESSION").ok(),
            home: std::env::var_os("HOME"),
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            run_user: run_user.is_dir().then_some(run_user),
        }
    }
}

fn nonempty(value: &Option<OsString>) -> Option<&OsStr> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// The socket of the core that `hc` addresses from these inputs. A session name or socket
/// that `hc` itself would refuse, and a configuration it cannot read, are refused here
/// too: Bridge never guesses another endpoint.
pub(super) fn resolve_socket(
    inputs: &EndpointInputs,
    config_socket: &dyn Fn(&Path) -> Result<Option<String>>,
) -> Result<PathBuf> {
    let session = inputs
        .hc_session
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("default");
    if session == "."
        || session == ".."
        || session
            .chars()
            .any(|c| c.is_control() || r#"/\:*?"<>|"#.contains(c))
    {
        bail!("HC_SESSION {session:?} cannot name a Herdr Core");
    }
    let home = || {
        nonempty(&inputs.home)
            .map(PathBuf::from)
            .context("HOME is not set, so the Herdr Core socket cannot be located")
    };
    let state = match nonempty(&inputs.xdg_state_home) {
        Some(state) => PathBuf::from(state),
        None => home()?.join(".local/state"),
    }
    .join("herdr-core")
    .join(session);
    let chosen = match nonempty(&inputs.hc_socket) {
        Some(socket) => Some(PathBuf::from(socket)),
        None => {
            let config = match nonempty(&inputs.xdg_config_home) {
                Some(config) => PathBuf::from(config),
                None => home()?.join(".config"),
            }
            .join("herdr-core")
            .join("config.toml");
            config_socket(&config)?.map(PathBuf::from)
        }
    };
    let socket = match chosen {
        Some(socket) => socket,
        None => {
            let run = match nonempty(&inputs.xdg_runtime_dir) {
                Some(run) => Some(PathBuf::from(run).join("herdr-core")),
                None => inputs.run_user.as_ref().map(|run| run.join("herdr-core")),
            };
            run.unwrap_or(state).join(format!("{session}.sock"))
        }
    };
    if !socket.is_absolute() {
        bail!(
            "the Herdr Core socket {} is not an absolute path",
            socket.display()
        );
    }
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        bail!(
            "the Herdr Core socket {} is longer than this system allows",
            socket.display()
        );
    }
    Ok(socket)
}

/// The `socket` setting of an `hc` configuration file: absent when the file does not exist
/// or does not set it; an error when the file cannot be read or parsed.
fn read_config_socket(path: &Path) -> Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot read the Herdr configuration {}; Bridge does not guess the core's socket",
                    path.display()
                )
            });
        }
    };
    config_socket(&text).with_context(|| {
        format!(
            "cannot parse the Herdr configuration {}; Bridge does not guess the core's socket",
            path.display()
        )
    })
}

pub(super) fn config_socket(text: &str) -> Result<Option<String>> {
    let table: toml::Table = text.parse()?;
    match table.get("socket") {
        None => Ok(None),
        Some(toml::Value::String(socket)) if socket.is_empty() => Ok(None),
        Some(toml::Value::String(socket)) => Ok(Some(socket.clone())),
        Some(_) => bail!("`socket` is not a string"),
    }
}

/// The connection of the installed adapter: the protected socket of a core that runs as
/// this user, with every read and write bounded by the deadline.
struct SocketConnection {
    stream: UnixStream,
    next_id: u64,
    pending: Vec<u8>,
}

fn not_running(socket: &Path) -> anyhow::Error {
    anyhow!(
        "no Herdr Core is running at {}; open Herdr or run `hc server start`, then retry. Bridge does not start or restart a core",
        socket.display()
    )
}

impl SocketConnection {
    fn open(socket: &Path, deadline: Instant) -> Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let metadata = match fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_running(socket));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect Herdr socket {}", socket.display()));
            }
        };
        let directory = socket
            .parent()
            .context("Herdr socket has no parent directory")?;
        let directory = fs::symlink_metadata(directory)
            .with_context(|| format!("cannot inspect {}", directory.display()))?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || !directory.is_dir()
            || !owned_and_not_writable_by_others(&directory, uid)
        {
            bail!(
                "{} is not a protected Herdr Core socket of this user",
                socket.display()
            );
        }
        let stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(not_running(socket));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot connect to {}", socket.display()));
            }
        };
        let mut peer_uid = 0;
        let mut peer_gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot identify the Herdr Core socket's user");
        }
        if peer_uid != uid {
            bail!("the process at {} is not this user's", socket.display());
        }
        if Instant::now() >= deadline {
            bail!("Herdr connection timed out");
        }
        Ok(Self {
            stream,
            next_id: 1,
            pending: Vec::new(),
        })
    }

    fn remaining(deadline: Instant) -> Option<Duration> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }

    fn read_line(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line = self.pending.drain(..=end).collect();
                return Ok(line);
            }
            if self.pending.len() > MAX_REPLY {
                bail!("Herdr reply exceeds {MAX_REPLY} bytes");
            }
            let remaining = Self::remaining(deadline).context("Herdr reply timed out")?;
            self.stream.set_read_timeout(Some(remaining))?;
            let read = match self.stream.read(&mut buffer) {
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    bail!("Herdr reply timed out")
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("lost the Herdr Core connection"),
            };
            if read == 0 {
                bail!("the Herdr Core closed the connection");
            }
            self.pending.extend_from_slice(&buffer[..read]);
        }
    }
}

impl Connection for SocketConnection {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
            .into_bytes();
        request.push(b'\n');
        let mut written = 0;
        while written < request.len() {
            let Some(remaining) = Self::remaining(deadline) else {
                let error = anyhow!("Herdr request timed out");
                return Err(if written == 0 {
                    CallError::NotSent(error)
                } else {
                    CallError::Unanswered(error)
                });
            };
            let result = self
                .stream
                .set_write_timeout(Some(remaining))
                .and_then(|()| self.stream.write(&request[written..]));
            match result {
                Ok(0) => {
                    let error = anyhow!("the Herdr Core stopped reading");
                    return Err(if written == 0 {
                        CallError::NotSent(error)
                    } else {
                        CallError::Unanswered(error)
                    });
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let error = anyhow::Error::new(error).context("cannot write to the Herdr Core");
                    return Err(if written == 0 {
                        CallError::NotSent(error)
                    } else {
                        CallError::Unanswered(error)
                    });
                }
            }
        }
        let line = self.read_line(deadline).map_err(CallError::Unanswered)?;
        parse_reply(&line, id)
    }
}

/// One JSON-RPC reply line for request `id`.
pub(super) fn parse_reply(line: &[u8], id: u64) -> Result<Value, CallError> {
    let reply: Value = serde_json::from_slice(line)
        .map_err(|error| CallError::Unanswered(anyhow!("Herdr reply is not JSON: {error}")))?;
    if reply.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || reply.get("id").and_then(Value::as_u64) != Some(id)
    {
        return Err(CallError::Unanswered(anyhow!(
            "Herdr reply does not answer request {id}"
        )));
    }
    if let Some(error) = reply.get("error") {
        let kind = error
            .pointer("/data/kind")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or_else(|| CallError::Unanswered(anyhow!("Herdr error reply has no kind")))?;
        return Err(CallError::Refused {
            kind: kind.to_owned(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| CallError::Unanswered(anyhow!("Herdr reply has neither result nor error")))
}

/// A connection whose `hello` named this core: generation 1, the methods Bridge uses, and,
/// when one is expected, the recorded core id.
struct Linked<'a> {
    connection: Box<dyn Connection + 'a>,
    socket: PathBuf,
    core_id: String,
}

fn link<'a>(
    core: &'a dyn Core,
    socket: &Path,
    expected_core: Option<&str>,
    deadline: Instant,
) -> Result<Linked<'a>> {
    let mut connection = core.connect(socket, deadline)?;
    let hello = connection
        .call("hello", json!({"generation": GENERATION}), deadline)
        .map_err(|error| error.into_error("hello"))?;
    let core_id = validate_hello(&hello)?;
    if let Some(expected) = expected_core
        && expected != core_id
    {
        bail!(
            "a different Herdr Core (id {core_id}) answers at {}; the recorded core {expected} is not reachable there, so the state of this terminal is unknown",
            socket.display()
        );
    }
    Ok(Linked {
        connection,
        socket: socket.to_path_buf(),
        core_id,
    })
}

/// The core id of a `hello` result that speaks generation 1 and offers Bridge's methods.
pub(super) fn validate_hello(hello: &Value) -> Result<String> {
    if hello.get("generation").and_then(Value::as_u64) != Some(GENERATION) {
        bail!("Herdr Core does not speak protocol generation {GENERATION}");
    }
    hello
        .get("version")
        .and_then(Value::as_str)
        .context("Herdr hello has no version")?;
    let methods: Vec<&str> = hello
        .get("methods")
        .and_then(Value::as_array)
        .context("Herdr hello has no method list")?
        .iter()
        .map(|method| method.as_str().context("Herdr method name is not text"))
        .collect::<Result<_>>()?;
    if let Some(missing) = REQUIRED_METHODS
        .iter()
        .find(|method| !methods.contains(method))
    {
        bail!("Herdr Core does not offer {missing}");
    }
    let id = hello
        .pointer("/core/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Herdr hello has no core id")?;
    Ok(id.to_owned())
}

/// A local terminal address: `t` and lowercase letters or digits. A remote terminal's
/// address (`machine/t7`) is never one.
pub(super) fn valid_address(address: &str) -> bool {
    address.len() <= 32
        && address.strip_prefix('t').is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum State {
    Live,
    Finished,
}

/// What the core reports about one terminal, validated field by field.
#[derive(Clone, Debug)]
pub(super) struct Observed {
    pub(super) address: String,
    pub(super) state: State,
    pub(super) pid: Option<u32>,
    pub(super) argv: Vec<String>,
    pub(super) keep: bool,
    pub(super) created_ms: u64,
    pub(super) launch: Option<String>,
    pub(super) name: Option<String>,
}

pub(super) fn parse_terminal(value: &Value) -> Result<Observed> {
    let address = value
        .get("address")
        .and_then(Value::as_str)
        .filter(|address| valid_address(address))
        .context("Herdr terminal has no local address")?
        .to_owned();
    let state = match value.get("state").and_then(Value::as_str) {
        Some("live") => State::Live,
        Some("finished") => State::Finished,
        other => bail!("Herdr terminal {address} has an unknown state {other:?}"),
    };
    let pid = match value.get("pid") {
        None | Some(Value::Null) => None,
        Some(pid) => Some(
            pid.as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .with_context(|| format!("Herdr terminal {address} has an invalid pid"))?,
        ),
    };
    let argv = value
        .get("argv")
        .and_then(Value::as_array)
        .with_context(|| format!("Herdr terminal {address} has no argv"))?
        .iter()
        .map(|argument| {
            argument
                .as_str()
                .map(str::to_owned)
                .context("Herdr argv is not text")
        })
        .collect::<Result<_>>()?;
    let keep = match value.get("keep") {
        None | Some(Value::Null) => false,
        Some(keep) => keep
            .as_bool()
            .with_context(|| format!("Herdr terminal {address} has an invalid keep"))?,
    };
    let created_ms = value
        .get("created_ms")
        .and_then(Value::as_u64)
        .with_context(|| format!("Herdr terminal {address} has no creation time"))?;
    let launch = value
        .pointer(&format!("/labels/{LAUNCH_LABEL}"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let name = match value.get("name") {
        None | Some(Value::Null) => None,
        Some(name) => Some(
            name.as_str()
                .with_context(|| format!("Herdr terminal {address} has an invalid name"))?
                .to_owned(),
        ),
    };
    Ok(Observed {
        address,
        state,
        pid,
        argv,
        keep,
        created_ms,
        launch,
        name,
    })
}

/// One terminal, by its address or by its local name; the answer must be the terminal asked
/// for (that address, or a terminal that carries that name).
fn get(linked: &mut Linked<'_>, terminal: &str, deadline: Instant) -> Result<Option<Observed>> {
    match linked
        .connection
        .call("terminal.get", json!({"terminal": terminal}), deadline)
    {
        Ok(value) => {
            let observed = parse_terminal(&value)?;
            let answers = if valid_address(terminal) {
                observed.address == terminal
            } else {
                observed.name.as_deref() == Some(terminal)
            };
            if !answers {
                bail!(
                    "Herdr answered for {} instead of {terminal}",
                    observed.address
                );
            }
            Ok(Some(observed))
        }
        Err(CallError::Refused { kind, .. }) if kind == "not_found" => Ok(None),
        Err(error) => Err(error.into_error("terminal.get")),
    }
}

/// The program Bridge asks the core to run: its launch host, given the session directory
/// and the launch command.
pub(super) fn host_program(executable: &Path, directory: &Path, command: &str) -> Vec<String> {
    vec![
        executable.to_string_lossy().into_owned(),
        HOST_COMMAND.into(),
        directory.to_string_lossy().into_owned(),
        crate::native::terminal::host::LAUNCH_COMMAND_FLAG.into(),
        command.into(),
    ]
}

/// The recorded identity of a Herdr handle.
fn target(session: &TerminalSession) -> Result<&HerdrSurface> {
    if session.kind != TerminalKind::Herdr || !valid_address(&session.id) {
        bail!("Herdr handle has no local terminal address");
    }
    let core = session
        .herdr_core()
        .context("Herdr handle has no recorded core")?;
    if core.core_id.is_empty() || !Path::new(&core.socket).is_absolute() {
        bail!("Herdr handle records no usable core");
    }
    Ok(core)
}

/// Whether the reported terminal is still the one this handle created. The argv names the
/// launch host of this session; while it runs, the program is the recorded incarnation. A
/// terminal set to keep could run a replacement program and is refused.
fn require_identity(
    core: &dyn Core,
    observed: &Observed,
    session: &TerminalSession,
    directory: Option<&Path>,
) -> Result<()> {
    let recorded = target(session)?;
    let address = &observed.address;
    let session_directory = match directory {
        Some(directory) => {
            observed.argv.get(2).map(String::as_str) == Some(&*directory.to_string_lossy())
        }
        None => session.managed_session_id.as_deref().is_some_and(|id| {
            observed
                .argv
                .get(2)
                .and_then(|path| Path::new(path).file_name())
                == Some(OsStr::new(id))
        }),
    };
    if observed.argv.len() != 5
        || observed.argv[1] != HOST_COMMAND
        || observed.argv[3] != crate::native::terminal::host::LAUNCH_COMMAND_FLAG
        || !session_directory
        || observed.created_ms != recorded.created_ms
        || observed.pid != Some(recorded.host_pid)
    {
        bail!("Herdr terminal {address} is not the terminal this session created");
    }
    if observed.keep {
        bail!(
            "Herdr terminal {address} is set to keep, so a replacement program can run in it; run `hc keep {address} --off` first"
        );
    }
    if observed.state == State::Live
        && let Some(birth) = core.process_start(recorded.host_pid)?
        && birth
            != (
                recorded.host_start_seconds,
                recorded.host_start_microseconds,
            )
    {
        bail!("the program of Herdr terminal {address} is another process than the one recorded");
    }
    Ok(())
}

/// The live terminal of this handle on its recorded core, on a connection that proved it.
fn live_target<'a>(
    core: &'a dyn Core,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<Linked<'a>> {
    let recorded = target(session)?;
    let mut linked = link(
        core,
        Path::new(&recorded.socket),
        Some(&recorded.core_id),
        deadline,
    )?;
    let observed = get(&mut linked, &session.id, deadline)?
        .with_context(|| format!("Herdr terminal {} no longer exists", session.id))?;
    require_identity(core, &observed, session, None)?;
    if observed.state != State::Live {
        bail!("Herdr terminal {} has finished", session.id);
    }
    if core.process_start(recorded.host_pid)?.is_none() {
        bail!("the program of Herdr terminal {} has ended", session.id);
    }
    Ok(linked)
}

fn created_handle(address: &str, core: HerdrSurface) -> TerminalSession {
    TerminalSession::new(TerminalKind::Herdr, address.to_owned()).with_herdr_core(core)
}

/// What this launch asked the core to create: its program, its launch label and its local
/// name. A terminal matches only when all three are this launch's.
struct Launch<'a> {
    argv: &'a [String],
    nonce: &'a str,
    name: &'a str,
}

impl Launch<'_> {
    fn matches(&self, observed: &Observed) -> bool {
        observed.argv == self.argv
            && observed.launch.as_deref() == Some(self.nonce)
            && observed.name.as_deref() == Some(self.name)
    }
}

/// Creates the terminal whose program is the launch host. When the core may have created
/// it without a usable reply (no reply, an error after creation, no address), the terminal
/// is looked up by this launch's local name on the same core, never by running it again;
/// the host it may have started waits for a binding that names its own address and PID, so
/// it starts nothing on its own.
fn create(
    core: &dyn Core,
    command: &str,
    directory: &Path,
    startup_deadline: Instant,
    cleanup_deadline: Instant,
) -> Result<TerminalSession> {
    let socket = core.endpoint()?;
    let mut linked = link(core, &socket, None, startup_deadline)
        .context("the Herdr terminal was not created")?;
    let argv = host_program(&core.executable()?, directory, command);
    let session_id = directory
        .file_name()
        .and_then(OsStr::to_str)
        .context("session directory has no session id")?;
    let nonce = core.launch_nonce()?;
    if nonce.len() < NAME_ENTROPY || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("the Herdr launch token is not hexadecimal");
    }
    let name = format!("{NAME_PREFIX}{}", &nonce[..NAME_ENTROPY]);
    let launch = Launch {
        argv: &argv,
        nonce: &nonce,
        name: &name,
    };
    let params = json!({
        "argv": argv,
        "name": name,
        "labels": {SESSION_LABEL: session_id, LAUNCH_LABEL: nonce},
        "no_parent": true,
        "no_notify": true,
        "keep": false,
        "rm": false,
    });
    let core_id = linked.core_id.clone();
    let reconcile =
        |cause: anyhow::Error| reconcile(core, &socket, &core_id, &launch, cleanup_deadline, cause);
    let address = match linked
        .connection
        .call("terminal.run", params, startup_deadline)
    {
        Ok(result) => result
            .get("address")
            .and_then(Value::as_str)
            .filter(|address| valid_address(address))
            .map(str::to_owned),
        Err(CallError::NotSent(error)) => {
            return Err(error.context("the Herdr terminal was not created"));
        }
        // Refused before the registry entry exists (herdr-core `terminal_run` and `start`):
        // a spawn the holder did not confirm is removed from the registry, and a program it
        // starts late waits for a binding that never comes.
        Err(CallError::Refused { kind, message })
            if RUN_REFUSED_BEFORE_CREATE.contains(&kind.as_str()) =>
        {
            bail!(
                "the Herdr terminal was not created: Herdr refused terminal.run ({kind}): {message}"
            );
        }
        Err(error) => return Err(reconcile(error.into_error("terminal.run"))),
    };
    let Some(address) = address else {
        return Err(reconcile(anyhow!(
            "terminal.run answered without a local address"
        )));
    };
    match confirm_created(core, &mut linked, &address, &launch, startup_deadline) {
        Ok(surface) => Ok(created_handle(&address, surface)),
        Err(error) => Err(remove_created(
            core,
            &socket,
            &core_id,
            &address,
            &launch,
            cleanup_deadline,
            error,
        )),
    }
}

/// The created terminal, read back on the creating connection: this launch's argv, label
/// and name, live, not kept, and a program whose birth is read while the core still
/// reports it.
fn confirm_created(
    core: &dyn Core,
    linked: &mut Linked<'_>,
    address: &str,
    launch: &Launch<'_>,
    deadline: Instant,
) -> Result<HerdrSurface> {
    let observed = get(linked, address, deadline)?
        .with_context(|| format!("created Herdr terminal {address} is not listed"))?;
    if !launch.matches(&observed) {
        bail!("created Herdr terminal {address} does not carry this launch's program and label");
    }
    if observed.keep || observed.state != State::Live {
        bail!("created Herdr terminal {address} is not a live terminal without keep");
    }
    let pid = observed
        .pid
        .with_context(|| format!("created Herdr terminal {address} reports no program yet"))?;
    let (host_start_seconds, host_start_microseconds) = core
        .process_start(pid)?
        .with_context(|| format!("the program of Herdr terminal {address} already ended"))?;
    let again = get(linked, address, deadline)?
        .with_context(|| format!("created Herdr terminal {address} disappeared"))?;
    if !launch.matches(&again) || again.keep || again.state != State::Live || again.pid != Some(pid)
    {
        bail!("created Herdr terminal {address} changed while it was read");
    }
    Ok(HerdrSurface {
        socket: linked.socket.to_string_lossy().into_owned(),
        core_id: linked.core_id.clone(),
        created_ms: observed.created_ms,
        host_pid: pid,
        host_start_seconds,
        host_start_microseconds,
    })
}

/// After `terminal.run` may have created the terminal without a usable reply: read this
/// launch's local name on the pinned core (a name resolves locally only, to the live
/// terminal that holds it or else the latest finished one), and remove that terminal only
/// when its address, argv, label and name are all this launch's. Anything else fails closed.
fn reconcile(
    core: &dyn Core,
    socket: &Path,
    core_id: &str,
    launch: &Launch<'_>,
    deadline: Instant,
    cause: anyhow::Error,
) -> anyhow::Error {
    let fence = format!(
        "a terminal named {} may remain on Herdr Core {core_id} at {}; its launch host starts nothing without a binding and ends when the launch deadline passes",
        launch.name,
        socket.display()
    );
    let found = link(core, socket, Some(core_id), deadline)
        .and_then(|mut linked| get(&mut linked, launch.name, deadline));
    match found {
        Err(error) => anyhow!(
            "Herdr terminal creation is uncertain: {cause:#}; looking for it failed: {error:#}; {fence}"
        ),
        Ok(None) => anyhow!(
            "Herdr terminal creation is uncertain: {cause:#}; no terminal is named {} on core {core_id}, so none was created or it is already gone; nothing was started",
            launch.name
        ),
        Ok(Some(observed)) if !launch.matches(&observed) || observed.keep => anyhow!(
            "Herdr terminal creation is uncertain: {cause:#}; the name {} resolves to {}, which is not this launch's terminal, so nothing was removed; {fence}",
            launch.name,
            observed.address
        ),
        Ok(Some(observed)) => remove_created(
            core,
            socket,
            core_id,
            &observed.address,
            launch,
            deadline,
            cause.context("Herdr did not confirm terminal.run"),
        ),
    }
}

/// Removes a terminal this launch created but cannot use, and returns the launch error.
/// The first observation must be this launch's terminal, and every later one the same
/// terminal with the same program. When removal fails after that first observation proved
/// a complete identity, the handle is retained so that a later explicit close can finish
/// it under the same checks.
fn remove_created(
    core: &dyn Core,
    socket: &Path,
    core_id: &str,
    address: &str,
    launch: &Launch<'_>,
    deadline: Instant,
    cause: anyhow::Error,
) -> anyhow::Error {
    let mut identity = None;
    let removed = (|| -> Result<()> {
        let mut linked = link(core, socket, Some(core_id), deadline)?;
        let Some(first) = get(&mut linked, address, deadline)? else {
            return Ok(());
        };
        let same = |observed: &Observed| -> Result<()> {
            if !launch.matches(observed)
                || observed.created_ms != first.created_ms
                || observed.pid != first.pid
            {
                bail!("Herdr terminal {address} is no longer this launch's terminal");
            }
            if observed.keep {
                bail!(
                    "Herdr terminal {address} is set to keep, so a replacement program can run in it"
                );
            }
            Ok(())
        };
        same(&first)?;
        if let Some(pid) = first.pid
            && let Ok(Some((seconds, microseconds))) = core.process_start(pid)
        {
            identity = Some(HerdrSurface {
                socket: socket.to_string_lossy().into_owned(),
                core_id: core_id.to_owned(),
                created_ms: first.created_ms,
                host_pid: pid,
                host_start_seconds: seconds,
                host_start_microseconds: microseconds,
            });
        }
        end_and_remove(core, &mut linked, address, deadline, &same).map(|_| ())
    })();
    match (removed, identity) {
        (Ok(()), _) => cause.context(format!(
            "the Herdr terminal {address} this launch created was removed; nothing was started"
        )),
        (Err(error), Some(identity)) => RetainedLaunchSurface::new(
            created_handle(address, identity),
            format!(
                "{cause:#}; Herdr terminal {address} could not be removed: {error:#}; its handle is retained for close"
            ),
        )
        .into(),
        (Err(error), None) => anyhow!(
            "{cause:#}; removing Herdr terminal {address} on core {core_id} failed: {error:#}; its launch host starts nothing without a binding"
        ),
    }
}

/// Ends the program of `address` and removes the finished terminal, on a linked connection.
/// Every step reads the terminal again and checks that observation before it acts, so a
/// repeated call finishes what an earlier one left (a removed terminal is not killed, a
/// finished one is only removed), and a terminal that changed in between is left alone.
/// The core offers no conditional kill or remove: another client of the same user can still
/// change the terminal between the last read and the action.
fn end_and_remove(
    core: &dyn Core,
    linked: &mut Linked<'_>,
    address: &str,
    deadline: Instant,
    same: &dyn Fn(&Observed) -> Result<()>,
) -> Result<CloseOutcome> {
    let mut outcome = CloseOutcome::Missing;
    let mut killed = false;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "Herdr terminal {address} did not end before the close deadline; handle retained"
            );
        }
        let Some(observed) = get(linked, address, deadline)? else {
            return Ok(outcome);
        };
        same(&observed).context("the close was stopped and the handle retained")?;
        match observed.state {
            State::Live if !killed => {
                match linked.connection.call(
                    "terminal.kill",
                    json!({"terminal": address}),
                    deadline,
                ) {
                    Ok(_) => {}
                    Err(CallError::Refused { kind, .. })
                        if kind == "not_found" || kind == "finished" => {}
                    Err(error) => return Err(error.into_error("terminal.kill")),
                }
                killed = true;
                outcome = CloseOutcome::Closed;
                continue;
            }
            State::Live => core.pause(deadline),
            State::Finished => {
                match linked
                    .connection
                    .call("terminal.rm", json!({"terminal": address}), deadline)
                {
                    Ok(_) => {}
                    Err(CallError::Refused { kind, .. }) if kind == "not_found" => {}
                    Err(error) => return Err(error.into_error("terminal.rm")),
                }
                outcome = CloseOutcome::Closed;
            }
        }
    }
}

/// Closes exactly the recorded terminal: absent only when its own core no longer holds it.
fn close_until(
    core: &dyn Core,
    session: &TerminalSession,
    directory: Option<&Path>,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_remaining(deadline)?;
    let recorded = target(session)?;
    let mut linked = link(
        core,
        Path::new(&recorded.socket),
        Some(&recorded.core_id),
        deadline,
    )?;
    let same = |observed: &Observed| require_identity(core, observed, session, directory);
    match get(&mut linked, &session.id, deadline)? {
        None => return Ok(CloseOutcome::Missing),
        Some(observed) => same(&observed)?,
    }
    close_remaining(deadline)?;
    end_and_remove(core, &mut linked, &session.id, deadline, &same)
}

fn surface_present(
    core: &dyn Core,
    session: &TerminalSession,
    directory: &Path,
    timeout: Duration,
) -> Result<bool> {
    let deadline = super::timeout_deadline(timeout)?;
    let recorded = target(session)?;
    let mut linked = link(
        core,
        Path::new(&recorded.socket),
        Some(&recorded.core_id),
        deadline,
    )?;
    match get(&mut linked, &session.id, deadline)? {
        None => Ok(false),
        Some(observed) => {
            require_identity(core, &observed, session, Some(directory))?;
            Ok(true)
        }
    }
}

fn verify_surface(
    core: &dyn Core,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    let deadline = super::timeout_deadline(timeout.unwrap_or(VERIFY_TIMEOUT))?;
    live_target(core, session, deadline).map(|_| ())
}

fn read_screen(core: &dyn Core, session: &TerminalSession, deadline: Instant) -> Result<String> {
    let mut linked = live_target(core, session, deadline)?;
    let read = linked
        .connection
        .call(
            "terminal.read",
            json!({"terminal": session.id, "lines": 0}),
            deadline,
        )
        .map_err(|error| error.into_error("terminal.read"))?;
    read.get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("Herdr screen read has no text")
}

/// Pastes the prompt into the exact terminal and submits it, through the core's own send.
/// A send the core refused before typing is not sent; one it may have started is uncertain.
fn send_file(
    core: &dyn Core,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let text = crate::native::session::RecordReader::at(prompt_path)
        .raw_bytes()
        .context("failed to read the prompt")
        .and_then(|bytes| {
            if bytes.len() > MAX_SEND_TEXT {
                bail!(
                    "the prompt is larger than {MAX_SEND_TEXT} bytes, which Bridge sends to Herdr"
                );
            }
            String::from_utf8(bytes).context("the prompt is not UTF-8 text")
        })
        .map_err(TerminalSendFailure::not_sent)?;
    let mut linked = live_target(core, session, deadline).map_err(TerminalSendFailure::not_sent)?;
    match linked.connection.call(
        "terminal.send",
        json!({"terminal": session.id, "text": text}),
        deadline,
    ) {
        Ok(_) => Ok(()),
        Err(CallError::NotSent(error)) => Err(TerminalSendFailure::not_sent(
            error.context("Herdr terminal.send was not sent"),
        )),
        Err(CallError::Refused { kind, message })
            if SEND_REFUSED_BEFORE_INPUT.contains(&kind.as_str()) =>
        {
            Err(TerminalSendFailure::not_sent(anyhow!(
                "Herdr refused the prompt before typing it ({kind}): {message}"
            )))
        }
        Err(error) => Err(TerminalSendFailure::delivery_uncertain(
            error
                .into_error("terminal.send")
                .context("the prompt may have reached the Herdr terminal; it is not sent again"),
        )),
    }
}

/// Inside Herdr, the caller's `HC_TERMINAL_ID` names a live terminal of the core this
/// environment addresses, and this process runs inside its program. Otherwise the
/// automatic choice of Herdr is refused rather than opening a surface elsewhere.
fn verify_caller(core: &dyn Core, deadline: Instant) -> Result<()> {
    let refuse = |reason: String| {
        anyhow!(
            "{reason}; the automatic choice of Herdr was refused and no other terminal was opened. Run inside Herdr, or choose a terminal with --terminal"
        )
    };
    let address = core
        .caller_terminal()
        .ok_or_else(|| refuse("HC_TERMINAL_ID is not set".into()))?;
    if !valid_address(&address) {
        return Err(refuse(format!(
            "HC_TERMINAL_ID {address:?} is not a local Herdr terminal"
        )));
    }
    let socket = core
        .endpoint()
        .map_err(|error| refuse(format!("{error:#}")))?;
    let mut linked =
        link(core, &socket, None, deadline).map_err(|error| refuse(format!("{error:#}")))?;
    let observed = get(&mut linked, &address, deadline)
        .map_err(|error| refuse(format!("{error:#}")))?
        .ok_or_else(|| {
            refuse(format!(
                "HC_TERMINAL_ID names {address}, which the Herdr Core at {} does not hold",
                socket.display()
            ))
        })?;
    let program = match (observed.state, observed.pid) {
        (State::Live, Some(pid)) => pid,
        _ => {
            return Err(refuse(format!(
                "HC_TERMINAL_ID names {address}, which is no longer running"
            )));
        }
    };
    let mut pid = core.current_pid();
    for _ in 0..MAX_ANCESTRY {
        if pid == program {
            return Ok(());
        }
        match core
            .parent_pid(pid)
            .map_err(|error| refuse(format!("{error:#}")))?
        {
            Some(parent) if parent > 1 && parent != pid => pid = parent,
            _ => break,
        }
    }
    Err(refuse(format!(
        "HC_TERMINAL_ID names Herdr terminal {address}, but this command does not run inside its program"
    )))
}

pub(in crate::native::terminal) fn verify_caller_terminal() -> Result<()> {
    verify_caller(&Installed, super::timeout_deadline(CALLER_TIMEOUT)?)
}

/// Whether `surface` names the Herdr terminal `terminal` whose program is `pid`: the one
/// rule by which the launch host recognizes its own surface.
pub(super) fn binding_names_host(
    surface: &TerminalSession,
    terminal: Option<&str>,
    pid: u32,
) -> bool {
    surface.kind == TerminalKind::Herdr
        && terminal == Some(surface.id.as_str())
        && surface
            .herdr_core()
            .is_some_and(|core| core.host_pid == pid)
}

/// The launch host's check, registered for Herdr: the bound surface names the terminal that
/// Herdr says this process runs in, and this process as that terminal's program.
pub(in crate::native::terminal) fn binding_names_this_host(surface: &TerminalSession) -> bool {
    binding_names_host(
        surface,
        std::env::var("HC_TERMINAL_ID").ok().as_deref(),
        std::process::id(),
    )
}

/// How a user watches or types in the terminal: Herdr itself, or `hc attach`.
pub(in crate::native) fn attach_command(session: &TerminalSession) -> Option<String> {
    let core = target(session).ok()?;
    Some(format!(
        "HC_SOCKET={} hc attach {}",
        crate::native::terminal::shell_quote(OsStr::from_bytes(core.socket.as_bytes())),
        session.id
    ))
}

pub(in crate::native::terminal) struct Adapter<C: Core> {
    core: C,
}
pub(in crate::native::terminal) type InstalledAdapter = Adapter<Installed>;
impl Adapter<Installed> {
    /// The adapter over the running Herdr Core's socket and this machine's processes.
    pub(super) const fn installed() -> Self {
        Self { core: Installed }
    }
}
#[cfg(test)]
impl<C: Core> Adapter<C> {
    pub(super) fn with_core(core: C) -> Self {
        Self { core }
    }
}
impl<C: Core> MacTerminalAdapter for Adapter<C> {
    // Herdr has no window to choose: the open mode does not apply.
    fn open_bound_tab(
        &self,
        _mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: super::BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        super::open_with(
            deadline,
            |startup_deadline| create(&self.core, command, directory, startup_deadline, deadline),
            // The program of the terminal waits for the binding above; nothing is typed.
            |_, _| Ok(()),
            |session, deadline| {
                close_until(&self.core, session, Some(directory), deadline).map(|_| ())
            },
            bind,
            unbind,
        )
    }
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult {
        send_file(&self.core, session, prompt_path, deadline)
    }
    // Herdr names no TTY for a terminal; the program's incarnation proves the surface.
    fn verify_surface(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>> {
        verify_surface(&self.core, session, timeout).map(|()| None)
    }
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        surface_present(&self.core, session, directory, timeout)
    }
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
        read_screen(&self.core, session, deadline)
    }
    // Guarded dialog input is declared unsupported in the registration.
    fn close_session_until(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_until(&self.core, session, Some(directory), deadline)
    }
    fn close_owned_session(
        &self,
        directory: &Path,
        id: &str,
        session: &TerminalSession,
        close: OwnedSurfaceClose,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if close == OwnedSurfaceClose::LiveOwner {
            record_live_owner_intent(directory, id, session, deadline)?;
        }
        close_until(&self.core, session, Some(directory), deadline)
    }
}

// Herdr's part of the owner stop, as for WezTerm: nothing is stopped before the terminal is
// ended, but the intent is recorded first, because ending the terminal ends the owner.
fn record_live_owner_intent(
    directory: &Path,
    id: &str,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<()> {
    ownership::stop_attested_owner(
        &Store::open_unchecked(directory),
        id,
        session,
        OwnerStopIntent::Close,
        OwnerStop {
            attest: || {
                verified_macos_terminal_owner_with(directory, id, session, None, SYSTEM_READS)
            },
            shell: ProvenShell,
            record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
            stop: |_: OwnerGroups<()>| Ok(()),
        },
        deadline,
    )
}

#[cfg(test)]
mod tests;
