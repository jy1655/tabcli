//! Herdr: a terminal of an already running Herdr Core (`hc`), reached through the core's own
//! local endpoint with newline JSON-RPC 2.0 (protocol generation 1): its protected Unix
//! socket on macOS and Linux (`unix.rs`), its named pipe on Windows (`pipe.rs`). TAB never
//! starts, restarts or stops a core, and never addresses a terminal it did not create.
//!
//! Every action opens one connection, checks `hello` (generation, the methods it uses and,
//! for an existing surface, the recorded permanent core id) and runs on that same
//! connection, so a core replaced between the identity check and the action is refused.
//! The handle names the address (`t7`, one terminal forever on its core), the endpoint, the
//! core id, the creation time and the incarnation of the program the core started:
//! TAB's launch host, which waits for a binding that names its own address and PID
//! before the provider can start. A terminal whose reply was lost therefore starts nothing.
//!
//! On macOS and Linux creation names a verified caller as parent with `outlive_parent`.
//! Without one, and on Windows, it passes `no_parent`.
//! Every launch uses `no_notify`, `keep: false` and `rm: false`. Prompts go through
//! `terminal.send` without `force`, which refuses a blocked program. Screen reads ask for
//! no history, which could scroll a full-screen program.
//! Status words (OSC 7501) are never request evidence. The endpoint is the one `hc` uses:
//! `HC_SOCKET`, else `socket` in the config, else the platform default for `HC_SESSION`
//! (herdr-core 0.1.21 `paths.rs`).
use super::record::HerdrHostBirth;
use crate::native::session::close::budget::close_remaining;
use crate::native::terminal::{
    CloseOutcome, RetainedLaunchSurface, TerminalKind, TerminalSendFailure, TerminalSendResult,
    TerminalSession,
};
use anyhow::{Context, Result, anyhow, bail};
use identity::{CLOSE_STOPPED, Launch, Recorded, Requirement, binding_names_host, target};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

mod identity;
#[cfg(windows)]
mod pipe;
#[cfg(unix)]
mod unix;
// Pure, so that every test platform resolves the Windows endpoint as hc does.
#[cfg(any(windows, test))]
mod windows_endpoint;

#[cfg(windows)]
pub(in crate::native::terminal) use pipe::Installed;
#[cfg(unix)]
pub(in crate::native::terminal) use unix::Installed;

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
/// The labels TAB gives its terminals: the session, and the launch attempt.
const SESSION_LABEL: &str = "tabcli-session";
const LAUNCH_LABEL: &str = "tabcli-launch";
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
    /// The local Herdr caller marker when the terminal environment selects Herdr.
    fn caller_terminal(&self) -> Option<String>;
    fn current_pid(&self) -> u32;
    /// The parent of a live process; `None` when there is no such process.
    fn parent_pid(&self, pid: u32) -> Result<Option<u32>>;
    /// The birth of a live process; `None` when there is no such process.
    fn process_start(&self, pid: u32) -> Result<Option<HerdrHostBirth>>;
    fn launch_nonce(&self) -> Result<String>;
    fn pause(&self, deadline: Instant) {
        thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// The `HC_TERMINAL_ID` of this process: the terminal hc says this process runs in.
fn caller_terminal_from_env() -> Option<String> {
    let address = std::env::var("HC_TERMINAL_ID").ok()?;
    super::herdr_environment(
        !address.is_empty(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
    .then_some(address)
}

/// The session name `hc` derives from `HC_SESSION` (herdr-core `paths.rs`): trimmed,
/// `default` when empty, and refused when it cannot name a folder or a pipe.
fn core_session(raw: Option<&str>) -> Result<&str> {
    let session = raw
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
    Ok(session)
}

fn deadline_after(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .context("terminal automation timeout is too large")
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
                    "cannot read the Herdr configuration {}; TAB does not guess the core's socket",
                    path.display()
                )
            });
        }
    };
    config_socket(&text).with_context(|| {
        format!(
            "cannot parse the Herdr configuration {}; TAB does not guess the core's socket",
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

fn not_running(endpoint: &Path) -> anyhow::Error {
    anyhow!(
        "no Herdr Core is running at {}; open Herdr from the desktop or start hc from a shell outside an agent session, then retry. TAB does not start or restart a core",
        endpoint.display()
    )
}

/// A byte stream of one connection whose every read and write ends by a deadline. A
/// timeout is `TimedOut` or `WouldBlock`. `Ok(0)` from a write means the peer stopped
/// reading, and from a read the end of the stream. A write that ran out of time after part
/// of its bytes reached the peer reports those bytes, so that the request is never
/// reported unsent when the core may have read it.
pub(super) trait Stream {
    fn write_before(&mut self, bytes: &[u8], deadline: Instant) -> std::io::Result<usize>;
    fn read_before(&mut self, buffer: &mut [u8], deadline: Instant) -> std::io::Result<usize>;
}

/// The time left before `deadline`, or a timeout once none is left.
pub(super) fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))
}

