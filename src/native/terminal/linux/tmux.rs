//! tmux: one tmux server per managed session, started by Bridge for that launch and ended
//! by its close. The user watches or types by attaching to the server's socket.
//!
//! The handle names the pane (`id`, `%N`), its window (`tab_id`, `@N`) and its tmux session
//! (`window_id`, `$N`); `linux_host` is the server's process incarnation, proven when it was
//! created, and `tmux_socket` its socket in the private session directory.
//!
//! Every operation first observes the recorded server incarnation from procfs and only then
//! addresses that server by its socket. Absence is only that incarnation gone (exited,
//! reused, or another boot). A live recorded server whose socket refuses is an error, never
//! absence. A pane id means nothing without its server incarnation.
use super::super::posix::{BindSurface, OwnedSurfaceClose, PosixTerminalAdapter, open_with};
use super::super::posix_ownership::{
    ProcessReads, SYSTEM_READS, verified_posix_terminal_owner_with,
};
use super::super::{
    CloseOutcome, LinuxBirth, LinuxHostIdentity, RetainedLaunchSurface, TerminalKind,
    TerminalSendFailure, TerminalSendResult, TerminalSession,
};
use super::pidfd::VerifiedPidfd;
use super::process::{creation_floor, process_birth};
use crate::native::clock::SystemClock;
use crate::native::process_env::CommandOutputFailure;
use crate::native::session::launch::BindingFacts;
use crate::native::session::{RecordReader, Store};
use crate::native::terminal::ownership::{
    self, NativeSessionOwner, OwnerGroups, OwnerStop, OwnerStopIntent, ProvenShell,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    ffi::{OsStr, OsString},
    io::{Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const HOST_COMMAND: &str = TerminalKind::Tmux.registration().host_command;
const BINDING: &BindingFacts = TerminalKind::Tmux.registration().binding();
/// The server socket, in the session directory: Bridge creates that directory private to
/// the user, so no other server can already listen there.
pub(in crate::native) const SOCKET_FILE: &str = "tmux.sock";
/// The tmux session's name. Every server holds only this one session, so the name needs no
/// session id; it must not repeat the Bridge session id, which Claude's agent listing shows
/// next to the tmux session it runs in, so a recipient name would be listed twice.
pub(in crate::native) const SESSION_NAME: &str = "agent-bridge";
const COLUMNS: &str = "200";
const ROWS: &str = "50";
/// The oldest tmux whose flags this adapter was written against (`new-session -x/-y -P -F`
/// with a detached session, `load-buffer -b <name> -`, `paste-buffer -d -r`).
pub(in crate::native) const MINIMUM_VERSION: (u32, u32) = (3, 2);
/// `sizeof(sockaddr_un.sun_path)` on Linux, the terminating NUL included.
const SUN_PATH: usize = 108;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// Kept back from a delivery's timeout, so that a failed delivery can still delete its buffer.
const BUFFER_CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const CREATED_FORMAT: &str = "#{pid}\t#{session_id}\t#{window_id}\t#{pane_id}\t#{socket_path}";
const PANE_FORMAT: &str = "#{pid}\t#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_tty}";
const SESSION_FORMAT: &str = "#{pid}\t#{session_id}";

/// What the adapter reads and runs on this machine. Production reads the system; a test
/// substitutes every observation and every tmux call.
pub(super) trait Host {
    /// One tmux client call against exactly this socket; `input` is its stdin.
    fn tmux(
        &self,
        socket: &Path,
        arguments: &[&OsStr],
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure>;
    /// No process created after this call has an earlier birth.
    fn creation_floor(&self) -> Result<LinuxBirth>;
    /// The birth of the process that holds this PID now; `None` when no process does.
    fn birth(&self, pid: u32) -> Result<Option<LinuxBirth>>;
    /// The recorded incarnation, held so that it can be signalled; `None` when it is gone.
    fn server(&self, pid: u32, birth: LinuxBirth) -> Result<Option<Box<dyn Server>>>;
    /// Whether anything exists at this path (a symlink is not followed).
    fn path_exists(&self, path: &Path) -> Result<bool>;
    /// This Bridge executable, which the pane runs as its host.
    fn bridge_executable(&self) -> Result<PathBuf>;
}

/// A server process held by its incarnation.
pub(super) trait Server {
    fn terminate(&self) -> Result<()>;
    /// Whether its exit was observed before the deadline.
    fn exited_by(&self, deadline: Instant) -> Result<bool>;
}

pub(super) struct Installed;

impl Host for Installed {
    fn tmux(
        &self,
        socket: &Path,
        arguments: &[&OsStr],
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure> {
        let executable = resolve_tmux().map_err(CommandOutputFailure::not_started)?;
        // A prompt is given on stdin: no other process can list it, and tmux never reads it
        // as an option or expands it as a format.
        let stdin = match input {
            None => Stdio::null(),
            Some(bytes) => {
                let staged = (|| -> Result<std::fs::File> {
                    let mut file = tempfile::tempfile()?;
                    file.write_all(bytes)?;
                    file.seek(SeekFrom::Start(0))?;
                    Ok(file)
                })()
                .context("failed to stage the input of tmux")
                .map_err(CommandOutputFailure::not_started)?;
                Stdio::from(staged)
            }
        };
        let mut command = tmux_command(&executable);
        command.arg("-S").arg(socket).args(arguments);
        crate::native::process_env::command_output_with_stdin_until_classified(
            &mut command,
            stdin,
            deadline,
            "tmux",
        )
    }
    fn creation_floor(&self) -> Result<LinuxBirth> {
        creation_floor()
    }
    fn birth(&self, pid: u32) -> Result<Option<LinuxBirth>> {
        process_birth(pid)
    }
    fn server(&self, pid: u32, birth: LinuxBirth) -> Result<Option<Box<dyn Server>>> {
        Ok(VerifiedPidfd::open(pid, birth)?.map(|pidfd| Box::new(pidfd) as Box<dyn Server>))
    }
    fn path_exists(&self, path: &Path) -> Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
        }
    }
    fn bridge_executable(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed to locate the current executable")
    }
}

impl Server for VerifiedPidfd {
    fn terminate(&self) -> Result<()> {
        self.signal(libc::SIGTERM)
    }
    fn exited_by(&self, deadline: Instant) -> Result<bool> {
        self.wait_until(deadline)
    }
}

/// The tmux of this machine: the first absolute PATH entry that holds an executable `tmux`,
/// as its canonical path.
pub(in crate::native) fn resolve_tmux() -> Result<PathBuf> {
    resolve_tmux_from_path(&std::env::var_os("PATH").context("PATH is not set")?)
}

fn resolve_tmux_from_path(path: &OsStr) -> Result<PathBuf> {
    use crate::native::provider_installation::is_usable_executable;
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join("tmux");
        if is_usable_executable(&candidate) {
            let canonical = candidate
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", candidate.display()))?;
            if is_usable_executable(&canonical) {
                return Ok(canonical);
            }
        }
    }
    bail!(
        "tmux was not found on an absolute PATH entry; install tmux to use managed sessions on Linux"
    )
}

/// A tmux client of this process's environment, with the provider-private variables removed.
/// A caller inside tmux does not hand its own server to this client: the socket is always
/// explicit, and `TMUX_TMPDIR` cannot redirect it.
pub(in crate::native) fn tmux_command(executable: &Path) -> Command {
    let mut command = crate::native::process_env::helper_command(executable);
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR");
    command
}

/// `tmux -V`: `tmux 3.6`, `tmux 3.2a`, `tmux next-3.7`. `None` when the text names no
/// release (a development build), which is not refused.
pub(in crate::native) fn parse_version(text: &str) -> Option<(u32, u32)> {
    let version = text.trim().strip_prefix("tmux ")?;
    let version = version.strip_prefix("next-").unwrap_or(version);
    let (major, rest) = version.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// How the version of the installed tmux compares with what managed sessions use.
pub(in crate::native) enum InstalledVersion {
    Supported(String),
    TooOld(String),
}

/// `tmux -V` of this executable; it reaches no server.
pub(in crate::native) fn installed_version(
    executable: &Path,
    deadline: Instant,
) -> Result<InstalledVersion> {
    let mut command = tmux_command(executable);
    command.arg("-V");
    let output =
        crate::native::process_env::command_output_until(&mut command, deadline, "tmux -V")?;
    if !output.status.success() {
        bail!("tmux -V failed: {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(match parse_version(&text) {
        Some(found) if found < MINIMUM_VERSION => InstalledVersion::TooOld(text),
        _ => InstalledVersion::Supported(text),
    })
}

fn require_version(host: &dyn Host, socket: &Path, deadline: Instant) -> Result<()> {
    let output = host
        .tmux(socket, &[OsStr::new("-V")], None, deadline)
        .map_err(CommandOutputFailure::into_error)?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!("tmux -V failed: {}", output.status);
    }
    if let Some(found) = parse_version(&text)
        && found < MINIMUM_VERSION
    {
        bail!(
            "{} is older than tmux {}.{}, which managed sessions require",
            text.trim(),
            MINIMUM_VERSION.0,
            MINIMUM_VERSION.1
        );
    }
    Ok(())
}

/// The handle's tmux identity, checked before any use.
struct Target<'a> {
    socket: &'a Path,
    server: &'a LinuxHostIdentity,
    session: &'a str,
    window: &'a str,
    pane: &'a str,
}

