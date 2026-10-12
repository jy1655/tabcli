use super::process;
use super::{GuardedDialogInput, MacTerminalAdapter, MacosOpenMode};
use crate::native::session::{Reader, Store};
#[cfg(test)]
use crate::native::terminal::ownership;
#[cfg(unix)]
use crate::native::terminal::shell_quote;
// WezTerm through its official CLI (pinned source: 20240203-110809-5046fc22).
// Default: discover protected gui-sock-<pid> sockets in the macOS runtime directory,
// verify the socket peer, installed GUI executable and process birth, then use a
// unique existing window. `cli spawn --window-id ID --domain-name local -- PROGRAM`
// returns the new pane ID; that response, a pre-spawn snapshot and a post-spawn identity
// check prove the new tab. Neither inherited WEZTERM_* nor the invoking terminal selects it.
// The built-in local domain is installed before configured domains, which skip an
// existing name (wezterm-gui/src/main.rs; wezterm-mux-server-impl/src/lib.rs).
// No safe unique window/capability: open a private `start --always-new-process` GUI
// with a truthful pre-mutation fallback reason. The explicit new-window mode does
// the same. Once spawn may have executed, never retry, fallback or close a delta.
// The handle stores socket, GUI birth, pane/tab/window and creation-time owns_gui.
// Shared GUIs are only addressed with kill-pane; their process and socket are never
// removed, even after their last pane disappears. A private GUI can end only when
// no sibling panes remain. Settings are not consulted during cleanup.
// The launch command is the program of the pane and never typed input: WezTerm executes
// the arguments itself, and their first command (`run_host`) holds the wrapper back until
// the launcher has bound exactly this pane.
// CLI send-text --no-paste writes a prompt to the exact pane's pty. Completion of kill-pane
// is checked by listing; failed reads are not absence. CLI has no atomic screen/key
// operation and no create-without-focus operation: a new tab is the active tab of its
// window, and the pane that had the keyboard is selected again only on the GUI's own
// evidence (`give_keyboard_back`). A private GUI holds nothing of the user to select.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::super::WezTermMux;
use super::{CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession};
use crate::native::process_env::CommandOutputFailure;

const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
// How long the GUI may take to paint a new tab and record its pane as focused. A window in
// the background never does, so a tab opened there waits this long. Not measured live: one
// frame is 16 ms at the default `max_fps`.
const KEYBOARD_SETTLE: Duration = Duration::from_millis(300);
// How long a process without panes may take to end by itself.
const EXIT_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(2)
};

