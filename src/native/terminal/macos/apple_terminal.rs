use super::process;
use crate::native::session::SessionState;
use crate::native::session::{CoreRecord, Reader, RecordStore, Store};
use crate::native::terminal::ownership;
use std::{
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    applescript, close_response,
};

// Ownership of the new window is proven by the tty of the tab that `do script`
// returned: exactly one window must hold a tab with that tty. Only windows that did
// not exist before the run are candidates, because a tty name alone is not unique
// over time. A window whose shell ended while a close confirmation was pending keeps
// reporting its old tty name after Terminal has released it, and the next tab
// receives the same name (2026-10-01, session-y5Wpkl: the proof matched the new
// window and that stale one, the launch failed, and the new window stayed open
// without an owner). A window list that could not be read before the run excludes
// nothing, which is the former rule.
//
// Terminal keeps a window that it has closed in its window list, without tabs and off
// the screen, until it releases the object, and making a window frontmost shows it
// (Terminal 2.15: `setScriptFrontmost:` sends `makeKeyAndOrderFront:`). With no other
// window, the window of the session closed before is `window 1`, and giving the
// keyboard back to it put it on the screen again, empty (2026-10-03: window 12678 read
// not visible before this script ran and visible after it; the user had seen window
// 12064 return that day after each of the next two launches, issue #64). The keyboard
// therefore goes back only to a window that is visible.
//
// Terminal starts a command in a tab only by typing it (Terminal 2.15, read from the
// binary on 2026-10-03). `do script X in tab` writes X and a carriage return to the
// tab's PTY. `do script X` without a target puts X and a line feed into the write
// buffer of the tab it creates, before the window is shown. A key typed into the new
// window is queued behind that text until Terminal knows the shell's process, and is
// written in front of it from then until Terminal writes the buffer, on the first
// output it decodes while the shell is in the foreground. An empty creation and a
// start command typed after the binding left an editable line in between: a key that
// reached the new window while it had the keyboard became the start of the command
// (2026-10-03, session-t9E3YV: `a. '<...>/launch.sh'`, `zsh: command not found: a.`,
// issue #58).
//
// The creation text is therefore the whole start: one short line that sources the
// session's bootstrap, with the tty's line kill (U+0015) in front. The PTY of a new
// tab is canonical with the kernel's control characters (lflag 0x5cb, kill ^U) until
// a shell changes that, so the kernel discards a key that Terminal wrote in front,
// and the default line editors of zsh and bash discard ordinary text in insertion
// mode. A prefix or quote key just before it (Escape or Ctrl-V), or a remapped line
// kill, can defeat this and leave the start unexecuted. Text already followed by
// Enter is a command line of its own; nothing typed can take it back. Replace the
// typed start when Terminal can create a tab that runs a command.
pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
on soleNewWindowWithTty(windowTtys, priorWindowIds, wantedTty)
    set matchedWindowId to missing value
    set matchCount to 0
    repeat with windowTty in windowTtys
        set candidateId to item 1 of windowTty
        if priorWindowIds is missing value or priorWindowIds does not contain candidateId then
            if item 2 of windowTty is wantedTty then
                set matchedWindowId to candidateId
                set matchCount to matchCount + 1
            end if
        end if
    end repeat
    if matchCount is not 1 then error "Agent Bridge could not prove the newly created Terminal.app window"
    return matchedWindowId
end soleNewWindowWithTty

on windowIdForTty(wantedTty, priorWindowIds)
    set windowTtys to {}
    tell application "Terminal"
        set candidateWindows to get windows
        repeat with candidateWindow in candidateWindows
            try
                set candidateTabs to get tabs of candidateWindow
                repeat with candidateTab in candidateTabs
                    set end of windowTtys to {id of candidateWindow, tty of candidateTab}
                end repeat
            end try
        end repeat
    end tell
    return my soleNewWindowWithTty(windowTtys, priorWindowIds, wantedTty)
end windowIdForTty

