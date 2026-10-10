use crate::native::session::Reader;
#[cfg(test)]
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

// iTerm2 3.7.3 selects every surface it creates, and a new window also makes
// iTerm2 activate itself (issue #58). The script gives back what its creation
// took and nothing else:
// - a new tab: the tab that was selected in that window, while the window still
//   shows the new session. `create tab` never activates iTerm2, so no
//   application is touched.
// - a new window: the earlier window, then the application that had the
//   foreground, once iTerm2 itself says that it is active. iTerm2 activates
//   itself asynchronously and makes the new window key again when it is active,
//   so anything given back earlier is taken again.
// An application, window or tab that the user chose meanwhile keeps the keyboard.
// None of this is authority over the surface or a reason to fail the launch. An
// application is kept as its NSRunningApplication object, never as a pid to look
// up. Native `command` creation and the bound launch host below keep the keys of
// this exposure from editing the launch line or answering the provider's first
// dialog. Remove the return when iTerm2 can create without selecting.
pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
use framework "AppKit"

-- The machine. Everything this script knows about AppKit and iTerm2 comes
-- through these handlers.

-- NSWorkspace learns of a new foreground on the main run loop, which a script
-- reaches only here. An application object learns in the same way that its
-- application has ended.
on foregroundApplication()
    current application's NSRunLoop's currentRunLoop()'s runUntilDate:(current application's NSDate's dateWithTimeIntervalSinceNow:0.01)
    return current application's NSWorkspace's sharedWorkspace()'s frontmostApplication()
end foregroundApplication

on itermIsRunning()
    return application id "com.googlecode.iterm2" is running
end itermIsRunning

-- AppleScript answers `frontmost of application` itself, from what the system
-- lists, and sends no event (AppleScript Language Guide, application class). The
-- system lists iTerm2 in front before iTerm2 has handled its own activation.
-- The record of its properties is iTerm2's own answer, and `frontmost` in it is
-- iTerm2's `isActive` (iTerm2.sdef): true once iTerm2 has become active and has
-- done what it had put off until then. The two terms are written as their
-- codes, `properties` and `frontmost`. In iTerm2's dictionary they compile to
-- the same script; without a dictionary, as in the replay of this handler,
-- `properties` would read as `every property`.
on itermIsActive()
    tell application id "com.googlecode.iterm2"
        set applicationProperties to «property pALL»
        return «property pisf» of applicationProperties
    end tell
end itermIsActive

on itermCurrentWindow()
    tell application id "com.googlecode.iterm2" to return current window
end itermCurrentWindow

on itermCurrentTabOf(aWindow)
    tell application id "com.googlecode.iterm2" to return current tab of aWindow
end itermCurrentTabOf

on itermSessionIdOfWindow(aWindow)
    tell application id "com.googlecode.iterm2" to return unique ID of current session of aWindow
end itermSessionIdOfWindow

on itermSelectedSessionId()
    tell application id "com.googlecode.iterm2"
        if current window is missing value then return missing value
        return unique ID of current session of current window
    end tell
end itermSelectedSessionId

on itermWindowIsVisible(aWindow)
    tell application id "com.googlecode.iterm2" to return visible of aWindow
end itermWindowIsVisible

on itermSelectTab(aTab)
    tell application id "com.googlecode.iterm2"
        tell aTab to select
    end tell
end itermSelectTab

on itermSelectWindow(aWindow)
    tell application id "com.googlecode.iterm2"
        tell aWindow to select
    end tell
end itermSelectWindow

-- Both creations return the unique ID of the session they created, read once.
on itermCreateWindow(bridgeCommand)
    tell application id "com.googlecode.iterm2"
        set newWindow to (create window with default profile command bridgeCommand)
        return unique ID of current session of newWindow
    end tell
end itermCreateWindow

on itermCreateTab(aWindow, bridgeCommand)
    tell application id "com.googlecode.iterm2"
        tell aWindow
            set newTab to (create tab with default profile command bridgeCommand)
        end tell
        return unique ID of current session of newTab
    end tell
end itermCreateTab

-- The flow. Nothing below names iTerm2 or an AppKit class, so the replay test
-- runs these handlers unchanged on a model of both.

-- How often iTerm2 is asked whether it has become active: about two seconds,
-- the time for which iTerm2 itself retries its activation on every turn of its
-- run loop.
property activationLooks : 200

-- An application without a bundle identifier is not iTerm2.
on isITerm(anApplication)
    set bundle to anApplication's bundleIdentifier()
    if bundle is missing value then return false
    return (bundle as text) is "com.googlecode.iterm2"
end isITerm

on isAnotherApplication(anApplication)
    if anApplication is missing value then return false
    return not (my isITerm(anApplication))
end isAnotherApplication

on isEarlierOrITerm(anApplication, earlierApplication)
    if anApplication is missing value then return false
    if my isITerm(anApplication) then return true
    return (anApplication's isEqual:earlierApplication) as boolean
end isEarlierOrITerm

-- A new tab is the selected tab of its window, and creating one never activates
-- iTerm2. The tab that was selected there is selected again while that window
-- still shows the new session. No application is touched: if iTerm2 is in front
-- now, the user put it there.
on returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)
    if keyboardTab is missing value then return
    if (my itermSessionIdOfWindow(keyboardWindow)) is newSessionId then my itermSelectTab(keyboardTab)
end returnFromNewTab