// What the adapter needs from the machine. Tests replace it; nothing else does.
pub(super) trait Host {
    fn now(&self) -> Instant {
        Instant::now()
    }
    // Verified local GUI incarnations; never a remote mux or a caller's socket hint.
    fn discover_guis(&self) -> Result<Vec<WezTermMux>> {
        Ok(Vec::new())
    }
    // Starts a WezTerm GUI process for one session; its only pane runs `program`.
    fn start_gui(&self, program: &[String]) -> Result<WezTermMux>;
    // The process that serves a socket, `None` when nothing listens there.
    fn socket_server(&self, socket: &str) -> Result<Option<u32>>;
    // The birth of a process, `None` when there is no such process.
    fn process_start(&self, pid: u32) -> Result<Option<(u64, u64)>>;
    // Asks a process to end.
    fn terminate(&self, pid: u32) -> Result<()>;
    // Removes the socket file of a process that has ended.
    fn remove_socket(&self, socket: &str);
    // `wezterm cli --no-auto-start <arguments>` against exactly that socket.
    fn cli(
        &self,
        socket: &str,
        arguments: &[&str],
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure>;
}

pub(super) struct Installed;

// A `wezterm` command that takes nothing about its target from the caller. A program
// inside WezTerm inherits its socket, its pane and its configuration paths.
fn wezterm_command(inherited: impl Iterator<Item = OsString>) -> Command {
    let bundled = Path::new("/Applications/WezTerm.app/Contents/MacOS/wezterm");
    let mut command = crate::native::process_env::helper_command(if bundled.is_file() {
        bundled
    } else {
        Path::new("wezterm")
    });
    for name in inherited {
        if name.to_string_lossy().starts_with("WEZTERM_") {
            command.env_remove(name);
        }
    }
    command
}

// `--no-auto-connect`: the process attaches to no mux domain of the user's configuration,
// so it holds no pane but the one it opens, which runs `program`.
fn gui_command(inherited: impl Iterator<Item = OsString>, program: &[String]) -> Command {
    let mut command = wezterm_command(inherited);
    command
        .args(["start", "--always-new-process", "--no-auto-connect", "--"])
        .args(program);
    command
}

// The runtime directory of macOS (config/src/config.rs, `compute_runtime_dir`) and the
// socket of a GUI process in it (wezterm-gui/src/main.rs, `async_run_terminal_gui`).
fn gui_socket(home: &str, pid: u32) -> String {
    format!("{home}/.local/share/wezterm/gui-sock-{pid}")
}

impl Host for Installed {
    fn discover_guis(&self) -> Result<Vec<WezTermMux>> {
        // The release's discover_gui_socks scans this directory. Do not use its
        // default-* symlink or inherit WEZTERM_UNIX_SOCKET; neither identifies all
        // existing GUIs, and the CLI's other discovery path can select a remote mux.
        let home = env::var("HOME").context("HOME is not set to a UTF-8 path")?;
        let directory = Path::new(&home).join(".local/share/wezterm");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("cannot discover local WezTerm GUIs"),
        };
        let mut guis = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(pid) = socket_pid(&path) else {
                continue;
            };
            safe_socket_path(&path)?;
            let socket = path.to_str().context("WezTerm socket path is not UTF-8")?;
            let Some(server) = self.socket_server(socket)? else {
                continue;
            };
            if server != pid {
                bail!("WezTerm GUI socket {socket} names PID {pid} but is served by {server}");
            }
            let Some((start_seconds, start_microseconds)) = self.process_start(pid)? else {
                continue;
            };
            verify_gui_executable(pid, &home)?;
            let mux = WezTermMux {
                socket: socket.to_owned(),
                pid,
                start_seconds,
                start_microseconds,
                owns_gui: false,
            };
            require_serving(self, &mux)?;
            guis.push(mux);
        }
        guis.sort_by(|left, right| left.socket.cmp(&right.socket));
        Ok(guis)
    }

    fn start_gui(&self, program: &[String]) -> Result<WezTermMux> {
        let home = env::var("HOME").context("HOME is not set to a UTF-8 path")?;
        let mut command = gui_command(env::vars_os().map(|(name, _)| name), program);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A key that interrupts the caller must not end the session's terminal.
            .process_group(0);
        let mut child = command.spawn().context("failed to start WezTerm")?;
        let pid = child.id();
        // The birth is read while the child is not reaped, so the pid is still its own.
        let (start_seconds, start_microseconds) = match process::macos_process_start(pid) {
            Ok(Some(birth)) => birth,
            failed => {
                let _ = child.kill();
                let _ = child.wait();
                failed?;
                bail!("WezTerm ended as it started");
            }
        };
        // The process outlives this one. While both live, its end is collected here.
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(WezTermMux {
            socket: gui_socket(&home, pid),
            pid,
            start_seconds,
            start_microseconds,
            owns_gui: true,
        })
    }

    fn socket_server(&self, socket: &str) -> Result<Option<u32>> {
        let stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to reach {socket}"));
            }
        };
        let mut pid: libc::pid_t = 0;
        let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>())
            .context("pid size is out of range")?;
        let read = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                std::ptr::addr_of_mut!(pid).cast(),
                &mut length,
            )
        };
        if read != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to read the process that serves {socket}"));
        }
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot identify WezTerm socket user");
        }
        if uid != unsafe { libc::geteuid() }
            || length as usize != std::mem::size_of::<libc::pid_t>()
        {
            bail!("WezTerm socket peer identity is not this user's local process");
        }
        Ok(u32::try_from(pid).ok())
    }

    fn process_start(&self, pid: u32) -> Result<Option<(u64, u64)>> {
        process::macos_process_start(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        // 0 and negative values address process groups.
        let target = libc::pid_t::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .context("PID is out of range")?;
        if unsafe { libc::kill(target, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| format!("failed to end process {pid}"));
            }
        }
        Ok(())
    }

    fn remove_socket(&self, socket: &str) {
        let _ = fs::remove_file(socket);
    }

    fn cli(
        &self,
        socket: &str,
        arguments: &[&str],
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure> {
        // The CLI reads its text from stdin, so a prompt is neither an argument that
        // other processes can list nor one that is parsed as an option.
        let stdin = match input {
            None => Stdio::null(),
            Some(bytes) => {
                let staged = (|| -> Result<fs::File> {
                    let mut file = tempfile::tempfile()?;
                    file.write_all(bytes)?;
                    file.seek(SeekFrom::Start(0))?;
                    Ok(file)
                })()
                .context("failed to stage the input of the WezTerm CLI")
                .map_err(CommandOutputFailure::not_started)?;
                Stdio::from(staged)
            }
        };
        let mut command = wezterm_command(env::vars_os().map(|(name, _)| name));
        command
            .args(["cli", "--no-auto-start"])
            .args(arguments)
            .env("WEZTERM_UNIX_SOCKET", socket);
        crate::native::process_env::command_output_with_stdin_until_classified(
            &mut command,
            stdin,
            deadline,
            "WezTerm CLI",
        )
    }
}

fn socket_pid(path: &Path) -> Option<u32> {
    let suffix = path.file_name()?.to_str()?.strip_prefix("gui-sock-")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok().filter(|pid| *pid > 0)
}

fn safe_socket_path(path: &Path) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let directory = fs::symlink_metadata(path.parent().context("socket has no parent")?)?;
    let socket = fs::symlink_metadata(path)?;
    if !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o022 != 0
        || !socket.file_type().is_socket()
        || socket.uid() != uid
    {
        bail!("{} is not a protected local GUI socket", path.display());
    }
    Ok(())
}

