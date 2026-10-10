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
                        set screenText to contents of s
                        if (count of argv) is 1 then return "AB_SCREEN_BEGIN" & screenText & "AB_SCREEN_END"
                        if screenText is not item 2 of argv then return "changed"
                        if item 3 of argv is "down-enter" then
                            write s text ((ASCII character 27) & "[B" & return) newline false
                        else
                            write s text return newline false
                        end if
                        return "sent"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "managed iTerm2 screen is missing"
end run
"#;

pub(in crate::native) const TERMINAL: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set wantedWindowId to item 2 of argv as integer
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
        -- "contents of <variable>" dereferences an AppleScript reference; use
        -- the tab specifier to read Terminal's text property instead.
        set screenText to contents of (first tab of w whose tty is wantedTty)
        if (count of argv) is 2 then return "AB_SCREEN_BEGIN" & screenText & "AB_SCREEN_END"
        if screenText is not item 3 of argv then return "changed"
        if item 4 of argv is "down-enter" then
            do script ((ASCII character 27) & "[B") in matchedTab
        else
            do script "" in matchedTab
        end if
        return "sent"
    end tell
end run
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
