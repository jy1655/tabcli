use super::process;
use crate::native::session::{Reader, Store};
#[cfg(test)]
use crate::native::terminal::ownership;
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
use crate::native::{CommandOutputFailure, shell_quote};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
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
    let mut command = Command::new(if bundled.is_file() {
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
        crate::native::command_output_with_stdin_until_classified(
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

fn panes(host: &dyn Host, mux: &WezTermMux, deadline: Instant) -> Result<Vec<Pane>> {
    let listing = call(host, mux, &["list", "--format", "json"], None, deadline)
        .map_err(CommandOutputFailure::into_error)?;
    serde_json::from_str(&listing).context("WezTerm returned an unreadable pane list")
}

// The panes of the session's process, `None` when the process is gone.
fn listed(host: &dyn Host, mux: &WezTermMux, deadline: Instant) -> Result<Option<Vec<Pane>>> {
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
                Err(error) => waiting_for = error,
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
fn existing_window(host: &dyn Host, deadline: Instant) -> Result<(WezTermMux, Vec<Pane>, u64)> {
    let guis = host.discover_guis()?;
    let mut targets = Vec::new();
    for mut mux in guis {
        mux.owns_gui = false;
        let snapshot = listed(host, &mux, deadline)?.context("discovered GUI ended")?;
        let mut ids = BTreeSet::new();
        if snapshot.iter().any(|pane| !ids.insert(pane.pane_id)) {
            bail!("existing GUI returned duplicate pane identities");
        }
        let windows: BTreeSet<_> = snapshot.iter().map(|pane| pane.window_id).collect();
        for window in windows {
            targets.push((mux.clone(), snapshot.clone(), window));
        }
    }
    if targets.len() != 1 {
        bail!(
            "{} verified local WezTerm windows; no unique tab target",
            targets.len()
        );
    }
    let target = targets.pop().unwrap();
    let help = call(host, &target.0, &["spawn", "--help"], None, deadline)
        .map_err(CommandOutputFailure::into_error)?;
    if !help.contains("--window-id") || !help.contains("--domain-name") {
        bail!("installed WezTerm CLI does not support an explicitly addressed local tab spawn");
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
    let executable = env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let gate = format!(
        "{} native-wezterm-host {}",
        shell_quote(executable.as_os_str()),
        shell_quote(directory.as_os_str())
    );
    create(
        host,
        force_new_window,
        &pane_program(&gate, command),
        deadline,
    )
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
        Err(reason) => {
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
    crate::native::require_valid_session_id(id)?;
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
    use crate::native::launch::{BindingHost, wait_for_binding};
    let surface = wait_for_binding(&Reader::open_unchecked(directory), id, BindingHost::WezTerm)?;
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

pub(super) fn close_session(host: &dyn Host, session: &TerminalSession) -> Result<CloseOutcome> {
    close_session_until(host, session, super::timeout_deadline(CLOSE_TIMEOUT)?)
}

// Every step reads the state again, so a close that failed part of the way is finished by
// the next one: a pane that is gone is not killed again, and a process that was left
// without panes is ended.
pub(super) fn close_session_until(
    host: &dyn Host,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let (mux, pane) = target(session)?;
    let mut outcome = CloseOutcome::Missing;
    let mut kill: Option<Result<()>> = None;
    let mut without_panes_since: Option<Instant> = None;
    let mut asked_to_end = false;
    loop {
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
                    host.terminate(mux.pid)?;
                }
                anyhow!(
                    "WezTerm process {} of this session is still running without a pane",
                    mux.pid
                )
            }
            // The process can be ending: its socket closes before it is gone.
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(waiting_for);
        }
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use crate::native::session::SessionState;

    #[test]
    fn host_binding_partial_transitions() {
        crate::native::launch::binding_tests::characterize("WezTerm", |directory, id| {
            wait_for_binding(directory, id, "host-id", Path::new("/tmp/binding-socket"))
        });
    }

    use std::{
        cell::RefCell, os::unix::process::ExitStatusExt, path::PathBuf, process::ExitStatus,
    };

    use super::*;

    const SOCKET: &str = "/Users/tester/.local/share/wezterm/gui-sock-4242";
    const PID: u32 = 4242;
    const BIRTH: (u64, u64) = (1_790_000_000, 123_456);

    // The program of a pane in these tests: a gate and a launch command, as in a launch.
    fn program() -> [String; 5] {
        pane_program("gate", ". '/tmp/s/launch.sh'")
    }

    fn create_tab(host: &Fake, deadline: Instant) -> Result<TerminalSession> {
        create(host, false, &program(), deadline)
    }

    // One element of `wezterm cli list-clients --format json`: every field of
    // `CliListClientsResultItem`.
    fn client(pid: u32, focused: Option<u64>) -> serde_json::Value {
        serde_json::json!({
            "username": "tester",
            "hostname": "host",
            "pid": pid,
            "connection_elapsed": {"secs": 9, "nanos": 0},
            "idle_time": {"secs": 1, "nanos": 0},
            "workspace": "default",
            "focused_pane_id": focused,
        })
    }

    // One element of `wezterm cli list --format json`: every field of `CliListResultItem`.
    fn item(pane: u64, tty: bool) -> serde_json::Value {
        serde_json::json!({
            "window_id": pane,
            "tab_id": pane,
            "pane_id": pane,
            "workspace": "default",
            "size": {"rows": 24, "cols": 80, "pixel_width": 1440, "pixel_height": 864, "dpi": 144},
            "title": "zsh",
            "cwd": "file://host/Users/tester",
            "cursor_x": 2,
            "cursor_y": 0,
            "cursor_shape": "Default",
            "cursor_visibility": "Visible",
            "left_col": 0,
            "top_row": 0,
            "tab_title": "",
            "window_title": "zsh",
            "is_active": true,
            "is_zoomed": false,
            "tty_name": tty.then(|| format!("/dev/ttys{pane:03}")),
        })
    }

    // A WezTerm GUI process as the release's source describes it: it serves its socket
    // once it listens, opens one window, numbers panes upwards, answers "no such pane"
    // for an id that is not there, and ends after its last window.
    #[derive(Default)]
    struct Mux {
        existing_gui: bool,
        discovery_error: bool,
        discovery_duplicates: bool,
        spawn_count: usize,
        spawn_reply: Option<String>,
        after_spawn_listing: Option<String>,
        reuse_after_spawn: bool,
        unsupported_spawn: bool,
        pane_windows: std::collections::BTreeMap<u64, u64>,
        started: usize,
        start_fails: bool,
        // The birth of process 4242 while it lives, and who serves its socket.
        birth: Option<(u64, u64)>,
        foreign_server: Option<u32>,
        unreadable_process: bool,
        // How often the socket is asked for before the process listens, and how often
        // the panes are listed before the window is there.
        polls_before_listening: usize,
        listings_before_window: usize,
        // What the process opens when it starts.
        startup_panes: Vec<u64>,
        without_tty: bool,
        panes: Vec<u64>,
        // `quit_when_all_windows_are_closed = false`, and a process that ignores the
        // request to end.
        stays_without_panes: bool,
        ignores_terminate: bool,
        terminated: Vec<u32>,
        removed: Vec<String>,
        // After `kill-pane` answered, the pane is listed this many times more.
        listings_after_kill: usize,
        dying: Vec<(u64, usize)>,
        // A command named here fails once: before it started, with an error answer, or
        // after it took effect, so that only the answer is lost.
        not_started: Vec<&'static str>,
        refused: Vec<&'static str>,
        answer_lost: Vec<&'static str>,
        calls: Vec<(String, Vec<String>, Option<Vec<u8>>)>,
        // The program that the process was started with, and the one of the spawned tab.
        gui_program: Option<Vec<String>>,
        spawn_program: Option<Vec<String>>,
        // The pane that the window presents: a spawn and `activate-pane` select it in any
        // window (mux/src/lib.rs, `spawn_tab_or_window`; `SetFocusedPane`).
        active: Option<u64>,
        // Whether the window of the GUI has the keyboard, and the pane that the GUI's own
        // client reports as focused: the one it painted last while its window had the
        // keyboard (wezterm-gui/src/termwindow/render/paint.rs). The record follows the
        // presented pane only in a focused window.
        window_focused: bool,
        focused: Option<u64>,
        // How many `list-clients` answers after a spawn still show the old record, because
        // the GUI has not painted the new tab yet.
        paints_after: usize,
        painting: Option<(u64, usize)>,
        // What `list-clients` answers after a spawn, instead of the record.
        after_spawn_clients: Option<String>,
    }

    struct Fake(RefCell<Mux>);

    fn fake() -> Fake {
        Fake(RefCell::new(Mux {
            startup_panes: vec![0],
            ..Mux::default()
        }))
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn take(commands: &mut Vec<&'static str>, command: &str) -> bool {
        match commands.iter().position(|named| *named == command) {
            Some(index) => {
                commands.remove(index);
                true
            }
            None => false,
        }
    }

    impl Host for Fake {
        fn discover_guis(&self) -> Result<Vec<WezTermMux>> {
            let mux = self.0.borrow();
            if mux.discovery_error {
                bail!("local GUI discovery unreadable");
            }
            let mut found = if mux.existing_gui {
                vec![WezTermMux {
                    socket: SOCKET.to_owned(),
                    pid: PID,
                    start_seconds: BIRTH.0,
                    start_microseconds: BIRTH.1,
                    owns_gui: false,
                }]
            } else {
                Vec::new()
            };
            if mux.discovery_duplicates {
                found.extend(found.clone());
            }
            Ok(found)
        }
        fn start_gui(&self, program: &[String]) -> Result<WezTermMux> {
            let mut mux = self.0.borrow_mut();
            if mux.start_fails {
                bail!("failed to start WezTerm");
            }
            mux.started += 1;
            mux.gui_program = Some(program.to_vec());
            mux.birth = Some(BIRTH);
            mux.panes = mux.startup_panes.clone();
            Ok(WezTermMux {
                socket: SOCKET.to_owned(),
                pid: PID,
                start_seconds: BIRTH.0,
                start_microseconds: BIRTH.1,
                owns_gui: true,
            })
        }

        fn socket_server(&self, socket: &str) -> Result<Option<u32>> {
            let mut mux = self.0.borrow_mut();
            assert_eq!(socket, SOCKET);
            if mux.polls_before_listening > 0 {
                mux.polls_before_listening -= 1;
                return Ok(None);
            }
            Ok(mux.foreign_server.or(mux.birth.map(|_| PID)))
        }

        fn process_start(&self, pid: u32) -> Result<Option<(u64, u64)>> {
            let mux = self.0.borrow();
            if mux.unreadable_process {
                bail!("failed to inspect process {pid}");
            }
            Ok(mux.birth.filter(|_| pid == PID))
        }

        fn terminate(&self, pid: u32) -> Result<()> {
            let mut mux = self.0.borrow_mut();
            mux.terminated.push(pid);
            if !mux.ignores_terminate {
                mux.birth = None;
                mux.panes.clear();
            }
            Ok(())
        }

        fn remove_socket(&self, socket: &str) {
            self.0.borrow_mut().removed.push(socket.to_owned());
        }

        fn cli(
            &self,
            socket: &str,
            arguments: &[&str],
            input: Option<&[u8]>,
            _deadline: Instant,
        ) -> std::result::Result<Output, CommandOutputFailure> {
            let mut mux = self.0.borrow_mut();
            assert_eq!(socket, SOCKET);
            assert!(
                mux.foreign_server.is_none(),
                "a socket that another process serves got a call"
            );
            mux.calls.push((
                socket.to_owned(),
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
                input.map(<[u8]>::to_vec),
            ));
            let command = arguments[0];
            if take(&mut mux.not_started, command) {
                return Err(CommandOutputFailure::not_started(anyhow!(
                    "failed to start WezTerm CLI"
                )));
            }
            if take(&mut mux.refused, command) {
                return Ok(output(1, "", "failed to connect to the mux"));
            }
            let pane = arguments
                .iter()
                .position(|argument| *argument == "--pane-id")
                .map(|index| arguments[index + 1].parse::<u64>().unwrap());
            let answer = match command {
                "spawn" if arguments.contains(&"--help") => output(
                    0,
                    if mux.unsupported_spawn {
                        "old spawn"
                    } else {
                        "--window-id --domain-name"
                    },
                    "",
                ),
                "spawn" => {
                    assert_eq!(arguments[1], "--window-id");
                    assert_eq!(&arguments[3..6], ["--domain-name", "local", "--"]);
                    mux.spawn_program =
                        Some(arguments[6..].iter().map(|a| (*a).to_owned()).collect());
                    let window = arguments[2].parse::<u64>().unwrap();
                    let id = mux.panes.iter().max().copied().unwrap_or(0) + 1;
                    mux.panes.push(id);
                    mux.pane_windows.insert(id, window);
                    mux.spawn_count += 1;
                    mux.active = Some(id);
                    if mux.window_focused {
                        if mux.paints_after == 0 {
                            mux.focused = Some(id);
                        } else {
                            mux.painting = Some((id, mux.paints_after));
                        }
                    }
                    if mux.reuse_after_spawn {
                        mux.birth = Some((BIRTH.0 + 1, 0));
                    }
                    output(
                        0,
                        mux.spawn_reply.as_deref().unwrap_or(&format!("{id}\n")),
                        "",
                    )
                }
                "list" => {
                    assert_eq!(arguments, ["list", "--format", "json"]);
                    if mux.spawn_count > 0 && mux.after_spawn_listing.is_some() {
                        return Ok(output(0, mux.after_spawn_listing.as_deref().unwrap(), ""));
                    }
                    if mux.listings_before_window > 0 {
                        mux.listings_before_window -= 1;
                        output(0, "[]", "")
                    } else {
                        let before = mux.panes.len();
                        let mut dying = std::mem::take(&mut mux.dying);
                        dying.retain_mut(|(id, listings)| {
                            if *listings == 0 {
                                mux.panes.retain(|pane| pane != id);
                                false
                            } else {
                                *listings -= 1;
                                true
                            }
                        });
                        mux.dying = dying;
                        if mux.panes.is_empty() && before > 0 && !mux.stays_without_panes {
                            mux.birth = None;
                        }
                        let listing: Vec<_> = mux
                            .panes
                            .iter()
                            .map(|pane| {
                                let mut item = item(*pane, !mux.without_tty);
                                if let Some(window) = mux.pane_windows.get(pane) {
                                    item["window_id"] = serde_json::json!(window);
                                }
                                item
                            })
                            .collect();
                        output(0, &serde_json::to_string_pretty(&listing).unwrap(), "")
                    }
                }
                "list-clients" => {
                    assert_eq!(arguments, ["list-clients", "--format", "json"]);
                    if mux.spawn_count > 0 && mux.after_spawn_clients.is_some() {
                        return Ok(output(0, mux.after_spawn_clients.as_deref().unwrap(), ""));
                    }
                    match mux.painting.take() {
                        Some((pane, 0)) => mux.focused = Some(pane),
                        Some((pane, answers)) => mux.painting = Some((pane, answers - 1)),
                        None => {}
                    }
                    // The GUI, and another client of its mux that is not the GUI.
                    let clients =
                        serde_json::json!([client(PID, mux.focused), client(99_999, None)]);
                    output(0, &serde_json::to_string_pretty(&clients).unwrap(), "")
                }
                "activate-pane" => {
                    assert_eq!(arguments.len(), 3);
                    let id = pane.expect("a call for one pane names it");
                    if !mux.panes.contains(&id) {
                        output(1, "", &format!("pane {id} not found"))
                    } else {
                        mux.active = Some(id);
                        if mux.window_focused {
                            mux.painting = None;
                            mux.focused = Some(id);
                        }
                        output(0, "", "")
                    }
                }
                "kill-pane" | "send-text" | "get-text" => {
                    let id = pane.expect("a call for one pane names it");
                    if !mux.panes.contains(&id) {
                        output(1, "", &format!("no such pane {id}"))
                    } else if command == "kill-pane" {
                        let listings = mux.listings_after_kill;
                        mux.dying.push((id, listings));
                        output(0, "", "")
                    } else if command == "get-text" {
                        output(0, "$ \n\n", "")
                    } else {
                        output(0, "", "")
                    }
                }
                other => panic!("the adapter must not call wezterm cli {other}"),
            };
            if !arguments.contains(&"--help") && take(&mut mux.answer_lost, command) {
                return Err(CommandOutputFailure::started(anyhow!(
                    "WezTerm CLI timed out"
                )));
            }
            Ok(answer)
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn shortly() -> Instant {
        Instant::now() + Duration::from_millis(700)
    }

    fn commands(host: &Fake) -> Vec<String> {
        host.0
            .borrow()
            .calls
            .iter()
            .map(|(_, arguments, _)| arguments[0].clone())
            .collect()
    }

    fn session(host: &Fake) -> TerminalSession {
        let session = create_tab(host, soon()).unwrap();
        host.0.borrow_mut().calls.clear();
        session
    }

    #[test]
    fn tab_first_uses_a_new_tab_in_the_existing_gui_and_preserves_its_siblings() {
        let host = fake();
        {
            let mut mux = host.0.borrow_mut();
            mux.existing_gui = true;
            mux.birth = Some(BIRTH);
            mux.panes = vec![7];
        }
        let handle = create_tab(&host, soon()).unwrap();
        assert_eq!(host.0.borrow().started, 0, "an existing GUI must get a tab");
        assert_ne!(handle.id, "7", "a user pane must never be adopted");
        close_session_until(&host, &handle, soon()).unwrap();
        let mux = host.0.borrow();
        assert_eq!(mux.panes, [7]);
        assert_eq!(mux.birth, Some(BIRTH));
        assert!(mux.terminated.is_empty());
        assert!(mux.removed.is_empty());
    }

    fn existing() -> Fake {
        let host = fake();
        {
            let mut mux = host.0.borrow_mut();
            mux.existing_gui = true;
            mux.birth = Some(BIRTH);
            mux.panes = vec![7];
        }
        host
    }

    #[test]
    fn multiple_existing_panes_in_one_window_still_allow_a_new_local_tab() {
        let host = existing();
        {
            let mut mux = host.0.borrow_mut();
            mux.panes.push(8);
            mux.pane_windows.insert(8, 7);
        }
        let handle = create_tab(&host, soon()).unwrap();
        assert_eq!(handle.id, "9");
        assert_eq!(handle.window_id.as_deref(), Some("7"));
        assert_eq!(host.0.borrow().started, 0);
        close_session_until(&host, &handle, soon()).unwrap();
        assert_eq!(host.0.borrow().panes, [7, 8]);
        assert_eq!(host.0.borrow().birth, Some(BIRTH));
    }

    #[test]
    fn a_user_closing_an_unrelated_pane_during_spawn_does_not_disown_the_new_tab() {
        let host = existing();
        let mut created = item(8, true);
        created["window_id"] = serde_json::json!(7);
        host.0.borrow_mut().after_spawn_listing = Some(serde_json::json!([created]).to_string());
        let handle = create_tab(&host, soon()).unwrap();
        assert_eq!(handle.id, "8");
        assert_eq!(handle.tab_id.as_deref(), Some("8"));
        assert_eq!(handle.window_id.as_deref(), Some("7"));
        let mux = host.0.borrow();
        assert_eq!(mux.spawn_count, 1);
        assert_eq!(mux.started, 0);
        assert!(mux.terminated.is_empty());
        assert!(mux.removed.is_empty());
        assert!(!handle.wezterm_mux.unwrap().owns_gui);
    }

    #[test]
    fn shared_gui_is_not_ended_or_unlinked_even_after_its_last_pane_is_gone() {
        let host = existing();
        let handle = create_tab(&host, soon()).unwrap();
        assert_eq!(handle.id, "8");
        assert_eq!(handle.tab_id.as_deref(), Some("8"));
        assert_eq!(handle.window_id.as_deref(), Some("7"));
        assert!(!handle.wezterm_mux.as_ref().unwrap().owns_gui);
        assert_eq!(handle.wezterm_mux.as_ref().unwrap().start_seconds, BIRTH.0);
        {
            let mut mux = host.0.borrow_mut();
            mux.panes = vec![8]; // user closed their own sibling
            mux.stays_without_panes = true;
        }
        assert_eq!(
            close_session_until(&host, &handle, shortly()).unwrap(),
            CloseOutcome::Closed
        );
        let mux = host.0.borrow();
        assert!(mux.panes.is_empty());
        assert_eq!(mux.birth, Some(BIRTH));
        assert!(mux.terminated.is_empty());
        assert!(mux.removed.is_empty());
        drop(mux);
        assert_eq!(
            close_session_until(&host, &handle, shortly()).unwrap(),
            CloseOutcome::Missing
        );
        host.0.borrow_mut().birth = None;
        close_session_until(&host, &handle, shortly()).unwrap();
        assert!(host.0.borrow().removed.is_empty());
    }

    #[test]
    fn a_spawn_response_must_prove_a_fresh_pane_tab_window_and_same_gui_birth() {
        type Arrange = fn(&mut Mux);
        let cases: [(&str, Arrange); 9] = [
            ("old pane", |m| m.spawn_reply = Some("7\n".into())),
            ("wrong pane", |m| m.spawn_reply = Some("99\n".into())),
            ("duplicate reply", |m| m.spawn_reply = Some("8\n8\n".into())),
            ("duplicate listing", |m| {
                m.after_spawn_listing =
                    Some(serde_json::json!([item(8, true), item(8, true)]).to_string())
            }),
            ("wrong window", |m| {
                m.after_spawn_listing =
                    Some(serde_json::json!([item(7, true), item(8, true)]).to_string())
            }),
            ("old tab", |m| {
                let mut pane = item(8, true);
                pane["window_id"] = serde_json::json!(7);
                pane["tab_id"] = serde_json::json!(7);
                m.after_spawn_listing = Some(serde_json::json!([item(7, true), pane]).to_string());
            }),
            ("unsafe tty", |m| {
                let mut pane = item(8, true);
                pane["window_id"] = serde_json::json!(7);
                pane["tty_name"] = serde_json::json!("/tmp/remote-tty");
                m.after_spawn_listing = Some(serde_json::json!([item(7, true), pane]).to_string());
            }),
            ("reused GUI", |m| m.reuse_after_spawn = true),
            ("unreadable listing", |m| {
                m.after_spawn_listing = Some("malformed".into())
            }),
        ];
        for (name, arrange) in cases {
            let host = existing();
            arrange(&mut host.0.borrow_mut());
            let error = create_tab(&host, soon()).unwrap_err();
            assert!(
                format!("{error:#}").contains("creation is uncertain"),
                "{name}: {error:#}"
            );
            let mux = host.0.borrow();
            assert_eq!(mux.spawn_count, 1, "{name}");
            assert_eq!(mux.started, 0, "{name}");
            assert_eq!(mux.panes, [7, 8], "{name}");
            assert!(mux.terminated.is_empty());
            assert!(mux.removed.is_empty());
            assert!(!commands(&host).contains(&"kill-pane".into()));
        }
    }

    #[test]
    fn a_lost_or_malformed_spawn_response_never_retries_or_cleans_up_a_delta() {
        for lost in [true, false] {
            let host = existing();
            {
                let mut mux = host.0.borrow_mut();
                if lost {
                    mux.answer_lost.push("spawn");
                } else {
                    mux.spawn_reply = Some("not a pane id".into());
                }
            }
            let error = create_tab(&host, shortly()).unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("no retry, fallback or pane cleanup"),
                "{text}"
            );
            assert!(text.contains(SOCKET), "{text}");
            assert!(text.contains("requested_window_id"), "{text}");
            let mux = host.0.borrow();
            assert_eq!(mux.spawn_count, 1);
            assert_eq!(mux.started, 0);
            assert_eq!(mux.panes, [7, 8]);
            assert!(mux.terminated.is_empty());
            assert!(mux.removed.is_empty());
        }
    }

    #[test]
    fn unavailable_unsupported_or_ambiguous_target_falls_back_before_any_spawn() {
        type Arrange = fn(&mut Mux);
        let cases: [Arrange; 5] = [
            |m| m.existing_gui = false,
            |m| m.panes.clear(),
            |m| m.panes = vec![7, 9],
            |m| m.discovery_error = true,
            |m| m.unsupported_spawn = true,
        ];
        for arrange in cases {
            let host = existing();
            arrange(&mut host.0.borrow_mut());
            let handle = create_tab(&host, soon()).unwrap();
            assert!(handle.wezterm_mux.unwrap().owns_gui);
            assert_eq!(host.0.borrow().started, 1);
            assert_eq!(host.0.borrow().spawn_count, 0);
        }
    }

    #[test]
    fn explicit_opening_mode_records_scope_without_a_later_policy_read() {
        let host = existing();
        let shared = create(&host, false, &program(), soon()).unwrap();
        let persisted = serde_json::to_string(&shared).unwrap();
        let shared: TerminalSession = serde_json::from_str(&persisted).unwrap();
        // No current settings field is passed to close: the saved creation scope wins.
        assert!(!shared.wezterm_mux.as_ref().unwrap().owns_gui);
        close_session_until(&host, &shared, soon()).unwrap();
        assert_eq!(host.0.borrow().panes, [7]);
        assert_eq!(host.0.borrow().birth, Some(BIRTH));
        assert!(host.0.borrow().terminated.is_empty());
        let host = existing();
        let private = create(&host, true, &program(), soon()).unwrap();
        assert!(private.wezterm_mux.as_ref().unwrap().owns_gui);
        assert_eq!(host.0.borrow().started, 1);
        assert_eq!(host.0.borrow().spawn_count, 0);
        close_session_until(&host, &private, soon()).unwrap();
        assert_eq!(host.0.borrow().birth, None);
    }

    #[test]
    fn persisted_scope_survives_settings_changes_after_launch() {
        use crate::native::settings::{MacosOpenMode, macos_open_mode};

        for initial in [MacosOpenMode::TabFirst, MacosOpenMode::NewWindow] {
            let root = tempfile::tempdir().unwrap();
            let setting = root.path().join("settings.json");
            let save_mode = |mode: MacosOpenMode| {
                crate::native::write_json_atomic(
                    &setting,
                    &serde_json::json!({"schema": 1, "macos_open_mode": mode.as_str()}),
                )
                .unwrap();
            };
            if initial == MacosOpenMode::NewWindow {
                save_mode(initial);
            }
            let selected = macos_open_mode(root.path()).unwrap();
            assert_eq!(selected, initial);
            let host = existing();
            host.0.borrow_mut().stays_without_panes = true;
            let handle = create(
                &host,
                selected == MacosOpenMode::NewWindow,
                &program(),
                soon(),
            )
            .unwrap();
            let saved_handle = root.path().join("terminal.json");
            crate::native::write_json_atomic(&saved_handle, &handle).unwrap();

            let later = if initial == MacosOpenMode::TabFirst {
                MacosOpenMode::NewWindow
            } else {
                MacosOpenMode::TabFirst
            };
            save_mode(later);
            assert_eq!(macos_open_mode(root.path()).unwrap(), later);
            let handle: TerminalSession = crate::native::read_json(&saved_handle).unwrap();
            assert_eq!(
                handle.wezterm_mux.as_ref().unwrap().owns_gui,
                initial == MacosOpenMode::NewWindow
            );
            close_session_until(&host, &handle, soon()).unwrap();
            let mux = host.0.borrow();
            if initial == MacosOpenMode::TabFirst {
                assert_eq!(mux.panes, [7]);
                assert_eq!(mux.birth, Some(BIRTH));
                assert!(mux.terminated.is_empty());
                assert!(mux.removed.is_empty());
            } else {
                assert!(mux.panes.is_empty());
                assert_eq!(mux.birth, None);
                assert_eq!(mux.terminated, [PID]);
            }
        }
    }

    #[test]
    fn reused_gui_birth_never_reports_a_successful_close() {
        for force_new_window in [false, true] {
            let host = existing();
            let handle = create(&host, force_new_window, &program(), soon()).unwrap();
            {
                let mut mux = host.0.borrow_mut();
                mux.birth = Some((BIRTH.0 + 60, 0));
                mux.calls.clear();
            }
            let result = close_session_until(&host, &handle, shortly());
            assert!(
                result.is_err(),
                "reused identity was reported as {result:?}"
            );
            let mux = host.0.borrow();
            assert!(mux.calls.is_empty());
            assert!(mux.terminated.is_empty());
            assert!(mux.removed.is_empty());
        }
    }

    #[test]
    fn shared_handles_never_address_a_wrong_tab_window_or_unreadable_process() {
        let prompt = tempfile::NamedTempFile::new().unwrap();
        fs::write(prompt.path(), "prompt").unwrap();
        let host = existing();
        let mut handle = create_tab(&host, soon()).unwrap();
        host.0.borrow_mut().calls.clear();
        handle.window_id = Some("99".into());
        let failure = send_file(&host, &handle, prompt.path(), shortly()).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert!(read_screen(&host, &handle, shortly()).is_err());
        assert!(close_session_until(&host, &handle, shortly()).is_err());
        assert!(verify_session(&host, &handle, None).is_err());
        assert!(
            !commands(&host)
                .iter()
                .any(|c| c == "kill-pane" || c == "send-text" || c == "get-text")
        );
        host.0.borrow_mut().unreadable_process = true;
        assert!(close_session_until(&host, &handle, shortly()).is_err());
        assert!(host.0.borrow().terminated.is_empty());
        assert!(host.0.borrow().removed.is_empty());
    }

    #[test]
    fn discovery_only_accepts_pid_named_protected_local_sockets() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("gui-sock-42");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert_eq!(socket_pid(&socket), Some(42));
        assert!(safe_socket_path(&socket).is_ok());
        for name in [
            "default-WezTerm",
            "gui-sock-0",
            "gui-sock-42-other",
            "mux-server",
        ] {
            assert_eq!(socket_pid(Path::new(name)), None);
        }
        let alias = dir.path().join("gui-sock-43");
        std::os::unix::fs::symlink(&socket, &alias).unwrap();
        assert!(safe_socket_path(&alias).is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(safe_socket_path(&socket).is_err());
    }

    #[test]
    fn a_session_gets_a_process_of_its_own_and_the_pane_that_process_opened() {
        let host = fake();
        {
            let mut mux = host.0.borrow_mut();
            mux.polls_before_listening = 2;
            mux.listings_before_window = 2;
        }

        let session = create_tab(&host, soon()).unwrap();

        assert_eq!(session.kind, TerminalKind::WezTerm);
        assert_eq!(session.id, "0");
        assert_eq!(
            session.wezterm_mux,
            Some(WezTermMux {
                socket: SOCKET.to_owned(),
                pid: PID,
                start_seconds: BIRTH.0,
                start_microseconds: BIRTH.1,
                owns_gui: true,
            })
        );
        let mux = host.0.borrow();
        assert_eq!(mux.started, 1);
        assert!(mux.terminated.is_empty());
        assert_eq!(mux.gui_program.as_deref(), Some(&program()[..]));
        // Nothing is created or selected in the process afterwards: its own pane is the
        // surface, and it holds no pane of the user.
        assert_eq!(commands(&host), ["list", "list", "list"]);
    }

    #[test]
    fn the_started_process_and_every_call_take_nothing_from_the_caller() {
        // What a pane of WezTerm, or a shell that kept the variables of one, passes on.
        let inherited = [
            "WEZTERM_UNIX_SOCKET",
            "WEZTERM_PANE",
            "WEZTERM_CONFIG_FILE",
            "WEZTERM_EXECUTABLE",
            "PATH",
            "TERM_PROGRAM",
        ];
        for command in [
            gui_command(inherited.iter().map(OsString::from), &program()),
            wezterm_command(inherited.iter().map(OsString::from)),
        ] {
            let removed: Vec<_> = command
                .get_envs()
                .map(|(name, value)| {
                    assert_eq!(value, None);
                    name.to_str().unwrap().to_owned()
                })
                .collect();
            assert_eq!(
                removed,
                [
                    "WEZTERM_CONFIG_FILE",
                    "WEZTERM_EXECUTABLE",
                    "WEZTERM_PANE",
                    "WEZTERM_UNIX_SOCKET"
                ]
            );
        }
        // Never the request to an existing process, never a mux domain of the user, and
        // the program of the pane as arguments of its own.
        let command = gui_command(std::iter::empty(), &program());
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(
            arguments,
            [
                "start",
                "--always-new-process",
                "--no-auto-connect",
                "--",
                "/bin/zsh",
                "-l",
                "-i",
                "-c",
                "gate || exit; . '/tmp/s/launch.sh'"
            ]
        );
        assert_eq!(
            gui_socket("/Users/tester", 4242),
            "/Users/tester/.local/share/wezterm/gui-sock-4242"
        );
    }

    #[test]
    fn explicit_target_dispatch_and_startup_ignore_the_invoking_host() {
        const CASE: &str = "AGENT_BRIDGE_WEZTERM_HOST_FIXTURE";
        const TEST: &str = "native::terminal::macos::wezterm::tests::explicit_target_dispatch_and_startup_ignore_the_invoking_host";
        let cases = [
            ("native", Some("WezTerm")),
            ("iterm", Some("iTerm.app")),
            ("terminal", Some("Apple_Terminal")),
            ("warp", Some("WarpTerminal")),
            ("empty", None),
            ("unknown", Some("unknown")),
            ("conflict", Some("iTerm.app")),
        ];
        if env::var_os(CASE).is_none() {
            for (name, program) in cases {
                let mut child = Command::new(env::current_exe().unwrap());
                child.args(["--exact", TEST, "--test-threads=1"]);
                child.env(CASE, name);
                for (key, _) in env::vars_os() {
                    if key.to_string_lossy().starts_with("WEZTERM_") {
                        child.env_remove(key);
                    }
                }
                for key in [
                    "TERM_PROGRAM",
                    "TERM",
                    "ITERM_SESSION_ID",
                    "TERM_SESSION_ID",
                ] {
                    child.env_remove(key);
                }
                if let Some(program) = program {
                    child.env("TERM_PROGRAM", program);
                }
                if name == "native" || name == "conflict" {
                    child.env("WEZTERM_UNIX_SOCKET", "/tmp/foreign-remote-mux");
                    child.env("WEZTERM_PANE", "77");
                    child.env("WEZTERM_CONFIG_FILE", "/tmp/foreign-config");
                    child.env("ITERM_SESSION_ID", "caller-session");
                    child.env("TERM_SESSION_ID", "caller-terminal");
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{name}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let selected = crate::native::terminal::select(Some(TerminalKind::WezTerm)).unwrap();
        let host = fake();
        let handle = match selected {
            TerminalKind::WezTerm => create_tab(&host, soon()).unwrap(),
            other => panic!("explicit target dispatched to {other:?}"),
        };
        let command = gui_command(env::vars_os().map(|(key, _)| key), &program());
        assert!(matches!(
            command.get_program().to_str().unwrap(),
            "/Applications/WezTerm.app/Contents/MacOS/wezterm" | "wezterm"
        ));
        assert_eq!(
            command.get_args().take(4).collect::<Vec<_>>(),
            ["start", "--always-new-process", "--no-auto-connect", "--"]
        );
        for (key, _) in
            env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("WEZTERM_"))
        {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == key && value.is_none())
            );
        }
        let mux = handle.wezterm_mux.as_ref().unwrap();
        assert_eq!(mux.socket, SOCKET);
        assert_eq!(mux.pid, PID);
        assert_eq!(
            close_session_until(&host, &handle, soon()).unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(host.0.borrow().started, 1);
        assert_eq!(host.0.borrow().birth, None);
        // The very same host hints must not suppress the existing-GUI tab path.
        let host = existing();
        let handle = create_tab(&host, soon()).unwrap();
        assert!(!handle.wezterm_mux.as_ref().unwrap().owns_gui);
        close_session_until(&host, &handle, soon()).unwrap();
        assert_eq!(host.0.borrow().panes, [7]);
        assert_eq!(host.0.borrow().birth, Some(BIRTH));
    }

    #[test]
    fn startup_failure_cleans_only_the_private_process_without_sibling_panes() {
        type Arrange = fn(&mut Mux);
        let cases: [(&str, Arrange); 5] = [
            ("opened 2 panes", |mux| mux.startup_panes = vec![0, 1]),
            ("no local tty", |mux| mux.without_tty = true),
            ("opened no window", |mux| mux.startup_panes = vec![]),
            ("did not listen", |mux| {
                mux.polls_before_listening = usize::MAX
            }),
            ("is served by process 999", |mux| {
                mux.foreign_server = Some(999)
            }),
        ];
        for (reason, arrange) in cases {
            let host = fake();
            arrange(&mut host.0.borrow_mut());

            let error = create_tab(&host, shortly()).unwrap_err();

            assert!(format!("{error:#}").contains(reason), "{error:#}");
            let mux = host.0.borrow();
            assert_eq!(mux.started, 1, "{reason}");
            if mux.startup_panes.len() > 1 {
                assert!(format!("{error:#}").contains("sibling panes"));
                assert!(mux.terminated.is_empty());
                assert!(mux.removed.is_empty());
                assert_eq!(mux.birth, Some(BIRTH));
                continue;
            }
            assert_eq!(mux.terminated, [PID], "{reason}");
            assert_eq!(mux.birth, None, "{reason}");
            // A socket that another process serves is not this session's to remove.
            assert_eq!(
                mux.removed.is_empty(),
                mux.foreign_server.is_some(),
                "{reason}"
            );
        }

        // A process that could not be started leaves nothing to end.
        let host = fake();
        host.0.borrow_mut().start_fails = true;
        assert!(create_tab(&host, shortly()).is_err());
        assert!(host.0.borrow().terminated.is_empty());
        assert!(host.0.borrow().calls.is_empty());

        // A process that does not end is named.
        let host = fake();
        {
            let mut mux = host.0.borrow_mut();
            mux.without_tty = true;
            mux.ignores_terminate = true;
        }
        let error = create_tab(&host, shortly()).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not be ended"),
            "{error:#}"
        );
    }

    // Issue #58 with WezTerm in front. `cli spawn` makes the new tab the active one of its
    // window (mux/src/lib.rs, `spawn_tab_or_window`), so in a focused window the keys go to
    // the new pane until the pane that had the keyboard is selected again.
    #[test]
    fn a_new_tab_gives_the_keyboard_back_to_the_pane_that_had_it() {
        // In the second run the GUI paints the new tab two answers later.
        for paints_after in [0, 2] {
            let host = existing();
            {
                let mut mux = host.0.borrow_mut();
                mux.window_focused = true;
                mux.focused = Some(7);
                mux.paints_after = paints_after;
            }
            let handle = create_tab(&host, soon()).unwrap();
            assert_eq!(handle.id, "8");
            assert_eq!(
                host.0.borrow().focused,
                Some(7),
                "the new tab kept the keyboard"
            );
            let mux = host.0.borrow();
            assert_eq!(mux.active, Some(7));
            // The focus is read right before the spawn, and the pane is selected once.
            let calls = commands(&host);
            let spawn = calls.iter().rposition(|call| call == "spawn").unwrap();
            assert_eq!(calls[spawn - 1], "list-clients");
            assert_eq!(
                mux.calls.last().unwrap().1,
                ["activate-pane", "--pane-id", "7"]
            );
            assert_eq!(
                calls.iter().filter(|call| *call == "activate-pane").count(),
                1
            );
        }
    }

    #[test]
    fn the_keyboard_is_given_back_on_the_gui_s_own_evidence_only() {
        type Arrange = fn(&mut Mux);
        // What the GUI answers after the spawn when the fake's record is not used.
        fn gui(focused: Option<u64>) -> Option<String> {
            Some(serde_json::json!([client(PID, focused)]).to_string())
        }
        fn created() -> serde_json::Value {
            let mut created = item(8, true);
            created["window_id"] = serde_json::json!(7);
            created
        }
        let cases: [(&str, Arrange); 10] = [
            // Its record does not follow the new tab, and its selected tab is unknown.
            ("a window in the background", |m| m.window_focused = false),
            ("no pane was focused", |m| m.focused = None),
            ("the focus was outside the window", |m| m.focused = Some(99)),
            ("unreadable before the spawn", |m| {
                m.refused.push("list-clients")
            }),
            ("unreadable after the spawn", |m| {
                m.after_spawn_clients = Some("malformed".into())
            }),
            ("the user chose another pane", |m| {
                m.panes.push(9);
                m.pane_windows.insert(9, 7);
                m.after_spawn_clients = gui(Some(9));
            }),
            ("no focus after the spawn", |m| {
                m.after_spawn_clients = gui(None)
            }),
            ("two clients with the pid of the GUI", |m| {
                m.after_spawn_clients = Some(
                    serde_json::json!([client(PID, Some(8)), client(PID, Some(8))]).to_string(),
                )
            }),
            ("the pane is gone", |m| {
                m.after_spawn_listing = Some(serde_json::json!([created()]).to_string())
            }),
            ("the pane was moved to another window", |m| {
                let mut moved = item(7, true);
                moved["window_id"] = serde_json::json!(9);
                m.after_spawn_listing = Some(serde_json::json!([moved, created()]).to_string())
            }),
        ];
        for (name, arrange) in cases {
            let host = existing();
            {
                let mut mux = host.0.borrow_mut();
                mux.window_focused = true;
                mux.focused = Some(7);
                arrange(&mut mux);
            }
            let handle = create_tab(&host, soon()).expect(name);
            assert!(!handle.wezterm_mux.unwrap().owns_gui, "{name}");
            let calls = commands(&host);
            assert!(!calls.contains(&"activate-pane".to_owned()), "{name}");
            let mux = host.0.borrow();
            assert_eq!(mux.spawn_count, 1, "{name}");
            assert_eq!(mux.started, 0, "{name}");
            if name == "a window in the background" {
                // The window still presents the new tab; the launch waited for a record
                // that never came and selected nothing.
                assert_eq!((mux.active, mux.focused), (Some(8), Some(7)));
                let asked = calls.iter().filter(|call| *call == "list-clients");
                assert!(asked.count() >= 3, "{calls:?}");
            }
        }
    }

    #[test]
    fn a_pane_that_cannot_be_selected_again_does_not_fail_the_launch() {
        let host = existing();
        {
            let mut mux = host.0.borrow_mut();
            mux.window_focused = true;
            mux.focused = Some(7);
            mux.refused.push("activate-pane");
        }
        let handle = create_tab(&host, soon()).unwrap();
        assert_eq!(handle.id, "8");
        assert_eq!(commands(&host).last().unwrap(), "activate-pane");
        assert_eq!(host.0.borrow().active, Some(8));
    }

    // What `open_bound_tab` (macos/mod.rs) does with a WezTerm session.
    fn launch(
        host: &Fake,
        force_new_window: bool,
        command: &str,
        directory: &Path,
    ) -> TerminalSession {
        create_surface(host, force_new_window, command, directory, soon()).unwrap()
    }

    // The launch command typed into the pane of the session, if it was typed.
    fn typed(host: &Fake) -> Option<Vec<u8>> {
        host.0
            .borrow()
            .calls
            .iter()
            .find(|(_, arguments, _)| arguments[0] == "send-text")
            .map(|(_, _, input)| input.clone().unwrap())
    }

    // The program of the session's pane: of the spawned tab, or of the started process.
    fn started(host: &Fake) -> Vec<String> {
        let mux = host.0.borrow();
        mux.spawn_program
            .clone()
            .or_else(|| mux.gui_program.clone())
            .unwrap()
    }

    // The gate as the launch writes it: the installed executable and the session directory.
    fn gate(directory: &Path) -> String {
        format!(
            "{} native-wezterm-host {}",
            shell_quote(env::current_exe().unwrap().as_os_str()),
            shell_quote(directory.as_os_str())
        )
    }

    #[test]
    fn the_launch_command_is_never_typed_into_the_pane() {
        for force_new_window in [false, true] {
            let host = existing();
            launch(
                &host,
                force_new_window,
                ". '/tmp/s/launch.sh'",
                Path::new("/tmp/s"),
            );
            assert_eq!(
                typed(&host).map(|input| String::from_utf8_lossy(&input).into_owned()),
                None,
                "the launch command went through the input of the pane"
            );
            // It is the program of the pane, behind the gate of exactly this session.
            assert_eq!(
                started(&host),
                [
                    "/bin/zsh",
                    "-l",
                    "-i",
                    "-c",
                    &format!(
                        "{} || exit; . '/tmp/s/launch.sh'",
                        gate(Path::new("/tmp/s"))
                    )
                ]
            );
            let mux = host.0.borrow();
            assert_eq!(mux.started, usize::from(force_new_window));
            assert_eq!(mux.spawn_count, usize::from(!force_new_window));
        }
    }

    // The gate is a command of the installed executable. Without its route every launch
    // would end in the pane with an unknown command.
    #[test]
    fn the_gate_of_the_pane_is_a_command_that_the_executable_routes_here() {
        use crate::native::{NativeCommand, is_command, parse_args};
        let gate = gate(Path::new("/s/session-a1"));
        assert!(gate.ends_with(" native-wezterm-host '/s/session-a1'"));
        assert!(is_command("native-wezterm-host"));
        assert!(matches!(
            parse_args(["native-wezterm-host", "/s/session-a1"]).unwrap(),
            NativeCommand::WezTermHost { directory } if directory == Path::new("/s/session-a1")
        ));
        assert!(parse_args(["native-wezterm-host", "s/session-a1"]).is_err());
        assert!(parse_args(["native-wezterm-host", "/s/not-a-session"]).is_err());
    }

    // A session directory as a launch leaves it while it waits for its terminal: private,
    // status `launching`, the claim and the pending receipt.
    fn launch_fixture() -> tempfile::TempDir {
        use crate::native::{acquire_turn_claim, launch, update_status};
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .prefix("session-wezterm-")
            .tempdir()
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        launch::begin(
            &crate::native::session::Store::open_unchecked(directory.path()),
            &token,
            Instant::now() + Duration::from_secs(20),
        )
        .unwrap();
        directory
    }

    // What the launcher's bind does with the created surface.
    fn bind(directory: &Path, session: &TerminalSession) {
        let mut session = session.clone();
        session.managed_session_id = directory.file_name().unwrap().to_str().map(str::to_owned);
        crate::native::write_json_atomic(
            &directory.join(crate::native::TERMINAL_HANDLE_FILE),
            &session,
        )
        .unwrap();
    }

    // This test binary in place of the installed executable: the same `run_host`, reached
    // through `pane_probe` instead of `native-wezterm-host`, and the wrapper's own proof.
    fn probe(mode: &str, directory: &Path) -> String {
        format!(
            "AB_WEZTERM_PROBE={mode} AB_WEZTERM_PROBE_DIR={} {} --exact native::terminal::macos::wezterm::tests::pane_probe --nocapture --test-threads=1",
            shell_quote(directory.as_os_str()),
            shell_quote(env::current_exe().unwrap().as_os_str())
        )
    }

    // Runs only as a program of the pane of the tests below.
    #[test]
    fn pane_probe() {
        let Ok(mode) = env::var("AB_WEZTERM_PROBE") else {
            return;
        };
        let directory = PathBuf::from(env::var_os("AB_WEZTERM_PROBE_DIR").unwrap());
        if mode == "host" {
            fs::write(directory.join("host-entered"), b"").unwrap();
            if let Err(error) = run_host(&directory) {
                eprintln!("{error:#}");
                std::process::exit(7);
            }
        } else {
            // The wrapper records itself as the leader of the foreground job of its shell;
            // this fails in a shell that does not run it as a job of its own.
            let id = directory.file_name().unwrap().to_str().unwrap();
            let owner = ownership::current_native_session_owner(id).unwrap();
            let mut queued: libc::c_int = -1;
            assert_eq!(unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) }, 0);
            crate::native::write_json_atomic(
                &directory.join("owner-probe.json"),
                &serde_json::json!({"pid": owner.pid, "queued_input": queued}),
            )
            .unwrap();
        }
    }

    // The human test of 2026-10-03 (iTerm2, same transport): a key typed while the new tab
    // had the keyboard was read in front of the typed launch command, `a. '/…/launch.sh'`,
    // and the provider never started. Replayed on a private pty with what the adapter sent;
    // no terminal application and no key of the machine is involved.
    #[test]
    fn keys_typed_while_the_pane_starts_cannot_change_the_launch_command() {
        for cancelled in [false, true] {
            replay_on_a_private_pty(cancelled);
        }
    }

    fn replay_on_a_private_pty(cancelled: bool) {
        use std::{fs::File, io::Read, os::fd::FromRawFd};
        let fixture = launch_fixture();
        let directory = fixture.path();
        // launch.sh as `launch::install_script` leaves it: sourced, and it ends the shell.
        let script = directory.join("launch.sh");
        fs::write(&script, format!("{}; exit $?\n", probe("owner", directory))).unwrap();
        let command = format!(". {}", shell_quote(script.as_os_str()));
        let host = existing();
        let session = launch(&host, false, &command, directory);
        assert_eq!(
            typed(&host).map(|input| String::from_utf8_lossy(&input).into_owned()),
            None,
            "the launch command went through the input of the pane"
        );
        let mut program = started(&host);
        assert!(program[4].starts_with(&gate(directory)), "{program:?}");
        program[4] = program[4].replacen(&gate(directory), &probe("host", directory), 1);

        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut pane = Command::new(&program[0]);
        pane.args(&program[1..])
            // What the mux gives every pane (mux/src/domain.rs, `build_command`).
            .env("WEZTERM_PANE", &session.id)
            .env("WEZTERM_UNIX_SOCKET", SOCKET)
            // No startup file of the user.
            .env("ZDOTDIR", directory)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // pty/src/unix.rs, `spawn_command`: a session of its own on the pty.
        unsafe {
            pane.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Keys typed before the program of the pane reads anything, an Enter among them.
        master.write_all(b"aaaa\n").unwrap();
        let mut child = pane.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let owner = directory.join("owner-probe.json");
        let mut released = false;
        let mut screen = Vec::new();
        let status = loop {
            let mut bytes = [0; 4096];
            if let Ok(count) = master.read(&mut bytes) {
                screen.extend_from_slice(&bytes[..count]);
            }
            if !released && directory.join("host-entered").exists() {
                // The gate holds for as long as the pane is unbound.
                thread::sleep(Duration::from_millis(300));
                assert!(!owner.exists(), "the wrapper ran before its pane was bound");
                // More keys while the gate waits: none may reach the wrapper either.
                master.write_all(b"more keys\n").unwrap_or_else(|error| {
                    panic!(
                        "the pane ended before it was bound: {error}; {}",
                        String::from_utf8_lossy(&screen)
                    )
                });
                if cancelled {
                    crate::native::update_status(directory, SessionState::Closed, None, None)
                        .unwrap();
                } else {
                    bind(directory, &session);
                }
                released = true;
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("the pane did not end: {}", String::from_utf8_lossy(&screen));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let screen = String::from_utf8_lossy(&screen);
        assert!(released, "the gate never ran: {screen}");
        if cancelled {
            assert!(!owner.exists(), "a cancelled launch started its wrapper");
            assert!(!status.success());
            let log = fs::read_to_string(directory.join(crate::native::launch::LOG)).unwrap();
            assert!(
                log.contains("wezterm_host_refused: WezTerm launch was cancelled"),
                "{log}"
            );
        } else {
            assert!(status.success(), "a typed key changed the launch: {screen}");
            let proof: serde_json::Value =
                serde_json::from_slice(&fs::read(owner).unwrap()).unwrap();
            assert_eq!(
                proof["queued_input"], 0,
                "typed keys reached the wrapper: {proof}"
            );
        }
    }

    #[test]
    fn the_launch_host_starts_only_for_the_bound_pane_of_a_pending_launch() {
        use crate::native::{TERMINAL_HANDLE_FILE, launch, write_json_atomic};
        let binding = |directory: &Path| {
            serde_json::json!({
                "terminal": "wezterm", "session_id": "8", "tab_id": "8", "window_id": "7",
                "managed_session_id": directory.file_name().unwrap().to_str().unwrap(),
                "wezterm_mux": {
                    "socket": SOCKET, "pid": PID, "start_seconds": BIRTH.0,
                    "start_microseconds": BIRTH.1, "owns_gui": false
                },
            })
        };
        // The same socket in another spelling is the same socket.
        for socket in [SOCKET, "/Users/tester//.local/share/wezterm/gui-sock-4242"] {
            let fixture = launch_fixture();
            let directory = fixture.path();
            let id = directory.file_name().unwrap().to_str().unwrap();
            write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &binding(directory)).unwrap();
            wait_for_binding(directory, id, "8", Path::new(socket)).unwrap();
        }
        for case in [
            "another pane",
            "another GUI",
            "no GUI",
            "another terminal",
            "another session",
            "unreadable binding",
            "cancelled",
            "expired",
            "another claim",
        ] {
            let fixture = launch_fixture();
            let directory = fixture.path();
            let id = directory.file_name().unwrap().to_str().unwrap();
            let mut bound = binding(directory);
            match case {
                "another pane" => bound["session_id"] = "9".into(),
                "another GUI" => {
                    bound["wezterm_mux"]["socket"] =
                        "/Users/tester/.local/share/wezterm/gui-sock-4243".into()
                }
                "no GUI" => {
                    bound.as_object_mut().unwrap().remove("wezterm_mux");
                }
                "another terminal" => bound["terminal"] = "ghostty".into(),
                "another session" => bound["managed_session_id"] = "session-other".into(),
                _ => {}
            }
            let handle = directory.join(TERMINAL_HANDLE_FILE);
            if case == "unreadable binding" {
                fs::write(&handle, b"{").unwrap();
            } else {
                write_json_atomic(&handle, &bound).unwrap();
            }
            match case {
                "cancelled" => {
                    crate::native::update_status(directory, SessionState::Closed, None, None)
                        .unwrap()
                }
                "expired" | "another claim" => {
                    let mut receipt =
                        launch::read(&crate::native::session::Reader::open_unchecked(directory))
                            .unwrap()
                            .unwrap();
                    if case == "expired" {
                        receipt.deadline_unix_ms = 0;
                    } else {
                        receipt.claim_token = "unrelated".into();
                    }
                    write_json_atomic(&directory.join(launch::FILE), &receipt).unwrap();
                }
                _ => {}
            }
            assert!(
                wait_for_binding(directory, id, "8", Path::new(SOCKET)).is_err(),
                "{case}"
            );
        }

        // A directory that another account can enter is not a launch of this user.
        use std::os::unix::fs::PermissionsExt;
        let fixture = launch_fixture();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let error = run_host(fixture.path()).unwrap_err();
        assert!(format!("{error:#}").contains("not private"), "{error:#}");
    }

    #[test]
    fn a_pane_id_of_a_later_process_is_never_addressed() {
        let prompt = tempfile::NamedTempFile::new().unwrap();
        fs::write(prompt.path(), "prompt").unwrap();
        // The process has ended. Another one got its pid, and so its socket path, and
        // numbers its first pane 0 again; or no process has the pid.
        for later_birth in [Some((BIRTH.0 + 60, 0)), None] {
            let host = fake();
            let session = session(&host);
            host.0.borrow_mut().birth = later_birth;

            if later_birth.is_some() {
                assert!(close_session_until(&host, &session, shortly()).is_err());
                assert!(surface_present(&host, &session, Duration::from_secs(1)).is_err());
            } else {
                assert_eq!(
                    close_session_until(&host, &session, shortly()).unwrap(),
                    CloseOutcome::Missing
                );
                assert!(!surface_present(&host, &session, Duration::from_secs(1)).unwrap());
            }
            assert!(verify_session(&host, &session, None).is_err());
            assert!(read_screen(&host, &session, soon()).is_err());
            let failure = send_file(&host, &session, prompt.path(), soon()).unwrap_err();
            assert!(!failure.delivery_may_have_occurred());

            let mux = host.0.borrow();
            assert!(mux.calls.is_empty(), "{:?}", mux.calls);
            assert!(mux.terminated.is_empty());
            assert_eq!(mux.panes, [0], "the pane of the later process stays");
            // The later process serves the same path; only a socket of nobody is removed.
            assert_eq!(mux.removed.is_empty(), later_birth.is_some());
        }
    }

    #[test]
    fn a_later_process_socket_is_preserved_even_before_it_listens() {
        let host = fake();
        let session = session(&host);
        {
            let mut mux = host.0.borrow_mut();
            mux.birth = Some((BIRTH.0 + 60, 0));
            mux.polls_before_listening = usize::MAX;
        }
        assert!(close_session_until(&host, &session, shortly()).is_err());
        let mux = host.0.borrow();
        assert!(mux.calls.is_empty());
        assert!(mux.terminated.is_empty());
        assert!(
            mux.removed.is_empty(),
            "the later process may have bound its socket before listening"
        );
    }

    #[test]
    fn a_socket_that_another_process_serves_gets_no_call_and_is_not_an_absence() {
        let prompt = tempfile::NamedTempFile::new().unwrap();
        fs::write(prompt.path(), "prompt").unwrap();
        let host = fake();
        let session = session(&host);
        host.0.borrow_mut().foreign_server = Some(999);

        assert!(close_session_until(&host, &session, shortly()).is_err());
        assert!(surface_present(&host, &session, Duration::from_secs(1)).is_err());
        assert!(verify_session(&host, &session, None).is_err());
        let failure = send_file(&host, &session, prompt.path(), soon()).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());

        let mux = host.0.borrow();
        assert!(mux.calls.is_empty());
        assert!(mux.terminated.is_empty());
    }

    #[test]
    fn a_close_ends_when_the_pane_is_no_longer_listed_and_the_process_has_ended() {
        let host = fake();
        let session = session(&host);
        host.0.borrow_mut().listings_after_kill = 2;

        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Closed
        );

        assert_eq!(
            commands(&host),
            ["list", "kill-pane", "list", "list", "list"]
        );
        let mux = host.0.borrow();
        assert_eq!(mux.calls[1].1, ["kill-pane", "--pane-id", "0"]);
        assert_eq!(mux.birth, None);
        // WezTerm ended after its last window; it was not asked to.
        assert!(mux.terminated.is_empty());
        assert_eq!(mux.removed, [SOCKET]);
    }

    #[test]
    fn a_process_that_stays_without_panes_is_ended_and_a_close_that_could_not_is_finished_later() {
        let host = fake();
        let session = session(&host);
        {
            let mut mux = host.0.borrow_mut();
            mux.stays_without_panes = true;
            mux.ignores_terminate = true;
        }

        // The pane is closed, the process stays and does not end when asked.
        let error = close_session_until(&host, &session, shortly()).unwrap_err();
        assert!(
            format!("{error:#}").contains("still running without a pane"),
            "{error:#}"
        );
        {
            let mux = host.0.borrow();
            assert!(mux.panes.is_empty());
            assert_eq!(mux.terminated, [PID]);
            assert_eq!(mux.birth, Some(BIRTH));
        }

        // The handle is what it was. The next close kills nothing and ends the process.
        {
            let mut mux = host.0.borrow_mut();
            mux.ignores_terminate = false;
            mux.calls.clear();
        }
        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Missing
        );
        let mux = host.0.borrow();
        assert!(!commands(&host).contains(&"kill-pane".to_owned()));
        assert_eq!(mux.terminated, [PID, PID]);
        assert_eq!(mux.birth, None);
    }

    #[test]
    fn panes_that_the_user_opened_in_the_process_stay_with_their_process() {
        let host = fake();
        let session = session(&host);
        host.0.borrow_mut().panes = vec![0, 1, 2];

        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Closed
        );

        let mux = host.0.borrow();
        assert_eq!(mux.panes, [1, 2]);
        assert_eq!(mux.birth, Some(BIRTH));
        assert!(mux.terminated.is_empty());
        assert!(mux.removed.is_empty());
        assert_eq!(commands(&host), ["list", "kill-pane", "list"]);
    }

    #[test]
    fn an_answered_kill_of_a_pane_that_stays_listed_fails_and_can_be_repeated() {
        let host = fake();
        let session = session(&host);
        host.0.borrow_mut().listings_after_kill = usize::MAX;

        let error = close_session_until(&host, &session, shortly()).unwrap_err();
        assert!(format!("{error:#}").contains("still listed"), "{error:#}");
        assert_eq!(host.0.borrow().panes, [0]);
        assert!(host.0.borrow().terminated.is_empty());

        {
            let mut mux = host.0.borrow_mut();
            mux.dying.clear();
            mux.listings_after_kill = 0;
        }
        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(host.0.borrow().birth, None);
    }

    #[test]
    fn a_pane_that_is_already_gone_is_missing_and_nothing_is_killed() {
        let host = fake();
        let session = session(&host);
        // The user closed the session's pane and kept a tab of their own.
        host.0.borrow_mut().panes = vec![1];

        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Missing
        );
        assert_eq!(commands(&host), ["list"]);
        assert!(host.0.borrow().terminated.is_empty());
    }

    #[test]
    fn a_listing_or_a_process_that_cannot_be_read_is_not_an_absence() {
        let host = fake();
        let session = session(&host);

        host.0.borrow_mut().refused = vec!["list"; 20];
        assert!(close_session_until(&host, &session, shortly()).is_err());
        assert!(surface_present(&host, &session, Duration::from_secs(1)).is_err());
        host.0.borrow_mut().refused.clear();

        host.0.borrow_mut().unreadable_process = true;
        assert!(close_session_until(&host, &session, shortly()).is_err());
        assert!(surface_present(&host, &session, Duration::from_secs(1)).is_err());

        assert!(!commands(&host).contains(&"kill-pane".to_owned()));
        let mux = host.0.borrow();
        assert_eq!(mux.panes, [0]);
        assert!(mux.terminated.is_empty());
    }

    #[test]
    fn a_refused_kill_fails_while_the_pane_is_listed_and_a_lost_answer_does_not() {
        let host = fake();
        let session = session(&host);

        host.0.borrow_mut().refused = vec!["kill-pane"];
        let error = close_session_until(&host, &session, soon()).unwrap_err();
        assert!(format!("{error:#}").contains("was not closed"), "{error:#}");
        assert_eq!(host.0.borrow().panes, [0]);

        // The mux closed the pane and the answer never arrived: the pane is gone.
        host.0.borrow_mut().answer_lost = vec!["kill-pane"];
        assert_eq!(
            close_session_until(&host, &session, soon()).unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(host.0.borrow().birth, None);
    }

    #[test]
    fn a_prompt_is_one_write_and_only_a_call_that_never_started_is_known_unsent() {
        let prompt = tempfile::NamedTempFile::new().unwrap();
        fs::write(prompt.path(), "\x1b[200~first\nsecond\x1b[201~").unwrap();
        let host = fake();
        let session = session(&host);

        send_file(&host, &session, prompt.path(), soon()).unwrap();
        {
            let mux = host.0.borrow();
            let (_, arguments, input) = mux.calls.last().unwrap();
            assert_eq!(arguments, &["send-text", "--pane-id", "0", "--no-paste"]);
            assert_eq!(
                input.as_deref(),
                Some(&b"\x1b[200~first\nsecond\x1b[201~\r"[..])
            );
            assert_eq!(mux.calls.len(), 1);
        }

        host.0.borrow_mut().not_started = vec!["send-text"];
        let failure = send_file(&host, &session, prompt.path(), soon()).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());

        for uncertain in ["refused", "answer_lost"] {
            {
                let mut mux = host.0.borrow_mut();
                if uncertain == "refused" {
                    mux.refused = vec!["send-text"];
                } else {
                    mux.answer_lost = vec!["send-text"];
                }
            }
            let failure = send_file(&host, &session, prompt.path(), soon()).unwrap_err();
            assert!(failure.delivery_may_have_occurred(), "{uncertain}");
        }

        // A prompt that the CLI cannot read as text is not handed to it.
        fs::write(prompt.path(), b"\xff").unwrap();
        let calls = host.0.borrow().calls.len();
        let failure = send_file(&host, &session, prompt.path(), soon()).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert_eq!(host.0.borrow().calls.len(), calls);
    }

    #[test]
    fn the_ownership_proof_is_the_tty_of_exactly_this_pane() {
        let host = fake();
        let session = session(&host);
        host.0.borrow_mut().panes = vec![0, 1];

        assert_eq!(
            verify_session(&host, &session, None).unwrap(),
            "/dev/ttys000"
        );
        assert!(surface_present(&host, &session, Duration::from_secs(1)).unwrap());
        assert_eq!(read_screen(&host, &session, soon()).unwrap(), "$ \n\n");

        host.0.borrow_mut().panes = vec![1];
        assert!(verify_session(&host, &session, None).is_err());
        assert!(!surface_present(&host, &session, Duration::from_secs(1)).unwrap());
        assert!(read_screen(&host, &session, soon()).is_err());
    }

    #[test]
    fn a_handle_without_its_process_is_refused() {
        let host = fake();
        let mut session = session(&host);
        session.wezterm_mux = None;

        assert!(close_session_until(&host, &session, soon()).is_err());
        assert!(surface_present(&host, &session, Duration::from_secs(1)).is_err());
        let mux = host.0.borrow();
        assert!(mux.calls.is_empty());
        assert!(mux.terminated.is_empty());
    }

    // LIVE. Starts a real WezTerm process of its own whose pane runs one harmless command
    // behind the gate, binds the pane as the launcher does and ends it. It reads and
    // changes nothing of a WezTerm that is already running.
    #[test]
    #[ignore = "starts a real WezTerm window"]
    fn live_a_session_process_is_started_bound_and_ended() {
        let fixture = launch_fixture();
        let directory = fixture.path();
        let marker = directory.join("marker");
        let host = Installed;
        let session = create(
            &host,
            true,
            &pane_program(
                &probe("host", directory),
                &format!(
                    "echo agent-bridge-wezterm-smoke | tee {}; sleep 600",
                    shell_quote(marker.as_os_str())
                ),
            ),
            Instant::now() + Duration::from_secs(20),
        )
        .unwrap();
        let mux = session.wezterm_mux.clone().unwrap();
        eprintln!(
            "LIVE created: pid={} birth={}.{:06} socket={} pane={}",
            mux.pid, mux.start_seconds, mux.start_microseconds, mux.socket, session.id
        );
        eprintln!(
            "LIVE socket is served by: {:?}",
            host.socket_server(&mux.socket)
        );
        let used = (|| -> Result<()> {
            let tty = verify_session(&host, &session, None)?;
            eprintln!("LIVE pane tty: {tty}");
            anyhow::ensure!(surface_present(&host, &session, Duration::from_secs(5))?);
            anyhow::ensure!(
                !marker.exists(),
                "the command ran before its pane was bound"
            );
            bind(directory, &session);
            let until = Instant::now() + Duration::from_secs(15);
            while fs::read_to_string(&marker).unwrap_or_default().trim()
                != "agent-bridge-wezterm-smoke"
            {
                anyhow::ensure!(
                    Instant::now() < until,
                    "the command did not run in the pane"
                );
                thread::sleep(POLL);
            }
            eprintln!("LIVE the command ran in the pane");
            let screen = read_screen(&host, &session, soon())?;
            eprintln!(
                "LIVE screen: {} lines, shows the command: {}",
                screen.lines().count(),
                screen.contains("agent-bridge-wezterm-smoke")
            );
            Ok(())
        })();
        let started = Instant::now();
        let closed = close_session_until(&host, &session, Instant::now() + Duration::from_secs(15));
        eprintln!("LIVE close: {closed:?} after {:?}", started.elapsed());
        let after = host.process_start(mux.pid);
        eprintln!("LIVE process after close: {after:?}");
        used.unwrap();
        assert_eq!(closed.unwrap(), CloseOutcome::Closed);
        assert_ne!(after.unwrap(), birth(&mux));
        assert!(!surface_present(&host, &session, Duration::from_secs(2)).unwrap());
        eprintln!(
            "LIVE socket file left behind: {}",
            Path::new(&mux.socket).exists()
        );
        assert!(!Path::new(&mux.socket).exists());
    }
}
