// Ghostty supplies no surface identity: prove the bound PTY by its frame and discard other input.
// Replace this fallback when Ghostty exposes a surface identity or an input API;
// see docs/verification/2026-10-03-v0.1.0-focus.md.
use super::{QUEUE_COMMAND_SCRIPT, Runner, ownership_proof};
use crate::native::private_to_user::owned_and_not_accessible_by_others;
use crate::native::session::launch::launcher::validate_shell_command_component;
use crate::native::session::primitives::unix_ms;
use crate::native::session::{CoreRecord, Reader, RecordStore, SessionState};
use crate::native::terminal::{TerminalKind, TerminalSession};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsStr,
    io::Write,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const HOST_COMMAND: &str = TerminalKind::Ghostty.registration().host_command;
// What the launcher writes to the bound terminal, and only there, once the surface is
// bound: `agent-bridge-ghostty-host:<claim token>:<length>:<session directory>`.
const HOST_FRAME: &str = "agent-bridge-ghostty-host:";
// A launch receipt lives for at most 30 s (`launch::begin`). The host cannot read it
// before the frame has named its session, so it waits that long for the frame.
const HOST_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(30)
};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Frame {
    token: String,
    directory: PathBuf,
}

// Use native command creation and an interactive clean shell for foreground jobs.
// Source only the host-proven launch; see docs/verification/2026-10-03-v0.1.0-focus.md.
pub(super) fn bootstrap(executable: &OsStr) -> Result<String> {
    validate_shell_command_component(executable, "Agent Bridge executable")?;
    Ok(format!(
        "/bin/zsh -f -i -c {}",
        crate::native::terminal::shell_quote(OsStr::new(&format!(
            "start=$({} {HOST_COMMAND}) || exit; . \"$start\"",
            crate::native::terminal::shell_quote(executable)
        )))
    ))
}
// One write to the terminal of the handle, and no Enter: what arrives is data for the
// host, which no shell reads.
pub(super) fn start_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    frame: &str,
    deadline: Instant,
) -> Result<()> {
    let (tab, window) = ownership_proof(session)?;
    let response = runner.run(
        QUEUE_COMMAND_SCRIPT,
        &[&session.id, tab, window, frame],
        deadline,
    )?;
    if response != "queued" {
        bail!("Ghostty command input was not confirmed: {response:?}; no paste retry");
    }
    Ok(())
}
pub(super) fn encode(directory: &Path, token: &str) -> Result<String> {
    let directory = directory
        .to_str()
        .context("Ghostty session directory is not UTF-8")?;
    let frame = format!("{HOST_FRAME}{token}:{}:{directory}", directory.len());
    // The frame can arrive while the terminal is still in canonical mode, whose input
    // queue holds 1024 bytes.
    if frame.len() >= 1024 || !frame_token(token.as_bytes()) {
        bail!("Ghostty launch frame cannot be written to its terminal");
    }
    Ok(frame)
}
fn frame_token(token: &[u8]) -> bool {
    !token.is_empty()
        && token
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}
// After the binding: the script that the host hands to its shell, then the frame that
// releases exactly the host on the bound terminal.
pub(super) fn start(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<()> {
    launch_command(directory, command, std::env::var_os("PATH").as_deref())?;
    let receipt = crate::native::session::launch::read(&Reader::open_unchecked(directory))?
        .context("Ghostty launch has no pending receipt")?;
    start_with_runner(
        runner,
        session,
        &encode(directory, &receipt.claim_token)?,
        deadline,
    )
}
pub(in crate::native::terminal) fn run_host() -> Result<()> {
    let start = {
        let _raw = RawInput::enter()?;
        let Frame { token, directory } = read_frame(Instant::now() + HOST_WAIT)?;
        let start = verify_launch(&directory, &token)?;
        // Only this terminal's queue: keys typed while Ghostty showed the surface.
        if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot discard Ghostty startup input");
        }
        start
    };
    let mut out = std::io::stdout().lock();
    out.write_all(start.as_os_str().as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
// Every byte as it was typed: no line editing, no echo, no signal or flow-control key.
// The terminal gets its settings back before the launch command inherits it.
struct RawInput(libc::termios);
impl RawInput {
    fn enter() -> Result<Self> {
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, saved.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("the Ghostty launch host has no terminal");
        }
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !libc::IXON;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // Not TCSAFLUSH: a frame that is already queued must stay.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot read the Ghostty terminal");
        }
        Ok(Self(saved))
    }
}
impl Drop for RawInput {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}
fn read_frame(deadline: Instant) -> Result<Frame> {
    let mut typed = Vec::new();
    loop {
        let mut bytes = [0u8; 1024];
        let asked = Instant::now();
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len()) };
        match usize::try_from(count) {
            // The timed read returns nothing after a tenth of a second, and at once
            // only when the terminal is gone.
            Ok(0) if asked.elapsed() < Duration::from_millis(20) => {
                bail!("the Ghostty terminal closed before its launch arrived")
            }
            Ok(count) => typed.extend_from_slice(&bytes[..count]),
            Err(_) => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("cannot read the Ghostty terminal");
                }
            }
        }
        if let Some(frame) = parse(&typed)? {
            return Ok(frame);
        }
        if Instant::now() >= deadline {
            bail!("no launch reached this Ghostty terminal");
        }
        // A frame is shorter than 1024 bytes; older keys cannot belong to one.
        if typed.len() > 8192 {
            typed.drain(..typed.len() - 2048);
        }
    }
}
// `None` while the frame is incomplete. What precedes it was typed by somebody else.
pub(super) fn parse(typed: &[u8]) -> Result<Option<Frame>> {
    let Some(at) = typed
        .windows(HOST_FRAME.len())
        .position(|window| window == HOST_FRAME.as_bytes())
    else {
        return Ok(None);
    };
    let mut fields = typed[at + HOST_FRAME.len()..].splitn(3, |byte| *byte == b':');
    let (Some(token), Some(length), Some(directory)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return Ok(None);
    };
    let length = std::str::from_utf8(length)
        .ok()
        .and_then(|length| length.parse::<usize>().ok())
        .filter(|length| (1..1024).contains(length));
    let (true, Some(length)) = (frame_token(token), length) else {
        bail!("malformed Ghostty launch frame");
    };
    if directory.len() < length {
        return Ok(None);
    }
    Ok(Some(Frame {
        token: String::from_utf8_lossy(token).into_owned(),
        directory: PathBuf::from(OsStr::from_bytes(&directory[..length])),
    }))
}
// The frame names a launch; the records decide whether it is this one and still open:
// the launcher's private session directory, its pending receipt and claim, the atomic
// binding of a Ghostty surface to this session, and the script written for the host.
fn verify_launch(directory: &Path, token: &str) -> Result<PathBuf> {
    use crate::native::session::{SessionStatus, launch};
    let private = |path: &Path, directory: bool| -> Result<bool> {
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(metadata.is_dir() == directory
            && (directory || metadata.is_file())
            && owned_and_not_accessible_by_others(&metadata, unsafe { libc::geteuid() }))
    };
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid Ghostty launch directory")?;
    crate::native::session::primitives::require_valid_session_id(id)?;
    if !directory.is_absolute() || !private(directory, true)? {
        bail!("Ghostty launch directory is not private to the current user");
    }
    let receipt = launch::read(&Reader::open_unchecked(directory))?
        .context("missing Ghostty launch receipt")?;
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if receipt.phase != launch::Phase::Pending
        || receipt.claim_token != token
        || unix_ms() >= receipt.deadline_unix_ms
        || status.state != SessionState::Launching
        || crate::native::session::turn::current_claim_token(
            &crate::native::session::Reader::open_unchecked(directory),
        )?
        .as_deref()
            != Some(token)
    {
        bail!("Ghostty launch was cancelled, timed out or is not the one sent to this terminal");
    }
    let surface: TerminalSession = serde_json::from_str(
        &Reader::open_unchecked(directory)
            .record(CoreRecord::Terminal)
            .text()?
            .context("Ghostty surface is not bound")?,
    )
    .context("invalid Ghostty surface binding")?;
    surface.verify_managed_session(id)?;
    if surface.kind != TerminalKind::Ghostty {
        bail!("the bound surface is not a Ghostty terminal");
    }
    let start = directory.join("ghostty-start.sh");
    if !private(&start, false)? {
        bail!("Ghostty start script is not private to the current user");
    }
    Ok(start)
}
fn launch_command(
    directory: &Path,
    command: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<String> {
    // Carry PATH for provider shebangs through the clean shell; source only the host's private script.
    let script = directory.join("ghostty-start.sh");
    validate_shell_command_component(script.as_os_str(), "Ghostty startup path")?;
    let input = format!(
        ". {}",
        crate::native::terminal::shell_quote(script.as_os_str())
    );
    if input.len() >= 1024 {
        bail!("Ghostty startup path exceeds the canonical input line limit");
    }
    let export = match path {
        Some(path) => {
            validate_shell_command_component(path, "Ghostty launch PATH")?;
            format!(
                "export PATH={}\n",
                crate::native::terminal::shell_quote(path)
            )
        }
        None => String::new(),
    };
    RecordStore::at(&script).write_private(format!("{export}{command}\n").as_bytes())?;
    Ok(input)
}

#[cfg(test)]
mod tests;