impl<'a> Target<'a> {
    fn of(handle: &'a TerminalSession) -> Result<Self> {
        if handle.kind != TerminalKind::Tmux {
            bail!("{} is not a tmux surface", handle.kind.display_name());
        }
        let socket = Path::new(
            handle
                .tmux_socket
                .as_deref()
                .context("tmux surface has no recorded socket")?,
        );
        if !socket.is_absolute() || socket.file_name() != Some(OsStr::new(SOCKET_FILE)) {
            bail!(
                "tmux surface records an invalid socket: {}",
                socket.display()
            );
        }
        Ok(Self {
            socket,
            server: handle
                .linux_host
                .as_ref()
                .context("tmux surface has no recorded server incarnation")?,
            session: handle
                .window_id
                .as_deref()
                .context("tmux surface has no recorded session")?,
            window: handle
                .tab_id
                .as_deref()
                .context("tmux surface has no recorded window")?,
            pane: &handle.id,
        })
    }

    /// Whether the recorded server incarnation still runs. Its exit is read from the held
    /// incarnation, so a server that ended but is not yet reaped is gone too. Any
    /// observation failure is an error, never absence.
    fn server_live(&self, host: &dyn Host) -> Result<bool> {
        match host.server(self.server.pid, self.server.linux_birth)? {
            None => Ok(false),
            Some(server) => Ok(!server.exited_by(Instant::now())?),
        }
    }