on run argv
    set bridgeCommand to item 1 of argv
    set terminalWasRunning to application "Terminal" is running
    tell application "Terminal"
        -- A window that exists before this run can never be the one it creates.
        -- The window that has the keyboard is remembered only to give it back
        -- below (issue #58). It is never the target, and failing to remember or
        -- restore it never fails the launch. Neither is asked of a Terminal that
        -- is not running: the first event it receives stays the creation.
        set priorWindowIds to {}
        set keyboardWindowId to missing value
        if terminalWasRunning then
            set priorWindowIds to missing value
            try
                set priorWindowIds to id of every window
            end try
            try
                if (count of windows) > 0 then set keyboardWindowId to id of window 1
            end try
        end if
        -- Untargeted do script creates a dedicated window and returns its new tab.
        -- Never derive ownership from a restored front/current/selected surface.
        -- The start is typed here and nowhere else, behind the tty's line kill.
        set targetTab to do script ((character id 21) & bridgeCommand)
        set targetTty to tty of targetTab
        set targetWindowId to my windowIdForTty(targetTty, priorWindowIds)
        set targetWindow to first window whose id is targetWindowId
        if id of targetWindow is not targetWindowId then error "Agent Bridge lost its newly created Terminal.app window"
        if tty of targetTab is not targetTty then error "Agent Bridge lost its newly created Terminal.app tty"
        -- Terminal is not brought forward, and the new window does not keep the
        -- keyboard. The front position is taken back only from the new window: a
        -- window the user chose meanwhile stays in front. It is given only to a
        -- window that is on the screen, tested in the event that moves it.
        if keyboardWindowId is not missing value and keyboardWindowId is not targetWindowId then
            try
                if (id of window 1) is targetWindowId then
                    set frontmost of (first window whose id is keyboardWindowId and visible is true) to true
                end if
            end try
        end if
        return targetTty & linefeed & (targetWindowId as text)
    end tell
end run
"#;

pub(in crate::native) const SEND_FILE_SCRIPT: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    set promptPath to item 3 of argv
    set promptText to read (POSIX file promptPath) as «class utf8»
    tell application "Terminal"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            error "Agent Bridge Terminal.app window not found"
        end try
        set targetTab to missing value
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if tty of candidateTab is wantedTty then
                set targetTab to candidateTab
                set matchCount to matchCount + 1
            end if
        end repeat
        if matchCount is not 1 then error "Agent Bridge Terminal.app ownership proof did not match exactly one tab"
        if id of targetWindow is not wantedWindowId then error "Agent Bridge Terminal.app window identity changed"
        if tty of targetTab is not wantedTty then error "Agent Bridge Terminal.app tty identity changed"
        do script promptText in targetTab
        return "sent"
    end tell
end run
"#;

pub(in crate::native) const VERIFY_TAB_SCRIPT: &str = r#"
on run argv
    if application "Terminal" is not running then return "missing"
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    tell application "Terminal"
        -- Only Terminal's own window list proves absence; a failed read is an error.
        if (id of every window) does not contain wantedWindowId then return "missing"
        set targetWindow to first window whose id is wantedWindowId
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if tty of candidateTab is wantedTty then set matchCount to matchCount + 1
        end repeat
        if matchCount is 1 then return wantedTty
        if matchCount is 0 then error "Agent Bridge Terminal.app window no longer matches a tab of the recorded tty"
        error "Agent Bridge Terminal.app ownership proof matched multiple tabs"
    end tell
end run
"#;

// Bridge kills the shell before it closes the window, and a tab whose shell was killed
// can report its tty followed by U+0001 (2026-10-01, Terminal.app 2.15, windows 8341
// and 8345 in docs/verification/2026-10-01-terminal-proof.md). An exact comparison
// takes that tab for a missing one and reports the close as done without closing it.
// The window is missing only when Terminal is not running or its own window list, read
// without error, lacks it; a window that holds no tab of this tty has changed, it is not
// absent. The running check launches nothing, unlike an event sent to a stopped Terminal.
pub(in crate::native) const CLOSE_TAB_SCRIPT: &str = r#"
on isOwnedTty(reportedTty, wantedTty)
    return reportedTty is wantedTty or reportedTty is (wantedTty & (character id 1))
end isOwnedTty

on run argv
    if application "Terminal" is not running then return "missing"
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    tell application "Terminal"
        if (id of every window) does not contain wantedWindowId then return "missing"
        set targetWindow to first window whose id is wantedWindowId

        set targetTab to missing value
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if my isOwnedTty(tty of candidateTab, wantedTty) then
                set targetTab to candidateTab
                set matchCount to matchCount + 1
            end if
        end repeat
        if matchCount is 0 then error "Agent Bridge Terminal.app window no longer holds its tab"
        if matchCount is not 1 then error "Agent Bridge Terminal.app ownership proof matched multiple tabs"
        if (count of tabs of targetWindow) is not 1 then error "Agent Bridge refuses to close a Terminal.app window containing another tab"

        -- Rust terminates the attested foreground process group before this
        -- script runs. Wait for Terminal to observe that transition without
        -- injecting control characters into a possibly changed shell surface.
        repeat 60 times
            if not busy of targetTab then exit repeat
            delay 0.05
        end repeat
        if busy of targetTab then error "Agent Bridge could not stop the foreground process in its Terminal.app tab"

        if id of targetWindow is not wantedWindowId then error "Agent Bridge Terminal.app window identity changed before close"
        if not my isOwnedTty(tty of targetTab, wantedTty) then error "Agent Bridge Terminal.app tab changed before close"
        if (count of tabs of targetWindow) is not 1 then error "Agent Bridge refuses to close a Terminal.app window containing another tab"
        close targetWindow
        return "closed"
    end tell
end run
"#;

// Without a successful close in this transaction, only Terminal's window list can
// prove the window missing. The tty changes when the shell ends even if the window
// stays on the screen, so it cannot prove absence.
//
// Terminal can keep listing a window that it has closed: closing removes the window's
// tabs and takes it off the screen, and the object stays in the list until Terminal
// releases it (2026-10-03: windows 12064, 12674, 12678 and 12679 read listed, without
// tabs and not visible after their close, 12064 for minutes; the second close then
// failed with `no longer holds its tab`). The close that has just closed the recorded
// window passes the close script's reply as the third argument, and only then is such
// a window answered as `closed`. A window on the screen or with a tab is present, and
// so is every window that this transaction did not close.
pub(in crate::native) const WAIT_FOR_CLOSE_SCRIPT: &str = r#"
on run argv
    set wantedWindowId to item 1 of argv as integer
    set attemptCount to item 2 of argv as integer
    set closeWasSent to (count of argv) > 2 and item 3 of argv is "closed"
    if not application "Terminal" is running then return "missing"
    tell application "Terminal"
        repeat attemptCount times
            if (id of every window) does not contain wantedWindowId then return "missing"
            if closeWasSent then
                try
                    set listedWindow to first window whose id is wantedWindowId
                    if (count of tabs of listedWindow) is 0 and not (visible of listedWindow) then return "closed"
                on error errorText number errorNumber
                    -- The window can leave the list between the two reads: the next
                    -- list read decides. Terminal 2.15 returns -1719 for a missing
                    -- whose match; -1728 also means a missing object. Other reads fail.
                    if errorNumber is not -1728 and errorNumber is not -1719 then error errorText number errorNumber
                end try
            end if
            delay 0.05
        end repeat
    end tell
    return "present"
end run
"#;

const BOOTSTRAP_FILE: &str = "terminal-start.sh";

pub(super) fn create_tab(
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<TerminalSession> {
    let executable =
        std::env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let host = format!(
        "{} native-terminal-host {}",
        crate::native::shell_quote(executable.as_os_str()),
        crate::native::shell_quote(directory.as_os_str())
    );
    let start = install_bootstrap(directory, &host, command)?;
    let response = applescript::run_until("Terminal.app", OPEN_TAB_SCRIPT, &[&start], deadline)?;
    let mut ids = response.lines();
    let id = ids.next().filter(|value| !value.is_empty());
    let window_id = ids.next().filter(|value| !value.is_empty());
    if id.is_none() || window_id.is_none() || ids.next().is_some() {
        bail!("Terminal.app did not return one tty and one window id");
    }
    Ok(TerminalSession {
        kind: TerminalKind::AppleTerminal,
        id: id.unwrap().to_owned(),
        tab_id: None,
        window_id: Some(window_id.unwrap().to_owned()),
        managed_session_id: None,
        wezterm_mux: None,
        windows_process_identity: None,
    })
}

// The typed line stays short (#50) and names a private file. The tab's own shell
// sources it, so the gate and the wrapper are two jobs of that shell and the wrapper
// keeps the shell as its parent and a foreground group of its own. A refused gate
// stops the bootstrap before the wrapper. A zsh with running or suspended jobs can
// refuse exit and return to its prompt; the failed launch keeps its close proof.
fn install_bootstrap(directory: &Path, host: &str, command: &str) -> Result<String> {
    let path = Reader::open_unchecked(directory)
        .private(BOOTSTRAP_FILE)
        .path()
        .to_owned();
    RecordStore::at(&path).write_private(format!("{host} || exit\n{command}\n").as_bytes())?;
    Ok(format!(
        ". {}",
        crate::native::shell_quote(path.as_os_str())
    ))
}

// The start is typed when the tab is created, before the launcher knows the tty that
// Terminal gave it. The launch receipt and the atomic surface binding that already
// exist are the gate: the wrapper starts only after the launcher has given the
// keyboard back and bound this exact tty, and a launch that failed, was closed or
// timed out starts nothing.
pub(in crate::native) fn run_host(directory: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !directory.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("Terminal.app launch directory is not private to the current user");
    }
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid Terminal.app launch directory")?;
    crate::native::require_valid_session_id(id)?;
    let released = release_start(directory, id);
    crate::native::launch::log(
        &Store::open_unchecked(directory),
        &match &released {
            Ok(tty) => format!("terminal_host_released tty={tty}"),
            Err(error) => format!("terminal_host_refused: {error:#}"),
        },
    );
    released.map(|_| ())
}

fn release_start(directory: &Path, id: &str) -> Result<String> {
    // The binding names the tab by its tty, and the flush below must be this tab's:
    // standard input has to be the controlling terminal of this process.
    let tty = process::current_terminal_tty()?;
    let live = process::live_native_process_identity(std::process::id())?;
    if live.terminal_tty_device != process::terminal_tty_device(Path::new(&tty))? {
        bail!("Terminal.app launch host is not attached to the tty of its standard input");
    }
    wait_for_binding(directory, id, &tty)?;
    // Keys typed while the creation gave the new window the keyboard must not answer
    // the provider's first dialog. Only the input queue of this tty is discarded.
    if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot discard Terminal.app startup input");
    }
    Ok(tty)
}