fn timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// One request, then its one reply line, at a time over a bounded stream: the framing both
/// installed transports share. A request is unsent only while none of its bytes was
/// written; after that, a missing or invalid reply leaves it unanswered, never unsent.
pub(super) struct Framed<S> {
    stream: S,
    next_id: u64,
    pending: Vec<u8>,
}

impl<S> Framed<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            next_id: 1,
            pending: Vec::new(),
        }
    }
}

impl<S: Stream> Framed<S> {
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
            let read = match self.stream.read_before(&mut buffer, deadline) {
                Ok(read) => read,
                Err(error) if timed_out(&error) => bail!("Herdr reply timed out"),
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

impl<S: Stream> Connection for Framed<S> {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
            .into_bytes();
        request.push(b'\n');
        let lost = |written: usize, error: anyhow::Error| {
            if written == 0 {
                CallError::NotSent(error)
            } else {
                CallError::Unanswered(error)
            }
        };
        let mut written = 0;
        while written < request.len() {
            match self.stream.write_before(&request[written..], deadline) {
                Ok(0) => {
                    return Err(lost(written, anyhow!("the Herdr Core stopped reading")));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if timed_out(&error) => {
                    return Err(lost(written, anyhow!("Herdr request timed out")));
                }
                Err(error) => {
                    return Err(lost(
                        written,
                        anyhow::Error::new(error).context("cannot write to the Herdr Core"),
                    ));
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

/// A connection whose `hello` named this core: generation 1, the methods TAB uses, and,
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

/// The core id of a `hello` result that speaks generation 1 and offers TAB's methods.
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

/// The program TAB asks the core to run: its launch host, given the session directory
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

/// The live terminal of this handle on its recorded core, on a connection that proved it.
fn live_target<'a>(
    core: &'a dyn Core,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<Linked<'a>> {
    Recorded::open(core, session, Requirement::Live, deadline)?
        .map(|(_, linked)| linked)
        .with_context(|| format!("Herdr terminal {} no longer exists", session.id))
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
    let launch = Launch::new(&argv, &nonce)?;
    let mut params = json!({
        "argv": argv,
        "name": launch.name,
        "labels": {SESSION_LABEL: session_id, LAUNCH_LABEL: launch.nonce},
        "no_notify": true,
        "keep": false,
        "rm": false,
    });
    // Establish ancestry on the connection that will receive terminal.run. An inherited
    // marker or an earlier admission on a different core cannot name a parent here.
    let parent = if cfg!(windows) {
        // The Windows surface launches an independent, parentless core terminal.
        None
    } else {
        match caller_on(core, &mut linked, startup_deadline) {
            Ok(Caller::Verified(parent)) => Some(parent),
            Ok(Caller::Outside(_)) => None,
            Err(_) => {
                // A failed read can leave a partial reply on this connection. No run has
                // been sent. An explicit launch can still proceed without a parent if the
                // same core answers on a fresh connection.
                linked = link(core, &socket, Some(&linked.core_id), startup_deadline)
                    .context("the Herdr terminal was not created")?;
                None
            }
        }
    };
    if let Some(parent) = parent {
        params["parent"] = json!(parent);
        params["outlive_parent"] = json!(true);
    } else {
        params["no_parent"] = json!(true);
    }
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
    launch
        .confirm(core, &mut linked, &address, startup_deadline)
        .map_err(|error| {
            remove_created(
                core,
                &socket,
                &core_id,
                &address,
                &launch,
                cleanup_deadline,
                error,
            )
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
        .and_then(|mut linked| get(&mut linked, &launch.name, deadline));
    match found {
        Err(error) => anyhow!(
            "Herdr terminal creation is uncertain: {cause:#}; looking for it failed: {error:#}; {fence}"
        ),
        Ok(None) => anyhow!(
            "Herdr terminal creation is uncertain: {cause:#}; no terminal is named {} on core {core_id}, so none was created or it is already gone; nothing was started",
            launch.name
        ),
        Ok(Some(observed)) if !launch.matches_unkept(&observed) => anyhow!(
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
    let mut handle = None;
    let removed = (|| -> Result<()> {
        let mut linked = link(core, socket, Some(core_id), deadline)?;
        let Some(first) = get(&mut linked, address, deadline)? else {
            return Ok(());
        };
        let same = |observed: &Observed| launch.check_cleanup(&first, observed);
        same(&first)?;
        handle = launch.recover_handle(core, &linked, &first);
        end_and_remove(core, &mut linked, address, deadline, &same).map(|_| ())
    })();
    match (removed, handle) {
        (Ok(()), _) => cause.context(format!(
            "the Herdr terminal {address} this launch created was removed; nothing was started"
        )),
        (Err(error), Some(handle)) => retain_unremoved(
            handle,
            format!("{cause:#}; Herdr terminal {address} could not be removed: {error:#}"),
        ),
        (Err(error), None) => anyhow!(
            "{cause:#}; removing Herdr terminal {address} on core {core_id} failed: {error:#}; its launch host starts nothing without a binding"
        ),
    }
}

// Only a complete identity, proved during creation, can be handed to the launcher.
fn retain_unremoved(surface: TerminalSession, reason: String) -> anyhow::Error {
    RetainedLaunchSurface::new(
        surface,
        format!("{reason}; returning the confirmed handle to the launcher"),
    )
    .into()
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
        same(&observed).context(CLOSE_STOPPED)?;
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

/// Closes exactly the recorded terminal, proven by `requirement` at every read: absent only
/// when its own core no longer holds it.
fn close_until(
    core: &dyn Core,
    session: &TerminalSession,
    requirement: Requirement<'_>,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_remaining(deadline)?;
    let Some((recorded, mut linked)) = Recorded::open(core, session, requirement, deadline)? else {
        return Ok(CloseOutcome::Missing);
    };
    let same = |observed: &Observed| recorded.check(core, observed);
    close_remaining(deadline)?;
    end_and_remove(core, &mut linked, &session.id, deadline, &same)
}

/// After the native-session owner is verified gone, on every OS: ends the recorded terminal
/// if it still runs (at most one `terminal.kill`) and removes it, once its creation record
/// proves it is the terminal this session created: its core, address, creation time,
/// program and launch label, and while it runs the program's birth. The owner record plays
/// no part. A terminal set to keep, or one that is not this session's, is left alone with its
/// handle retained; only `not_found` from its own core proves it gone.
fn close_created(
    core: &dyn Core,
    session: &TerminalSession,
    directory: &Path,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_until(core, session, Requirement::Created(directory), deadline)
}

fn surface_present(
    core: &dyn Core,
    session: &TerminalSession,
    directory: &Path,
    timeout: Duration,
) -> Result<bool> {
    let deadline = deadline_after(timeout)?;
    Ok(Recorded::open(core, session, Requirement::Bound(directory), deadline)?.is_some())
}

fn verify_surface(
    core: &dyn Core,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    let deadline = deadline_after(timeout.unwrap_or(VERIFY_TIMEOUT))?;
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
                bail!("the prompt is larger than {MAX_SEND_TEXT} bytes, which TAB sends to Herdr");
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

enum Caller {
    Verified(String),
    Outside(String),
}

/// The terminal marker alone can be inherited. Verify that the local terminal holds an
/// ancestor of this process; never send a remote address or a name lookup to the core.
fn caller_on(core: &dyn Core, linked: &mut Linked<'_>, deadline: Instant) -> Result<Caller> {
    let Some(address) = core.caller_terminal() else {
        return Ok(Caller::Outside("HC_TERMINAL_ID is not set".into()));
    };
    if !valid_address(&address) {
        return Ok(Caller::Outside(format!(
            "HC_TERMINAL_ID {address:?} is not a local Herdr terminal"
        )));
    }
    let Some(observed) = get(linked, &address, deadline)? else {
        return Ok(Caller::Outside(format!(
            "HC_TERMINAL_ID names {address}, which the Herdr Core at {} does not hold",
            linked.socket.display()
        )));
    };
    let (State::Live, Some(program)) = (observed.state, observed.pid) else {
        return Ok(Caller::Outside(format!(
            "HC_TERMINAL_ID names {address}, which is no longer running"
        )));
    };
    let mut pid = core.current_pid();
    for _ in 0..MAX_ANCESTRY {
        if pid == program {
            return Ok(Caller::Verified(address));
        }
        match core.parent_pid(pid)? {
            Some(parent) if parent > 1 && parent != pid => pid = parent,
            _ => break,
        }
    }
    Ok(Caller::Outside(format!(
        "HC_TERMINAL_ID names Herdr terminal {address}, but this command does not run inside its program"
    )))
}

/// Automatic selection requires a verified caller before creating any Session records.
fn verify_caller(core: &dyn Core, deadline: Instant) -> Result<()> {
    let verify = || {
        let mut linked = link(core, &core.endpoint()?, None, deadline)?;
        match caller_on(core, &mut linked, deadline)? {
            Caller::Verified(_) => Ok(()),
            Caller::Outside(reason) => bail!(reason),
        }
    };
    verify().map_err(|error| anyhow!(
        "{error:#}; the automatic choice of Herdr was refused and no other terminal was opened. Run inside Herdr, or choose a terminal with --terminal"
    ))
}

pub(in crate::native::terminal) fn verify_caller_terminal() -> Result<()> {
    verify_caller(&Installed, deadline_after(CALLER_TIMEOUT)?)
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

/// How a user watches or types in the terminal: Herdr itself, or `hc attach` given the
/// recorded endpoint, in the platform's own shell (PowerShell on Windows).
pub(in crate::native) fn attach_command(session: &TerminalSession) -> Option<String> {
    let core = target(session).ok()?;
    Some(hc_command(
        &core.socket,
        "attach",
        &session.id,
        cfg!(windows),
    ))
}

// POSIX shells scope an assignment before a command to that command. PowerShell has no such
// form, so the line sets `HC_SOCKET` in its own script block and restores what was there
// (nothing, when it was unset) however `hc` ends, Ctrl+C included.
fn hc_command(endpoint: &str, command: &str, address: &str, powershell: bool) -> String {
    if powershell {
        format!(
            "& {{ $prior = $env:HC_SOCKET; $env:HC_SOCKET = '{}'; try {{ hc {command} {address} }} finally {{ $env:HC_SOCKET = $prior }} }}",
            endpoint.replace('\'', "''")
        )
    } else {
        format!(
            "HC_SOCKET='{}' hc {command} {address}",
            endpoint.replace('\'', "'\"'\"'")
        )
    }
}

pub(in crate::native::terminal) struct Adapter<C: Core> {
    core: C,
}
pub(in crate::native::terminal) type InstalledAdapter = Adapter<Installed>;
impl Adapter<Installed> {
    /// The adapter over the running Herdr Core's endpoint and this machine's processes.
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

/// The surface operations, the same on every platform; each platform's adapter trait reaches
/// them (`PosixTerminalAdapter` on macOS and Linux, `WindowsTerminalAdapter` on Windows).
impl<C: Core> Adapter<C> {
    /// Creates the terminal and binds it before its launch host starts anything. Herdr has
    /// no window to choose. The program of the terminal waits for the binding; nothing is
    /// typed. A failed binding removes exactly the created terminal.
    pub(in crate::native::terminal) fn open(
        &self,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: impl FnOnce(&mut TerminalSession) -> Result<()>,
        unbind: impl FnOnce() -> Result<()>,
    ) -> Result<TerminalSession> {
        super::open_with(
            deadline,
            |startup_deadline| create(&self.core, command, directory, startup_deadline, deadline),
            |_, _| Ok(()),
            |session, deadline| {
                close_until(&self.core, session, Requirement::Bound(directory), deadline)
                    .map(|_| ())
                    .map_err(|error| {
                        retain_unremoved(
                            session.clone(),
                            format!(
                                "Herdr terminal {} could not be removed: {error:#}",
                                session.id
                            ),
                        )
                    })
            },
            bind,
            unbind,
        )
    }

    pub(in crate::native::terminal) fn send(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult {
        send_file(&self.core, session, prompt_path, deadline)
    }

    /// Herdr names no TTY for a terminal; the program's incarnation proves the surface.
    pub(in crate::native::terminal) fn verify(
        &self,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<()> {
        verify_surface(&self.core, session, timeout)
    }

    pub(in crate::native::terminal) fn present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        surface_present(&self.core, session, directory, timeout)
    }

    pub(in crate::native::terminal) fn screen(
        &self,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<String> {
        read_screen(&self.core, session, deadline)
    }

    pub(in crate::native::terminal) fn close(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_until(&self.core, session, Requirement::Bound(directory), deadline)
    }

    pub(in crate::native::terminal) fn close_created(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_created(&self.core, session, directory, deadline)
    }
}

#[cfg(unix)]
mod posix_adapter {
    use super::{Adapter, Core};
    use crate::native::session::Store;
    #[cfg(target_os = "macos")]
    use crate::native::settings::MacosOpenMode;
    use crate::native::terminal::ownership::{
        self, NativeSessionOwner, OwnerGroups, OwnerStop, OwnerStopIntent, ProvenShell,
    };
    use crate::native::terminal::posix::{self, OwnedSurfaceClose, PosixTerminalAdapter};
    use crate::native::terminal::posix_ownership::{
        SYSTEM_READS, verified_posix_terminal_owner_with,
    };
    use crate::native::terminal::{CloseOutcome, TerminalSendResult, TerminalSession};
    use anyhow::Result;
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    impl<C: Core> PosixTerminalAdapter for Adapter<C> {
        // Herdr has no window to choose: the open mode does not apply.
        fn open_bound_tab(
            &self,
            #[cfg(target_os = "macos")] _mode: MacosOpenMode,
            command: &str,
            directory: &Path,
            deadline: Instant,
            bind: posix::BindSurface<'_>,
            unbind: Box<dyn FnOnce() -> Result<()> + '_>,
        ) -> Result<TerminalSession> {
            self.open(command, directory, deadline, bind, unbind)
        }
        fn send_file(
            &self,
            session: &TerminalSession,
            prompt_path: &Path,
            deadline: Instant,
        ) -> TerminalSendResult {
            self.send(session, prompt_path, deadline)
        }
        fn verify_surface(
            &self,
            _directory: &Path,
            session: &TerminalSession,
            timeout: Option<Duration>,
        ) -> Result<Option<String>> {
            self.verify(session, timeout).map(|()| None)
        }
        fn surface_present(
            &self,
            directory: &Path,
            session: &TerminalSession,
            timeout: Duration,
        ) -> Result<bool> {
            self.present(directory, session, timeout)
        }
        fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
            self.screen(session, deadline)
        }
        // Guarded dialog input is declared unsupported in the registration.
        #[cfg(test)]
        fn close_session_until(
            &self,
            directory: &Path,
            session: &TerminalSession,
            deadline: Instant,
        ) -> Result<CloseOutcome> {
            self.close(directory, session, deadline)
        }
        fn close_owned_session(
            &self,
            directory: &Path,
            id: &str,
            session: &TerminalSession,
            close: OwnedSurfaceClose,
            deadline: Instant,
        ) -> Result<CloseOutcome> {
            if close == OwnedSurfaceClose::CreatedSurface {
                return self.close_created(directory, session, deadline);
            }
            if close == OwnedSurfaceClose::LiveOwner {
                record_live_owner_intent(directory, id, session, deadline)?;
            }
            self.close(directory, session, deadline)
        }
    }

    // Herdr's part of the owner stop, as for WezTerm: nothing is stopped before the terminal
    // is ended, but the intent is recorded first, because ending the terminal ends the owner.
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
                    verified_posix_terminal_owner_with(directory, id, session, None, SYSTEM_READS)
                },
                shell: ProvenShell,
                record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
                stop: |_: OwnerGroups<()>| Ok(()),
            },
            deadline,
        )
    }
}

// Compiled into every test build with the Windows dispatch, so that its routing to this
// adapter is tested on every CI target.
#[cfg(any(windows, test))]
mod windows_adapter {
    use super::{Adapter, Core};
    use crate::native::terminal::windows_dispatch::{
        BindSurface, UnbindSurface, WindowsTerminalAdapter,
    };
    use crate::native::terminal::{
        CloseOutcome, TerminalKind, TerminalSendResult, TerminalSession,
    };
    use anyhow::Result;
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    impl<C: Core> WindowsTerminalAdapter for Adapter<C> {
        fn open_bound_tab(
            &self,
            kind: TerminalKind,
            command: &str,
            directory: &Path,
            deadline: Instant,
            bind: BindSurface<'_>,
            unbind: UnbindSurface<'_>,
        ) -> Result<TerminalSession> {
            if kind != TerminalKind::Herdr {
                anyhow::bail!(
                    "the Windows Herdr adapter cannot open {}",
                    kind.display_name()
                );
            }
            self.open(command, directory, deadline, bind, unbind)
        }
        fn send_file(
            &self,
            session: &TerminalSession,
            prompt_path: &Path,
            deadline: Instant,
        ) -> TerminalSendResult {
            self.send(session, prompt_path, deadline)
        }
        fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
            self.screen(session, deadline)
        }
        // Guarded dialog input is declared unsupported in the registration.
        fn verify_surface(
            &self,
            session: &TerminalSession,
            timeout: Option<Duration>,
        ) -> Result<()> {
            self.verify(session, timeout)
        }
        fn close_session_until(
            &self,
            directory: &Path,
            session: &TerminalSession,
            deadline: Instant,
        ) -> Result<CloseOutcome> {
            self.close(directory, session, deadline)
        }
        fn close_created_surface(
            &self,
            directory: &Path,
            session: &TerminalSession,
            deadline: Instant,
        ) -> Result<CloseOutcome> {
            self.close_created(directory, session, deadline)
        }
        fn surface_present(
            &self,
            directory: &Path,
            session: &TerminalSession,
            timeout: Duration,
        ) -> Result<bool> {
            self.present(directory, session, timeout)
        }
    }
}

#[cfg(test)]
mod tests;
