use std::{
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
        set targetTab to do script ""
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

pub(in crate::native) const START_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    set bridgeCommand to item 3 of argv
    tell application "Terminal"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            error "Agent Bridge Terminal.app window not found before startup"
        end try
        set targetTab to missing value
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if tty of candidateTab is wantedTty then
                set targetTab to candidateTab
                set matchCount to matchCount + 1
            end if
        end repeat
        if matchCount is not 1 then error "Agent Bridge Terminal.app startup proof did not match exactly one tab"
        do script bridgeCommand in targetTab
        return "started"
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

// A closed window is one that Terminal's window list lacks. The tty proves nothing
// here: it changes when the shell ends, and the window stays on the screen.
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
                    -- list read decides. Every other failed read is an error.
                    if errorNumber is not -1728 then error errorText number errorNumber
                end try
            end if
            delay 0.05
        end repeat
    end tell
    return "present"
end run
"#;

pub(super) fn create_tab(deadline: Instant) -> Result<TerminalSession> {
    let response = applescript::run_until("Terminal.app", OPEN_TAB_SCRIPT, &[], deadline)?;
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

pub(super) fn start_session(
    session: &TerminalSession,
    command: &str,
    deadline: Instant,
) -> Result<()> {
    let window_id = ownership_proof(session)?;
    let response = applescript::run_until(
        "Terminal.app",
        START_SESSION_SCRIPT,
        &[&session.id, window_id, command],
        deadline,
    )?;
    if response != "started" {
        bail!("unexpected Terminal.app start response: {response:?}");
    }
    Ok(())
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
    app: &crate::native::MacTerminalAppIdentity,
) -> Result<CloseOutcome> {
    close_attested_session_with(
        session,
        app,
        crate::native::macos_process_start,
        crate::native::terminal_app_instances,
        |script, arguments| run_terminal_automation(script, arguments, None),
    )
}

fn close_attested_session_with(
    session: &TerminalSession,
    app: &crate::native::MacTerminalAppIdentity,
    mut process_birth: impl FnMut(u32) -> Result<Option<(u64, u64)>>,
    mut instances: impl FnMut() -> Result<Vec<crate::native::MacTerminalAppIdentity>>,
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
    use super::{CLOSE_TAB_SCRIPT, VERIFY_TAB_SCRIPT, WAIT_FOR_CLOSE_SCRIPT};

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
        let app = crate::native::MacTerminalAppIdentity {
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

    // `do script ""` opens a window in front of the others. Setting `frontmost` is
    // `makeKeyAndOrderFront:` in Terminal 2.15, which shows a window that is not on
    // the screen; the windows that were made frontmost are reported after the run.
    const OPEN_MOCK: &str = r#"
on mockWindowList()
    mockEvent()
    return mockWindows
end mockWindowList

on mockDoScript()
    mockEvent()
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
            ("do script \"\"", "my mockDoScript()"),
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
        let open = |windows: &[&str]| replay(&script, &running(windows), &[]);
        let opened = Ok("/dev/ttys030\n9000".to_owned());
        let fronted = |windows: &[&str]| -> Vec<String> {
            windows.iter().map(|id| format!("fronted {id}")).collect()
        };

        assert_eq!(open(&[ON_SCREEN]), (opened.clone(), fronted(&["8100"])));
        assert_eq!(open(&[CLOSED_LISTED]), (opened.clone(), fronted(&[])));
        assert_eq!(open(&[CLOSED_MEANWHILE]), (opened, fronted(&[])));
    }
}