fn wait_for_binding(directory: &Path, id: &str, tty: &str) -> Result<()> {
    use crate::native::{SessionStatus, launch, unix_ms};
    let initial = launch::read(&Reader::open_unchecked(directory))?
        .context("missing Terminal.app launch receipt")?;
    // The receipt's deadline is wall-clock time; the launch itself never waits longer.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let record = launch::read(&Reader::open_unchecked(directory))?
            .context("missing Terminal.app launch receipt")?;
        let status: SessionStatus = Reader::open_unchecked(directory).status()?;
        if Instant::now() >= deadline
            || unix_ms() >= record.deadline_unix_ms
            || record.phase != launch::Phase::Pending
            || record.claim_token != initial.claim_token
            || status.state != SessionState::Launching
            || crate::native::session::turn::current_claim_token(
                &crate::native::session::Reader::open_unchecked(directory),
            )?
            .as_deref()
                != Some(initial.claim_token.as_str())
        {
            bail!("Terminal.app launch was cancelled or timed out before its surface was bound");
        }
        if let Some(text) = Reader::open_unchecked(directory)
            .record(CoreRecord::Terminal)
            .text()?
        {
            let surface: TerminalSession =
                serde_json::from_str(&text).context("invalid Terminal.app surface binding")?;
            surface.verify_managed_session(id)?;
            if surface.kind != TerminalKind::AppleTerminal
                || surface.id != tty
                || surface.window_id.as_deref().is_none_or(str::is_empty)
            {
                bail!("Terminal.app surface binding does not name the tty of this launch host");
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let window_id = ownership_proof(session).map_err(TerminalSendFailure::not_sent)?;
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let response = applescript::run_send_until(
        "Terminal.app",
        SEND_FILE_SCRIPT,
        &[&session.id, window_id, prompt_path],
        deadline,
    )?;
    if response != "sent" {
        return Err(TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
            "unexpected Terminal.app send response: {response:?}"
        )));
    }
    Ok(())
}

pub(super) fn verify_tab(session: &TerminalSession, timeout: Option<Duration>) -> Result<String> {
    let window_id = ownership_proof(session)?;
    let response = run_terminal_automation(
        VERIFY_TAB_SCRIPT,
        &[&session.id, window_id],
        timeout.map(super::timeout_deadline).transpose()?,
    )?;
    if response != session.id {
        bail!("Agent Bridge Terminal.app owned tab is missing");
    }
    Ok(response)
}

pub(in crate::native) fn process_group_signal_target(process_group: u32) -> Result<libc::pid_t> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("Terminal.app process group is out of range")?;
    if process_group <= 0 {
        bail!("Terminal.app process group must be positive")
    }
    process_group
        .checked_neg()
        .context("Terminal.app process group cannot be represented as a signal target")
}

pub(in crate::native) fn close_signal_plan(
    managed_process_group: u32,
    shell_process_group: u32,
) -> Result<[(libc::pid_t, libc::c_int); 2]> {
    if managed_process_group == shell_process_group {
        bail!("Terminal.app managed and shell process groups must be distinct")
    }
    Ok([
        (
            process_group_signal_target(managed_process_group)?,
            libc::SIGTERM,
        ),
        (
            process_group_signal_target(shell_process_group)?,
            libc::SIGKILL,
        ),
    ])
}

pub(in crate::native) fn terminate_process_groups(
    managed_process_group: u32,
    shell_process_group: u32,
) -> Result<()> {
    for (target, signal) in close_signal_plan(managed_process_group, shell_process_group)? {
        let result = unsafe { libc::kill(target, signal) };
        if result == 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            continue;
        }
        return Err(error).with_context(|| {
            format!(
                "failed to send signal {signal} to Terminal.app process group {}",
                target.checked_neg().unwrap_or_default()
            )
        });
    }
    Ok(())
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    close_session_with_deadline(session, None)
}

pub(super) fn close_session_until(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_session_with_deadline(session, Some(deadline))
}

fn close_session_with_deadline(
    session: &TerminalSession,
    deadline: Option<Instant>,
) -> Result<CloseOutcome> {
    close_session_with(session, |script, arguments| {
        run_terminal_automation(script, arguments, deadline)
    })
}

pub(in crate::native) fn close_attested_session(
    session: &TerminalSession,
    app: &ownership::MacTerminalAppIdentity,
) -> Result<CloseOutcome> {
    close_attested_session_with(
        session,
        app,
        process::macos_process_start,
        crate::native::terminal_app_instances,
        |script, arguments| run_terminal_automation(script, arguments, None),
    )
}

fn close_attested_session_with(
    session: &TerminalSession,
    app: &ownership::MacTerminalAppIdentity,
    mut process_birth: impl FnMut(u32) -> Result<Option<(u64, u64)>>,
    mut instances: impl FnMut() -> Result<Vec<ownership::MacTerminalAppIdentity>>,
    mut run: impl FnMut(&str, &[&str]) -> Result<String>,
) -> Result<CloseOutcome> {
    // Recheck before every transaction, including retries; a restarted app must
    // never inherit the old window/tty's close authority.
    close_session_with(session, |script, arguments| {
        if !crate::native::terminal_app_alive_with(app, &mut process_birth)? {
            return Ok("missing".into());
        }
        crate::native::require_unique_terminal_app(app, &instances()?)?;
        let reply = run(script, arguments)?;
        if !crate::native::terminal_app_alive_with(app, &mut process_birth)? {
            return Ok("missing".into());
        }
        crate::native::require_unique_terminal_app(app, &instances()?)?;
        Ok(reply)
    })
}

fn close_session_with(
    session: &TerminalSession,
    mut run: impl FnMut(&str, &[&str]) -> Result<String>,
) -> Result<CloseOutcome> {
    let window_id = ownership_proof(session)?;
    let response = run(CLOSE_TAB_SCRIPT, &[&session.id, window_id])?;
    let outcome = close_response(TerminalKind::AppleTerminal, &response)?;
    if outcome == CloseOutcome::Missing {
        return Ok(outcome);
    }

    // The wait receives the close script's reply: only the close that was just sent to
    // the proven window may take the window that Terminal still lists for closed.
    let verification = run(WAIT_FOR_CLOSE_SCRIPT, &[window_id, "20", &response])?;
    if matches!(verification.as_str(), "missing" | "closed") {
        return Ok(CloseOutcome::Closed);
    }

    // Terminal may consume the first close request by terminating the foreground
    // process while leaving its shell surface alive. Retry only the same owned
    // window/TTY proof, then verify from a separate AppleScript transaction.
    let retry = run(CLOSE_TAB_SCRIPT, &[&session.id, window_id])?;
    if close_response(TerminalKind::AppleTerminal, &retry)? == CloseOutcome::Missing {
        return Ok(CloseOutcome::Closed);
    }
    let verification = run(WAIT_FOR_CLOSE_SCRIPT, &[window_id, "100", &retry])?;
    if !matches!(verification.as_str(), "missing" | "closed") {
        bail!("Terminal.app reported a closed tab twice but window {window_id} is still present");
    }
    Ok(CloseOutcome::Closed)
}

fn run_terminal_automation(
    script: &str,
    arguments: &[&str],
    deadline: Option<Instant>,
) -> Result<String> {
    match deadline {
        Some(deadline) => applescript::run_until("Terminal.app", script, arguments, deadline),
        None => applescript::run("Terminal.app", script, arguments),
    }
}

fn ownership_proof(session: &TerminalSession) -> Result<&str> {
    session
        .window_id
        .as_deref()
        .context("Terminal.app session record is missing its window id")
}

// The shipped scripts, executed with Terminal replaced by records: only the reads and
// the close are swapped for mock handlers, the scripts' own decisions run in
// `osascript`, and nothing talks to Terminal.
#[cfg(test)]
mod tests {
    use super::{CLOSE_TAB_SCRIPT, VERIFY_TAB_SCRIPT, WAIT_FOR_CLOSE_SCRIPT, ownership};
    use crate::native::session::SessionState;

