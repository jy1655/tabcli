use super::super::{DialogKey, GuardedDialogInput};
use super::*;

// Stable creation-time handle only. A compare-and-send is one automation call;
// an intervening repaint or changed selection sends no keys. Never targets front tab.
pub(in crate::native) const ITERM: &str = r#"
on run argv
    if application id "com.googlecode.iterm2" is not running then error "managed iTerm2 screen is missing"
    set wantedId to item 1 of argv
    tell application id "com.googlecode.iterm2"
        repeat with w in windows
            repeat with t in tabs of w
                repeat with s in sessions of t
                    if unique ID of s is wantedId then
                        if (count of argv) is 1 then return "AB_SCREEN_BEGIN" & my screenOf(s) & "AB_SCREEN_END"
                        return my guardedAnswer(s, item 2 of argv, item 3 of argv)
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "managed iTerm2 screen is missing"
end run

on screenOf(targetSession)
    tell application id "com.googlecode.iterm2"
        return contents of targetSession
    end tell
end screenOf

on sendKey(targetSession, keyName)
    tell application id "com.googlecode.iterm2"
        if keyName is "down-enter" then
            write targetSession text ((ASCII character 27) & "[B" & return) newline false
        else
            write targetSession text return newline false
        end if
    end tell
end sendKey

-- The flow.
on guardedAnswer(targetSession, expectedScreen, keyName)
    set screenText to my screenOf(targetSession)
    if screenText is not expectedScreen then return "changed"
    my sendKey(targetSession, keyName)
    return "sent"
end guardedAnswer
"#;

pub(in crate::native) const TERMINAL: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
    if not application "Terminal" is running then error "Agent Bridge Terminal.app screen: not running"
    tell application "Terminal"
        set w to first window whose id is wantedWindowId
        set matchedTab to missing value
        set matchCount to 0
        repeat with t in tabs of w
            if tty of t is wantedTty then
                set matchedTab to t
                set matchCount to matchCount + 1
            end if
        end repeat
        if matchCount is not 1 then error "managed Terminal screen is ambiguous or missing"
        if (count of argv) is 2 then return "AB_SCREEN_BEGIN" & my screenOf(w, wantedTty) & "AB_SCREEN_END"
        return my guardedAnswer(w, wantedTty, matchedTab, item 3 of argv, item 4 of argv)
    end tell
end run

on screenOf(w, wantedTty)
    if not application "Terminal" is running then error "Agent Bridge Terminal.app screen: not running"
    tell application "Terminal"
        -- "contents of <variable>" dereferences an AppleScript reference; use
        -- the tab specifier to read Terminal's text property instead.
        return contents of (first tab of w whose tty is wantedTty)
    end tell
end screenOf

on sendKey(targetTab, keyName)
    if not application "Terminal" is running then error "Agent Bridge Terminal.app screen: not running"
    tell application "Terminal"
        if keyName is "down-enter" then
            do script ((ASCII character 27) & "[B") in targetTab
        else
            do script "" in targetTab
        end if
    end tell
end sendKey

-- The flow.
on guardedAnswer(w, wantedTty, targetTab, expectedScreen, keyName)
    set screenText to my screenOf(w, wantedTty)
    if screenText is not expectedScreen then return "changed"
    my sendKey(targetTab, keyName)
    return "sent"
end guardedAnswer
"#;

fn script_args(session: &TerminalSession) -> Result<(&'static str, Vec<&str>)> {
    match session.kind {
        TerminalKind::Iterm2 => Ok((ITERM, vec![&session.id])),
        TerminalKind::AppleTerminal => Ok((
            TERMINAL,
            vec![
                &session.id,
                session
                    .window_id
                    .as_deref()
                    .context("Terminal window identity missing")?,
            ],
        )),
        TerminalKind::Warp => bail!(
            "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit"
        ),
        // `get-text` and `send-text` are two calls, and the screen can change between
        // them. A key is sent only by a call that compared the screen itself.
        TerminalKind::WezTerm => bail!(
            "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane"
        ),
        _ => bail!("managed dialog input unsupported for this terminal"),
    }
}

pub(in crate::native::terminal) fn read_screen(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<String> {
    if session.kind == TerminalKind::WezTerm {
        return wezterm::read_screen(&wezterm::Installed, session, deadline);
    }
    let (script, args) = script_args(session)?;
    let text = applescript::run_until(session.kind.display_name(), script, &args, deadline)?;
    Ok(text
        .strip_prefix("AB_SCREEN_BEGIN")
        .and_then(|s| s.strip_suffix("AB_SCREEN_END"))
        .context("invalid managed screen response")?
        .to_owned())
}

pub(in crate::native::terminal) fn guarded_dialog_input(
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    let (script, mut args) = script_args(session)?;
    args.extend([
        input.screen.as_str(),
        match input.key {
            DialogKey::Enter => "enter",
            DialogKey::DownEnter => "down-enter",
        },
    ]);
    let answer = applescript::run_until(session.kind.display_name(), script, &args, deadline)?;
    match answer.as_str() {
        "sent" => Ok(true),
        "changed" => Ok(false),
        _ => bail!("unexpected guarded dialog input response"),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{ITERM, TERMINAL};

    fn osascript(script: &str) -> String {
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    // Replay the shipped decision handler; only its screen and key handlers are
    // models. No application dictionary or live terminal is used by this script.
    fn replay(script: &str, screen: &str, key: &str) -> String {
        let (_, flow) = script.split_once("-- The flow.").unwrap();
        assert!(!flow.contains("application"));
        let (read_parameters, read_arguments, target_arguments) = if script == ITERM {
            ("targetSession", "targetSession", "\"session-id\"")
        } else {
            (
                "w, wantedTty",
                "w & \":\" & wantedTty",
                "\"window-id\", \"tty-id\", \"tab-id\"",
            )
        };
        osascript(&format!(
            r#"
property calls : ""
on screenOf({read_parameters})
    set calls to calls & "read=" & ({read_arguments}) & ";"
    return "{screen}"
end screenOf
on sendKey(target, keyName)
    set calls to calls & "send=" & target & ":" & keyName & ";"
end sendKey
{flow}
on run
    set answer to my guardedAnswer({target_arguments}, "trust this directory?", "{key}")
    return answer & ";" & calls
end run
"#
        ))
    }

    fn assert_guarded_replay(script: &str, read_target: &str, send_target: &str) {
        for key in ["enter", "down-enter"] {
            assert_eq!(
                replay(script, "trust this directory?", key),
                format!("sent;read={read_target};send={send_target}:{key};")
            );
            // A one-character repaint between capture and comparison must send
            // nothing, for either key sequence.
            assert_eq!(
                replay(script, "trust this directory!", key),
                format!("changed;read={read_target};")
            );
        }
    }

    #[test]
    fn iterm_guarded_answer_replays_matching_and_changed_screens() {
        assert_guarded_replay(ITERM, "session-id", "session-id");
    }

    #[test]
    fn terminal_guarded_answer_replays_matching_and_changed_screens() {
        assert_guarded_replay(TERMINAL, "window-id:tty-id", "tab-id");
    }
}