fn verify_gui_executable(pid: u32, home: &str) -> Result<()> {
    let mut buffer = [0u8; 4096];
    let count = unsafe {
        process::proc_pidpath(
            pid.try_into()?,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if count <= 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read local GUI executable");
    }
    let actual =
        Path::new(std::str::from_utf8(&buffer[..usize::try_from(count)?])?.trim_end_matches('\0'));
    // Match the executable in an installed bundle or next to an installed CLI. A
    // process named wezterm-mux-server (including a local relay to a remote mux) is
    // never a GUI target. canonicalize handles Homebrew's executable symlinks.
    let mut candidates = vec![
        Path::new("/Applications/WezTerm.app/Contents/MacOS/wezterm-gui").to_path_buf(),
        Path::new(home).join("Applications/WezTerm.app/Contents/MacOS/wezterm-gui"),
    ];
    if let Some(paths) = env::var_os("PATH") {
        candidates.extend(
            env::split_paths(&paths)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join("wezterm-gui")),
        );
    }
    let actual = fs::canonicalize(actual).context("cannot resolve local GUI executable")?;
    if !candidates
        .iter()
        .any(|candidate| fs::canonicalize(candidate).ok().as_ref() == Some(&actual))
    {
        bail!(
            "process {pid} is not a verified installed WezTerm GUI: {}",
            actual.display()
        );
    }
    Ok(())
}

// The fields of `CliListResultItem` (wezterm/src/cli/list.rs) that identify a pane. The
// source declares that struct a stable output format; its other fields are not read.
#[derive(Clone, Debug, Deserialize)]
struct Pane {
    window_id: u64,
    tab_id: u64,
    pane_id: u64,
    tty_name: Option<String>,
}

fn target(session: &TerminalSession) -> Result<(&WezTermMux, u64)> {
    let mux = session
        .wezterm_mux
        .as_ref()
        .context("WezTerm handle does not name the process that owns its pane")?;
    if !mux.owns_gui && (session.tab_id.is_none() || session.window_id.is_none()) {
        bail!("shared WezTerm GUI handle is missing its created tab/window identity");
    }
    let pane = session
        .id
        .parse()
        .context("WezTerm handle has an unreadable pane id")?;
    Ok((mux, pane))
}

fn birth(mux: &WezTermMux) -> Option<(u64, u64)> {
    Some((mux.start_seconds, mux.start_microseconds))
}

// Whether the session's process is there. Only a dead PID proves absence. A reused
// identity or a socket that is not its own leaves cleanup unverified.
fn serving(host: &dyn Host, mux: &WezTermMux) -> Result<bool> {
    match host.process_start(mux.pid)? {
        None => return Ok(false),
        Some(current) if Some(current) == birth(mux) => {}
        Some(_) => bail!("WezTerm GUI PID was reused; exact surface absence is unverified"),
    }
    match host.socket_server(&mux.socket)? {
        Some(pid) if pid == mux.pid => Ok(true),
        Some(pid) => bail!(
            "{} is served by process {pid}, not by WezTerm process {} of this session",
            mux.socket,
            mux.pid
        ),
        None => bail!(
            "WezTerm process {} is running but does not serve {}",
            mux.pid,
            mux.socket
        ),
    }
}

fn require_serving(host: &dyn Host, mux: &WezTermMux) -> Result<()> {
    if !serving(host, mux)? {
        bail!("the WezTerm process of this session is gone");
    }
    Ok(())
}

fn call(
    host: &dyn Host,
    mux: &WezTermMux,
    arguments: &[&str],
    input: Option<&[u8]>,
    deadline: Instant,
) -> std::result::Result<String, CommandOutputFailure> {
    let output = host.cli(&mux.socket, arguments, input, deadline)?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CommandOutputFailure::started(anyhow!(
            "wezterm cli {} failed: {}",
            arguments[0],
            if error.is_empty() {
                output.status.to_string()
            } else {
                error
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// Kept typed through discovery: only a started list that times out proves busy.
// Capability failures can use the private-GUI fallback, but are never list evidence.
#[derive(Debug)]
enum LookupFailure {
    Busy(anyhow::Error),
    Exhausted,
    Unusable(anyhow::Error),
    Capability(anyhow::Error),
}

impl std::fmt::Display for LookupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(error) => {
                write!(f, "WezTerm pane listing did not answer in time: {error:#}")
            }
            Self::Exhausted => write!(f, "WezTerm deadline expired"),
            Self::Unusable(error) => write!(f, "{error:#}"),
            Self::Capability(error) => {
                write!(f, "WezTerm spawn capability check failed: {error:#}")
            }
        }
    }
}

impl std::error::Error for LookupFailure {}

impl From<anyhow::Error> for LookupFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Unusable(error)
    }
}

fn panes(
    host: &dyn Host,
    mux: &WezTermMux,
    deadline: Instant,
) -> std::result::Result<Vec<Pane>, LookupFailure> {
    if host.now() >= deadline {
        return Err(LookupFailure::Exhausted);
    }
    let listing =
        call(host, mux, &["list", "--format", "json"], None, deadline).map_err(|failure| {
            if failure.timed_out() {
                LookupFailure::Busy(failure.into_error())
            } else if !failure.process_started() && host.now() >= deadline {
                LookupFailure::Exhausted
            } else {
                LookupFailure::Unusable(failure.into_error())
            }
        })?;
    serde_json::from_str(&listing)
        .context("WezTerm returned an unreadable pane list")
        .map_err(LookupFailure::Unusable)
}