    fn require_server(&self, host: &dyn Host) -> Result<()> {
        if !self.server_live(host)? {
            bail!("the tmux server of this session is gone");
        }
        Ok(())
    }
}

/// One answered tmux call; a call that fails after it started is reported as started.
fn call(
    host: &dyn Host,
    socket: &Path,
    arguments: &[&OsStr],
    input: Option<&[u8]>,
    deadline: Instant,
) -> std::result::Result<String, CommandOutputFailure> {
    // The command follows the server options (`-f <file>`) that only creation passes.
    let name = match arguments {
        [flag, _, command, ..] if *flag == "-f" => command,
        [command, ..] => command,
        [] => OsStr::new("(no command)"),
    }
    .to_string_lossy();
    let output = host.tmux(socket, arguments, input, deadline)?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CommandOutputFailure::started(anyhow!(
            "tmux {name} failed: {}",
            if error.is_empty() {
                output.status.to_string()
            } else {
                error
            }
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        CommandOutputFailure::started(anyhow!("tmux {name} answered with text that is not UTF-8"))
    })
}

fn answer(host: &dyn Host, socket: &Path, arguments: &[&str], deadline: Instant) -> Result<String> {
    let arguments: Vec<&OsStr> = arguments.iter().map(OsStr::new).collect();
    call(host, socket, &arguments, None, deadline).map_err(CommandOutputFailure::into_error)
}

