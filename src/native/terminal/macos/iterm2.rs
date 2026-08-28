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
on run
    set itermWasRunning to application "iTerm2" is running
    tell application "iTerm2"
        activate
        if not itermWasRunning then
            set targetWindow to (create window with default profile)
            set targetSession to current session of targetWindow
        else if (count of windows) is 0 then
            set targetWindow to (create window with default profile)
            set targetSession to current session of targetWindow
        else
            set targetWindow to current window
            tell targetWindow
                set targetTab to (create tab with default profile)
                set targetSession to current session of targetTab
            end tell
        end if
        tell targetSession
            return unique ID
        end tell
    end tell
end run
"#;

pub(in crate::native) const START_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set bridgeCommand to item 2 of argv
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        tell targetSession to write text bridgeCommand
                        return "started"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "Agent Bridge iTerm session not found before startup"
end run
"#;

pub(in crate::native) const SEND_FILE_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set promptPath to item 2 of argv
    set carriageReturn to return
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        write targetSession contents of file (POSIX file promptPath)
                        write targetSession text carriageReturn newline false
                        return "sent"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "Agent Bridge iTerm session not found"
end run
"#;

pub(in crate::native) const VERIFY_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set matchedTty to missing value
    set matchCount to 0
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        set matchedTty to tty of targetSession
                        set matchCount to matchCount + 1
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    if matchCount is not 1 then error "Agent Bridge iTerm2 ownership proof did not match exactly one session"
    if matchedTty is missing value or matchedTty is "" then error "Agent Bridge iTerm2 session has no tty"
    return matchedTty
end run
"#;

pub(in crate::native) const CLOSE_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        close targetSession
                        return "closed"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    return "missing"
end run
"#;

pub(super) fn create_tab(deadline: Instant) -> Result<TerminalSession> {
    let id = applescript::run_until("iTerm2", OPEN_TAB_SCRIPT, &[], deadline)?;
    if id.is_empty() {
        bail!("iTerm2 did not return a session id");
    }
    Ok(TerminalSession {
        kind: TerminalKind::Iterm2,
        id,
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        windows_process_identity: None,
    })
}

pub(super) fn start_session(
    session: &TerminalSession,
    command: &str,
    deadline: Instant,
) -> Result<()> {
    let response = applescript::run_until(
        "iTerm2",
        START_SESSION_SCRIPT,
        &[&session.id, command],
        deadline,
    )?;
    if response != "started" {
        bail!("unexpected iTerm2 start response: {response:?}");
    }
    Ok(())
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let response = applescript::run_send_until(
        "iTerm2",
        SEND_FILE_SCRIPT,
        &[&session.id, prompt_path],
        deadline,
    )?;
    if response != "sent" {
        return Err(TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
            "unexpected iTerm2 send response: {response:?}"
        )));
    }
    Ok(())
}

pub(super) fn verify_session(
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    let tty = match timeout {
        Some(timeout) => applescript::run_until(
            "iTerm2",
            VERIFY_SESSION_SCRIPT,
            &[&session.id],
            super::timeout_deadline(timeout)?,
        ),
        None => applescript::run("iTerm2", VERIFY_SESSION_SCRIPT, &[&session.id]),
    }?;
    if tty.is_empty() {
        bail!("iTerm2 ownership proof returned an empty tty");
    }
    Ok(tty)
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    let response = applescript::run("iTerm2", CLOSE_SESSION_SCRIPT, &[&session.id])?;
    close_response(TerminalKind::Iterm2, &response)
}

pub(super) fn close_session_until(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let response =
        applescript::run_until("iTerm2", CLOSE_SESSION_SCRIPT, &[&session.id], deadline)?;
    close_response(TerminalKind::Iterm2, &response)
}