// The panes of the session's process, `None` when the process is gone.
fn listed(
    host: &dyn Host,
    mux: &WezTermMux,
    deadline: Instant,
) -> std::result::Result<Option<Vec<Pane>>, LookupFailure> {
    if !serving(host, mux)? {
        return Ok(None);
    }
    let panes = panes(host, mux, deadline)?;
    require_serving(host, mux)?;
    Ok(Some(panes))
}

// A GUI process that ends with its last window leaves its socket file behind. Once the
// session's process is gone and nothing serves the file, it is removed, as WezTerm's own
// discovery does (wezterm-client/src/discovery.rs, `discover_gui_socks`). A later process
// with the same pid may already have bound the same path before it listens; any
// live process at that pid leaves the socket alone.
fn forget_socket(host: &dyn Host, mux: &WezTermMux) {
    if mux.owns_gui
        && matches!(host.process_start(mux.pid), Ok(None))
        && matches!(host.socket_server(&mux.socket), Ok(None))
    {
        host.remove_socket(&mux.socket);
    }
}

// The fields of `CliListClientsResultItem` (wezterm/src/cli/list_clients.rs) that say which
// pane a client has focused. The source declares that struct a stable output format too.
#[derive(Deserialize)]
struct Client {
    pid: u32,
    focused_pane_id: Option<u64>,
}

// The pane that the GUI reports as focused, `None` unless exactly its own client says so.
// The GUI records a pane when it paints it while its window has the keyboard
// (wezterm-gui/src/termwindow/render/paint.rs). The record of a window in the background
// is old: it follows neither a new tab nor a tab that another CLI call selected.
fn keyboard_pane(host: &dyn Host, mux: &WezTermMux, deadline: Instant) -> Option<u64> {
    let clients = call(
        host,
        mux,
        &["list-clients", "--format", "json"],
        None,
        deadline,
    )
    .ok()?;
    let clients: Vec<Client> = serde_json::from_str(&clients).ok()?;
    let mut gui = clients.iter().filter(|client| client.pid == mux.pid);
    match (gui.next(), gui.next()) {
        (Some(client), None) => client.focused_pane_id,
        _ => None,
    }
}

// A new tab is the active tab of its window (mux/src/lib.rs, `spawn_tab_or_window`), and
// the CLI cannot create one unselected, so in a focused window the keys of the user go to
// the session's pane (issue #58). The pane that had the keyboard right before the spawn is
// selected again, on the GUI's own word only: it must report the new pane as focused, which
// it does only for a window that has the keyboard. Everything else is no authority: a
// record that stayed (a window in the background, whose selected tab the CLI cannot
// tell), a pane the user chose meanwhile, a pane that the listing after the spawn no
// longer shows in its tab and window, a failed read. The selection follows the GUI's
// answer with no call in between, and nothing here fails the launch.
fn give_keyboard_back(
    host: &dyn Host,
    mux: &WezTermMux,
    previous: Option<&Pane>,
    created: &Pane,
    after: &[Pane],
    deadline: Instant,
) {
    let Some(previous) = previous else {
        return;
    };
    let mut same = after.iter().filter(|pane| pane.pane_id == previous.pane_id);
    if !matches!(
        (same.next(), same.next()),
        (Some(pane), None)
            if pane.tab_id == previous.tab_id && pane.window_id == previous.window_id
    ) {
        return;
    }
    let settled = Instant::now() + KEYBOARD_SETTLE;
    loop {
        match keyboard_pane(host, mux, deadline) {
            Some(pane) if pane == created.pane_id => break,
            Some(pane) if pane == previous.pane_id && Instant::now() < settled => {
                thread::sleep(POLL)
            }
            _ => return,
        }
    }
    let _ = call(
        host,
        mux,
        &["activate-pane", "--pane-id", &previous.pane_id.to_string()],
        None,
        deadline,
    );
}

// Writes bytes to the pane's pty, as typed input.
fn write(
    host: &dyn Host,
    mux: &WezTermMux,
    pane: u64,
    bytes: &[u8],
    deadline: Instant,
) -> std::result::Result<(), CommandOutputFailure> {
    call(
        host,
        mux,
        &["send-text", "--pane-id", &pane.to_string(), "--no-paste"],
        Some(bytes),
        deadline,
    )
    .map(|_| ())
}

