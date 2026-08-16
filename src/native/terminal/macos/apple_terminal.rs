use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession, applescript, close_response};

pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
on windowIdForTty(wantedTty)
    tell application "Terminal"
        set matchedWindowId to missing value
        set matchCount to 0
        repeat with candidateWindow in windows
            repeat with candidateTab in tabs of candidateWindow
                if tty of candidateTab is wantedTty then
                    set matchedWindowId to id of candidateWindow
                    set matchCount to matchCount + 1
                end if
            end repeat
        end repeat
        if matchCount is not 1 then error "Agent Bridge could not prove the newly created Terminal.app window"
        return matchedWindowId
    end tell
end windowIdForTty

on run argv
    set bridgeCommand to item 1 of argv
    tell application "Terminal"
        -- Untargeted do script creates a dedicated window and returns its new tab.
        -- Never derive ownership from a restored front/current/selected surface.
        set targetTab to do script bridgeCommand
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

pub(in crate::native) const CLOSE_TAB_SCRIPT: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    set controlC to character id 3
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

        -- Terminal ignores native close requests while a foreground process is
        -- busy. Re-check the full ownership proof before every interrupt.
        repeat 3 times
            if not busy of targetTab then exit repeat
            if id of targetWindow is not wantedWindowId then return "missing"
            if tty of targetTab is not wantedTty then return "missing"
            do script controlC in targetTab
            repeat 20 times
                delay 0.05
                if not busy of targetTab then exit repeat
            end repeat
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

pub(super) fn open_tab(command: &str) -> Result<TerminalSession> {
    let response = applescript::run("Terminal.app", OPEN_TAB_SCRIPT, &[command])?;
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

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    let window_id = ownership_proof(session)?;
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    let response = applescript::run(
        "Terminal.app",
        SEND_FILE_SCRIPT,
        &[&session.id, window_id, prompt_path],
    )?;
    if response != "sent" {
        bail!("unexpected Terminal.app send response: {response:?}");
    }
    Ok(())
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    let window_id = ownership_proof(session)?;
    let response = applescript::run("Terminal.app", CLOSE_TAB_SCRIPT, &[&session.id, window_id])?;
    let outcome = close_response(TerminalKind::AppleTerminal, &response)?;
    if outcome == CloseOutcome::Missing {
        return Ok(outcome);
    }

    let verification = applescript::run(
        "Terminal.app",
        WAIT_FOR_CLOSE_SCRIPT,
        &[&session.id, window_id, "20"],
    )?;
    if verification == "missing" {
        return Ok(CloseOutcome::Closed);
    }

    // Terminal may consume the first close request by terminating the foreground
    // process while leaving its shell surface alive. Retry only the same owned
    // window/TTY proof, then verify from a separate AppleScript transaction.
    let retry = applescript::run("Terminal.app", CLOSE_TAB_SCRIPT, &[&session.id, window_id])?;
    if close_response(TerminalKind::AppleTerminal, &retry)? == CloseOutcome::Missing {
        return Ok(CloseOutcome::Closed);
    }
    let verification = applescript::run(
        "Terminal.app",
        WAIT_FOR_CLOSE_SCRIPT,
        &[&session.id, window_id, "100"],
    )?;
    if verification != "missing" {
        bail!(
            "Terminal.app reported a closed tab twice but tty {:?} is still present",
            session.id
        );
    }
    Ok(CloseOutcome::Closed)
}

fn ownership_proof(session: &TerminalSession) -> Result<&str> {
    session
        .window_id
        .as_deref()
        .context("Terminal.app session record is missing its window id")
}