#[derive(Debug, Eq, PartialEq)]
struct Pane {
    session: String,
    window: String,
    pane: String,
    tty: String,
}

/// Every pane of the server at the socket, which must be the recorded server: each line
/// carries the PID of the server that answered.
fn panes(host: &dyn Host, target: &Target<'_>, deadline: Instant) -> Result<Vec<Pane>> {
    let text = answer(
        host,
        target.socket,
        &["list-panes", "-a", "-F", PANE_FORMAT],
        deadline,
    )?;
    text.lines()
        .map(|line| {
            let [pid, session, window, pane, tty] = fields(line)?;
            if pid != target.server.pid.to_string() {
                bail!(
                    "the tmux server answering at {} is not the recorded server {}",
                    target.socket.display(),
                    target.server.pid
                );
            }
            Ok(Pane {
                session: session.to_owned(),
                window: window.to_owned(),
                pane: pane.to_owned(),
                tty: tty.to_owned(),
            })
        })
        .collect()
}

fn fields<const N: usize>(line: &str) -> Result<[&str; N]> {
    let parts: Vec<&str> = line.split('\t').collect();
    parts
        .try_into()
        .map_err(|_| anyhow!("unexpected tmux answer: {line:?}"))
}

/// The recorded pane, exactly once, in its recorded window and session.
fn listed_pane(host: &dyn Host, target: &Target<'_>, deadline: Instant) -> Result<Option<Pane>> {
    let mut matching = panes(host, target, deadline)?
        .into_iter()
        .filter(|pane| pane.pane == target.pane);
    match (matching.next(), matching.next()) {
        (None, _) => Ok(None),
        (Some(pane), None) if pane.session == target.session && pane.window == target.window => {
            Ok(Some(pane))
        }
        _ => bail!(
            "tmux pane {} does not match its recorded window and session",
            target.pane
        ),
    }
}

fn require_pane(host: &dyn Host, target: &Target<'_>, deadline: Instant) -> Result<Pane> {
    target.require_server(host)?;
    listed_pane(host, target, deadline)?
        .with_context(|| format!("tmux pane {} of this session is gone", target.pane))
}

/// The pane program: this Bridge's tmux host, given the session directory and the launch
/// command as separate arguments. With more than one argument tmux runs it without a shell.
pub(super) fn host_program(executable: &Path, directory: &Path, command: &str) -> Vec<OsString> {
    vec![
        executable.as_os_str().to_owned(),
        HOST_COMMAND.into(),
        directory.as_os_str().to_owned(),
        crate::native::terminal::host::LAUNCH_COMMAND_FLAG.into(),
        command.into(),
    ]
}

