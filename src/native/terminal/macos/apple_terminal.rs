use super::OwnedSurfaceClose;
use super::applescript::{Automation, Osascript};
use super::{DialogKey, GuardedDialogInput, MacTerminalAdapter, MacosOpenMode};
use crate::native::session::close::budget::close_remaining;
pub(in crate::native) mod close_evidence;
pub(in crate::native::terminal::macos) mod incarnation;
#[cfg(test)]
mod startup_absence_tests;

use super::process;
use super::process::macos_process_start;
use crate::native::session::{CoreRecord, Reader, RecordStore, Store};
use crate::native::terminal;
use crate::native::terminal::ownership;
use crate::native::terminal::ownership::{MacTerminalAppIdentity, NativeSessionOwner};
use agent_bridge::process_is_alive;
use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    applescript, applescript::close_response,
};

use crate::native::clock::{Clock, SystemClock};
use crate::native::session::launch::BindingFacts;

const HOST_COMMAND: &str = TerminalKind::AppleTerminal.registration().host_command;
const BINDING: &BindingFacts = TerminalKind::AppleTerminal.registration().binding();

// Bind only one new window with the returned tty; restore only visible windows.
// Start with line-kill plus the whole bootstrap; see docs/verification/2026-10-03-v0.1.0-focus.md.
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
    if not application "Terminal" is running then error "Agent Bridge Terminal.app send: not running"
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

// A killed shell can leave U+0001 after its tty; only a successful window-list read proves absence.
// See docs/verification/2026-10-01-terminal-proof.md.
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

// Only this transaction's successful close can accept an invisible, tabless listed window.
// Otherwise require absence from the list; see docs/verification/2026-10-03-v0.1.0-focus.md.
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