// The pane that the process opened when it started.
fn startup_pane(host: &dyn Host, mux: &WezTermMux, deadline: Instant) -> Result<Pane> {
    let mut waiting_for = anyhow!(
        "WezTerm did not listen on {} before the deadline",
        mux.socket
    );
    loop {
        if host.process_start(mux.pid)? != birth(mux) {
            bail!("WezTerm process {} ended while it started", mux.pid);
        }
        match host.socket_server(&mux.socket)? {
            Some(pid) if pid == mux.pid => match panes(host, mux, deadline) {
                Ok(opened) => match opened.as_slice() {
                    [] => waiting_for = anyhow!("WezTerm opened no window before the deadline"),
                    [pane] => {
                        if !local_tty(pane.tty_name.as_deref()) {
                            bail!(
                                "the pane that WezTerm opened has no local tty; a default domain that is not local cannot hold a session"
                            );
                        }
                        return Ok(pane.clone());
                    }
                    more => bail!(
                        "WezTerm opened {} panes when it started; with a startup configuration that opens its own windows the pane of the session cannot be told apart",
                        more.len()
                    ),
                },
                // A deadline before the next read adds no observation. Keep the
                // last startup evidence (for example, an answered empty list).
                Err(LookupFailure::Exhausted) => return Err(waiting_for),
                Err(error) => waiting_for = error.into(),
            },
            Some(pid) => bail!(
                "{} is served by process {pid}, not by the WezTerm process {} that was started",
                mux.socket,
                mux.pid
            ),
            None => {}
        }
        if Instant::now() >= deadline {
            return Err(waiting_for);
        }
        thread::sleep(POLL);
    }
}

// Ends the process of a session that got no handle. Only the process with the recorded
// birth is asked, and the end is waited for.
fn end_unbound_process(host: &dyn Host, mux: &WezTermMux) -> Result<()> {
    let deadline = Instant::now() + EXIT_GRACE;
    let mut asked = false;
    loop {
        if host.process_start(mux.pid)? != birth(mux) {
            forget_socket(host, mux);
            return Ok(());
        }
        if !asked {
            if host.socket_server(&mux.socket)? == Some(mux.pid)
                && panes(host, mux, deadline)?.len() > 1
            {
                bail!("private GUI has sibling panes; its process and socket were retained");
            }
            asked = true;
            host.terminate(mux.pid)?;
        } else if Instant::now() >= deadline {
            bail!("it is still running");
        } else {
            thread::sleep(POLL);
        }
    }
}

fn create_private_gui(
    host: &dyn Host,
    program: &[String],
    deadline: Instant,
) -> Result<TerminalSession> {
    if host.now() >= deadline {
        return Err(LookupFailure::Exhausted.into());
    }
    let mux = host.start_gui(program)?;
    match startup_pane(host, &mux, deadline) {
        Ok(pane) => Ok(TerminalSession {
            kind: TerminalKind::WezTerm,
            id: pane.pane_id.to_string(),
            tab_id: Some(pane.tab_id.to_string()),
            window_id: Some(pane.window_id.to_string()),
            managed_session_id: None,
            windows_process_identity: None,
            wezterm_mux: Some(mux),
        }),
        // The process exists for this session alone, and without a handle nothing would
        // end it later.
        Err(error) => match end_unbound_process(host, &mux) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(anyhow!(
                "{error:#}; WezTerm process {} that was started for the session could not be ended: {cleanup:#}",
                mux.pid
            )),
        },
    }
}