    const WINDOW: &str = "8341";
    const TTY: &str = "/dev/ttys014";
    const LIVE: &str = r#"{id:8341, tabs:{{tty:"/dev/ttys014", busy:false}}}"#;
    // The form that windows 8341 and 8345 reported after their shells were killed
    // with no confirmation pending: the old name followed by U+0001
    // (docs/verification/2026-10-01-terminal-proof.md). Bridge kills the shell
    // before it closes the window.
    const KILLED: &str = r#"{id:8341, tabs:{{tty:"/dev/ttys014" & (character id 1), busy:false}}}"#;
    const CHANGED: &str = r#"{id:8341, tabs:{{tty:"/dev/ttys020", busy:false}}}"#;
    const SHARED: &str =
        r#"{id:8341, tabs:{{tty:"/dev/ttys014", busy:false}, {tty:"/dev/ttys020", busy:false}}}"#;
    const UNRELATED: &str = r#"{id:8000, tabs:{{tty:"/dev/ttys014", busy:false}}}"#;
    const EMPTY: &str = r#"{id:8341, tabs:{}}"#;
    // Window 12064 changed from invisible to visible with no tabs after close
    // (2026-10-03, user-confirmed residual session-mpL0WX). Neither state is
    // evidence of absence. Only the close that was just sent takes the invisible
    // one for the window it closed; no later transaction does.
    const HIDDEN_EMPTY: &str = r#"{id:8341, tabs:{}, visible:false}"#;
    const VISIBLE_EMPTY: &str = r#"{id:8341, tabs:{}, visible:true}"#;
    // Not visible, and its tab is still there.
    const HIDDEN_LIVE: &str =
        r#"{id:8341, tabs:{{tty:"/dev/ttys014", busy:false}}, visible:false}"#;
    // Listed as 8341, but its id reads 8342 at the final check before the close.
    const RENUMBERED: &str = r#"{listedId:8341, id:8342, tabs:{{tty:"/dev/ttys014", busy:false}}}"#;
    const DENIED: &str = r#"{-1743, "Not authorized to send Apple events to Terminal."}"#;
    const TIMED_OUT: &str = r#"{-1712, "AppleEvent timed out."}"#;

    fn session() -> super::TerminalSession {
        serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal", "session_id": TTY, "window_id": WINDOW,
            "managed_session_id": "session-test"
        }))
        .unwrap()
    }

    #[test]
    fn attested_close_rechecks_app_before_every_transaction() {
        let session = session();
        let app = ownership::MacTerminalAppIdentity {
            pid: 1234,
            start_seconds: 100,
            start_microseconds: 42,
        };
        for mode in [
            "same",
            "dead",
            "reused",
            "unreadable",
            "restart-after-close",
            "multiple-instances",
            "instance-added-after-close",
        ] {
            let mut observations = 0;
            let mut instance_reads = 0;
            let mut calls = 0;
            let result = super::close_attested_session_with(
                &session,
                &app,
                |pid| {
                    assert_eq!(pid, app.pid);
                    observations += 1;
                    match mode {
                        "dead" => Ok(None),
                        "unreadable" => anyhow::bail!("injected OS error"),
                        "reused" => Ok(Some((101, 42))),
                        "restart-after-close" if observations > 1 => Ok(Some((101, 42))),
                        _ => Ok(Some((100, 42))),
                    }
                },
                || {
                    instance_reads += 1;
                    let mut found = vec![app.clone()];
                    if mode == "multiple-instances"
                        || (mode == "instance-added-after-close" && instance_reads > 1)
                    {
                        let mut other = app.clone();
                        other.pid += 1;
                        found.push(other);
                    }
                    Ok(found)
                },
                |_, _| {
                    calls += 1;
                    Ok(match calls {
                        1 | 3 => "closed",
                        2 => "present",
                        _ => "missing",
                    }
                    .into())
                },
            );
            assert_eq!(
                result.is_ok(),
                matches!(mode, "same" | "dead"),
                "{mode}: {result:?}"
            );
            assert_eq!(
                calls,
                match mode {
                    "same" => 4,
                    "restart-after-close" | "instance-added-after-close" => 1,
                    _ => 0,
                },
                "{mode}"
            );
        }
    }

    // An Apple Event to a Terminal that is not running launches it, without the
    // windows Bridge created.
    const MOCK: &str = r#"
on mockEvent()
    if not mockRunning then
        log "launched Terminal"
        set mockRunning to true
        set mockWindows to {}
    end if
    if mockError is not missing value then error (item 2 of mockError) number (item 1 of mockError)
end mockEvent

on mockWindowIds()
    mockEvent()
    set windowIds to {}
    repeat with mockWindow in mockWindows
        try
            set end of windowIds to listedId of mockWindow
        on error
            set end of windowIds to id of mockWindow
        end try
    end repeat
    return windowIds
end mockWindowIds

on mockWindowWithId(wantedId)
    set windowIds to mockWindowIds()
    repeat with windowIndex from 1 to count of windowIds
        if item windowIndex of windowIds is wantedId then return item windowIndex of mockWindows
    end repeat
    error "Can't get window 1 whose id = " & wantedId & "." number -1728
end mockWindowWithId

on mockClose(targetWindow)
    mockEvent()
    log "closed " & (id of targetWindow)
