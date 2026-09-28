use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    applescript, close_response,
};

pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
on windowIdForTty(wantedTty)
    tell application "Terminal"
        set matchedWindowId to missing value
        set matchCount to 0
        set candidateWindows to get windows
        repeat with candidateWindow in candidateWindows
            try
                set candidateTabs to get tabs of candidateWindow
                repeat with candidateTab in candidateTabs
                    if tty of candidateTab is wantedTty then
                        set matchedWindowId to id of candidateWindow
                        set matchCount to matchCount + 1
                    end if
                end repeat
            end try
        end repeat
        if matchCount is not 1 then error "Agent Bridge could not prove the newly created Terminal.app window"
        return matchedWindowId
    end tell
end windowIdForTty

on run argv
    tell application "Terminal"
        -- Untargeted do script creates a dedicated window and returns its new tab.
        -- Never derive ownership from a restored front/current/selected surface.
        set targetTab to do script ""
        set targetTty to tty of targetTab
        set targetWindowId to my windowIdForTty(targetTty)
        set targetWindow to first window whose id is targetWindowId
        if id of targetWindow is not targetWindowId then error "Agent Bridge lost its newly created Terminal.app window"
        if tty of targetTab is not targetTty then error "Agent Bridge lost its newly created Terminal.app tty"
        activate
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
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if tty of candidateTab is wantedTty then set matchCount to matchCount + 1
        end repeat
        if matchCount is 1 then return wantedTty
        if matchCount is 0 then return "missing"
        error "Agent Bridge Terminal.app ownership proof matched multiple tabs"
    end tell
end run
"#;

pub(in crate::native) const CLOSE_TAB_SCRIPT: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    tell application "Terminal"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try

        set targetTab to missing value
        set matchCount to 0
        repeat with candidateTab in tabs of targetWindow
            if tty of candidateTab is wantedTty then
                set targetTab to candidateTab
                set matchCount to matchCount + 1
            end if
        end repeat
        if matchCount is 0 then return "missing"
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

        if id of targetWindow is not wantedWindowId then return "missing"
        if tty of targetTab is not wantedTty then return "missing"
        if (count of tabs of targetWindow) is not 1 then error "Agent Bridge refuses to close a Terminal.app window containing another tab"
        close targetWindow
        return "closed"
    end tell
end run
"#;

pub(in crate::native) const WAIT_FOR_CLOSE_SCRIPT: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    set attemptCount to item 3 of argv as integer
    if not application "Terminal" is running then return "missing"
    tell application "Terminal"
        repeat attemptCount times
            set targetExists to false
            try
                set targetWindow to first window whose id is wantedWindowId
                repeat with candidateTab in tabs of targetWindow
                    if tty of candidateTab is wantedTty then
                        set targetExists to true
                        exit repeat
                    end if
                end repeat
            end try
            if not targetExists then return "missing"
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
    let window_id = ownership_proof(session)?;
    let response = run_terminal_automation(CLOSE_TAB_SCRIPT, &[&session.id, window_id], deadline)?;
    let outcome = close_response(TerminalKind::AppleTerminal, &response)?;
    if outcome == CloseOutcome::Missing {
        return Ok(outcome);
    }

    let verification = run_terminal_automation(
        WAIT_FOR_CLOSE_SCRIPT,
        &[&session.id, window_id, "20"],
        deadline,
    )?;
    if verification == "missing" {
        return Ok(CloseOutcome::Closed);
    }

    // Terminal may consume the first close request by terminating the foreground
    // process while leaving its shell surface alive. Retry only the same owned
    // window/TTY proof, then verify from a separate AppleScript transaction.
    let retry = run_terminal_automation(CLOSE_TAB_SCRIPT, &[&session.id, window_id], deadline)?;
    if close_response(TerminalKind::AppleTerminal, &retry)? == CloseOutcome::Missing {
        return Ok(CloseOutcome::Closed);
    }
    let verification = run_terminal_automation(
        WAIT_FOR_CLOSE_SCRIPT,
        &[&session.id, window_id, "100"],
        deadline,
    )?;
    if verification != "missing" {
        bail!(
            "Terminal.app reported a closed tab twice but tty {:?} is still present",
            session.id
        );
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