// Only a single verified local window is an unambiguous target. Multiple panes/tabs
// of that window are fine: none is used as the source pane or receives any input.
fn existing_window(
    host: &dyn Host,
    deadline: Instant,
) -> std::result::Result<(WezTermMux, Vec<Pane>, u64), LookupFailure> {
    if host.now() >= deadline {
        return Err(LookupFailure::Exhausted);
    }
    let guis = host.discover_guis()?;
    let mut targets = Vec::new();
    let mut unusable = None;
    for mut mux in guis {
        mux.owns_gui = false;
        let snapshot = match listed(host, &mux, deadline) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                unusable.get_or_insert_with(|| anyhow!("discovered GUI ended"));
                continue;
            }
            Err(LookupFailure::Unusable(error)) => {
                unusable.get_or_insert(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut ids = BTreeSet::new();
        if snapshot.iter().any(|pane| !ids.insert(pane.pane_id)) {
            unusable
                .get_or_insert_with(|| anyhow!("existing GUI returned duplicate pane identities"));
            continue;
        }
        let windows: BTreeSet<_> = snapshot.iter().map(|pane| pane.window_id).collect();
        for window in windows {
            targets.push((mux.clone(), snapshot.clone(), window));
        }
    }
    // Inspect every discovered GUI before choosing fallback: an unusable listing
    // must not hide a later GUI's timeout.
    if let Some(error) = unusable {
        return Err(LookupFailure::Unusable(error));
    }
    if targets.len() != 1 {
        return Err(LookupFailure::Unusable(anyhow!(
            "{} verified local WezTerm windows; no unique tab target",
            targets.len()
        )));
    }
    let target = targets.pop().unwrap();
    let help = call(host, &target.0, &["spawn", "--help"], None, deadline)
        .map_err(|failure| LookupFailure::Capability(failure.into_error()))?;
    if !help.contains("--window-id") || !help.contains("--domain-name") {
        return Err(LookupFailure::Unusable(anyhow!(
            "installed WezTerm CLI does not support an explicitly addressed local tab spawn"
        )));
    }
    require_serving(host, &target.0)?;
    Ok(target)
}

// The program of the session's pane. WezTerm executes these arguments itself (`cli spawn
// -- PROG`, `start -- PROG`), so no line that the user can type into carries the launch
// command: a key typed in front of the typed command made it `a. '/…/launch.sh'`, and the
// provider never started (issue #58, human test of 2026-10-03). A login, interactive zsh
// keeps the user's PATH and runs the gate and the wrapper as foreground jobs of the shell
// that sources the launch script, which the owner record of the wrapper requires.
fn pane_program(gate: &str, command: &str) -> [String; 5] {
    [
        "/bin/zsh",
        "-l",
        "-i",
        "-c",
        &format!("{gate} || exit; {command}"),
    ]
    .map(str::to_owned)
}

pub(super) fn create_surface(
    host: &dyn Host,
    force_new_window: bool,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<TerminalSession> {
    create(
        host,
        force_new_window,
        &host_pane_program(command, directory)?,
        deadline,
    )
}

fn host_pane_program(command: &str, directory: &Path) -> Result<[String; 5]> {
    let executable = env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let gate = format!(
        "{} native-wezterm-host {}",
        shell_quote(executable.as_os_str()),
        shell_quote(directory.as_os_str())
    );
    Ok(pane_program(&gate, command))
}

fn create(
    host: &dyn Host,
    force_new_window: bool,
    program: &[String],
    deadline: Instant,
) -> Result<TerminalSession> {
    if force_new_window {
        return create_private_gui(host, program, deadline);
    }
    let (mux, snapshot, window) = match existing_window(host, deadline) {
        Ok(target) => target,
        Err(reason @ LookupFailure::Busy(_)) => {
            return Err(anyhow::Error::from(reason))
                .context("the running WezTerm did not answer in time; no new window was opened; close the sessions you have finished with and retry when WezTerm responds");
        }
        Err(LookupFailure::Exhausted) => return Err(LookupFailure::Exhausted.into()),
        Err(reason) => {
            if host.now() >= deadline {
                return Err(anyhow::Error::from(LookupFailure::Exhausted))
                    .with_context(|| format!("WezTerm new-window fallback after: {reason:#}"));
            }
            eprintln!("WezTerm tab-first: {reason:#}; opening a new private GUI window");
            return create_private_gui(host, program, deadline)
                .with_context(|| format!("WezTerm new-window fallback after: {reason:#}"));
        }
    };
    // Read last: the user can select another tab until the spawn.
    let keyboard = keyboard_pane(host, &mux, deadline).and_then(|id| {
        snapshot
            .iter()
            .find(|pane| pane.pane_id == id && pane.window_id == window)
            .cloned()
    });
    // The CLI's exact SpawnResponse pane id is mandatory; a list delta alone is
    // never ownership. Its --domain-name overrides the user's default domain.
    let window_id = window.to_string();
    let mut arguments = vec![
        "spawn",
        "--window-id",
        &window_id,
        "--domain-name",
        "local",
        "--",
    ];
    arguments.extend(program.iter().map(String::as_str));
    let returned = call(host, &mux, &arguments, None, deadline);
    let evidence = || {
        serde_json::json!({
            "gui": mux, "requested_window_id": window,
            "before": snapshot.iter().map(|p| serde_json::json!({
                "pane_id": p.pane_id, "tab_id": p.tab_id, "window_id": p.window_id
            })).collect::<Vec<_>>()
        })
        .to_string()
    };
    let reply = match returned {
        Ok(reply) => reply,
        Err(failure) => {
            let uncertain = failure.process_started();
            bail!(
                "WezTerm tab spawn {}: {:#}; creation evidence={}; no retry, fallback or pane cleanup was attempted",
                if uncertain {
                    "may have executed"
                } else {
                    "did not start"
                },
                failure.into_error(),
                evidence()
            );
        }
    };
    let validated = (|| -> Result<(Pane, Vec<Pane>)> {
        let id = reply.trim();
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("spawn response is not exactly one pane id");
        }
        let id: u64 = id.parse()?;
        if snapshot.iter().any(|pane| pane.pane_id == id) {
            bail!("spawn returned a preexisting pane {id}");
        }
        let after = listed(host, &mux, deadline)?.context("GUI ended after spawn")?;
        let matching: Vec<_> = after.iter().filter(|pane| pane.pane_id == id).collect();
        let [pane] = matching.as_slice() else {
            bail!("spawn reply did not match exactly one newly listed pane");
        };
        if pane.window_id != window || snapshot.iter().any(|old| old.tab_id == pane.tab_id) {
            bail!("spawn pane is not a new tab in the requested window");
        }
        if !local_tty(pane.tty_name.as_deref()) {
            bail!("spawn pane has no safe local tty");
        }
        require_serving(host, &mux)?;
        Ok(((*pane).clone(), after))
    })();
    let (pane, after) = validated.with_context(|| format!(
        "WezTerm tab creation is uncertain; spawn reply={reply:?}; creation evidence={}; no retry, fallback or pane cleanup was attempted", evidence()
    ))?;
    give_keyboard_back(host, &mux, keyboard.as_ref(), &pane, &after, deadline);
    Ok(TerminalSession {
        kind: TerminalKind::WezTerm,
        id: pane.pane_id.to_string(),
        tab_id: Some(pane.tab_id.to_string()),
        window_id: Some(pane.window_id.to_string()),
        managed_session_id: None,
        windows_process_identity: None,
        wezterm_mux: Some(mux),
    })
}

fn local_tty(tty: Option<&str>) -> bool {
    tty.and_then(|tty| tty.strip_prefix("/dev/ttys"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn matches_handle(session: &TerminalSession, pane: &Pane) -> bool {
    session
        .tab_id
        .as_deref()
        .is_none_or(|id| id == pane.tab_id.to_string())
        && session
            .window_id
            .as_deref()
            .is_none_or(|id| id == pane.window_id.to_string())
}

fn require_shared_pane(
    host: &dyn Host,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<()> {
    let (mux, id) = target(session)?;
    if !mux.owns_gui {
        let opened = listed(host, mux, deadline)?.context("shared WezTerm GUI is gone")?;
        let matching: Vec<_> = opened.iter().filter(|pane| pane.pane_id == id).collect();
        if !matches!(matching.as_slice(), [pane] if matches_handle(session, pane) && local_tty(pane.tty_name.as_deref()))
        {
            bail!("shared WezTerm pane no longer matches its created tab/window/local tty");
        }
    }
    Ok(())
}

// The first command of the session's pane (`pane_program`). The pane exists before the
// launcher knows its id, so the wrapper may start only once the launcher has bound exactly
// this pane: the handle must name the pane and the GUI socket that the mux itself gave this
// process (`WEZTERM_PANE`, `WEZTERM_UNIX_SOCKET`; mux/src/domain.rs, `build_command`, set
// after the user's configured environment). It waits on the receipt, the claim and the
// status that every launch has and keeps no record of its own. A launch that failed or
// timed out never falls back to typed input.
pub(in crate::native) fn run_host(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("no WezTerm launch directory {}", directory.display()))?;
    if !directory.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("WezTerm launch directory is not private to the current user");
    }
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid WezTerm launch directory")?;
    crate::native::session::primitives::require_valid_session_id(id)?;
    let started = (|| -> Result<()> {
        let pane =
            env::var("WEZTERM_PANE").context("WezTerm did not name the pane of this host")?;
        let socket = env::var_os("WEZTERM_UNIX_SOCKET")
            .context("WezTerm did not name the GUI of this host")?;
        wait_for_binding(directory, id, &pane, Path::new(&socket))?;
        // Keys that arrived while the creation had selected this pane must not answer the
        // provider's first dialog. This discards the input of this process's own pty only.
        if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot discard WezTerm startup input");
        }
        Ok(())
    })();
    // The pane ends with this process and takes the message with it.
    if let Err(error) = &started {
        crate::native::launch::log(
            &Store::open_unchecked(directory),
            &format!("wezterm_host_refused: {error:#}"),
        );
    }
    started
}

fn wait_for_binding(directory: &Path, id: &str, pane: &str, socket: &Path) -> Result<()> {
    use crate::native::launch::wait_for_binding;
    use crate::native::terminal::facts::wezterm::BINDING;
    let surface = wait_for_binding(&Reader::open_unchecked(directory), id, &BINDING)?;
    if surface.kind != TerminalKind::WezTerm
        || surface.id != pane
        || surface
            .wezterm_mux
            .is_none_or(|mux| Path::new(&mux.socket) != socket)
    {
        bail!("WezTerm surface binding does not name the pane of this launch host");
    }
    Ok(())
}

// The prompt and its Enter are one write to the pty. Only a call that never started is
// known not to have reached the pane.
pub(super) fn send_file(
    host: &dyn Host,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let (mux, pane, input) = (|| -> Result<_> {
        let (mux, pane) = target(session)?;
        let mut input = crate::native::session::RecordReader::at(prompt_path)
            .raw_bytes()
            .context("failed to read the prompt")?;
        std::str::from_utf8(&input)
            .context("the prompt is not UTF-8 text, which the WezTerm CLI requires")?;
        input.push(b'\r');
        require_serving(host, mux)?;
        require_shared_pane(host, session, deadline)?;
        Ok((mux, pane, input))
    })()
    .map_err(TerminalSendFailure::not_sent)?;
    write(host, mux, pane, &input, deadline).map_err(|failure| {
        if failure.process_started() {
            TerminalSendFailure::delivery_uncertain(failure.into_error())
        } else {
            TerminalSendFailure::not_sent(failure.into_error())
        }
    })
}

// The tty of exactly this pane, for the comparison with the tty of the session owner.
pub(super) fn verify_session(
    host: &dyn Host,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    let deadline = super::timeout_deadline(timeout.unwrap_or(VERIFY_TIMEOUT))?;
    let (mux, pane) = target(session)?;
    let mut matches = listed(host, mux, deadline)?
        .context("the WezTerm process of this session is gone")?
        .into_iter()
        .filter(|listed| listed.pane_id == pane);
    match (matches.next(), matches.next()) {
        (Some(listed), None) if matches_handle(session, &listed) => listed
            .tty_name
            .filter(|tty| !tty.is_empty())
            .context("WezTerm pane has no tty"),
        _ => bail!("WezTerm ownership proof did not match exactly one pane"),
    }
}

pub(super) fn surface_present(
    host: &dyn Host,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    let (mux, pane) = target(session)?;
    // A read-only absence proof must not be inferred from a PID reused by a
    // different process, even though no command would be sent to that process.
    if host
        .process_start(mux.pid)?
        .is_some_and(|current| Some(current) != birth(mux))
    {
        bail!("WezTerm GUI PID was reused; exact surface absence is unverified");
    }
    Ok(listed(host, mux, super::timeout_deadline(timeout)?)?
        .is_some_and(|panes| panes.iter().any(|listed| listed.pane_id == pane)))
}

pub(super) fn read_screen(
    host: &dyn Host,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<String> {
    let (mux, pane) = target(session)?;
    require_serving(host, mux)?;
    require_shared_pane(host, session, deadline)?;
    call(
        host,
        mux,
        &["get-text", "--pane-id", &pane.to_string()],
        None,
        deadline,
    )
    .map_err(CommandOutputFailure::into_error)
}

// Every step reads the state again, so a close that failed part of the way is finished by
// the next one: a pane that is gone is not killed again, and a process that was left
// without panes is ended.
pub(super) fn close_session_until(
    host: &dyn Host,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    let (mux, pane) = target(session)?;
    let mut outcome = CloseOutcome::Missing;
    let mut kill: Option<Result<()>> = None;
    let mut without_panes_since: Option<Instant> = None;
    let mut asked_to_end = false;
    loop {
        crate::native::session::close::budget::close_remaining(deadline)?;
        let waiting_for = match listed(host, mux, deadline) {
            Ok(None) => {
                forget_socket(host, mux);
                return Ok(outcome);
            }
            Ok(Some(panes)) if panes.iter().any(|listed| listed.pane_id == pane) => {
                let matching: Vec<_> = panes
                    .iter()
                    .filter(|listed| listed.pane_id == pane)
                    .collect();
                if !matches!(matching.as_slice(), [listed] if matches_handle(session, listed)) {
                    bail!(
                        "WezTerm close target does not match exactly its created pane/tab/window"
                    );
                }
                match kill.take() {
                    None => {
                        crate::native::session::close::budget::close_remaining(deadline)?;
                        outcome = CloseOutcome::Closed;
                        kill = Some(
                            call(
                                host,
                                mux,
                                &["kill-pane", "--pane-id", &pane.to_string()],
                                None,
                                deadline,
                            )
                            .map(|_| ())
                            .map_err(CommandOutputFailure::into_error),
                        );
                        // The next look waits one interval: a listing sent while the
                        // process ends with its last pane fails and leaves a CLI log.
                        anyhow!("WezTerm pane {pane} is being closed")
                    }
                    Some(Err(error)) => {
                        return Err(error.context(format!("WezTerm pane {pane} was not closed")));
                    }
                    Some(Ok(())) => {
                        kill = Some(Ok(()));
                        anyhow!("WezTerm answered kill-pane, but pane {pane} is still listed")
                    }
                }
            }
            // Panes that the user opened in the process: it is theirs now and stays.
            Ok(Some(panes)) if !panes.is_empty() => return Ok(outcome),
            Ok(Some(_)) if !mux.owns_gui => return Ok(outcome),
            Ok(Some(_)) => {
                let since = *without_panes_since.get_or_insert_with(Instant::now);
                if !asked_to_end && since.elapsed() >= EXIT_GRACE {
                    asked_to_end = true;
                    crate::native::session::close::budget::close_remaining(deadline)?;
                    host.terminate(mux.pid)?;
                }
                anyhow!(
                    "WezTerm process {} of this session is still running without a pane",
                    mux.pid
                )
            }
            // The process can be ending: its socket closes before it is gone.
            Err(error) => error.into(),
        };
        if Instant::now() >= deadline {
            return Err(waiting_for);
        }
        thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        if Instant::now() >= deadline {
            return Err(waiting_for);
        }
    }
}

pub(super) struct Adapter<H: Host> {
    host: H,
}
pub(super) static ADAPTER: Adapter<Installed> = Adapter { host: Installed };
#[cfg(test)]
impl<H: Host> Adapter<H> {
    fn with_host(host: H) -> Self {
        Self { host }
    }
}
impl<H: Host> MacTerminalAdapter for Adapter<H> {
    fn open_bound_tab(
        &self,
        mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: super::BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        super::open_with(
            deadline,
            |startup_deadline| {
                create_surface(
                    &self.host,
                    mode == MacosOpenMode::NewWindow,
                    command,
                    directory,
                    startup_deadline,
                )
            },
            // The program of the pane waits for the binding above; nothing is typed.
            |_, _| Ok(()),
            |session, deadline| close_session_until(&self.host, session, deadline).map(|_| ()),
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
    fn guarded_dialog_input(
        &self,
        _session: &TerminalSession,
        _input: &GuardedDialogInput,
        _deadline: Instant,
    ) -> Result<bool> {
        bail!(
            "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane"
        )
    }
    fn close_session_until(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_session_until(&self.host, session, deadline)
    }
    fn close_owned_session(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_session_until(&self.host, session, deadline)
    }
}
#[cfg(test)]
mod tests;