-- A new window makes iTerm2 activate itself. It does so asynchronously, retries
-- for seconds, and makes the new window key again once it is active, so whatever
-- is given back before that is taken again. This waits, within a bound, until
-- iTerm2 itself says that it is active. Then it selects the earlier window and
-- gives the foreground to the application that iTerm2 took it from: the last one
-- seen in front, unless iTerm2 was in front at the start. Each step is taken
-- only while iTerm2 still selects what this script selected; everything else
-- returns without a change.
on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)
    set looks to 0
    repeat until my itermIsActive()
        set looks to looks + 1
        if looks > activationLooks then return
        set seenApplication to my foregroundApplication()
        if my isAnotherApplication(earlierApplication) and my isAnotherApplication(seenApplication) then set earlierApplication to seenApplication
    end repeat
    if (my itermSelectedSessionId()) is not newSessionId then return
    set expectedSessionId to newSessionId
    if keyboardWindow is not missing value then
        if my itermWindowIsVisible(keyboardWindow) then
            my itermSelectWindow(keyboardWindow)
            -- Only a selection that took effect is expected to last.
            set earlierSessionId to my itermSessionIdOfWindow(keyboardWindow)
            if (my itermSelectedSessionId()) is earlierSessionId then set expectedSessionId to earlierSessionId
        end if
    end if
    if not (my isAnotherApplication(earlierApplication)) then return
    if earlierApplication's bundleIdentifier() is missing value then return
    if not (my isEarlierOrITerm(my foregroundApplication(), earlierApplication)) then return
    if earlierApplication's isTerminated() as boolean then return
    if not (my itermIsActive()) then return
    if (my itermSelectedSessionId()) is not expectedSessionId then return
    earlierApplication's activateWithOptions:2
end returnFromNewWindow

on run argv
    set forceNewWindow to (item 1 of argv) is "new-window"
    set bridgeCommand to item 2 of argv
    -- The application object itself is kept. No pid is looked up later.
    set earlierApplication to missing value
    try
        set earlierApplication to my foregroundApplication()
    end try
    -- The selected window is read once. A tab is created in that window, and it
    -- is the one that gets the keyboard back. Without one, no window of iTerm2
    -- is adopted.
    set keyboardWindow to missing value
    set keyboardTab to missing value
    if my itermIsRunning() then
        try
            set keyboardWindow to my itermCurrentWindow()
            if keyboardWindow is not missing value then set keyboardTab to my itermCurrentTabOf(keyboardWindow)
        end try
    end if
    if forceNewWindow or keyboardWindow is missing value then
        set newSessionId to my itermCreateWindow(bridgeCommand)
        try
            my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)
        end try
    else
        set newSessionId to my itermCreateTab(keyboardWindow, bridgeCommand)
        try
            my returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)
        end try
    end if
    return newSessionId
end run
"#;

pub(in crate::native) const SEND_FILE_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set promptPath to item 2 of argv
    set carriageReturn to return
    tell application id "com.googlecode.iterm2"
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
    tell application id "com.googlecode.iterm2"
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

pub(in crate::native) const PRESENCE_SCRIPT: &str = r#"
on run argv
    if application id "com.googlecode.iterm2" is not running then return "missing"
    set wantedId to item 1 of argv
    tell application id "com.googlecode.iterm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then return "present"
                end repeat
            end repeat
        end repeat
    end tell
    return "missing"
end run
"#;

pub(in crate::native) const CLOSE_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    tell application id "com.googlecode.iterm2"
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

pub(super) fn create_tab(
    mode: crate::native::settings::MacosOpenMode,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<TerminalSession> {
    let executable =
        std::env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let host = format!(
        "{} native-iterm2-host {}",
        crate::native::shell_quote(executable.as_os_str()),
        crate::native::shell_quote(directory.as_os_str())
    );
    let bootstrap = shell_command(&host, command);
    let id = applescript::run_until(
        "iTerm2",
        OPEN_TAB_SCRIPT,
        &[mode.as_str(), &bootstrap],
        deadline,
    )?;
    if id.is_empty() {
        bail!("iTerm2 did not return a session id");
    }
    Ok(TerminalSession {
        kind: TerminalKind::Iterm2,
        id,
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        wezterm_mux: None,
        windows_process_identity: None,
    })
}

// iTerm's native `command` argument runs a program instead of editing a shell
// line. Use a login, interactive zsh so startup files/PATH and distinct foreground
// jobs are retained. The gate and wrapper are separate jobs in the original shell.
// Unlike the profile's arbitrary command, this bootstrap is always a POSIX shell.
fn shell_command(host: &str, command: &str) -> String {
    format!(
        "/bin/zsh -l -i -c {}",
        iterm_argument(&format!("{host} || exit; {command}"))
    )
}

// One argument of an iTerm2 `command`. iTerm2 3.7.3 hands that string to no
// shell. It replaces `$$…$$` variables, asking the user for one that it does not
// know (`$$$$` is a literal `$$`), and splits the rest with a parser of its own,
// in which a backslash escapes even inside single quotes (PTYSession.m
// `computeArgvForCommand:`; NSStringITerm.m `doubleDollarVariables`,
// `componentsInShellCommand`). Double quotes with `\\` and `\"` are what
// iTerm2 itself writes for that parser (ITAddressBookMgr.m
// `standardLoginCommand`).
fn iterm_argument(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace("$$", "$$$$")
    )
}

// Creation necessarily precedes knowing iTerm's returned UUID. Reuse the existing
// launch receipt and atomic terminal binding as the gate: no extra persisted
// protocol and no provider before the launcher has restored selection and bound
// this exact surface. Failure/timeout never falls back to typed shell input.
pub(in crate::native) fn run_host(directory: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !directory.is_absolute()
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("iTerm2 launch directory is not private to the current user");
    }
    let id = directory
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid iTerm2 launch directory")?;
    crate::native::require_valid_session_id(id)?;
    let iterm_id =
        std::env::var("ITERM_SESSION_ID").context("iTerm2 did not supply its session identity")?;
    let iterm_id = iterm_id
        .rsplit(':')
        .next()
        .filter(|id| !id.is_empty())
        .context("empty iTerm2 session identity")?;
    wait_for_binding(directory, id, iterm_id)?;
    // Keys that arrived while creation temporarily selected this surface must not
    // answer a provider's first dialog. This flush touches only this process's PTY.
    if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot discard iTerm2 startup input");
    }
    Ok(())
}