fn create(
    host: &dyn Host,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<TerminalSession> {
    let socket = directory.join(SOCKET_FILE);
    let socket_text = socket
        .to_str()
        .with_context(|| format!("tmux socket path is not UTF-8: {}", socket.display()))?;
    if socket_text.len() >= SUN_PATH {
        bail!(
            "tmux socket path {socket_text} is {} bytes, more than the {} a Unix socket address holds; use a shorter state directory",
            socket_text.len(),
            SUN_PATH - 1
        );
    }
    if host.path_exists(&socket)? {
        bail!("refusing to start tmux: {socket_text} already exists");
    }
    require_version(host, &socket, deadline)?;
    let program = host_program(&host.bridge_executable()?, directory, command);
    let mut arguments: Vec<&OsStr> = [
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        SESSION_NAME,
        "-x",
        COLUMNS,
        "-y",
        ROWS,
        "-P",
        "-F",
        CREATED_FORMAT,
        "--",
    ]
    .into_iter()
    .map(OsStr::new)
    .collect();
    arguments.extend(program.iter().map(OsString::as_os_str));
    // Every server born at the socket from here on was started by this call.
    let floor = host.creation_floor()?;
    let created = call(host, &socket, &arguments, None, deadline);
    match created {
        Ok(text) => {
            let line = text.lines().next().unwrap_or_default();
            let [pid, session, window, pane, answered_socket] = fields(line)?;
            let pid: u32 = pid
                .parse()
                .with_context(|| format!("tmux answered an invalid server PID: {pid:?}"))?;
            let server = prove_server(host, &socket, pid, floor)?;
            let handle = TerminalSession {
                kind: TerminalKind::Tmux,
                id: pane.to_owned(),
                tab_id: Some(window.to_owned()),
                window_id: Some(session.to_owned()),
                managed_session_id: None,
                windows_process_identity: None,
                wezterm_mux: None,
                linux_host: Some(server),
                tmux_socket: Some(socket_text.to_owned()),
            };
            let proven = (|| {
                if answered_socket != socket_text {
                    bail!("tmux answered from socket {answered_socket}, not {socket_text}");
                }
                prove_single_session(host, &handle, deadline)
            })();
            match proven {
                Ok(()) => Ok(handle),
                Err(error) => Err(end_created(host, handle, error, deadline)),
            }
        }
        // The call may have started a server before it failed: one proven to be born at the
        // socket after the floor is this launch's and is ended; anything else stays, with no
        // authority over it.
        Err(failure) => {
            let error = failure.into_error();
            match residual_server(host, &socket, floor, deadline) {
                Ok(None) => Err(error),
                Ok(Some(server)) => {
                    let handle = TerminalSession {
                        kind: TerminalKind::Tmux,
                        id: String::new(),
                        tab_id: None,
                        window_id: None,
                        managed_session_id: None,
                        windows_process_identity: None,
                        wezterm_mux: None,
                        linux_host: Some(server),
                        tmux_socket: Some(socket_text.to_owned()),
                    };
                    Err(end_created(host, handle, error, deadline))
                }
                Err(residual) => Err(error.context(format!(
                    "a tmux server may remain at {socket_text}; it was not ended: {residual:#}"
                ))),
            }
        }
    }
}

/// The server at the socket is this launch's: its PID's birth is on this boot, no earlier
/// than the floor taken before the creating call.
fn prove_server(
    host: &dyn Host,
    socket: &Path,
    pid: u32,
    floor: LinuxBirth,
) -> Result<LinuxHostIdentity> {
    let birth = host
        .birth(pid)?
        .with_context(|| format!("tmux server {pid} exited during its creation"))?;
    if birth.boot_id != floor.boot_id || birth.starttime_ticks < floor.starttime_ticks {
        bail!(
            "the tmux server {pid} answering at {} was not started by this launch",
            socket.display()
        );
    }
    Ok(LinuxHostIdentity {
        pid,
        linux_birth: birth,
    })
}

fn prove_single_session(
    host: &dyn Host,
    handle: &TerminalSession,
    deadline: Instant,
) -> Result<()> {
    let target = Target::of(handle)?;
    let sessions = answer(
        host,
        target.socket,
        &["list-sessions", "-F", SESSION_FORMAT],
        deadline,
    )?;
    let expected = format!("{}\t{}", target.server.pid, target.session);
    if sessions.lines().collect::<Vec<_>>() != [expected.as_str()] {
        bail!("the new tmux server does not hold exactly its one session");
    }
    let panes = panes(host, &target, deadline)?;
    match panes.as_slice() {
        [pane]
            if pane.pane == target.pane
                && pane.window == target.window
                && pane.session == target.session
                && !pane.tty.is_empty() =>
        {
            Ok(())
        }
        _ => bail!("the new tmux server does not hold exactly its one pane"),
    }
}

fn residual_server(
    host: &dyn Host,
    socket: &Path,
    floor: LinuxBirth,
    deadline: Instant,
) -> Result<Option<LinuxHostIdentity>> {
    if !host.path_exists(socket)? {
        return Ok(None);
    }
    let pid = answer(host, socket, &["display-message", "-p", "#{pid}"], deadline)?;
    let pid: u32 = pid
        .trim()
        .parse()
        .with_context(|| format!("tmux answered an invalid server PID: {pid:?}"))?;
    prove_server(host, socket, pid, floor).map(Some)
}

/// Ends a server this launch proved it created. When its end is not confirmed, the proven
/// handle is retained so that a later close can end it.
fn end_created(
    host: &dyn Host,
    handle: TerminalSession,
    error: anyhow::Error,
    deadline: Instant,
) -> anyhow::Error {
    match end_server(host, &handle, deadline) {
        Ok(_) => error,
        Err(cleanup) => anyhow::Error::new(RetainedLaunchSurface::new(
            handle,
            format!(
                "{error:#}; the tmux server this launch started was not confirmed ended: {cleanup:#}"
            ),
        )),
    }
}

/// Ends the recorded server through its held incarnation. A server already gone is not
/// signalled again. Its socket file is left in place: once the incarnation is gone it grants
/// nothing (every operation proves the incarnation first, and a launch never reuses a session
/// directory), no later check could prove that the path still belongs to it, and the session
/// directory is removed with the session.
fn end_server(
    host: &dyn Host,
    handle: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let recorded = handle
        .linux_host
        .as_ref()
        .context("tmux surface has no recorded server incarnation")?;
    let Some(server) = host.server(recorded.pid, recorded.linux_birth)? else {
        return Ok(CloseOutcome::Missing);
    };
    // Ended before this close (not yet reaped): it is not signalled again.
    if server.exited_by(Instant::now())? {
        return Ok(CloseOutcome::Missing);
    }
    crate::native::session::close::budget::close_remaining(deadline)?;
    server.terminate()?;
    if !server.exited_by(deadline)? {
        bail!(
            "tmux server {} of this session did not exit before the close deadline",
            recorded.pid
        );
    }
    Ok(CloseOutcome::Closed)
}

// The prompt and its submit are one buffer, pasted raw into exactly this pane. Only a
// failure before the paste started is known not to have reached the pane; nothing is
// replayed after that. The buffer holds prompt text, so every failed path deletes it in
// time kept back for that.
pub(super) fn send_file(
    host: &dyn Host,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let (target, input, send_deadline) = (|| -> Result<_> {
        let send_deadline = deadline
            .checked_sub(BUFFER_CLEANUP_RESERVE)
            .filter(|candidate| *candidate > Instant::now())
            .context("the delivery timeout leaves no room to remove its tmux buffer")?;
        let target = Target::of(session)?;
        let mut input = RecordReader::at(prompt_path)
            .raw_bytes()
            .context("failed to read the prompt")?;
        input.push(b'\r');
        require_pane(host, &target, send_deadline)?;
        Ok((target, input, send_deadline))
    })()
    .map_err(TerminalSendFailure::not_sent)?;
    let buffer = buffer_name();
    let delete = || {
        let _ = host.tmux(
            target.socket,
            &[
                OsStr::new("delete-buffer"),
                OsStr::new("-b"),
                OsStr::new(&buffer),
            ],
            None,
            deadline,
        );
    };
    if let Err(failure) = call(
        host,
        target.socket,
        &[
            OsStr::new("load-buffer"),
            OsStr::new("-b"),
            OsStr::new(&buffer),
            OsStr::new("-"),
        ],
        Some(&input),
        send_deadline,
    ) {
        delete();
        return Err(TerminalSendFailure::not_sent(failure.into_error()));
    }
    call(
        host,
        target.socket,
        &[
            OsStr::new("paste-buffer"),
            OsStr::new("-d"),
            OsStr::new("-r"),
            OsStr::new("-b"),
            OsStr::new(&buffer),
            OsStr::new("-t"),
            OsStr::new(target.pane),
        ],
        None,
        send_deadline,
    )
    .map(|_| ())
    .map_err(|failure| {
        // A failed paste can return before tmux deletes the buffer (`-d`).
        delete();
        if failure.process_started() {
            TerminalSendFailure::delivery_uncertain(failure.into_error())
        } else {
            TerminalSendFailure::not_sent(failure.into_error())
        }
    })
}

fn buffer_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("agent-bridge-{}-{nanos}", std::process::id())
}

