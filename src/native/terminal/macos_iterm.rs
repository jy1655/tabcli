use super::super::ItermCloseOutcome;
use anyhow::{Context, Result, bail};
use std::{path::Path, process::Command};

pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
on run argv
    set bridgeCommand to item 1 of argv
    set tabTitle to item 2 of argv
    tell application "iTerm2"
        activate
        if (count of windows) is 0 then
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
            set name to tabTitle
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
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        tell targetSession
                            write contents of file promptPath
                            write text (ASCII character 13) newline NO
                        end tell
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

pub(in crate::native) fn ensure_available() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("native visible sessions currently require macOS and iTerm2");
    }
    Ok(())
}

pub(in crate::native) fn open_tab(command: &str, title: &str) -> Result<String> {
    let id = run_osascript(OPEN_TAB_SCRIPT, &[command, title])?;
    if id.is_empty() {
        bail!("iTerm2 did not return a session id");
    }
    Ok(id)
}

pub(in crate::native) fn send_file(iterm_session_id: &str, prompt_path: &Path) -> Result<()> {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    let response = run_osascript(SEND_FILE_SCRIPT, &[iterm_session_id, prompt_path])?;
    if response != "sent" {
        bail!("unexpected iTerm2 send response: {response:?}");
    }
    Ok(())
}

pub(in crate::native) fn close_session(iterm_session_id: &str) -> Result<ItermCloseOutcome> {
    match run_osascript(CLOSE_SESSION_SCRIPT, &[iterm_session_id])?.as_str() {
        "closed" => Ok(ItermCloseOutcome::Closed),
        "missing" => Ok(ItermCloseOutcome::Missing),
        response => bail!("unexpected iTerm2 close response: {response:?}"),
    }
}

fn run_osascript(script: &str, arguments: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(arguments)
        .output()
        .context("failed to execute /usr/bin/osascript")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "iTerm2 automation failed: {}",
            if error.is_empty() {
                output.status.to_string()
            } else {
                error
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