fn create_tab(
    automation: &dyn Automation,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<TerminalSession> {
    let start = launch_bootstrap(command, directory)?;
    let response = automation.run_until("Terminal.app", OPEN_TAB_SCRIPT, &[&start], deadline)?;
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

fn launch_bootstrap(command: &str, directory: &Path) -> Result<String> {
    let executable =
        std::env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let host = format!(
        "{} {HOST_COMMAND} {}",
        crate::native::terminal::shell_quote(executable.as_os_str()),
        crate::native::terminal::shell_quote(directory.as_os_str())
    );
    install_bootstrap(directory, &host, command)
}

// Source a short private bootstrap so gate and wrapper keep distinct foreground jobs.
// A zsh that refuses exit can leave a prompt; retain the failed launch's close proof.
fn install_bootstrap(directory: &Path, host: &str, command: &str) -> Result<String> {
    let path = Reader::open_unchecked(directory)
        .private(BOOTSTRAP_FILE)
        .path()
        .to_owned();
    RecordStore::at(&path).write_private(format!("{host} || exit\n{command}\n").as_bytes())?;
    Ok(format!(
        ". {}",
        crate::native::terminal::shell_quote(path.as_os_str())
    ))
}

// The typed start precedes tty discovery; wait for restored focus and the exact tty binding.
// A failed, closed or timed-out launch must start nothing.
pub(in crate::native) fn run_host(directory: &Path) -> Result<()> {
    use super::bound_host::{BoundHostSpec, run_tty_bound_host};
    run_tty_bound_host(
        directory,
        &BoundHostSpec { name: BINDING.host },
        || {
            let tty = process::current_terminal_tty()?;
            let live = process::live_native_process_identity(std::process::id())?;
            if live.terminal_tty_device != process::terminal_tty_device(Path::new(&tty))? {
                bail!("Terminal.app launch host is not attached to the tty of its standard input");
            }
            Ok(tty)
        },
        |directory, id, tty| wait_for_binding(directory, id, tty, &mut SystemClock),
        |released| {
            crate::native::session::launch::log(
                &Store::open_unchecked(directory),
                &match released {
                    Ok(tty) => format!("terminal_host_released tty={tty}"),
                    Err(error) => format!("terminal_host_refused: {error:#}"),
                },
            );
        },
    )
}

fn wait_for_binding(directory: &Path, id: &str, tty: &str, clock: &mut dyn Clock) -> Result<()> {
    use crate::native::session::launch::wait_for_binding;
    let surface = wait_for_binding(&Reader::open_unchecked(directory), id, BINDING, clock)?;
    super::bound_host::binding_names_host(
        &surface,
        TerminalKind::AppleTerminal,
        |surface| {
            surface.id == tty
                && surface
                    .window_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty())
        },
        "Terminal.app surface binding does not name the tty of this launch host",
    )
}

fn send_file(
    automation: &dyn Automation,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let window_id = ownership_proof(session).map_err(TerminalSendFailure::not_sent)?;
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let response = automation.run_send_until(
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

fn verify_tab(
    automation: &dyn Automation,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    verify_tab_with_deadline(
        automation,
        session,
        timeout.map(super::timeout_deadline).transpose()?,
    )
}

fn verify_tab_with_deadline(
    automation: &dyn Automation,
    session: &TerminalSession,
    deadline: Option<Instant>,
) -> Result<String> {
    let window_id = ownership_proof(session)?;
    let response = run_terminal_automation(
        automation,
        VERIFY_TAB_SCRIPT,
        &[&session.id, window_id],
        deadline,
    )?;
    if response != session.id {
        bail!("Agent Bridge Terminal.app owned tab is missing");
    }
    Ok(response)
}

fn close_owned_session(
    automation: &dyn Automation,
    directory: &Path,
    session: &TerminalSession,
    deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    let owner: NativeSessionOwner = Reader::open_unchecked(directory).owner()?;
    close_attested_session(
        automation,
        session,
        owner
            .terminal_app
            .as_ref()
            .context("Terminal.app close has no app incarnation")?,
        deadline,
    )
}

fn close_session_until(
    automation: &dyn Automation,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_session_with(session, automation, Some(deadline))
}

fn close_session_with(
    session: &TerminalSession,
    automation: &dyn Automation,
    deadline: Option<Instant>,
) -> Result<CloseOutcome> {
    close_transactions(session, |script, arguments| {
        run_terminal_automation(automation, script, arguments, deadline)
    })
}

fn close_attested_session(
    automation: &dyn Automation,
    session: &TerminalSession,
    app: &ownership::MacTerminalAppIdentity,
    deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    close_attested_session_with(
        session,
        app,
        process::macos_process_start,
        incarnation::terminal_app_instances,
        automation,
        deadline,
    )
}

fn close_attested_session_with(
    session: &TerminalSession,
    app: &ownership::MacTerminalAppIdentity,
    mut process_birth: impl FnMut(u32) -> Result<Option<(u64, u64)>>,
    mut instances: impl FnMut() -> Result<Vec<ownership::MacTerminalAppIdentity>>,
    automation: &dyn Automation,
    deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    // Every transaction, including retries, runs inside the incarnation's recheck; an
    // ended incarnation answers as Terminal's own "missing".
    close_transactions(session, |script, arguments| {
        let reply =
            incarnation::around_close(app, deadline, &mut process_birth, &mut instances, || {
                automation.run_until("Terminal.app", script, arguments, deadline)
            })?;
        Ok(reply.unwrap_or_else(|| "missing".into()))
    })
}

fn close_transactions(
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
    automation: &dyn Automation,
    script: &str,
    arguments: &[&str],
    deadline: Option<Instant>,
) -> Result<String> {
    match deadline {
        Some(deadline) => automation.run_until("Terminal.app", script, arguments, deadline),
        None => automation.run("Terminal.app", script, arguments),
    }
}

fn ownership_proof(session: &TerminalSession) -> Result<&str> {
    session
        .window_id
        .as_deref()
        .context("Terminal.app session record is missing its window id")
}

// The second verification of a close, through the automation of the adapter that
// closes, so a test's recording automation sees every call of the close.
#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn verified_close_tty(
    automation: &dyn Automation,
    session: &terminal::TerminalSession,
    deadline: Instant,
) -> Result<String> {
    if session.kind != terminal::TerminalKind::AppleTerminal {
        bail!("Terminal.app ownership proof received a different terminal kind")
    }
    crate::native::session::close::budget::close_remaining(deadline)?;
    verify_tab_with_deadline(automation, session, Some(deadline))
}

// Startup command failure can precede the wrapper's owner and app identity record. The
// window and tty alone do not authorize a close after an app restart. Only the proven
// absence of the exact surface can unbind here; never close/signal.
#[cfg(target_os = "macos")]
pub(super) fn apple_terminal_startup_absent(
    directory: &Path,
    surface: &terminal::TerminalSession,
    deadline: Instant,
) -> Result<bool> {
    apple_terminal_startup_absent_with(
        directory,
        surface,
        process_is_alive,
        macos_process_start,
        incarnation::terminal_app_instances,
        || {
            terminal::surface_present(
                directory,
                surface,
                deadline.saturating_duration_since(Instant::now()),
            )
        },
    )
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn apple_terminal_startup_absent_with(
    directory: &Path,
    surface: &terminal::TerminalSession,
    owner_alive: impl FnOnce(u32) -> bool,
    process_birth: impl Fn(u32) -> Result<Option<(u64, u64)>>,
    terminal_instances: impl Fn() -> Result<Vec<MacTerminalAppIdentity>>,
    surface_present: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    let bound: terminal::TerminalSession = Reader::open_unchecked(directory)
        .terminal()
        .context("Terminal.app startup has no durable surface binding; cleanup is unverified")?;
    if surface.kind != terminal::TerminalKind::AppleTerminal
        || bound.kind != surface.kind
        || bound.id != surface.id
        || surface.id.is_empty()
        || surface.window_id.as_deref().is_none_or(str::is_empty)
        || bound.window_id != surface.window_id
        || bound.tab_id != surface.tab_id
        || bound.managed_session_id.is_none()
    {
        bail!(
            "Terminal.app startup surface identity is incomplete or changed; the binding is retained"
        );
    }
    // The wrapper records itself only once it runs: a start command that was never
    // typed, or a wrapper that failed first, leaves no owner and no app incarnation.
    let owner = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .text()?
        .map(|text| serde_json::from_str::<NativeSessionOwner>(&text))
        .transpose()
        .context("invalid Terminal.app startup owner record; the binding is retained")?;
    if let Some(owner) = &owner {
        if bound.managed_session_id != owner.managed_session_id
            || owner.terminal_tty.as_deref() != Some(surface.id.as_str())
            || !matches!(
                (
                    owner.terminal_tty_device,
                    owner.process_start_seconds,
                    owner.process_start_microseconds,
                    owner.process_group,
                    owner.terminal_process_group
                ),
                (Some(_), Some(_), Some(_), Some(_), Some(_))
            )
        {
            bail!(
                "Terminal.app startup owner/surface identity is incomplete or changed; the binding is retained"
            );
        }
        if owner_alive(owner.pid) {
            bail!(
                "Terminal.app startup owner is still live; cleanup is unverified and the binding is retained"
            );
        }
    }
    incarnation::terminal_surface_absent(
        owner.as_ref().and_then(|owner| owner.terminal_app.as_ref()),
        process_birth,
        terminal_instances,
        surface_present,
    )
}

type Start = fn(&TerminalSession, Instant) -> Result<()>;
type Absence = fn(&Path, &TerminalSession, Instant) -> Result<bool>;
pub(super) struct Adapter<S, A, R: Automation> {
    automation: R,
    start: S,
    absent: A,
}
// Terminal.app typed its start when it created the tab, and the start waits for the
// same binding. Nothing is typed after the creation.
pub(super) fn started_at_creation(_: &TerminalSession, _: Instant) -> Result<()> {
    Ok(())
}
pub(super) type InstalledAdapter = Adapter<Start, Absence, Osascript>;
impl Adapter<Start, Absence, Osascript> {
    /// The adapter over the installed `osascript` with the production start and
    /// startup-absence observations.
    pub(super) const fn installed() -> Self {
        Self {
            automation: Osascript,
            start: started_at_creation,
            absent: apple_terminal_startup_absent,
        }
    }
}
// The production observations over a test automation: for operations on an existing
// surface, which never start or clean up.
#[cfg(test)]
pub(super) fn with_automation<R: Automation>(automation: R) -> Adapter<Start, Absence, R> {
    Adapter {
        automation,
        start: started_at_creation,
        absent: apple_terminal_startup_absent,
    }
}
#[cfg(test)]
pub(super) fn with_observations<'a, S, A, R: Automation>(
    start: &'a S,
    absent: &'a A,
    automation: R,
) -> Adapter<&'a S, &'a A, R>
where
    S: Fn(&TerminalSession, Instant) -> Result<()>,
    A: Fn(&Path, &TerminalSession, Instant) -> Result<bool>,
{
    Adapter {
        start,
        absent,
        automation,
    }
}
// Stable creation-time handle only. A compare-and-send is one automation call;
// a changed captured screen sends no keys. This is not atomic with repaint or provider
// consumption after that comparison. Never targets front tab.
pub(in crate::native) const SCREEN_SCRIPT: &str = r#"
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
    considering case
        if screenText is not expectedScreen then return "changed"
    end considering
    my sendKey(targetTab, keyName)
    return "sent"
end guardedAnswer
"#;
impl<S, A, R: Automation> MacTerminalAdapter for Adapter<S, A, R>
where
    S: Fn(&TerminalSession, Instant) -> Result<()>,
    A: Fn(&Path, &TerminalSession, Instant) -> Result<bool>,
{
    fn open_bound_tab(
        &self,
        mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: super::BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        super::open_with(
            deadline,
            |startup_deadline| {
                if mode != MacosOpenMode::NewWindow {
                    // Terminal.sdef exposes the tabs collection read-only. `do script in`
                    // addresses an existing tab; it is not a new-tab creation command.
                    // Replace this fallback if Terminal gains a native owned-tab API.
                    eprintln!(
                        "Terminal.app tab-first: its native scripting interface has no new-tab creation command; opening a new owned window"
                    );
                }
                create_tab(&self.automation, command, directory, startup_deadline)
            },
            |session, deadline| (self.start)(session, deadline),
            |session, deadline| {
                if (self.absent)(directory, session, deadline)? {
                    return Ok(());
                }
                bail!(
                    "Terminal.app startup failed with a remaining surface; no new close or signal authority was granted and its binding is retained"
                )
            },
            bind,
            unbind,
        )
    }
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult {
        send_file(&self.automation, session, prompt_path, deadline)
    }
    fn verify_surface(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>> {
        verify_tab(&self.automation, session, timeout).map(Some)
    }
    fn surface_present(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        let response = self.automation.run_until(
            "Terminal.app",
            VERIFY_TAB_SCRIPT,
            &[
                session.id.as_str(),
                session
                    .window_id
                    .as_deref()
                    .context("Terminal.app window id is missing")?,
            ],
            super::timeout_deadline(timeout)?,
        )?;
        match response.as_str() {
            "missing" => Ok(false),
            "present" => Ok(true),
            tty if tty == session.id => Ok(true),
            _ => bail!("unexpected terminal presence response: {response:?}"),
        }
    }
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
        let args = vec![
            session.id.as_str(),
            session
                .window_id
                .as_deref()
                .context("Terminal window identity missing")?,
        ];
        let text = self.automation.run_until(
            session.kind.display_name(),
            SCREEN_SCRIPT,
            &args,
            deadline,
        )?;
        applescript::screen_text(&text)
    }
    fn guarded_dialog_input(
        &self,
        session: &TerminalSession,
        input: &GuardedDialogInput,
        deadline: Instant,
    ) -> Result<bool> {
        let mut args = vec![
            session.id.as_str(),
            session
                .window_id
                .as_deref()
                .context("Terminal window identity missing")?,
        ];
        args.extend([
            input.screen.as_str(),
            match input.key {
                DialogKey::Enter => "enter",
                DialogKey::DownEnter => "down-enter",
            },
        ]);
        let answer = self.automation.run_until(
            session.kind.display_name(),
            SCREEN_SCRIPT,
            &args,
            deadline,
        )?;
        applescript::guarded_answer(&answer)
    }
    fn close_session_until(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_session_until(&self.automation, session, deadline)
    }
    fn close_owned_session(
        &self,
        directory: &Path,
        id: &str,
        session: &TerminalSession,
        close: OwnedSurfaceClose,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if close == OwnedSurfaceClose::LiveOwner {
            close_remaining(deadline)?;
            close_evidence::terminate_owner(&self.automation, directory, id, session, deadline)?;
        }
        close_owned_session(&self.automation, directory, session, deadline)
    }
}
// The shipped scripts, executed with Terminal replaced by records: only the reads and
// the close are swapped for mock handlers, the scripts' own decisions run in
// `osascript`, and nothing talks to Terminal.
#[cfg(test)]
mod tests {
    use super::MacTerminalAdapter;
    use super::applescript::testing::{Call, Method, Replay, Reply, ScriptId};
    fn automation_call(
        method: Method,
        script: ScriptId,
        arguments: &[&str],
        deadline: Option<Instant>,
    ) -> Call {
        Call::new("Terminal.app", method, script, arguments, deadline)
    }
    #[test]
    fn automation_open_verify_presence_screen_and_send_preserve_arguments() {
        use super::{DialogKey, GuardedDialogInput, MacosOpenMode, TerminalSendFailure};
        use std::path::Path;
        let directory = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let timeout = Duration::from_secs(2);
        let replay = Replay::normal(&[
            &format!("{TTY}\n{WINDOW}"),
            TTY,
            TTY,
            "present",
            "missing",
            TTY,
            "AB_SCREEN_BEGIN  screen\nAB_SCREEN_END",
            "sent",
            "changed",
            "sent",
            "changed",
        ]);
        let absent = |_: &Path, _: &TerminalSession, _: Instant| -> anyhow::Result<bool> {
            panic!("successful open must not clean up")
        };
        let adapter =
            super::with_observations(&super::started_at_creation, &absent, replay.clone());
        // The adapter's open installs the bootstrap itself (create-new); the expected
        // start argument is derived from the path without writing the file first.
        let bootstrap_path = directory.path().join(super::BOOTSTRAP_FILE);
        let start = format!(
            ". {}",
            crate::native::terminal::shell_quote(bootstrap_path.as_os_str())
        );
        let open = automation_call(
            Method::RunUntil,
            ScriptId::TerminalOpen,
            &[&start],
            Some(deadline - Duration::from_secs(2)),
        );
        let session = adapter
            .open_bound_tab(
                MacosOpenMode::NewWindow,
                "echo card10",
                directory.path(),
                deadline,
                Box::new(|session| {
                    assert_eq!(replay.calls(), std::slice::from_ref(&open));
                    assert_eq!(session.id, TTY);
                    assert_eq!(session.window_id.as_deref(), Some(WINDOW));
                    session.managed_session_id = Some("managed-id".into());
                    Ok(())
                }),
                Box::new(|| panic!("successful open must retain binding")),
            )
            .unwrap();
        assert_eq!(session.managed_session_id.as_deref(), Some("managed-id"));
        let bootstrap = std::fs::read_to_string(&bootstrap_path).unwrap();
        assert!(bootstrap.contains(&format!(
            "native-terminal-host {}",
            crate::native::terminal::shell_quote(directory.path().as_os_str())
        )));
        assert!(bootstrap.ends_with("echo card10\n"));
        let mut expected = vec![open];
        assert_eq!(
            adapter
                .verify_surface(directory.path(), &session, None)
                .unwrap(),
            Some(TTY.into())
        );
        expected.push(automation_call(
            Method::Run,
            ScriptId::TerminalVerify,
            &[TTY, WINDOW],
            None,
        ));
        let before = Instant::now();
        assert_eq!(
            adapter
                .verify_surface(directory.path(), &session, Some(timeout))
                .unwrap(),
            Some(TTY.into())
        );
        let after = Instant::now();
        let observed = replay.calls().last().unwrap().deadline.unwrap();
        assert!((before + timeout..=after + timeout).contains(&observed));
        expected.push(automation_call(
            Method::RunUntil,
            ScriptId::TerminalVerify,
            &[TTY, WINDOW],
            Some(observed),
        ));
        for present in [true, false, true] {
            let before = Instant::now();
            assert_eq!(
                adapter
                    .surface_present(directory.path(), &session, timeout)
                    .unwrap(),
                present
            );
            let after = Instant::now();
            let observed = replay.calls().last().unwrap().deadline.unwrap();
            assert!((before + timeout..=after + timeout).contains(&observed));
            expected.push(automation_call(
                Method::RunUntil,
                ScriptId::TerminalVerify,
                &[TTY, WINDOW],
                Some(observed),
            ));
        }
        assert_eq!(
            adapter.read_screen(&session, deadline).unwrap(),
            "  screen\n"
        );
        expected.push(automation_call(
            Method::RunUntil,
            ScriptId::TerminalScreen,
            &[TTY, WINDOW],
            Some(deadline),
        ));
        for (key, name) in [
            (DialogKey::Enter, "enter"),
            (DialogKey::DownEnter, "down-enter"),
        ] {
            for sent in [true, false] {
                assert_eq!(
                    adapter
                        .guarded_dialog_input(
                            &session,
                            &GuardedDialogInput {
                                expected_claim: None,
                                expires_unix_ms: None,
                                screen: "  screen\n".into(),
                                key
                            },
                            deadline
                        )
                        .unwrap(),
                    sent
                );
                expected.push(automation_call(
                    Method::RunUntil,
                    ScriptId::TerminalScreen,
                    &[TTY, WINDOW, "  screen\n", name],
                    Some(deadline),
                ));
            }
        }
        assert_eq!(replay.calls(), expected);
        replay.assert_finished();
        for response in ["sent", "bad", "not-sent", "uncertain"] {
            let reply = match response {
                "not-sent" => Err(TerminalSendFailure::not_sent(anyhow::anyhow!("not-sent"))),
                "uncertain" => Err(TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
                    "uncertain"
                ))),
                _ => Ok(response.into()),
            };
            let replay = Replay::new([Reply::Send(reply)]);
            let adapter =
                super::with_observations(&super::started_at_creation, &absent, replay.clone());
            let result = adapter.send_file(&session, Path::new("/tmp/prompt"), deadline);
            match response {
                "sent" => result.unwrap(),
                "not-sent" => {
                    let failure = result.unwrap_err();
                    assert!(!failure.delivery_may_have_occurred());
                    assert_eq!(failure.error().to_string(), "not-sent");
                }
                _ => {
                    let failure = result.unwrap_err();
                    assert!(failure.delivery_may_have_occurred());
                    assert_eq!(
                        failure.error().to_string(),
                        if response == "bad" {
                            "unexpected Terminal.app send response: \"bad\""
                        } else {
                            "uncertain"
                        }
                    );
                }
            }
            assert_eq!(
                replay.calls(),
                [automation_call(
                    Method::RunSendUntil,
                    ScriptId::TerminalSend,
                    &[TTY, WINDOW, "/tmp/prompt"],
                    Some(deadline)
                )]
            );
            replay.assert_finished();
        }
    }

    use super::Automation;
    use std::cell::RefCell;
    // Runs the shipped close scripts against the existing Terminal model. It has
    // no transaction decisions: those remain in close_transactions.
    struct ScriptAutomation<F>(RefCell<F>);
    impl<F: FnMut(&str, &[&str]) -> anyhow::Result<String>> ScriptAutomation<F> {
        fn new(run: F) -> Self {
            Self(RefCell::new(run))
        }
    }
    impl<F: FnMut(&str, &[&str]) -> anyhow::Result<String>> Automation for ScriptAutomation<F> {
        fn run(
            &self,
            application: &str,
            script: &str,
            arguments: &[&str],
        ) -> anyhow::Result<String> {
            assert_eq!(application, "Terminal.app");
            (self.0.borrow_mut())(script, arguments)
        }
        fn run_until(&self, _: &str, _: &str, _: &[&str], _: Instant) -> anyhow::Result<String> {
            panic!("script replay uses unbounded close")
        }
        fn run_send_until(
            &self,
            _: &str,
            _: &str,
            _: &[&str],
            _: Instant,
        ) -> Result<String, super::TerminalSendFailure> {
            panic!("close test must not send input")
        }
    }
    struct AttestedAutomation<F>(RefCell<F>);
    impl<F: FnMut(&str, &[&str]) -> anyhow::Result<String>> AttestedAutomation<F> {
        fn new(run: F) -> Self {
            Self(RefCell::new(run))
        }
    }
    impl<F: FnMut(&str, &[&str]) -> anyhow::Result<String>> Automation for AttestedAutomation<F> {
        fn run(&self, _: &str, _: &str, _: &[&str]) -> anyhow::Result<String> {
            panic!("attested close must carry its deadline")
        }
        fn run_until(
            &self,
            application: &str,
            script: &str,
            arguments: &[&str],
            _: Instant,
        ) -> anyhow::Result<String> {
            assert_eq!(application, "Terminal.app");
            (self.0.borrow_mut())(script, arguments)
        }
        fn run_send_until(
            &self,
            _: &str,
            _: &str,
            _: &[&str],
            _: Instant,
        ) -> Result<String, super::TerminalSendFailure> {
            panic!("close test must not send input")
        }
    }
    struct ClockAutomation<'a> {
        clock: &'a std::rc::Rc<std::cell::Cell<Instant>>,
        calls: &'a std::cell::Cell<usize>,
        expire_at: usize,
        deadline: Instant,
    }
    impl Automation for ClockAutomation<'_> {
        fn run(&self, _: &str, _: &str, _: &[&str]) -> anyhow::Result<String> {
            panic!("attested close must carry its deadline")
        }
        fn run_until(
            &self,
            application: &str,
            _: &str,
            _: &[&str],
            deadline: Instant,
        ) -> anyhow::Result<String> {
            assert_eq!(application, "Terminal.app");
            assert_eq!(deadline, self.deadline);
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == self.expire_at {
                self.clock.set(deadline);
            }
            Ok(if self.calls.get() == 1 {
                "closed"
            } else {
                "missing"
            }
            .into())
        }
        fn run_send_until(
            &self,
            _: &str,
            _: &str,
            _: &[&str],
            _: Instant,
        ) -> Result<String, super::TerminalSendFailure> {
            panic!("close test must not send input")
        }
    }
    #[test]
    fn launch_context_does_not_depend_on_terminal_environment() {
        use super::{BOOTSTRAP_FILE, launch_bootstrap};
        if !crate::native::process_env::tests::without_bridge_environment(
            module_path!(),
            "launch_context_does_not_depend_on_terminal_environment",
        ) {
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-e1");
        std::fs::create_dir(&directory).unwrap();
        let wrapper = crate::native::process_env::tests::wrapper_for(&directory);
        let bootstrap = launch_bootstrap(&wrapper, &directory).unwrap();
        assert_eq!(
            bootstrap,
            format!(
                ". {}",
                crate::native::terminal::shell_quote(directory.join(BOOTSTRAP_FILE).as_os_str())
            )
        );
        let script = std::fs::read_to_string(directory.join(BOOTSTRAP_FILE)).unwrap();
        assert!(script.contains(&format!(
            "native-terminal-host {}",
            crate::native::terminal::shell_quote(directory.as_os_str())
        )));
        assert!(script.ends_with(&format!("{wrapper}\n")));
        let invocation = script.split_once(" || exit\n").unwrap().0;
        let args = super::super::test_support::host_argv(invocation);
        assert_eq!(
            args,
            [
                super::HOST_COMMAND.to_owned(),
                directory.to_str().unwrap().to_owned()
            ]
        );
        assert!(
            matches!(crate::native::parse_args(args).unwrap(), crate::native::NativeCommand::Host(crate::native::terminal::host::HostCommand::AppleTerminal { directory: actual }) if actual == directory)
        );
    }

    #[test]
    fn host_binding_partial_transitions() {
        crate::native::session::launch::binding_tests::characterize(
            "Terminal.app",
            |directory, id, clock| super::wait_for_binding(directory, id, "host-id", clock),
        );
    }

    use super::{
        CLOSE_TAB_SCRIPT, SEND_FILE_SCRIPT, TerminalKind, TerminalSession, VERIFY_TAB_SCRIPT,
        WAIT_FOR_CLOSE_SCRIPT, applescript, ownership, ownership_proof,
    };
    use crate::native::session::SessionState;
    use std::time::{Duration, Instant};

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
    fn close_deadline_attested_transactions_stop_after_expiry() {
        for expire_at in [0, 1, 2] {
            let app = ownership::MacTerminalAppIdentity {
                pid: 1234,
                start_seconds: 100,
                start_microseconds: 42,
            };
            let now = Instant::now();
            let fake = crate::native::clock::FakeClock::new(now);
            let clock = fake.shared();
            let _scope = crate::native::session::close::budget::Scope::with_clock(fake);
            let deadline = now + Duration::from_secs(1);
            let calls = std::cell::Cell::new(0);
            let births = std::cell::Cell::new(0);
            let result = super::close_attested_session_with(
                &session(),
                &app,
                |_| {
                    births.set(births.get() + 1);
                    Ok(Some((100, 42)))
                },
                || {
                    if expire_at == 0 {
                        clock.set(deadline);
                    }
                    Ok(vec![app.clone()])
                },
                &ClockAutomation {
                    clock: &clock,
                    calls: &calls,
                    expire_at,
                    deadline,
                },
                deadline,
            );
            assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
            assert_eq!(calls.get(), expire_at);
            assert!(births.get() >= 1);
        }
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
                &AttestedAutomation::new(|_, _| {
                    calls += 1;
                    Ok(match calls {
                        1 | 3 => "closed",
                        2 => "present",
                        _ => "missing",
                    }
                    .into())
                }),
                crate::native::session::close::budget::close_deadline(),
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

on mockSend(promptText, targetTab)
    mockEvent()
    log "sent " & promptText & " to " & (tty of targetTab)
end mockSend

on mockScreen(w, wantedTty)
    mockEvent()
    return "trust this directory?"
end mockScreen

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
        let script = mocked_script(script, terminal);
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .args(arguments)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = stderr
            .lines()
            .filter(|line| {
                line.starts_with("closed ")
                    || line.starts_with("fronted ")
                    || line.starts_with("sent ")
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

    fn mocked_script(script: &str, terminal: &Terminal) -> String {
        let script = [
            (
                "do script promptText in targetTab",
                "my mockSend(promptText, targetTab)",
            ),
            (
                "do script ((ASCII character 27) & \"[B\") in targetTab",
                "my mockSend(\"down-enter\", targetTab)",
            ),
            (
                "do script \"\" in targetTab",
                "my mockSend(\"enter\", targetTab)",
            ),
            (
                "contents of (first tab of w whose tty is wantedTty)",
                "my mockScreen(w, wantedTty)",
            ),
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
        format!("{state}{MOCK}{script}")
    }

    #[test]
    fn terminal_send_and_screen_refuse_stopped_application_without_access() {
        let prompt = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(prompt.path(), "hello").unwrap();
        let path = prompt.path().to_str().unwrap();
        for (script, args, error) in [
            (
                SEND_FILE_SCRIPT,
                vec![TTY, WINDOW, path],
                "Agent Bridge Terminal.app send: not running",
            ),
            (
                super::SCREEN_SCRIPT,
                vec![TTY, WINDOW],
                "Agent Bridge Terminal.app screen: not running",
            ),
            (
                super::SCREEN_SCRIPT,
                vec![TTY, WINDOW, "trust this directory?", "enter"],
                "Agent Bridge Terminal.app screen: not running",
            ),
        ] {
            let (response, events) = replay(script, &STOPPED, &args);
            assert!(response.unwrap_err().contains(error));
            assert!(events.is_empty(), "{events:?}");
        }
        assert_eq!(
            replay(SEND_FILE_SCRIPT, &running(&[LIVE]), &[TTY, WINDOW, path]),
            (Ok("sent".into()), vec![format!("sent hello to {TTY}")])
        );
        assert_eq!(
            replay(super::SCREEN_SCRIPT, &running(&[LIVE]), &[TTY, WINDOW]),
            (
                Ok("AB_SCREEN_BEGINtrust this directory?AB_SCREEN_END".into()),
                vec![]
            )
        );
        for key in ["enter", "down-enter"] {
            for (screen, answer) in [
                ("trust this directory?", "sent"),
                ("trust this directory!", "changed"),
            ] {
                let events = if answer == "sent" {
                    vec![format!("sent {key} to {TTY}")]
                } else {
                    vec![]
                };
                assert_eq!(
                    replay(
                        super::SCREEN_SCRIPT,
                        &running(&[LIVE]),
                        &[TTY, WINDOW, screen, key]
                    ),
                    (Ok(answer.into()), events)
                );
            }
        }
    }

    #[test]
    fn terminal_screen_handlers_recheck_running_before_application_access() {
        for call in [
            "my screenOf(missing value, \"unused\")",
            "my sendKey(missing value, \"enter\")",
            "my sendKey(missing value, \"down-enter\")",
        ] {
            // Invoke the handler after the application's earlier running check.
            let script = super::SCREEN_SCRIPT
                .replace("on run argv", "on originalRun(argv)")
                .replacen("end run", "end originalRun", 1);
            let script = format!("{script}\non run\nreturn {call}\nend run\n");
            let (response, events) = replay(&script, &STOPPED, &[]);
            let error = response.unwrap_err();
            assert!(
                error.contains("Agent Bridge Terminal.app screen: not running"),
                "{error}"
            );
            assert!(events.is_empty(), "{events:?}");
        }
    }

    #[test]
    fn terminal_disappearing_after_send_ownership_proof_is_delivery_uncertain() {
        let session = TerminalSession {
            kind: TerminalKind::AppleTerminal,
            id: TTY.into(),
            window_id: Some(WINDOW.into()),
            tab_id: None,
            managed_session_id: None,
            windows_process_identity: None,
            wezterm_mux: None,
        };
        let window = ownership_proof(&session).unwrap();
        let prompt = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(prompt.path(), "hello").unwrap();
        // The earlier proof passed; the application is gone when osascript runs.
        let failure = applescript::run_send_until(
            "Terminal.app",
            &mocked_script(SEND_FILE_SCRIPT, &STOPPED),
            &[&session.id, window, prompt.path().to_str().unwrap()],
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        let error = format!("{:#}", failure.error());
        assert!(
            error.contains("Agent Bridge Terminal.app send: not running"),
            "{error}"
        );
        assert!(!error.contains("launched Terminal"), "{error}");
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
        let outcome = super::close_session_with(
            &session(),
            &ScriptAutomation::new(|script, arguments| {
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
            }),
            None,
        );
        (outcome, closes)
    }

    // The cleanup of session-mpL0WX failed on 2026-10-03 with `window no longer holds
    // its tab`: the close had closed window 12064, Terminal still listed it without
    // tabs and not visible, and the second close found no tab in it.
    #[test]
    fn terminal_app_close_finishes_when_terminal_still_lists_the_window_it_closed() {
        for (replies, outcome) in [
            (vec!["missing"], super::CloseOutcome::Missing),
            (vec!["closed", "missing"], super::CloseOutcome::Closed),
            (vec!["closed", "closed"], super::CloseOutcome::Closed),
            (
                vec!["closed", "present", "missing"],
                super::CloseOutcome::Closed,
            ),
            (
                vec!["closed", "present", "closed", "missing"],
                super::CloseOutcome::Closed,
            ),
            (
                vec!["closed", "present", "closed", "closed"],
                super::CloseOutcome::Closed,
            ),
        ] {
            for deadline in [None, Some(Instant::now() + Duration::from_secs(10))] {
                let replay = Replay::normal(&replies);
                assert_eq!(
                    super::close_session_with(&session(), &replay, deadline).unwrap(),
                    outcome
                );
                let method = if deadline.is_some() {
                    Method::RunUntil
                } else {
                    Method::Run
                };
                let mut expected = vec![automation_call(
                    method,
                    ScriptId::TerminalClose,
                    &[TTY, WINDOW],
                    deadline,
                )];
                if replies.len() >= 2 {
                    expected.push(automation_call(
                        method,
                        ScriptId::TerminalWait,
                        &[WINDOW, "20", replies[0]],
                        deadline,
                    ));
                }
                if replies.len() >= 3 {
                    expected.push(automation_call(
                        method,
                        ScriptId::TerminalClose,
                        &[TTY, WINDOW],
                        deadline,
                    ));
                }
                if replies.len() >= 4 {
                    expected.push(automation_call(
                        method,
                        ScriptId::TerminalWait,
                        &[WINDOW, "100", replies[2]],
                        deadline,
                    ));
                }
                assert_eq!(replay.calls(), expected);
                replay.assert_finished();
            }
        }

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

    #[test]
    fn close_deadline_lost_reply_leaves_zero_tab_window_unconfirmed_on_retry() {
        use crate::native::session::{CoreRecord, SessionState, Store, close};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        store
            .update_status(SessionState::Ready, None, None, None)
            .unwrap();
        store.write_terminal(&session()).unwrap();
        let handle = store.record(CoreRecord::Terminal).bytes().unwrap();
        let mut terminal = running(&[KILLED]);
        let mut closes = Vec::new();
        let first = close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |surface| {
                super::close_session_with(
                    surface,
                    &ScriptAutomation::new(|script, arguments| {
                        let (reply, events) = replay(script, &terminal, arguments);
                        if !events.is_empty() {
                            terminal = running(&[HIDDEN_EMPTY]);
                            closes.extend(events);
                            anyhow::bail!("automation deadline expired after close, reply lost");
                        }
                        reply.map_err(anyhow::Error::msg)
                    }),
                    None,
                )
                .map(Into::into)
            },
        );
        assert!(first.unwrap_err().to_string().contains("reply lost"));
        assert_eq!(closes, ["closed 8341"]);
        assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), handle);
        assert!(store.closed_if_present().unwrap().is_none());
        let (presence, events) = replay(VERIFY_TAB_SCRIPT, &terminal, &[TTY, WINDOW]);
        assert!(presence.unwrap_err().contains("no longer matches a tab"));
        assert!(events.is_empty());
        let retry = close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |surface| {
                super::close_session_with(
                    surface,
                    &ScriptAutomation::new(|script, arguments| {
                        let (reply, events) = replay(script, &terminal, arguments);
                        assert!(
                            events.is_empty(),
                            "a new close has no reply-based authority"
                        );
                        reply.map_err(anyhow::Error::msg)
                    }),
                    None,
                )
                .map(Into::into)
            },
        );
        assert!(
            retry
                .unwrap_err()
                .to_string()
                .contains("no longer holds its tab")
        );
        assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), handle);
        assert!(store.closed_if_present().unwrap().is_none());
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
                        crate::native::terminal::shell_quote(marker.as_os_str())
                    ),
                )
                .unwrap();
                let bridge_command = format!(
                    ". {}",
                    crate::native::terminal::shell_quote(script.as_os_str())
                );
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
        super::super::test_support::launch_fixture(
            "session-terminal-",
            std::time::Duration::from_secs(20),
            &std::env::temp_dir(),
        )
    }

    fn binding(directory: &std::path::Path, tty: &str) -> serde_json::Value {
        serde_json::json!({
            "terminal": "apple-terminal", "session_id": tty, "window_id": WINDOW,
            "managed_session_id": directory.file_name().unwrap().to_str().unwrap()
        })
    }

    fn bind(directory: &std::path::Path, binding: &serde_json::Value) {
        crate::native::session::primitives::write_json_atomic(
            &directory.join(crate::native::session::test_support::TERMINAL_HANDLE_FILE),
            binding,
        )
        .unwrap();
    }

    // The gate opens for the launcher's binding of this tty in a pending launch, and
    // for nothing else.
    #[test]
    fn terminal_app_host_starts_only_for_its_bound_tty_in_a_pending_launch() {
        use crate::native::session::primitives::write_json_atomic;
        use crate::native::session::test_support::{TERMINAL_HANDLE_FILE, TURN_CLAIM_FILE};
        use crate::native::session::{launch, update_status};
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
                super::wait_for_binding(directory, id, TTY, &mut crate::native::clock::SystemClock)
                    .is_ok(),
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
        let log = std::fs::read(shared.path().join(crate::native::session::launch::LOG)).unwrap();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        refused(shared.path(), "not private");

        let private = launch_fixture();
        let link = root.path().join("session-link");
        std::os::unix::fs::symlink(private.path(), &link).unwrap();
        refused(&link, "refusing non-directory Terminal.app launch path");

        let unnamed = root.path().join("not-a-session");
        std::fs::create_dir(&unnamed).unwrap();
        crate::native::session::primitives::set_private_directory_permissions(&unnamed).unwrap();
        refused(&unnamed, "invalid Agent Bridge session id");

        refused(
            &root.path().join("session-missing"),
            "no such Terminal.app launch directory",
        );
        assert_eq!(
            std::fs::read(shared.path().join(crate::native::session::launch::LOG)).unwrap(),
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
            format!(
                ". {}",
                crate::native::terminal::shell_quote(path.as_os_str())
            )
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
        crate::native::session::primitives::write_json_atomic(
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
            crate::native::terminal::shell_quote(std::env::current_exe().unwrap().as_os_str())
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
                _ => crate::native::session::update_status(
                    directory,
                    SessionState::Closed,
                    None,
                    None,
                )
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
        let log =
            std::fs::read_to_string(directory.join(crate::native::session::launch::LOG)).unwrap();
        let expected_host_log = match outcome {
            "bound" => format!("terminal_host_released tty={tty}\n"),
            "closed" => "terminal_host_refused: Terminal.app launch was cancelled or timed out before its surface was bound\n".into(),
            "another tty" => "terminal_host_refused: Terminal.app surface binding does not name the tty of this launch host\n".into(),
            "another standard input" => "terminal_host_refused: Terminal.app launch host is not attached to the tty of its standard input\n".into(),
            _ => unreachable!(),
        };
        assert_eq!(
            super::super::test_support::host_log_messages(directory, "terminal_host_"),
            [expected_host_log]
        );

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

#[cfg(test)]
mod screen_replay_tests {
    #[test]
    fn terminal_guarded_answer_replays_matching_and_changed_screens() {
        super::applescript::replay::assert_guarded_replay(
            super::SCREEN_SCRIPT,
            "w, wantedTty",
            "w & \":\" & wantedTty",
            "\"window-id\", \"tty-id\", \"tab-id\"",
            "window-id:tty-id",
            "tab-id",
        );
    }
}