/// The TTY of exactly this pane, for the comparison with the TTY of the session owner.
pub(super) fn verify_session(
    host: &dyn Host,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    let deadline = super::super::posix::timeout_deadline(timeout.unwrap_or(VERIFY_TIMEOUT))?;
    let target = Target::of(session)?;
    let pane = require_pane(host, &target, deadline)?;
    if pane.tty.is_empty() {
        bail!("tmux pane {} has no tty", target.pane);
    }
    Ok(pane.tty)
}

pub(super) fn surface_present(
    host: &dyn Host,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    let deadline = super::super::posix::timeout_deadline(timeout)?;
    let target = Target::of(session)?;
    if !target.server_live(host)? {
        return Ok(false);
    }
    Ok(listed_pane(host, &target, deadline)?.is_some())
}

/// The visible rows of the pane: what the application drew, also while a viewer scrolls in
/// copy mode.
pub(super) fn read_screen(
    host: &dyn Host,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<String> {
    let target = Target::of(session)?;
    require_pane(host, &target, deadline)?;
    answer(
        host,
        target.socket,
        &["capture-pane", "-p", "-t", target.pane],
        deadline,
    )
}

/// The command a user runs to watch or type in this session's pane.
pub(in crate::native) fn attach_command(session: &TerminalSession) -> Option<String> {
    let socket = session.tmux_socket.as_deref()?;
    (session.kind == TerminalKind::Tmux).then(|| {
        format!(
            "tmux -S {} attach",
            crate::native::terminal::shell_quote(OsStr::new(socket))
        )
    })
}

pub(super) struct Adapter<H: Host> {
    host: H,
}

impl Adapter<Installed> {
    pub(super) const fn installed() -> Self {
        Self { host: Installed }
    }
}

#[cfg(test)]
impl<H: Host> Adapter<H> {
    fn with_host(host: H) -> Self {
        Self { host }
    }
}

impl<H: Host> PosixTerminalAdapter for Adapter<H> {
    fn open_bound_tab(
        &self,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        open_with(
            deadline,
            |startup_deadline| create(&self.host, command, directory, startup_deadline),
            // The pane's host waits for the binding above; nothing is typed.
            |_, _| Ok(()),
            // A server whose end is not confirmed is handed back with its proven identity.
            |session, deadline| {
                end_server(&self.host, session, deadline)
                    .map(|_| ())
                    .map_err(|cleanup| {
                        RetainedLaunchSurface::new(
                            session.clone(),
                            format!(
                                "the tmux server this launch started was not confirmed ended: {cleanup:#}"
                            ),
                        )
                        .into()
                    })
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
        send_file(&self.host, session, prompt_path, deadline)
    }
    fn verify_surface(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>> {
        verify_session(&self.host, session, timeout).map(Some)
    }
    fn surface_present(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        surface_present(&self.host, session, timeout)
    }
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
        read_screen(&self.host, session, deadline)
    }
    // Guarded dialog input is declared unsupported in the registration.
    fn close_session_until(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        end_server(&self.host, session, deadline)
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
            record_live_owner_intent(directory, id, session, SYSTEM_READS, deadline)?;
        }
        end_server(&self.host, session, deadline)
    }
}

// The intent is recorded before the server is ended, because ending it ends the owner even
// when the rest of the close fails. The owner itself is not signalled: the pane's terminal
// hangs up when its server ends.
fn record_live_owner_intent(
    directory: &Path,
    id: &str,
    session: &TerminalSession,
    reads: ProcessReads<'_>,
    deadline: Instant,
) -> Result<()> {
    ownership::stop_attested_owner(
        &Store::open_unchecked(directory),
        id,
        session,
        OwnerStopIntent::Close,
        OwnerStop {
            attest: || verified_posix_terminal_owner_with(directory, id, session, None, reads),
            shell: ProvenShell,
            record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
            stop: |_: OwnerGroups<()>| Ok(()),
        },
        deadline,
    )
}

/// Whether a binding names the pane and server this host runs in, as tmux names them in the
/// pane's environment: `TMUX` is `<socket>,<server pid>,<session index>` (read from the right,
/// since the socket path can hold commas) and `TMUX_PANE` the pane id.
fn binding_names_host(
    surface: &TerminalSession,
    directory: &Path,
    tmux: Option<&OsStr>,
    pane: Option<&OsStr>,
) -> bool {
    let Ok(target) = Target::of(surface) else {
        return false;
    };
    let Some(tmux) = tmux.and_then(OsStr::to_str) else {
        return false;
    };
    let mut parts = tmux.rsplitn(3, ',');
    let (Some(_index), Some(pid), Some(socket)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    target.socket == directory.join(SOCKET_FILE)
        && Path::new(socket) == target.socket
        && pid == target.server.pid.to_string()
        && pane == Some(OsStr::new(target.pane))
}

/// What bash runs. A non-interactive bash runs the file named by `BASH_ENV` before its
/// command, which `--noprofile --norc` does not stop; the launch must not run it, but the
/// provider keeps the variable, so the script exports it again. A value that is not UTF-8
/// cannot be quoted exactly and is dropped.
#[derive(Debug, Eq, PartialEq)]
enum BashScript {
    Plain(String),
    WithoutStartupFile(String),
}

fn bash_script(command: &str, bash_env: Option<&OsStr>) -> BashScript {
    match bash_env {
        None => BashScript::Plain(command.to_owned()),
        Some(value) => BashScript::WithoutStartupFile(match value.to_str() {
            Some(_) => format!(
                "export BASH_ENV={}\n{command}",
                crate::native::terminal::shell_quote(value)
            ),
            None => command.to_owned(),
        }),
    }
}

/// The pane program: admitted, bound to exactly this pane and server, its startup input
/// discarded, it becomes bash running the launch command with job control, so that the
/// command leads its own foreground process group on the pane's terminal. bash reads no
/// startup file: the pane has the environment of the process that started the server.
pub(in crate::native) fn run_host(directory: &Path, command: &str) -> Result<()> {
    use super::super::bound_host::{BoundHostSpec, prepare_bound_launch};
    let prepared = prepare_bound_launch(
        directory,
        &BoundHostSpec { name: BINDING.host },
        BINDING,
        &mut SystemClock,
        |surface| {
            if binding_names_host(
                surface,
                directory,
                std::env::var_os("TMUX").as_deref(),
                std::env::var_os("TMUX_PANE").as_deref(),
            ) {
                Ok(())
            } else {
                bail!("tmux surface binding does not name the pane of this launch host")
            }
        },
    );
    if let Err(error) = prepared {
        // The pane ends with this process and takes the message with it.
        crate::native::session::launch::log(
            &Store::open_unchecked(directory),
            &format!("tmux_host_refused: {error:#}"),
        );
        return Err(error);
    }
    let mut bash = Command::new("/bin/bash");
    bash.arg0("bash");
    let script = match bash_script(command, std::env::var_os("BASH_ENV").as_deref()) {
        BashScript::Plain(script) => script,
        BashScript::WithoutStartupFile(script) => {
            bash.env_remove("BASH_ENV");
            script
        }
    };
    let error = bash
        .args(["--noprofile", "--norc", "-m", "-c", &script])
        .exec();
    let error = anyhow::Error::new(error).context("could not start bash for the tmux launch");
    crate::native::session::launch::log(
        &Store::open_unchecked(directory),
        &format!("tmux_host_refused: {error:#}"),
    );
    Err(error)
}

#[cfg(test)]
mod tests;