fn wait_for_binding(directory: &Path, id: &str, iterm_id: &str) -> Result<()> {
    use crate::native::launch::{BindingHost, wait_for_binding};
    let surface = wait_for_binding(&Reader::open_unchecked(directory), id, BindingHost::Iterm2)?;
    if surface.kind != TerminalKind::Iterm2 || surface.id != iterm_id {
        bail!("iTerm2 surface binding does not match this launch host");
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

#[cfg(test)]
mod tests {
    use crate::native::session::SessionState;

    #[test]
    fn host_binding_partial_transitions() {
        crate::native::launch::binding_tests::characterize("iTerm2", |directory, id| {
            wait_for_binding(directory, id, "host-id")
        });
    }

    use super::*;

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

    // A model of iTerm2 3.7.3 and of the foreground for the replay below. It
    // addresses nothing of the machine.
    const MODEL: &str = r#"
-- A model of iTerm2 3.7.3 and of the foreground, for the replay of the open script.
-- What it encodes, from iTerm2's source at tag v3.7.3:
--   * the window of a first or new tab is the current terminal at once
--     (PseudoTerminal.m:13266-13278);
--   * `create window` lets iTerm2 activate itself: asynchronously, and when it has become
--     active it makes the new window key again (iTermSessionLauncher.m:237-271,
--     iTermApplication.m:1015-1088). `create tab` never activates
--     (iTermWindowScriptingImpl.m:37-60);
--   * selecting a tab changes no window (PTYTab+Scripting.m:78-80); selecting a window makes
--     it the current terminal and activates nothing (iTermController.m:2219-2241).
-- The application in front (`mFront`) is what the system lists. Whether iTerm2 is active
-- (`mItermActive`) is what iTerm2 itself has handled; the two differ for a moment.
-- Time is counted in calls to the model: one call stands for one Apple Event or one look
-- at the workspace.
property mWindows : {}
property mKey : missing value
property mRunning : true
property mItermActive : false
property mFront : "earlier"
property mEarlierEnded : false
property mEndedSeen : false
property mPending : missing value
property mCalls : 0
property mNames : {}
property mLast : ""
property mCreatedAt : missing value
property mNextWid : 100
property mActivated : {}
property mSelects : {}

on mFind(wantedWid)
	repeat with candidate in mWindows
		if (wid of candidate) is wantedWid then return contents of candidate
	end repeat
	error "model: no window " & wantedWid
end mFind

on mShownSession(wantedWid)
	set found to my mFind(wantedWid)
	return item (sel of found) of (sids of found)
end mShownSession

on mAct(act)
	if act is "activation-completes" then
		if mPending is not missing value then
			set mItermActive to true
			set mFront to "iterm"
			set mKey to mPending
			set mPending to missing value
		end if
	else if act begins with "listed-in-front " then
		-- What the system lists changes; iTerm2 has not handled the change yet.
		set mFront to text 17 thru -1 of act
	else if act is "user-to-iterm" then
		set mFront to "iterm"
		set mItermActive to true
		if mPending is not missing value then
			set mKey to mPending
			set mPending to missing value
		end if
	else if act is "user-to-other" then
		set mFront to "other"
		set mItermActive to false
	else if act is "user-to-earlier" then
		set mFront to "earlier"
		set mItermActive to false
	else if act begins with "user-selects-window " then
		set mKey to (text 21 thru -1 of act) as integer
	else if act begins with "user-selects-tab " then
		set spec to text 18 thru -1 of act
		set cut to offset of ":" in spec
		set found to my mFind((text 1 thru (cut - 1) of spec) as integer)
		set sel of found to (text (cut + 1) thru -1 of spec) as integer
	else if act is "user-closes-current-window" then
		set mKey to missing value
	else if act is "earlier-ends" then
		set mEarlierEnded to true
	else
		error "model: unknown act " & act
	end if
end mAct

on mRun(acts)
	repeat with act in acts
		my mAct(contents of act)
	end repeat
end mRun

-- Every access of the script to the machine is one call. What the user does is tied to a
-- call: to the nth call of a handler, or to the time right after it (`sAtCall`), or to
-- the number of calls since the creation (`sAfterCreation`).
on mCall(handlerName)
	set mCalls to mCalls + 1
	if sAtCall is not {} then
		set end of mNames to handlerName
		set nth to 0
		repeat with earlierName in mNames
			if (contents of earlierName) is handlerName then set nth to nth + 1
		end repeat
		set this to handlerName & " " & nth
		repeat with entry in sAtCall
			if (done of entry) is false and ((onCall of entry) is this or (onCall of entry) is ("after " & mLast)) then
				set done of entry to true
				my mAct(act of entry)
			end if
		end repeat
		set mLast to this
	end if
	if mCreatedAt is not missing value then
		set sinceCreation to mCalls - mCreatedAt
		repeat with entry in sAfterCreation
			if (done of entry) is false and (calls of entry) ≤ sinceCreation then
				set done of entry to true
				my mAct(act of entry)
			end if
		end repeat
		if mPending is not missing value and sinceCreation > sActivationLatency then my mAct("activation-completes")
	end if
end mCall

on mApp(which)
	script anApplication
		property who : which
		on bundleIdentifier()
			if who is "iterm" then return "com.googlecode.iterm2"
			if who is "nobundle" then return missing value
			-- Another instance of the earlier application.
			if who is "twin" then return "com.example.earlier"
			return "com.example." & who
		end bundleIdentifier
		on isTerminated()
			-- An application object learns that its application has ended on the run loop.
			if who is "earlier" then return mEndedSeen
			return false
		end isTerminated
		on isEqual:another
			return who is (who of another)
		end isEqual:
		on activateWithOptions:options
			set end of mActivated to who
			set mFront to who
			set mItermActive to false
			return true
		end activateWithOptions:
	end script
	return anApplication
end mApp

on mJoin(values)
	set joined to ""
	repeat with value in values
		if joined is not "" then set joined to joined & ","
		set joined to joined & (contents of value)
	end repeat
	return joined
end mJoin

-- The machine, as the open script reaches it.
on foregroundApplication()
	my mCall("foregroundApplication")
	set mEndedSeen to mEarlierEnded
	if mFront is "unreadable" then return missing value
	return my mApp(mFront)
end foregroundApplication

on itermIsRunning()
	my mCall("itermIsRunning")
	return mRunning
end itermIsRunning

on itermIsActive()
	my mCall("itermIsActive")
	return mItermActive
end itermIsActive

on itermCurrentWindow()
	my mCall("itermCurrentWindow")
	return mKey
end itermCurrentWindow

on itermCurrentTabOf(aWindow)
	my mCall("itermCurrentTabOf")
	return {tabWid:aWindow, tabNo:(sel of (my mFind(aWindow)))}
end itermCurrentTabOf

on itermSessionIdOfWindow(aWindow)
	my mCall("itermSessionIdOfWindow")
	return my mShownSession(aWindow)
end itermSessionIdOfWindow

on itermSelectedSessionId()
	my mCall("itermSelectedSessionId")
	if mKey is missing value then return missing value
	return my mShownSession(mKey)
end itermSelectedSessionId

on itermWindowIsVisible(aWindow)
	my mCall("itermWindowIsVisible")
	return vis of (my mFind(aWindow))
end itermWindowIsVisible

on itermSelectTab(aTab)
	my mCall("itermSelectTab")
	set end of mSelects to "tab " & (tabWid of aTab) & ":" & (tabNo of aTab)
	set sel of (my mFind(tabWid of aTab)) to tabNo of aTab
end itermSelectTab

on itermSelectWindow(aWindow)
	my mCall("itermSelectWindow")
	set end of mSelects to "window " & aWindow
	if not sSelectWindowHasNoEffect then set mKey to aWindow
end itermSelectWindow

on mNewWindow()
	set newWid to mNextWid
	set mNextWid to mNextWid + 1
	set end of mWindows to {wid:newWid, sids:{"new"}, sel:1, vis:true}
	set mRunning to true
	set mKey to newWid
	if not mItermActive then set mPending to newWid
	set mCreatedAt to mCalls
	my mRun(sDuringCreation)
	return newWid
end mNewWindow

on mNewTab(aWindow)
	set found to my mFind(aWindow)
	set end of (sids of found) to "new"
	set sel of found to (count of (sids of found))
	set mKey to aWindow
	set mCreatedAt to mCalls
	my mRun(sDuringCreation)
	return {tabWid:aWindow, tabNo:(sel of found)}
end mNewTab

on itermCreateWindow(bridgeCommand)
	my mCall("itermCreateWindow")
	my mNewWindow()
	return "new"
end itermCreateWindow

on itermCreateTab(aWindow, bridgeCommand)
	my mCall("itermCreateTab")
	my mNewTab(aWindow)
	return "new"
end itermCreateTab

on run
	set mWindows to sWindows
	set mKey to sKey
	set mRunning to sRunning
	set mFront to sFront
	set mItermActive to (sFront is "iterm")
	set returned to my bridgeRun({sMode, "COMMAND"})
	-- An activation that is still pending completes after the script has returned.
	my mAct("activation-completes")
	set selected to "none"
	if mKey is not missing value then set selected to my mShownSession(mKey)
	-- Every window with its sessions; the one it shows is marked.
	set shown to {}
	repeat with candidate in mWindows
		set listed to ""
		repeat with position from 1 to count of (sids of candidate)
			if listed is not "" then set listed to listed & "/"
			set listed to listed & (item position of (sids of candidate))
			if position is (sel of candidate) then set listed to listed & "*"
		end repeat
		set end of shown to ((wid of candidate) as text) & ":" & listed
	end repeat
	return "returned=" & returned & " front=" & mFront & " selected=" & selected & " activated=" & (my mJoin(mActivated)) & " selects=" & (my mJoin(mSelects)) & " windows=" & (my mJoin(shown))
end run
"#;

    // The flow of the open script as shipped, on the model instead of the machine.
    fn flow() -> String {
        let (_, flow) = OPEN_TAB_SCRIPT.split_once("-- The flow.").unwrap();
        // Only the machine part may address iTerm2 or AppKit.
        assert!(!flow.contains("application \"iTerm2\""));
        assert!(!flow.contains("current application"));
        assert_eq!(flow.matches("on run argv").count(), 1);
        assert_eq!(flow.matches("end run").count(), 1);
        format!(
            "--{}\n{MODEL}",
            flow.replace("on run argv", "on bridgeRun(argv)")
                .replace("end run", "end bridgeRun")
        )
    }

    #[derive(Clone, Copy)]
    struct Scene {
        mode: &'static str,
        // iTerm2's windows as the model keeps them, and its current window.
        windows: &'static str,
        current_window: &'static str,
        running: bool,
        // The application in front when the script starts.
        front: &'static str,
        // Calls of the model between a window creation and iTerm2 being active.
        activation_latency: usize,
        // What the user or the system does: while the creation runs, a number of
        // calls after it, and at the nth call of a handler of the machine.
        during_creation: &'static str,
        after_creation: &'static str,
        at_call: &'static str,
        select_window_has_no_effect: bool,
    }

    const ONE_WINDOW: &str = r#"{{wid:1, sids:{"old"}, sel:1, vis:true}}"#;
    const TWO_WINDOWS: &str =
        r#"{{wid:1, sids:{"old"}, sel:1, vis:true}, {wid:2, sids:{"other"}, sel:1, vis:true}}"#;
    // Another application is in front and iTerm2 has one window with one tab.
    const WINDOW: Scene = Scene {
        mode: "new-window",
        windows: ONE_WINDOW,
        current_window: "1",
        running: true,
        front: "earlier",
        activation_latency: 0,
        during_creation: "{}",
        after_creation: "{}",
        at_call: "{}",
        select_window_has_no_effect: false,
    };
    const TAB: Scene = Scene {
        mode: "tab-first",
        ..WINDOW
    };

    // Runs the flow on the model and tells where the keyboard is afterwards and
    // what the script did: the application in front, the session that iTerm2 has
    // selected, the applications that the script activated, what it selected in
    // iTerm2, and every window with its sessions (`*` marks the one it shows).
    fn replay(scene: Scene) -> String {
        let state = osascript(&format!(
            "property sMode : \"{}\"\nproperty sRunning : {}\nproperty sWindows : {}\nproperty sKey : {}\nproperty sFront : \"{}\"\nproperty sActivationLatency : {}\nproperty sDuringCreation : {}\nproperty sAfterCreation : {}\nproperty sAtCall : {}\nproperty sSelectWindowHasNoEffect : {}\n{}",
            scene.mode,
            scene.running,
            scene.windows,
            scene.current_window,
            scene.front,
            scene.activation_latency,
            scene.during_creation,
            scene.after_creation,
            scene.at_call,
            scene.select_window_has_no_effect,
            flow()
        ));
        // The script returns the id of the session that it created.
        state
            .strip_prefix("returned=new ")
            .unwrap_or_else(|| panic!("the script did not return its session: {state}"))
            .to_owned()
    }

    // Review finding 1 of 2026-10-03, on the script's own flow. iTerm2 activates
    // itself asynchronously after `create window` and makes the new window key
    // again when it is active. Whenever that happens, the earlier window and the
    // earlier application must have the keyboard in the end. 0: active before the
    // script looks, the order of the live runs. Later: active only after a script
    // that does not wait has selected the earlier window, or has returned.
    #[test]
    fn a_new_window_returns_the_keyboard_whenever_iterm2_becomes_active() {
        const RETURNED: &str =
            "front=earlier selected=old activated=earlier selects=window 1 windows=1:old*,100:new*";
        for activation_latency in [0, 1, 14, 40, 300] {
            assert_eq!(
                replay(Scene {
                    activation_latency,
                    ..WINDOW
                }),
                RETURNED,
                "iTerm2 active {activation_latency} calls after the creation"
            );
        }
        // The system lists iTerm2 in front before iTerm2 has handled its
        // activation. Only iTerm2's own answer ends the wait.
        assert_eq!(
            replay(Scene {
                activation_latency: 14,
                after_creation: r#"{{calls:2, act:"listed-in-front iterm", done:false}}"#,
                ..WINDOW
            }),
            RETURNED
        );
    }

    // Review finding 2. A tab creation never activates iTerm2: if iTerm2 is in
    // front afterwards, the user chose it, and it keeps the foreground.
    #[test]
    fn a_new_tab_never_takes_the_foreground_from_iterm2() {
        assert_eq!(
            replay(TAB),
            "front=earlier selected=old activated= selects=tab 1:1 windows=1:old*/new"
        );
        for scene in [
            Scene {
                during_creation: r#"{"user-to-iterm"}"#,
                ..TAB
            },
            Scene {
                front: "iterm",
                ..TAB
            },
        ] {
            assert_eq!(
                replay(scene),
                "front=iterm selected=old activated= selects=tab 1:1 windows=1:old*/new"
            );
        }
    }

    #[test]
    fn a_new_window_gives_back_only_what_its_creation_took() {
        let cases = [
            (
                "iTerm2 was in front: its earlier window, and no application",
                Scene {
                    front: "iterm",
                    ..WINDOW
                },
                "front=iterm selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                // iTerm2 was active when it created the window, so it did not
                // activate itself: the user brought it back to the front.
                "iTerm2 was in front, the user left it and came back",
                Scene {
                    front: "iterm",
                    during_creation: r#"{"user-to-other"}"#,
                    after_creation: r#"{{calls:4, act:"user-to-iterm", done:false}}"#,
                    ..WINDOW
                },
                "front=iterm selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the user went to another application, which iTerm2 then took the foreground from",
                Scene {
                    activation_latency: 8,
                    after_creation: r#"{{calls:3, act:"user-to-other", done:false}}"#,
                    ..WINDOW
                },
                "front=other selected=old activated=other selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the user went to another application and back",
                Scene {
                    activation_latency: 12,
                    after_creation: r#"{{calls:3, act:"user-to-other", done:false}, {calls:7, act:"user-to-earlier", done:false}}"#,
                    ..WINDOW
                },
                "front=earlier selected=old activated=earlier selects=window 1 windows=1:old*,100:new*",
            ),
            (
                // The object that was seen gets the foreground, not its bundle.
                "another instance of the earlier application came to the front",
                Scene {
                    activation_latency: 8,
                    after_creation: r#"{{calls:3, act:"listed-in-front twin", done:false}}"#,
                    ..WINDOW
                },
                "front=twin selected=old activated=twin selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the earlier window is not on the screen: it is not selected",
                Scene {
                    windows: r#"{{wid:1, sids:{"old"}, sel:1, vis:false}}"#,
                    ..WINDOW
                },
                "front=earlier selected=new activated=earlier selects= windows=1:old*,100:new*",
            ),
            (
                "iTerm2 had windows but no current window: none is adopted",
                Scene {
                    current_window: "missing value",
                    ..TAB
                },
                "front=earlier selected=new activated=earlier selects= windows=1:old*,100:new*",
            ),
            (
                "iTerm2 was not running",
                Scene {
                    running: false,
                    windows: "{}",
                    current_window: "missing value",
                    ..TAB
                },
                "front=earlier selected=new activated=earlier selects= windows=100:new*",
            ),
            (
                // The foreground still goes back; the window is not said to be
                // selected.
                "selecting the earlier window had no effect",
                Scene {
                    select_window_has_no_effect: true,
                    ..WINDOW
                },
                "front=earlier selected=new activated=earlier selects=window 1 windows=1:old*,100:new*",
            ),
            (
                // The script did nothing. The state is what iTerm2 makes of it later.
                "iTerm2 did not become active within the bound",
                Scene {
                    activation_latency: 1000,
                    ..WINDOW
                },
                "front=iterm selected=new activated= selects= windows=1:old*,100:new*",
            ),
        ];
        for (name, scene, expected) in cases {
            assert_eq!(replay(scene), expected, "{name}");
        }
    }

    // Every condition of the return of the foreground, one case each. The cases
    // with `at_call` change the state at the very call that looks at it: the
    // second look at the foreground is the one before the application's end is
    // read, the second `itermIsActive` and the third `itermSelectedSessionId`
    // are the last looks before the foreground is given back.
    #[test]
    fn a_new_window_keeps_iterm2_in_front_when_the_return_is_not_certain() {
        const KEPT: &str =
            "front=iterm selected=old activated= selects=window 1 windows=1:old*,100:new*";
        let cases = [
            (
                "the user selected another window of iTerm2",
                Scene {
                    windows: TWO_WINDOWS,
                    activation_latency: 4,
                    at_call: r#"{{onCall:"itermSelectedSessionId 1", act:"user-selects-window 2", done:false}}"#,
                    ..WINDOW
                },
                "front=iterm selected=other activated= selects= windows=1:old*,2:other*,100:new*",
            ),
            (
                "the foreground could not be read at the start",
                Scene {
                    front: "unreadable",
                    ..WINDOW
                },
                KEPT,
            ),
            (
                "the earlier application has no bundle identifier",
                Scene {
                    front: "nobundle",
                    ..WINDOW
                },
                KEPT,
            ),
            (
                "the earlier application ended during the wait",
                Scene {
                    activation_latency: 6,
                    after_creation: r#"{{calls:2, act:"earlier-ends", done:false}}"#,
                    ..WINDOW
                },
                KEPT,
            ),
            (
                // Its end is known only after another turn of the run loop.
                "the earlier application ended while the window was selected",
                Scene {
                    at_call: r#"{{onCall:"itermSelectWindow 1", act:"earlier-ends", done:false}}"#,
                    ..WINDOW
                },
                KEPT,
            ),
            (
                "another application is listed in front, iTerm2 has not noticed",
                Scene {
                    at_call: r#"{{onCall:"foregroundApplication 2", act:"listed-in-front other", done:false}}"#,
                    ..WINDOW
                },
                "front=other selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "another instance of the earlier application is listed in front",
                Scene {
                    at_call: r#"{{onCall:"foregroundApplication 2", act:"listed-in-front twin", done:false}}"#,
                    ..WINDOW
                },
                "front=twin selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the foreground cannot be read",
                Scene {
                    at_call: r#"{{onCall:"foregroundApplication 2", act:"listed-in-front unreadable", done:false}}"#,
                    ..WINDOW
                },
                "front=unreadable selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the user left iTerm2 at the last look",
                Scene {
                    at_call: r#"{{onCall:"itermIsActive 2", act:"user-to-other", done:false}}"#,
                    ..WINDOW
                },
                "front=other selected=old activated= selects=window 1 windows=1:old*,100:new*",
            ),
            (
                "the user selected another window at the last look",
                Scene {
                    windows: TWO_WINDOWS,
                    at_call: r#"{{onCall:"itermSelectedSessionId 3", act:"user-selects-window 2", done:false}}"#,
                    ..WINDOW
                },
                "front=iterm selected=other activated= selects=window 1 windows=1:old*,2:other*,100:new*",
            ),
            (
                "iTerm2 has no current window at the last look",
                Scene {
                    at_call: r#"{{onCall:"itermSelectedSessionId 3", act:"user-closes-current-window", done:false}}"#,
                    ..WINDOW
                },
                "front=iterm selected=none activated= selects=window 1 windows=1:old*,100:new*",
            ),
        ];
        for (name, scene, expected) in cases {
            assert_eq!(replay(scene), expected, "{name}");
        }
        assert!(!OPEN_TAB_SCRIPT.contains("runningApplicationWithProcessIdentifier"));
    }

    // Review findings 3 and 4. The earlier tab is selected again whenever its
    // window still shows the new session; neither the foreground nor the
    // visibility of the window decides that.
    #[test]
    fn a_new_tab_is_left_whenever_its_window_still_shows_it() {
        const LEFT: &str =
            "front=earlier selected=old activated= selects=tab 1:1 windows=1:old*/new";
        let cases = [
            (
                "another application came to the front",
                Scene {
                    during_creation: r#"{"user-to-other"}"#,
                    ..TAB
                },
                "front=other selected=old activated= selects=tab 1:1 windows=1:old*/new",
            ),
            (
                "the foreground cannot be read",
                Scene {
                    front: "unreadable",
                    ..TAB
                },
                "front=unreadable selected=old activated= selects=tab 1:1 windows=1:old*/new",
            ),
            (
                "the application in front has no bundle identifier",
                Scene {
                    front: "nobundle",
                    ..TAB
                },
                "front=nobundle selected=old activated= selects=tab 1:1 windows=1:old*/new",
            ),
            (
                "the window is not on the screen",
                Scene {
                    windows: r#"{{wid:1, sids:{"old"}, sel:1, vis:false}}"#,
                    ..TAB
                },
                LEFT,
            ),
            (
                "the user selected another tab of that window: it stays",
                Scene {
                    windows: r#"{{wid:1, sids:{"old", "second"}, sel:1, vis:true}}"#,
                    during_creation: r#"{"user-selects-tab 1:2"}"#,
                    ..TAB
                },
                "front=earlier selected=second activated= selects= windows=1:old/second*/new",
            ),
            (
                // The tab goes into the window whose tab was remembered.
                "the user selected another window between the read and the creation",
                Scene {
                    windows: TWO_WINDOWS,
                    at_call: r#"{{onCall:"after itermCurrentTabOf 1", act:"user-selects-window 2", done:false}}"#,
                    ..TAB
                },
                "front=earlier selected=old activated= selects=tab 1:1 windows=1:old*/new,2:other*",
            ),
        ];
        for (name, scene, expected) in cases {
            assert_eq!(replay(scene), expected, "{name}");
        }
    }

    // AppleScript answers `frontmost of application "iTerm2"` itself, from what the
    // system lists. The handler must read `frontmost` in the record that iTerm2
    // returns for its properties: here the target says the opposite of its record.
    #[test]
    fn the_activity_is_read_from_iterm2_s_own_properties() {
        let start = OPEN_TAB_SCRIPT.find("on itermIsActive()").unwrap();
        let end = OPEN_TAB_SCRIPT.find("end itermIsActive").unwrap();
        let handler = &OPEN_TAB_SCRIPT[start..end];
        assert_eq!(
            handler
                .matches("tell application id \"com.googlecode.iterm2\"")
                .count(),
            1
        );
        let handler = handler.replace(
            "tell application id \"com.googlecode.iterm2\"",
            "tell applicationModel",
        );
        for active in [true, false] {
            assert_eq!(
                osascript(&format!(
                    "property applicationModel : {{«property pALL»:{{«property pisf»:{active}}}, «property pisf»:{}}}\n{handler}end itermIsActive\non run\n    return my itermIsActive()\nend run",
                    !active
                )),
                active.to_string()
            );
        }
    }

    // A directory name with everything that iTerm2's parser or a shell could take
    // for syntax.
    const ODD_NAME: &str = "it's a \"state\" \\new \\a\\t\\r\\ $$ $$$ ~ ; 한글 🙂";

    // What iTerm2 3.7.3 makes of a `command` before it executes it. It replaces
    // `$$…$$` variables, and asks the user for one other than `$$$$`; then it
    // splits with its own parser (PTYSession.m `computeArgvForCommand:`;
    // NSStringITerm.m `doubleDollarVariables`,
    // `componentsBySplittingStringWithQuotesAndBackslashEscaping:`).
    fn iterm2_arguments(command: &str) -> Vec<String> {
        let (mut from, mut open) = (0, None);
        while let Some(found) = command[from..].find("$$").map(|at| at + from) {
            match open.take() {
                None => open = Some(found),
                Some(start) => assert_eq!(
                    &command[start..found + 2],
                    "$$$$",
                    "iTerm2 would ask the user for a variable"
                ),
            }
            from = found + 2;
        }
        let program = command.replace("$$$$", "$$");
        let (mut single, mut double, mut escape) = (false, false, false);
        let (mut first, mut first_was_quoted) = (true, true);
        let mut current = String::new();
        let mut arguments = Vec::new();
        for character in program.chars().map(Some).chain([None]) {
            let c = match character {
                Some('\0') => ' ',
                Some(c) => c,
                None => {
                    escape = false;
                    '\0'
                }
            };
            if c == '\\' && !escape {
                escape = true;
                continue;
            }
            if escape {
                first = false;
                escape = false;
                match c {
                    'n' => current.push('\n'),
                    'a' => current.push('\u{7}'),
                    't' => current.push('\t'),
                    'r' => current.push('\r'),
                    '"' | '\\' if double => current.push(c),
                    '\'' if single && !double => current.push('\\'),
                    _ if double || single => current.extend(['\\', c]),
                    _ => current.push(c),
                }
                continue;
            }
            if c == '"' && !single {
                double = !double;
                first = false;
                continue;
            }
            if c == '\'' && !double {
                single = !single;
                first = false;
                continue;
            }
            if c == '\0' {
                single = false;
                double = false;
            }
            if !single && !double && (c == '\0' || c.is_whitespace()) {
                if !first {
                    // iTerm2 expands a tilde in a word that began unquoted.
                    assert!(first_was_quoted || !current.starts_with('~'), "{current}");
                    arguments.push(std::mem::take(&mut current));
                    first_was_quoted = true;
                    first = true;
                }
                continue;
            }
            if first {
                first_was_quoted = single || double;
                first = false;
            }
            current.push(c);
        }
        arguments
    }

    // Review finding 6. iTerm2 turns the bootstrap into arguments itself, and
    // not as a shell does: the script must reach zsh as it was written.
    #[test]
    fn the_bootstrap_reaches_zsh_as_written_through_iterm2_s_own_parser() {
        let quote = |value: &str| crate::native::shell_quote(std::ffi::OsStr::new(value));
        for root in [
            "/Users/tester/.agent-bridge/native-sessions".to_owned(),
            format!("/Users/tester/{ODD_NAME}"),
        ] {
            let host = format!(
                "{} native-iterm2-host {}",
                quote(&format!("{root}/bin/agent-bridge")),
                quote(&format!("{root}/session-a1"))
            );
            let command = format!(". {}", quote(&format!("{root}/session-a1/launch.sh")));
            assert_eq!(
                iterm2_arguments(&shell_command(&host, &command)),
                [
                    "/bin/zsh",
                    "-l",
                    "-i",
                    "-c",
                    &format!("{host} || exit; {command}")
                ],
                "{root}"
            );
        }
        // The quoting of a shell is not what that parser reads. This is what
        // was sent before: a backslash in front of `n` became a line feed.
        assert_eq!(
            iterm2_arguments(&format!(
                "/bin/zsh -c {}",
                quote("'/state\\new/session-a1'")
            )),
            ["/bin/zsh", "-c", "'/state\new/session-a1'"]
        );
    }

    // The live #58 failure was `a. '/.../launch.sh'`: the native write-text path
    // appends to an editable shell line. Replay that input on a private PTY; no
    // terminal app or global keyboard is used here.
    #[test]
    fn startup_keys_cannot_change_the_launch_command() {
        use std::{
            fs::File,
            io::{Read, Write},
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::process::CommandExt,
            },
            process::Command,
            thread,
        };
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
        let bridge_command = format!(". {}", crate::native::shell_quote(script.as_os_str()));
        let mut master = -1;
        let mut slave = -1;
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
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut process = Command::new("/bin/zsh");
        process.args(["-f", "-i"]);
        // The old production script has no creation command; it starts an editing
        // shell and then sends the sourced path through that same input stream.
        let command_is_argument =
            OPEN_TAB_SCRIPT.contains("with default profile command bridgeCommand");
        if command_is_argument {
            process.args(["-c", &bridge_command]);
        }
        process
            .env("PS1", "AB_READY> ")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Keystrokes are queued before the shell starts, just as when create-tab
        // selects its surface before returning control to the launcher.
        master.write_all(b"a").unwrap();
        let mut child = process.spawn().unwrap();
        if !command_is_argument {
            master
                .write_all(format!("{bridge_command}\nexit\n").as_bytes())
                .unwrap();
        }
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut output = Vec::new();
        loop {
            let mut bytes = [0; 2048];
            if let Ok(n) = master.read(&mut bytes) {
                output.extend_from_slice(&bytes[..n]);
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker.exists(),
            "typed input corrupted startup: {}",
            String::from_utf8_lossy(&output)
        );
    }

    fn launch_fixture() -> tempfile::TempDir {
        launch_fixture_in(&std::env::temp_dir())
    }

    fn launch_fixture_in(parent: &Path) -> tempfile::TempDir {
        use crate::native::*;
        let directory = tempfile::Builder::new()
            .prefix("session-iterm-")
            .tempdir_in(parent)
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        launch::begin(
            &crate::native::session::Store::open_unchecked(directory.path()),
            &token,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        directory
    }

    fn bind_fixture(directory: &Path, terminal_id: &str) {
        crate::native::write_json_atomic(
            &directory.join(crate::native::TERMINAL_HANDLE_FILE),
            &serde_json::json!({
                "terminal": "iterm2", "session_id": terminal_id,
                "managed_session_id": directory.file_name().unwrap().to_str().unwrap()
            }),
        )
        .unwrap();
    }

    #[test]
    fn iterm_host_refuses_changed_binding_and_cancelled_launch() {
        for mode in [
            "wrong-id",
            "wrong-owner",
            "cancelled",
            "expired",
            "changed-claim",
            "invalid-binding",
        ] {
            let directory = launch_fixture();
            let directory = directory.path();
            let id = directory.file_name().unwrap().to_str().unwrap();
            bind_fixture(
                directory,
                if mode == "wrong-id" {
                    "other-id"
                } else {
                    "owned-id"
                },
            );
            match mode {
                "wrong-owner" => crate::native::write_json_atomic(&directory.join(crate::native::TERMINAL_HANDLE_FILE), &serde_json::json!({ "terminal": "iterm2", "session_id": "owned-id", "managed_session_id": "session-other" })).unwrap(),
                "cancelled" => crate::native::update_status(directory, SessionState::Closed, None, None).unwrap(),
                "expired" | "changed-claim" => {
                    let mut receipt = crate::native::launch::read(&crate::native::session::Reader::open_unchecked(directory)).unwrap().unwrap();
                    if mode == "expired" { receipt.deadline_unix_ms = 0; }
                    else { receipt.claim_token = "unrelated".into(); }
                    crate::native::write_json_atomic(&directory.join(crate::native::launch::FILE), &receipt).unwrap();
                }
                "invalid-binding" => std::fs::write(directory.join(crate::native::TERMINAL_HANDLE_FILE), b"{").unwrap(),
                _ => {}
            }
            assert!(
                wait_for_binding(directory, id, "owned-id").is_err(),
                "{mode}"
            );
        }
    }

    // Runs only as a subprocess of the private PTY test below.
    #[test]
    fn iterm_owned_process_probe() {
        let Ok(mode) = std::env::var("AB_ITERM_PROBE_MODE") else {
            return;
        };
        let directory = std::path::PathBuf::from(std::env::var_os("AB_ITERM_PROBE_DIR").unwrap());
        if mode == "host" {
            std::fs::write(directory.join("host-entered"), b"ready").unwrap();
            if let Err(error) = run_host(&directory) {
                eprintln!("{error:#}");
                std::process::exit(7);
            }
        } else {
            let id = directory.file_name().unwrap().to_str().unwrap();
            let owner = ownership::current_native_session_owner(id).unwrap();
            let mut queued: libc::c_int = -1;
            assert_eq!(unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) }, 0);
            crate::native::write_json_atomic(&directory.join("owner-probe.json"), &serde_json::json!({ "pid": owner.pid, "group": owner.process_group, "queued_input": queued })).unwrap();
        }
    }

    #[test]
    fn iterm_creation_command_waits_for_binding_and_discards_startup_keys() {
        for cancel in [false, true] {
            host_pty_fixture(cancel);
        }
    }

    fn host_pty_fixture(cancel: bool) {
        use std::{
            fs::File,
            io::{Read, Write},
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::process::CommandExt,
            },
            process::Command,
            thread,
        };
        // The session directory lies under a name with everything that iTerm2's parser or
        // a shell could take for syntax, and the bootstrap names it.
        let parent = tempfile::tempdir().unwrap();
        let odd = parent.path().join(ODD_NAME);
        std::fs::create_dir(&odd).unwrap();
        let directory = launch_fixture_in(&odd);
        let executable = std::env::current_exe().unwrap();
        let probe = format!(
            "AB_ITERM_PROBE_DIR={} {} --exact native::terminal::macos::iterm2::tests::iterm_owned_process_probe --nocapture --test-threads=1",
            crate::native::shell_quote(directory.path().as_os_str()),
            crate::native::shell_quote(executable.as_os_str())
        );
        let bootstrap = shell_command(
            &format!("AB_ITERM_PROBE_MODE=host {probe}"),
            &format!("AB_ITERM_PROBE_MODE=owner {probe}; exit $?"),
        );
        // iTerm2 executes the arguments that its own parser makes of the command.
        let arguments = iterm2_arguments(&bootstrap);
        let mut master = -1;
        let mut slave = -1;
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
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut command = Command::new(&arguments[0]);
        command
            .args(&arguments[1..])
            .env("ZDOTDIR", directory.path())
            .env("ITERM_SESSION_ID", "w0t0p0:owned-id")
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
        // Include Enter: none of these bytes may be interpreted as a startup
        // command or remain queued for the provider's first dialog.
        master.write_all(b"aaaa\n").unwrap();
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut released = false;
        let mut output = Vec::new();
        loop {
            let mut bytes = [0; 4096];
            if let Ok(n) = master.read(&mut bytes) {
                output.extend_from_slice(&bytes[..n]);
            }
            if !released && directory.path().join("host-entered").exists() {
                assert!(
                    !directory.path().join("owner-probe.json").exists(),
                    "wrapper ran before binding"
                );
                master.write_all(b"more-keys\n").unwrap_or_else(|error| {
                    panic!(
                        "PTY closed before binding: {error}; {}",
                        String::from_utf8_lossy(&output)
                    )
                });
                if cancel {
                    crate::native::update_status(
                        directory.path(),
                        SessionState::Closed,
                        None,
                        None,
                    )
                    .unwrap();
                } else {
                    bind_fixture(directory.path(), "owned-id");
                }
                released = true;
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "iTerm host fixture timed out: {}",
                    String::from_utf8_lossy(&output)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            released,
            "host never reached gate: {}",
            String::from_utf8_lossy(&output)
        );
        let owner = directory.path().join("owner-probe.json");
        if cancel {
            assert!(!owner.exists(), "cancelled host launched a wrapper");
            assert!(!child.wait().unwrap().success());
        } else {
            assert!(
                child.wait().unwrap().success(),
                "{}",
                String::from_utf8_lossy(&output)
            );
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(owner).unwrap()).unwrap();
            assert_eq!(
                value["queued_input"], 0,
                "startup keys reached wrapper: {value}"
            );
        }
    }
}
