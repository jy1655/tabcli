use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession, applescript, close_response};

pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
on run argv
    set bridgeCommand to item 1 of argv
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
            write text bridgeCommand
            return unique ID
        end tell
    end tell
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

pub(super) fn open_tab(command: &str) -> Result<TerminalSession> {
    let id = applescript::run("iTerm2", OPEN_TAB_SCRIPT, &[command])?;
    if id.is_empty() {
        bail!("iTerm2 did not return a session id");
    }
    Ok(TerminalSession {
        kind: TerminalKind::Iterm2,
        id,
        tab_id: None,
        window_id: None,
        managed_session_id: None,
    })
}

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    let response = applescript::run("iTerm2", SEND_FILE_SCRIPT, &[&session.id, prompt_path])?;
    if response != "sent" {
        bail!("unexpected iTerm2 send response: {response:?}");
    }
    Ok(())
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    let response = applescript::run("iTerm2", CLOSE_SESSION_SCRIPT, &[&session.id])?;
    close_response(TerminalKind::Iterm2, &response)
}