end mockClose
"#;

    #[derive(Clone, Copy)]
    struct Terminal<'a> {
        running: bool,
        windows: &'a [&'a str],
        error: Option<&'a str>,
    }

    const fn running<'a>(windows: &'a [&'a str]) -> Terminal<'a> {
        Terminal {
            running: true,
            windows,
            error: None,
        }
    }

    const STOPPED: Terminal = Terminal {
        running: false,
        windows: &[LIVE],
        error: None,
    };

    const fn failing(error: &str) -> Terminal<'_> {
        Terminal {
            running: true,
            windows: &[LIVE],
            error: Some(error),
        }
    }

    // The script's response or error, and the windows it closed or brought to the front,
    // or the launch it caused.
    fn replay(
        script: &str,
        terminal: &Terminal,
        arguments: &[&str],
    ) -> (Result<String, String>, Vec<String>) {
        let script = [
            ("tell application \"Terminal\"", "tell me"),
            ("application \"Terminal\" is not running", "not mockRunning"),
            ("application \"Terminal\" is running", "mockRunning"),
            (
                "first window whose id is wantedWindowId",
                "my mockWindowWithId(wantedWindowId)",
            ),
            ("id of every window", "my mockWindowIds()"),
            ("close targetWindow", "my mockClose(targetWindow)"),
        ]
        .iter()
        .fold(script.to_owned(), |script, (term, mock)| {
            script.replace(term, mock)
        });
        for term in ["application \"Terminal\"", " window whose ", "every window"] {
            assert!(!script.contains(term), "unmocked Terminal term {term:?}");
        }
        let state = format!(
            "property mockRunning : {}\nproperty mockError : {}\nproperty mockWindows : {{{}}}\n",
            terminal.running,
            terminal.error.unwrap_or("missing value"),
            terminal.windows.join(", ")
        );
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("{state}{MOCK}{script}"))
            .args(arguments)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = stderr
            .lines()
            .filter(|line| {
                line.starts_with("closed ")
                    || line.starts_with("fronted ")
                    || *line == "launched Terminal"
            })
            .map(str::to_owned)
            .collect();
        let response = if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(stderr)
        };
        (response, events)
    }

    fn check(
        failures: &mut Vec<String>,
        case: &str,
        actual: (Result<String, String>, Vec<String>),
        expected: Result<&str, &str>,
        closes_owned_window: bool,
    ) {
        let events: &[&str] = if closes_owned_window {
            &["closed 8341"]
        } else {
            &[]
        };
        let response_matches = match (&actual.0, expected) {
            (Ok(response), Ok(wanted)) => response == wanted,
            (Err(error), Err(wanted)) => error.contains(wanted),
            _ => false,
        };
        if !response_matches || actual.1 != events {
            failures.push(format!(
                "{case}: expected {expected:?} with {events:?}, got {:?} with {:?}",
                actual.0, actual.1
            ));
        }
    }

    #[test]
    fn terminal_app_close_closes_its_killed_shell_tab_and_reports_only_proven_absence() {
        let close = |terminal: &Terminal| replay(CLOSE_TAB_SCRIPT, terminal, &[TTY, WINDOW]);
        let mut failures = Vec::new();
        let cases = [
            ("Terminal not running", STOPPED, Ok("missing"), false),
            ("live owned tab", running(&[LIVE]), Ok("closed"), true),
            ("no windows", running(&[]), Ok("missing"), false),
            (
                "only another window",
                running(&[UNRELATED]),
                Ok("missing"),
                false,
            ),
            ("killed shell", running(&[KILLED]), Ok("closed"), true),
            (
                "hidden empty window is not closed",
                running(&[HIDDEN_EMPTY]),
                Err("no longer holds its tab"),
                false,
            ),
            (
                "visible empty window is not closed",
                running(&[VISIBLE_EMPTY]),
                Err("no longer holds its tab"),
                false,
            ),
            (
                "window list denied",
                failing(DENIED),
                Err("Not authorized"),
                false,
            ),
            (
                "tab replaced in the owned window",
                running(&[CHANGED]),
                Err("no longer holds its tab"),
                false,
            ),
            (
                "window shared with another tab",
                running(&[SHARED]),
                Err("refuses to close a Terminal.app window containing another tab"),
                false,
            ),
            (
                "owned window without tabs",
                running(&[EMPTY]),
                Err("no longer holds its tab"),
                false,
            ),
            (
                "window identity changed before close",
                running(&[RENUMBERED]),
                Err("window identity changed"),
                false,
            ),
        ];
        for (case, terminal, expected, closed) in cases {
            check(&mut failures, case, close(&terminal), expected, closed);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn terminal_app_close_wait_reports_absence_only_from_the_window_list() {
        let wait = |terminal: &Terminal| replay(WAIT_FOR_CLOSE_SCRIPT, terminal, &[WINDOW, "2"]);
        let mut failures = Vec::new();
        let cases = [
            ("Terminal not running", STOPPED, Ok("missing")),
            ("window closed", running(&[UNRELATED]), Ok("missing")),
            (
                "hidden empty window remains",
                running(&[HIDDEN_EMPTY]),
                Ok("present"),
            ),
            (
                "visible empty window remains",
                running(&[VISIBLE_EMPTY]),
                Ok("present"),
            ),
            ("live owned tab", running(&[LIVE]), Ok("present")),
            ("killed shell", running(&[KILLED]), Ok("present")),
            ("tab replaced", running(&[CHANGED]), Ok("present")),
            (
                "owned window without tabs",
                running(&[EMPTY]),
                Ok("present"),
            ),
            (
                "window list timed out",
                failing(TIMED_OUT),
                Err("timed out"),
            ),
        ];
        for (case, terminal, expected) in cases {
            check(&mut failures, case, wait(&terminal), expected, false);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // Told that the close script answered `closed`, the wait also knows the window that
    // Terminal has closed and still lists: no tabs, and not on the screen.
    #[test]
    fn terminal_app_close_wait_knows_the_listed_window_that_terminal_closed() {
        let wait = |terminal: &Terminal, close_reply: &str| {
            replay(WAIT_FOR_CLOSE_SCRIPT, terminal, &[WINDOW, "2", close_reply])
        };
        let mut failures = Vec::new();
        let cases = [
            ("Terminal not running", STOPPED, "closed", Ok("missing")),
            (
                "window left the list",
                running(&[UNRELATED]),
                "closed",
                Ok("missing"),
            ),
            (
                "closed window still listed",
                running(&[HIDDEN_EMPTY]),
                "closed",
                Ok("closed"),
            ),
            (
                "no close was sent",
                running(&[HIDDEN_EMPTY]),
                "missing",
                Ok("present"),
            ),
            (
                "empty window on the screen",
                running(&[VISIBLE_EMPTY]),
                "closed",
                Ok("present"),
            ),
            (
                "hidden window that holds its tab",
                running(&[HIDDEN_LIVE]),
                "closed",
                Ok("present"),
            ),
            ("live owned tab", running(&[LIVE]), "closed", Ok("present")),
            ("killed shell", running(&[KILLED]), "closed", Ok("present")),
            (
                "window list timed out",
                failing(TIMED_OUT),
                "closed",
                Err("timed out"),
            ),
        ];
        for (case, terminal, close_reply, expected) in cases {
            check(
                &mut failures,
                case,
                wait(&terminal, close_reply),
                expected,
                false,
            );
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // The window can leave the list between the list read and its own read: only the
    // next list read is absence. Any other failed read stays an error.
    #[test]
    fn terminal_app_close_wait_takes_no_failed_read_for_a_closed_window() {
        const LOOKUP: &str = "set listedWindow to first window whose id is wantedWindowId";
        assert!(WAIT_FOR_CLOSE_SCRIPT.contains(LOOKUP));
        for (lookup, expected) in [
            (
                "set mockWindows to {}\nerror \"Invalid index.\" number -1719",
                Ok("missing"),
            ),
            ("error \"Invalid index.\" number -1719", Ok("present")),
            (
                "set mockWindows to {}\nerror \"Can't get window.\" number -1728",
                Ok("missing"),
            ),
            ("error \"Can't get window.\" number -1728", Ok("present")),
            (
                "error \"Not authorized to send Apple events to Terminal.\" number -1743",
                Err("Not authorized"),
            ),
        ] {
            let script = WAIT_FOR_CLOSE_SCRIPT.replace(LOOKUP, lookup);
            let (reply, events) =
                replay(&script, &running(&[HIDDEN_EMPTY]), &[WINDOW, "2", "closed"]);
            match expected {
                Ok(wanted) => assert_eq!(reply.as_deref(), Ok(wanted), "{lookup}"),
                Err(wanted) => assert!(reply.unwrap_err().contains(wanted), "{lookup}"),
            }
            assert!(events.is_empty(), "{lookup}: {events:?}");
        }
    }

    // The whole close as `close_session_with` runs it, with the shipped scripts, against
    // a Terminal that holds the killed shell's tab until it is told to close the window
    // and `after_close` from then on. Returns the outcome and the closes that were sent.
    fn close_lifecycle(
        after_close: Terminal,
    ) -> (anyhow::Result<super::CloseOutcome>, Vec<String>) {
        let mut terminal = running(&[KILLED]);
        let mut closes = Vec::new();
        let outcome = super::close_session_with(&session(), |script, arguments| {
            let mut arguments = arguments.to_vec();
            if script == WAIT_FOR_CLOSE_SCRIPT {
                arguments[1] = "2";
            }
            let (reply, events) = replay(script, &terminal, &arguments);
            if !events.is_empty() {
                terminal = after_close;
                closes.extend(events);
            }
            reply.map_err(anyhow::Error::msg)
        });
        (outcome, closes)
    }

    // The cleanup of session-mpL0WX failed on 2026-10-03 with `window no longer holds
    // its tab`: the close had closed window 12064, Terminal still listed it without
    // tabs and not visible, and the second close found no tab in it.
    #[test]
    fn terminal_app_close_finishes_when_terminal_still_lists_the_window_it_closed() {
        for (case, after_close) in [
            ("closed window still listed", running(&[HIDDEN_EMPTY])),
            ("window left the list", running(&[])),
        ] {
            let (outcome, closes) = close_lifecycle(after_close);
            assert_eq!(
                outcome.map_err(|error| format!("{error:#}")),
                Ok(super::CloseOutcome::Closed),
                "{case}"
            );
            assert_eq!(closes, ["closed 8341"], "{case}");
        }
    }

    // What is not a closed window after the close keeps the close failing, and with it
    // the handle: an empty window on the screen, a tab that both closes left in place,
    // and a window list that can no longer be read.
    #[test]
    fn terminal_app_close_fails_while_its_window_is_on_the_screen_or_unread() {
        for (case, after_close, error, closes_sent) in [
            (
                "empty window on the screen",
                running(&[VISIBLE_EMPTY]),
                "no longer holds its tab",
                1,
            ),
            (
                "tab still there",
                running(&[KILLED]),
                "window 8341 is still present",
                2,
            ),
            ("window list denied", failing(DENIED), "Not authorized", 1),
        ] {
            let (outcome, closes) = close_lifecycle(after_close);
            let failure = format!("{:#}", outcome.expect_err(case));
            assert!(failure.contains(error), "{case}: {failure}");
            assert_eq!(closes, vec!["closed 8341"; closes_sent], "{case}");
        }
    }

    #[test]
    fn terminal_app_verify_reports_a_read_error_not_absence() {
        let verify = |terminal: &Terminal| replay(VERIFY_TAB_SCRIPT, terminal, &[TTY, WINDOW]);
        let mut failures = Vec::new();
        let cases = [
            ("Terminal not running", STOPPED, Ok("missing")),
            ("window closed", running(&[UNRELATED]), Ok("missing")),
            (
                "hidden empty window is not absence",
                running(&[HIDDEN_EMPTY]),
                Err("no longer matches a tab"),
            ),
            (
                "visible empty window is not absence",
                running(&[VISIBLE_EMPTY]),
                Err("no longer matches a tab"),
            ),
            ("live owned tab", running(&[LIVE]), Ok(TTY)),
            ("window list denied", failing(DENIED), Err("Not authorized")),
            (
                "killed shell",
                running(&[KILLED]),
                Err("no longer matches a tab"),
            ),
            (
                "changed tty",
                running(&[CHANGED]),
                Err("no longer matches a tab"),
            ),
            (
                "empty window",
                running(&[EMPTY]),
                Err("no longer matches a tab"),
            ),
        ];
        for (case, terminal, expected) in cases {
            check(&mut failures, case, verify(&terminal), expected, false);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // The window of an earlier session, closed and still listed, and a window on the
    // screen. The last one is closed by its own session while the new window opens.
    const CLOSED_LISTED: &str = r#"{id:8341, tabs:{}, visible:false, frontmost:false}"#;
    const ON_SCREEN: &str =
        r#"{id:8100, tabs:{{tty:"/dev/ttys001", busy:false}}, visible:true, frontmost:false}"#;
    const CLOSED_MEANWHILE: &str = r#"{id:8100, tabs:{{tty:"/dev/ttys001", busy:false}}, visible:true, frontmost:false, closesDuringOpen:true}"#;

    // An untargeted `do script` opens a window in front of the others and types its
    // text there: the mock accepts only the line kill followed by the start line.
    // Setting `frontmost` is `makeKeyAndOrderFront:` in Terminal 2.15, which shows a
    // window that is not on the screen; the windows that were made frontmost are
    // reported after the run.
    const START_LINE: &str = ". '/state/session-test/terminal-start.sh'";
    const OPEN_MOCK: &str = r#"
on mockWindowList()
    mockEvent()
    return mockWindows
end mockWindowList

on mockDoScript(creationText)
    mockEvent()
    if creationText is not ((character id 21) & ". '/state/session-test/terminal-start.sh'") then error "unexpected creation text"
    repeat with mockWindow in mockWindows
        try
            if closesDuringOpen of mockWindow then
                set tabs of mockWindow to {}
                set visible of mockWindow to false
            end if
        end try
    end repeat
    set newTab to {tty:"/dev/ttys030", busy:false}
    set mockWindows to {{id:9000, tabs:{newTab}, visible:true, frontmost:false}} & mockWindows
    return newTab
end mockDoScript

on mockVisibleWindowWithId(wantedId)
    set foundWindow to mockWindowWithId(wantedId)
    if visible of foundWindow then return foundWindow
    error "Can't get window 1 whose id = " & wantedId & " and visible = true." number -1728
end mockVisibleWindowWithId

on run argv
    set reply to openRun(argv)
    repeat with mockWindow in mockWindows
        if frontmost of mockWindow then log "fronted " & (id of mockWindow)
    end repeat
    return reply
end run
"#;

    // A window that Terminal had closed and still listed came back, empty, when the next
    // session was opened: it was `window 1`, and the keyboard was given back to it
    // (2026-10-03: window 12678, read before and after the script; the user saw window
    // 12064 return twice).
    #[test]
    fn terminal_app_open_gives_the_keyboard_back_only_to_a_window_on_the_screen() {
        let script = [
            ("on run argv", "on openRun(argv)"),
            ("end run", "end openRun"),
            ("get windows", "my mockWindowList()"),
            ("(count of windows)", "(count of mockWindows)"),
            ("window 1", "(item 1 of mockWindows)"),
            (
                "do script ((character id 21) & bridgeCommand)",
                "my mockDoScript((character id 21) & bridgeCommand)",
            ),
            (
                "first window whose id is targetWindowId",
                "my mockWindowWithId(targetWindowId)",
            ),
            (
                "first window whose id is keyboardWindowId and visible is true",
                "my mockVisibleWindowWithId(keyboardWindowId)",
            ),
            (
                "first window whose id is keyboardWindowId",
                "my mockWindowWithId(keyboardWindowId)",
            ),
        ]
        .iter()
        .fold(super::OPEN_TAB_SCRIPT.to_owned(), |script, (term, mock)| {
            script.replace(term, mock)
        }) + OPEN_MOCK;
        assert!(!script.contains("do script ("), "unmocked typed start");
        let open = |windows: &[&str]| replay(&script, &running(windows), &[START_LINE]);
        let opened = Ok("/dev/ttys030\n9000".to_owned());
        let fronted = |windows: &[&str]| -> Vec<String> {
            windows.iter().map(|id| format!("fronted {id}")).collect()
        };

        assert_eq!(open(&[ON_SCREEN]), (opened.clone(), fronted(&["8100"])));
        assert_eq!(open(&[CLOSED_LISTED]), (opened.clone(), fronted(&[])));
        assert_eq!(open(&[CLOSED_MEANWHILE]), (opened, fronted(&[])));
        // Any other text than the start line behind the line kill is not typed.
        let (reply, events) = replay(&script, &running(&[ON_SCREEN]), &["exit"]);
        assert!(reply.unwrap_err().contains("unexpected creation text"));
        assert!(events.is_empty(), "{events:?}");
    }

    // A private PTY in the state that Terminal 2.15 gives a new tab: canonical input
    // with echo and the kernel's control characters (Terminal sets lflag 0x5cb, which is
    // TTYDEF_LFLAG, and leaves c_cc alone). Nothing below talks to a terminal
    // application or to the keyboard.
    fn open_tab_pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::{AsRawFd, FromRawFd};
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
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        };
        let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
            assert_eq!(libc::tcgetattr(slave.as_raw_fd(), modes.as_mut_ptr()), 0);
        }
        let modes = unsafe { modes.assume_init() };
        assert_eq!(
            modes.c_lflag & (libc::ICANON | libc::ECHO),
            libc::ICANON | libc::ECHO
        );
        assert_eq!(modes.c_cc[libc::VKILL], 0x15);
        (master, slave)
    }

    // The shell of the tab: the session leader of the PTY, as `login` starts it.
    fn spawn_tab_shell(
        slave: &std::fs::File,
        command: &mut std::process::Command,
    ) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }

    // Collects what the tab shows until `done` holds. False at the deadline.
    fn read_tab_until(
        master: &mut std::fs::File,
        screen: &mut Vec<u8>,
        deadline: std::time::Instant,
        mut done: impl FnMut(&[u8]) -> bool,
    ) -> bool {
        use std::io::Read;
        loop {
            let mut bytes = [0; 4096];
            if let Ok(count) = master.read(&mut bytes) {
                screen.extend_from_slice(&bytes[..count]);
            }
            if done(screen) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    const PROMPT: &str = "AB_READY> ";

    // The start of session-t9E3YV failed on 2026-10-03: the user was typing `a` into
    // another Terminal window, one `a` reached the new window while it had the
    // keyboard, the start command was typed behind it, and the shell ran
    // `a. '<...>/launch.sh'` (`zsh: command not found: a.`). Terminal 2.15 types every
    // command that a script gives it, so a key can be in the tab's input in front of
    // the command, with the shell still starting (the recorded case) or already in its
    // line editor. Replayed here with the bytes that the shipped script makes Terminal
    // write, in that order.
    #[test]
    fn terminal_app_startup_keys_cannot_change_the_launch_command() {
        use std::io::Write;
        // An empty creation leaves the command to a later `do script ... in tab`,
        // which ends it with a carriage return; a creation text ends with a line feed.
        let script = super::OPEN_TAB_SCRIPT;
        let typed_after_creation = script.contains("do script \"\"");
        let line_kill = script.contains("do script ((character id 21) & bridgeCommand)");
        let mut failures = Vec::new();
        for (shell, arguments, editor) in [
            ("/bin/zsh", ["-f", "-i"].as_slice(), "ed"),
            ("/bin/zsh", ["-f", "-i"].as_slice(), "vi"),
            (
                "/bin/bash",
                ["--noprofile", "--norc", "-i"].as_slice(),
                "ed",
            ),
        ] {
            for line_editor_active in [false, true] {
                let case = format!(
                    "{shell} EDITOR={editor}, {}",
                    if line_editor_active {
                        "key typed into the line editor"
                    } else {
                        "key typed while the shell starts"
                    }
                );
                let directory = tempfile::tempdir().unwrap();
                let marker = directory.path().join("started");
                let script = directory.path().join("launch.sh");
                std::fs::write(
                    &script,
                    format!(
                        "printf started > {}; exit\n",
                        crate::native::shell_quote(marker.as_os_str())
                    ),
                )
                .unwrap();
                let bridge_command =
                    format!(". {}", crate::native::shell_quote(script.as_os_str()));
                let end = if typed_after_creation { "\r" } else { "\n" };
                let typed = format!(
                    "{}{bridge_command}{end}exit{end}",
                    if line_kill { "\u{15}" } else { "" }
                );

                let (mut master, slave) = open_tab_pty();
                let mut command = std::process::Command::new(shell);
                command
                    .args(arguments)
                    .env("PS1", PROMPT)
                    .env("EDITOR", editor)
                    .env_remove("VISUAL")
                    .env("ZDOTDIR", directory.path())
                    .current_dir(directory.path());
                let mut screen = Vec::new();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut child = if line_editor_active {
                    let child = spawn_tab_shell(&slave, &mut command);
                    assert!(
                        read_tab_until(&mut master, &mut screen, deadline, |screen| {
                            String::from_utf8_lossy(screen).contains(PROMPT)
                        }),
                        "{case}: no prompt: {}",
                        String::from_utf8_lossy(&screen)
                    );
                    master.write_all(b"a").unwrap();
                    assert!(
                        read_tab_until(&mut master, &mut screen, deadline, |screen| {
                            String::from_utf8_lossy(screen)
                                .rsplit(PROMPT)
                                .next()
                                .is_some_and(|line| line.contains('a'))
                        }),
                        "{case}: the line editor did not take the key: {}",
                        String::from_utf8_lossy(&screen)
                    );
                    master.write_all(typed.as_bytes()).unwrap();
                    child
                } else {
                    master.write_all(b"a").unwrap();
                    master.write_all(typed.as_bytes()).unwrap();
                    spawn_tab_shell(&slave, &mut command)
                };
                let ended = read_tab_until(&mut master, &mut screen, deadline, |_| {
                    child.try_wait().unwrap().is_some()
                });
                if !ended {
                    child.kill().unwrap();
                    child.wait().unwrap();
                }
                if !ended || !marker.exists() {
                    failures.push(format!(
                        "{case}: the typed key changed the launch command: {:?}",
                        String::from_utf8_lossy(&screen)
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // A launch as it stands when the surface is opened: a private session directory,
    // the status `launching`, the retained initial claim and its pending receipt.
    fn launch_fixture() -> tempfile::TempDir {
        use crate::native::{
            acquire_turn_claim, launch, set_private_directory_permissions, update_status,
        };
        let directory = tempfile::Builder::new()
            .prefix("session-terminal-")
            .tempdir()
            .unwrap();
        set_private_directory_permissions(directory.path()).unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        launch::begin(
            &crate::native::session::Store::open_unchecked(directory.path()),
            &token,
            std::time::Instant::now() + std::time::Duration::from_secs(20),
        )
        .unwrap();
        directory
    }

    fn binding(directory: &std::path::Path, tty: &str) -> serde_json::Value {
        serde_json::json!({
            "terminal": "apple-terminal", "session_id": tty, "window_id": WINDOW,
            "managed_session_id": directory.file_name().unwrap().to_str().unwrap()
        })
    }

    fn bind(directory: &std::path::Path, binding: &serde_json::Value) {
        crate::native::write_json_atomic(
            &directory.join(crate::native::TERMINAL_HANDLE_FILE),
            binding,
        )
        .unwrap();
    }

    // The gate opens for the launcher's binding of this tty in a pending launch, and
    // for nothing else.
    #[test]
    fn terminal_app_host_starts_only_for_its_bound_tty_in_a_pending_launch() {
        use crate::native::{
            TERMINAL_HANDLE_FILE, TURN_CLAIM_FILE, launch, update_status, write_json_atomic,
        };
        for case in [
            "bound",
            "another tty",
            "another session",
            "another terminal",
            "no window",
            "unreadable binding",
            "closed",
            "failed",
            "expired",
            "another claim",
            "released claim",
            "spawn attempted",
        ] {
            let directory = launch_fixture();
            let directory = directory.path();
            let id = directory.file_name().unwrap().to_str().unwrap();
            let mut handle = binding(directory, TTY);
            match case {
                "another tty" => handle["session_id"] = "/dev/ttys020".into(),
                "another session" => handle["managed_session_id"] = "session-other".into(),
                "another terminal" => handle["terminal"] = "iterm2".into(),
                "no window" => {
                    handle.as_object_mut().unwrap().remove("window_id");
                }
                _ => {}
            }
            if case == "unreadable binding" {
                std::fs::write(directory.join(TERMINAL_HANDLE_FILE), b"{").unwrap();
            } else {
                bind(directory, &handle);
            }
            match case {
                "closed" | "failed" => {
                    update_status(directory, case.parse().unwrap(), None, None).unwrap()
                }
                "expired" | "another claim" | "spawn attempted" => {
                    let mut receipt =
                        launch::read(&crate::native::session::Reader::open_unchecked(directory))
                            .unwrap()
                            .unwrap();
                    match case {
                        "expired" => receipt.deadline_unix_ms = 0,
                        "another claim" => receipt.claim_token = "unrelated".into(),
                        _ => receipt.phase = launch::Phase::Spawning,
                    }
                    write_json_atomic(&directory.join(launch::FILE), &receipt).unwrap();
                }
                "released claim" => std::fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap(),
                _ => {}
            }
            assert_eq!(
                super::wait_for_binding(directory, id, TTY).is_ok(),
                case == "bound",
                "{case}"
            );
        }
    }

    // The gate reads no record in a directory that another account could have prepared,
    // and writes nothing there.
    #[test]
    fn terminal_app_host_refuses_a_directory_that_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let refused = |directory: &std::path::Path, reason: &str| {
            let error = format!("{:#}", super::run_host(directory).unwrap_err());
            assert!(error.contains(reason), "{}: {error}", directory.display());
        };

        let shared = launch_fixture();
        bind(shared.path(), &binding(shared.path(), TTY));
        let log = std::fs::read(shared.path().join(crate::native::launch::LOG)).unwrap();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        refused(shared.path(), "not private");

        let private = launch_fixture();
        let link = root.path().join("session-link");
        std::os::unix::fs::symlink(private.path(), &link).unwrap();
        refused(&link, "not private");

        let unnamed = root.path().join("not-a-session");
        std::fs::create_dir(&unnamed).unwrap();
        crate::native::set_private_directory_permissions(&unnamed).unwrap();
        refused(&unnamed, "invalid Agent Bridge session id");

        refused(&root.path().join("session-missing"), "failed to inspect");
        assert_eq!(
            std::fs::read(shared.path().join(crate::native::launch::LOG)).unwrap(),
            log
        );
    }

    #[test]
    fn terminal_app_bootstrap_is_a_private_file_behind_a_short_line() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let start = super::install_bootstrap(
            directory.path(),
            "'/bin/agent bridge' native-terminal-host '/state/session-test'",
            ". '/state/session-test/launch.sh'",
        )
        .unwrap();
        let path = directory.path().join("terminal-start.sh");
        assert_eq!(
            start,
            format!(". {}", crate::native::shell_quote(path.as_os_str()))
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "'/bin/agent bridge' native-terminal-host '/state/session-test' || exit\n. '/state/session-test/launch.sh'\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Every byte that is queued for a reader of standard input, whatever the line
    // discipline holds back in canonical mode: a provider's TUI reads without it.
    fn queued_input_bytes() -> libc::c_int {
        let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
        let mut queued: libc::c_int = -1;
        unsafe {
            assert_eq!(libc::tcgetattr(0, modes.as_mut_ptr()), 0);
            let saved = modes.assume_init();
            let mut raw = saved;
            raw.c_lflag &= !libc::ICANON;
            assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &raw), 0);
            assert_eq!(libc::ioctl(0, libc::FIONREAD, &mut queued), 0);
            assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &saved), 0);
        }
        queued
    }

    // Runs only as the gate or as the wrapper of the private PTY test below.
    #[test]
    fn terminal_app_start_probe() {
        let Ok(mode) = std::env::var("AB_TERMINAL_PROBE_MODE") else {
            return;
        };
        let directory =
            std::path::PathBuf::from(std::env::var_os("AB_TERMINAL_PROBE_DIR").unwrap());
        if mode == "host" {
            std::fs::write(directory.join("host-entered"), b"").unwrap();
            if let Err(error) = super::run_host(&directory) {
                eprintln!("{error:#}");
                std::process::exit(7);
            }
            return;
        }
        // The wrapper: what `native-session` records about itself in a Terminal.app
        // tab, which fails unless the tab's shell is its parent and it leads a
        // foreground group of its own.
        let id = directory.file_name().unwrap().to_str().unwrap();
        let owner = ownership::current_native_session_owner(id).unwrap();
        let shell = owner.terminal_shell.unwrap();
        crate::native::write_json_atomic(
            &directory.join("owner-probe.json"),
            &serde_json::json!({
                "group": owner.process_group, "shell": shell.pid,
                "shell_group": shell.process_group, "queued_input": queued_input_bytes()
            }),
        )
        .unwrap();
    }

    // The whole typed start on a private PTY: the bytes that Terminal writes for the
    // creation text, a key in front of it and lines behind it, the tab's shell, and
    // the gate and the wrapper as two jobs of that shell.
    #[test]
    fn terminal_app_start_waits_for_the_binding_and_discards_startup_keys() {
        for outcome in ["bound", "closed", "another tty", "another standard input"] {
            start_on_private_pty(outcome);
        }
    }

    fn tty_name(tty: &std::fs::File) -> String {
        use std::os::fd::AsRawFd;
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(
            unsafe { libc::ttyname_r(tty.as_raw_fd(), name.as_mut_ptr(), name.len()) },
            0
        );
        unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn start_on_private_pty(outcome: &str) {
        use std::io::Write;
        let fixture = launch_fixture();
        let directory = fixture.path();
        let (mut master, slave) = open_tab_pty();
        let tty = tty_name(&slave);
        // A gate whose standard input is another tty than its controlling terminal:
        // the binding of that other tty must not start the wrapper in this tab.
        let (_other_master, other) = open_tab_pty();
        let other = tty_name(&other);
        let foreign_input = outcome == "another standard input";
        let probe = format!(
            "{} --exact native::terminal::macos::apple_terminal::tests::terminal_app_start_probe --nocapture --test-threads=1",
            crate::native::shell_quote(std::env::current_exe().unwrap().as_os_str())
        );
        let start = super::install_bootstrap(
            directory,
            &if foreign_input {
                format!("AB_TERMINAL_PROBE_MODE=host {probe} < {other}")
            } else {
                format!("AB_TERMINAL_PROBE_MODE=host {probe}")
            },
            &format!("AB_TERMINAL_PROBE_MODE=owner {probe}; exit $?"),
        )
        .unwrap();
        if foreign_input {
            bind(directory, &binding(directory, &other));
        }
        // The script hands Terminal the line kill and the start line, and Terminal
        // ends the creation text with a line feed.
        assert!(
            super::OPEN_TAB_SCRIPT
                .contains("set targetTab to do script ((character id 21) & bridgeCommand)")
        );
        // A key that Terminal wrote in front of the creation text, the text, and a
        // line that was typed behind it while the new window had the keyboard.
        master
            .write_all(format!("a\u{15}{start}\naaaa\n").as_bytes())
            .unwrap();
        let mut command = std::process::Command::new("/bin/zsh");
        command
            .args(["-f", "-i"])
            .env("PS1", PROMPT)
            .env("EDITOR", "ed")
            .env_remove("VISUAL")
            .env("ZDOTDIR", directory)
            .env("AB_TERMINAL_PROBE_DIR", directory)
            .current_dir(directory);
        let mut child = spawn_tab_shell(&slave, &mut command);
        let mut screen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let shown = |screen: &[u8]| String::from_utf8_lossy(screen).into_owned();
        assert!(
            read_tab_until(&mut master, &mut screen, deadline, |_| {
                directory.join("host-entered").exists()
            }),
            "{outcome}: the gate never ran: {}",
            shown(&screen)
        );
        let wrapper = directory.join("owner-probe.json");
        if !foreign_input {
            // The launcher is still proving the window. Whatever is typed, nothing
            // starts.
            master.write_all(b"more keys\n").unwrap();
            assert!(
                !read_tab_until(
                    &mut master,
                    &mut screen,
                    std::time::Instant::now() + std::time::Duration::from_millis(300),
                    |_| wrapper.exists() || child.try_wait().unwrap().is_some()
                ),
                "{outcome}: the start did not wait for the binding: {}",
                shown(&screen)
            );
            match outcome {
                "bound" => bind(directory, &binding(directory, &tty)),
                "another tty" => bind(directory, &binding(directory, "/dev/ttys999")),
                _ => crate::native::update_status(directory, SessionState::Closed, None, None)
                    .unwrap(),
            }
        }
        if !read_tab_until(&mut master, &mut screen, deadline, |_| {
            child.try_wait().unwrap().is_some()
        }) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{outcome}: the tab's shell did not end: {}", shown(&screen));
        }
        let status = child.wait().unwrap();
        let log = std::fs::read_to_string(directory.join(crate::native::launch::LOG)).unwrap();
        if outcome == "bound" {
            assert!(status.success(), "{}", shown(&screen));
            let probe: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&wrapper).unwrap()).unwrap();
            assert_eq!(
                probe["queued_input"], 0,
                "startup keys reached the wrapper: {probe}"
            );
            assert_eq!(probe["shell"], child.id(), "{probe}");
            assert_eq!(probe["shell_group"], child.id(), "{probe}");
            assert_ne!(probe["group"], probe["shell_group"], "{probe}");
            assert!(
                log.contains(&format!("terminal_host_released tty={tty}\n")),
                "{log}"
            );
        } else {
            assert!(!wrapper.exists(), "{outcome}: the wrapper started");
            assert_eq!(status.code(), Some(7), "{outcome}: {}", shown(&screen));
            assert!(log.contains("terminal_host_refused: "), "{outcome}: {log}");
        }
    }
}
