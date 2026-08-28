use std::{path::Path, thread, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;

use super::{CloseOutcome, TerminalKind, TerminalSession, applescript, close_response};

pub(in crate::native) const VERSION_SCRIPT: &str = r#"
on run
    tell application "Ghostty" to return version
end run
"#;

pub(in crate::native) const CREATE_SURFACE_SCRIPT: &str = r#"
on run
    set ghosttyWasRunning to application "Ghostty" is running
    tell application "Ghostty"
        activate
        if not ghosttyWasRunning then
            set targetWindow to new window
            set targetTab to selected tab of targetWindow
        else if (count of windows) is 0 then
            set targetWindow to new window
            set targetTab to selected tab of targetWindow
        else
            set targetWindow to front window
            set targetTab to new tab in targetWindow
        end if
        return (id of targetTab) & linefeed & (id of targetWindow)
    end tell
end run
"#;

pub(in crate::native) const DISCOVER_TERMINAL_SCRIPT: &str = r#"
on run argv
    set wantedTabId to item 1 of argv
    set wantedWindowId to item 2 of argv
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error errorText number errorNumber
            if errorNumber is -10000 then return "not-ready"
            return "missing"
        end try
        set targetTab to missing value
        try
            repeat with candidateTab in tabs of targetWindow
                if id of candidateTab is wantedTabId then
                    set targetTab to candidateTab
                    exit repeat
                end if
            end repeat
        on error
            return "not-ready"
        end try
        if targetTab is missing value then return "missing"
        try
            set targetTerminal to focused terminal of targetTab
            set targetTerminalId to id of targetTerminal
        on error
            return "not-ready"
        end try
        return "ready" & linefeed & targetTerminalId
    end tell
end run
"#;

pub(in crate::native) const QUEUE_COMMAND_SCRIPT: &str = r#"
on run argv
    set wantedTerminalId to item 1 of argv
    set wantedTabId to item 2 of argv
    set wantedWindowId to item 3 of argv
    set bridgeCommand to item 4 of argv
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try
        set targetTab to missing value
        repeat with candidateTab in tabs of targetWindow
            if id of candidateTab is wantedTabId then
                set targetTab to candidateTab
                exit repeat
            end if
        end repeat
        if targetTab is missing value then return "missing"
        set targetTerminal to missing value
        repeat with candidateTerminal in terminals of targetTab
            if id of candidateTerminal is wantedTerminalId then
                set targetTerminal to candidateTerminal
                exit repeat
            end if
        end repeat
        if targetTerminal is missing value then return "missing"
        try
            input text bridgeCommand to targetTerminal
        on error errorText number errorNumber
            if errorNumber is -10000 then return "not-ready"
            error errorText number errorNumber
        end try
        return "queued"
    end tell
end run
"#;

pub(in crate::native) const PRESS_ENTER_SCRIPT: &str = r#"
on run argv
    set wantedTerminalId to item 1 of argv
    set wantedTabId to item 2 of argv
    set wantedWindowId to item 3 of argv
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try
        set targetTab to missing value
        repeat with candidateTab in tabs of targetWindow
            if id of candidateTab is wantedTabId then
                set targetTab to candidateTab
                exit repeat
            end if
        end repeat
        if targetTab is missing value then return "missing"
        set targetTerminal to missing value
        repeat with candidateTerminal in terminals of targetTab
            if id of candidateTerminal is wantedTerminalId then
                set targetTerminal to candidateTerminal
                exit repeat
            end if
        end repeat
        if targetTerminal is missing value then return "missing"
        send key "enter" to targetTerminal
        return "pressed"
    end tell
end run
"#;

pub(in crate::native) const SEND_FILE_SCRIPT: &str = r#"
on run argv
    set wantedTerminalId to item 1 of argv
    set wantedTabId to item 2 of argv
    set wantedWindowId to item 3 of argv
    set promptPath to item 4 of argv
    set promptText to read (POSIX file promptPath) as «class utf8»
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            error "Agent Bridge Ghostty window not found"
        end try
        set targetTab to missing value
        repeat with candidateTab in tabs of targetWindow
            if id of candidateTab is wantedTabId then
                set targetTab to candidateTab
                exit repeat
            end if
        end repeat
        if targetTab is missing value then error "Agent Bridge Ghostty tab not found"
        set targetTerminal to missing value
        repeat with candidateTerminal in terminals of targetTab
            if id of candidateTerminal is wantedTerminalId then
                set targetTerminal to candidateTerminal
                exit repeat
            end if
        end repeat
        if targetTerminal is missing value then error "Agent Bridge Ghostty terminal not found"
        input text promptText to targetTerminal
        send key "enter" to targetTerminal
        return "sent"
    end tell
end run
"#;

pub(in crate::native) const VERIFY_SURFACE_SCRIPT: &str = r#"
on run argv
    set wantedTerminalId to item 1 of argv
    set wantedTabId to item 2 of argv
    set wantedWindowId to item 3 of argv
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try
        set matchCount to 0
        repeat with targetTab in tabs of targetWindow
            if id of targetTab is wantedTabId then
                repeat with candidateTerminal in terminals of targetTab
                    if id of candidateTerminal is wantedTerminalId then set matchCount to matchCount + 1
                end repeat
            end if
        end repeat
        if matchCount is 1 then return "present"
        if matchCount is 0 then return "missing"
        error "Agent Bridge Ghostty ownership proof matched multiple terminal surfaces"
    end tell
end run
"#;

pub(in crate::native) const CLOSE_TAB_SCRIPT: &str = r#"
on run argv
    set wantedTerminalId to item 1 of argv
    set wantedTabId to item 2 of argv
    set wantedWindowId to item 3 of argv
    tell application "Ghostty"
        try
            set targetWindow to first window whose id is wantedWindowId
        on error
            return "missing"
        end try
        repeat with targetTab in tabs of targetWindow
            if id of targetTab is wantedTabId then
                set terminalMatches to false
                repeat with candidateTerminal in terminals of targetTab
                    if id of candidateTerminal is wantedTerminalId then
                        set terminalMatches to true
                        exit repeat
                    end if
                end repeat
                if not terminalMatches then return "missing"
                close tab targetTab
                return "closed"
            end if
        end repeat
    end tell
    return "missing"
end run
"#;

const DISCOVERY_ATTEMPTS: usize = 50;
const DISCOVERY_DELAY: Duration = Duration::from_millis(100);
const QUEUE_ATTEMPTS: usize = 50;
const QUEUE_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Eq, PartialEq)]
struct CreatedSurface {
    tab_id: String,
    window_id: String,
}

#[derive(Debug, Eq, PartialEq)]
enum Discovery {
    Ready(String),
    NotReady,
    Missing,
}

#[derive(Debug, Eq, PartialEq)]
enum QueueOutcome {
    Queued,
    NotReady,
    Missing,
}

pub(super) fn open_tab(command: &str) -> Result<TerminalSession> {
    let version = applescript::run("Ghostty", VERSION_SCRIPT, &[])
        .context("failed to read Ghostty version before creating a terminal surface")?;
    validate_ghostty_version(&version)?;

    open_tab_with_operations(
        command,
        create_surface,
        discover_terminal,
        queue_command,
        press_enter,
        cleanup_created_surface,
        thread::sleep,
    )
}

fn validate_ghostty_version(raw_version: &str) -> Result<()> {
    let version = Version::parse(raw_version)
        .with_context(|| format!("Ghostty returned an invalid version {raw_version:?}"))?;
    if version != Version::new(1, 3, 0) {
        bail!(
            "unsupported Ghostty version {version}; Agent Bridge v0.0.3 supports Ghostty 1.3.0 only because Ghostty 1.3.1 has an AppleScript-created terminal surface regression and later releases are not yet verified; use --terminal iterm2 or --terminal terminal"
        );
    }
    Ok(())
}

fn open_tab_with_operations<Create, Discover, Queue, PressEnter, Cleanup, Pause>(
    command: &str,
    mut create_surface: Create,
    mut discover_terminal: Discover,
    mut queue_command: Queue,
    mut press_enter: PressEnter,
    mut cleanup_surface: Cleanup,
    mut pause: Pause,
) -> Result<TerminalSession>
where
    Create: FnMut() -> Result<CreatedSurface>,
    Discover: FnMut(&CreatedSurface) -> Result<Discovery>,
    Queue: FnMut(&CreatedSurface, &str, &str) -> Result<QueueOutcome>,
    PressEnter: FnMut(&CreatedSurface, &str) -> Result<()>,
    Cleanup: FnMut(&CreatedSurface, &str) -> Result<()>,
    Pause: FnMut(Duration),
{
    let surface = create_surface()?;
    let mut terminal_id = None;
    for attempt in 0..DISCOVERY_ATTEMPTS {
        match discover_terminal(&surface)? {
            Discovery::Ready(id) => {
                terminal_id = Some(id);
                break;
            }
            Discovery::NotReady => {}
            Discovery::Missing => {
                bail!("created Ghostty surface disappeared before terminal discovery")
            }
        }
        if attempt + 1 < DISCOVERY_ATTEMPTS {
            pause(DISCOVERY_DELAY);
        }
    }
    let terminal_id = terminal_id.context("Ghostty terminal surface did not become ready")?;

    let mut queued = false;
    for attempt in 0..QUEUE_ATTEMPTS {
        let outcome = match queue_command(&surface, &terminal_id, command) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(failure_after_cleanup(
                    "Ghostty command queue failed",
                    error,
                    &mut cleanup_surface,
                    &surface,
                    &terminal_id,
                ));
            }
        };
        match outcome {
            QueueOutcome::Queued => {
                queued = true;
                break;
            }
            QueueOutcome::NotReady => {}
            QueueOutcome::Missing => {
                return Err(failure_after_cleanup(
                    "Ghostty command queue failed",
                    anyhow!("created Ghostty surface disappeared before command queue"),
                    &mut cleanup_surface,
                    &surface,
                    &terminal_id,
                ));
            }
        }
        if attempt + 1 < QUEUE_ATTEMPTS {
            pause(QUEUE_DELAY);
        }
    }
    if !queued {
        return Err(failure_after_cleanup(
            "Ghostty command queue failed",
            anyhow!("Ghostty terminal surface did not accept command input"),
            &mut cleanup_surface,
            &surface,
            &terminal_id,
        ));
    }

    if let Err(error) = press_enter(&surface, &terminal_id) {
        return Err(failure_after_cleanup(
            "Ghostty Enter submission failed",
            error,
            &mut cleanup_surface,
            &surface,
            &terminal_id,
        ));
    }

    Ok(TerminalSession {
        kind: TerminalKind::Ghostty,
        id: terminal_id,
        tab_id: Some(surface.tab_id),
        window_id: Some(surface.window_id),
        managed_session_id: None,
        windows_process_identity: None,
    })
}

fn failure_after_cleanup<Cleanup>(
    phase: &'static str,
    error: anyhow::Error,
    cleanup_surface: &mut Cleanup,
    surface: &CreatedSurface,
    terminal_id: &str,
) -> anyhow::Error
where
    Cleanup: FnMut(&CreatedSurface, &str) -> Result<()>,
{
    match cleanup_surface(surface, terminal_id) {
        Ok(()) => error.context(phase),
        Err(cleanup_error) => {
            anyhow!("{phase}: {error:#}; exact composite cleanup also failed: {cleanup_error:#}")
        }
    }
}

fn create_surface() -> Result<CreatedSurface> {
    let response = applescript::run("Ghostty", CREATE_SURFACE_SCRIPT, &[])?;
    let mut ids = response.lines();
    let tab_id = ids
        .next()
        .filter(|value| !value.is_empty())
        .context("Ghostty did not return its created tab id")?;
    let window_id = ids
        .next()
        .filter(|value| !value.is_empty())
        .context("Ghostty did not return its created window id")?;
    if ids.next().is_some() {
        bail!("Ghostty returned extra created surface identifiers");
    }
    Ok(CreatedSurface {
        tab_id: tab_id.to_owned(),
        window_id: window_id.to_owned(),
    })
}

fn discover_terminal(surface: &CreatedSurface) -> Result<Discovery> {
    let response = applescript::run(
        "Ghostty",
        DISCOVER_TERMINAL_SCRIPT,
        &[&surface.tab_id, &surface.window_id],
    )?;
    match response.as_str() {
        "not-ready" => Ok(Discovery::NotReady),
        "missing" => Ok(Discovery::Missing),
        _ => {
            let mut lines = response.lines();
            if lines.next() != Some("ready") {
                bail!("unexpected Ghostty terminal discovery response: {response:?}");
            }
            let terminal_id = lines
                .next()
                .filter(|value| !value.is_empty())
                .context("Ghostty returned an empty terminal id")?;
            if lines.next().is_some() {
                bail!("Ghostty returned extra terminal discovery data");
            }
            Ok(Discovery::Ready(terminal_id.to_owned()))
        }
    }
}

fn queue_command(
    surface: &CreatedSurface,
    terminal_id: &str,
    command: &str,
) -> Result<QueueOutcome> {
    let response = applescript::run(
        "Ghostty",
        QUEUE_COMMAND_SCRIPT,
        &[terminal_id, &surface.tab_id, &surface.window_id, command],
    )?;
    match response.as_str() {
        "queued" => Ok(QueueOutcome::Queued),
        "not-ready" => Ok(QueueOutcome::NotReady),
        "missing" => Ok(QueueOutcome::Missing),
        _ => bail!("unexpected Ghostty command queue response: {response:?}"),
    }
}

fn press_enter(surface: &CreatedSurface, terminal_id: &str) -> Result<()> {
    let response = applescript::run(
        "Ghostty",
        PRESS_ENTER_SCRIPT,
        &[terminal_id, &surface.tab_id, &surface.window_id],
    )?;
    if response != "pressed" {
        bail!("unexpected Ghostty Enter response: {response:?}");
    }
    Ok(())
}

fn cleanup_created_surface(surface: &CreatedSurface, terminal_id: &str) -> Result<()> {
    let response = applescript::run(
        "Ghostty",
        CLOSE_TAB_SCRIPT,
        &[terminal_id, &surface.tab_id, &surface.window_id],
    )?;
    close_response(TerminalKind::Ghostty, &response)?;
    Ok(())
}

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    let (tab_id, window_id) = ownership_proof(session)?;
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    let response = applescript::run(
        "Ghostty",
        SEND_FILE_SCRIPT,
        &[&session.id, tab_id, window_id, prompt_path],
    )?;
    if response != "sent" {
        bail!("unexpected Ghostty send response: {response:?}");
    }
    Ok(())
}

pub(super) fn verify_surface(session: &TerminalSession) -> Result<()> {
    let (tab_id, window_id) = ownership_proof(session)?;
    let response = applescript::run(
        "Ghostty",
        VERIFY_SURFACE_SCRIPT,
        &[&session.id, tab_id, window_id],
    )?;
    if response != "present" {
        bail!("Agent Bridge Ghostty owned terminal surface is missing");
    }
    Ok(())
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    let (tab_id, window_id) = ownership_proof(session)?;
    let response = applescript::run(
        "Ghostty",
        CLOSE_TAB_SCRIPT,
        &[&session.id, tab_id, window_id],
    )?;
    close_response(TerminalKind::Ghostty, &response)
}

fn ownership_proof(session: &TerminalSession) -> Result<(&str, &str)> {
    let tab_id = session
        .tab_id
        .as_deref()
        .context("Ghostty session record is missing its tab id")?;
    let window_id = session
        .window_id
        .as_deref()
        .context("Ghostty session record is missing its window id")?;
    Ok((tab_id, window_id))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use anyhow::bail;

    use super::{
        CreatedSurface, DISCOVERY_ATTEMPTS, DISCOVERY_DELAY, Discovery, QUEUE_ATTEMPTS,
        QUEUE_DELAY, QueueOutcome, open_tab_with_operations, validate_ghostty_version,
    };

    #[test]
    fn ghostty_version_gate_accepts_only_the_verified_applescript_release() {
        validate_ghostty_version("1.3.0").unwrap();

        for version in ["1.2.3", "1.3.1", "1.3.2", "2.0.0"] {
            let error = validate_ghostty_version(version).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(version));
            assert!(message.contains("Ghostty 1.3.0"));
            assert!(message.contains("--terminal iterm2"));
            assert!(message.contains("--terminal terminal"));
        }
    }

    #[test]
    fn rust_discovery_loop_is_bounded_and_fail_closed_before_terminal_uuid() {
        let create_calls = Cell::new(0);
        let discover_calls = Cell::new(0);
        let queue_calls = Cell::new(0);
        let enter_calls = Cell::new(0);
        let cleanup_calls = Cell::new(0);
        let pauses = Cell::new(0);

        let error = open_tab_with_operations(
            "bridge command",
            || {
                create_calls.set(create_calls.get() + 1);
                Ok(CreatedSurface {
                    tab_id: "tab-id".to_owned(),
                    window_id: "window-id".to_owned(),
                })
            },
            |_| {
                discover_calls.set(discover_calls.get() + 1);
                Ok(Discovery::NotReady)
            },
            |_, _, _| {
                queue_calls.set(queue_calls.get() + 1);
                Ok(QueueOutcome::Queued)
            },
            |_, _| {
                enter_calls.set(enter_calls.get() + 1);
                Ok(())
            },
            |_, _| {
                cleanup_calls.set(cleanup_calls.get() + 1);
                Ok(())
            },
            |duration| {
                assert_eq!(duration, DISCOVERY_DELAY);
                pauses.set(pauses.get() + 1);
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("did not become ready"));
        assert_eq!(create_calls.get(), 1);
        assert_eq!(discover_calls.get(), DISCOVERY_ATTEMPTS);
        assert_eq!(pauses.get(), DISCOVERY_ATTEMPTS - 1);
        assert_eq!(queue_calls.get(), 0);
        assert_eq!(enter_calls.get(), 0);
        assert_eq!(cleanup_calls.get(), 0);
    }

    #[test]
    fn rust_queue_retries_across_transactions_then_presses_enter_once() {
        let queue_calls = Cell::new(0);
        let enter_calls = Cell::new(0);
        let cleanup_calls = Cell::new(0);
        let pauses = Cell::new(0);

        let session = open_tab_with_operations(
            "bridge command",
            || {
                Ok(CreatedSurface {
                    tab_id: "tab-id".to_owned(),
                    window_id: "window-id".to_owned(),
                })
            },
            |_| Ok(Discovery::Ready("terminal-uuid".to_owned())),
            |surface, terminal_id, command| {
                let call = queue_calls.get() + 1;
                queue_calls.set(call);
                assert_eq!(surface.tab_id, "tab-id");
                assert_eq!(surface.window_id, "window-id");
                assert_eq!(terminal_id, "terminal-uuid");
                assert_eq!(command, "bridge command");
                Ok(if call < 3 {
                    QueueOutcome::NotReady
                } else {
                    QueueOutcome::Queued
                })
            },
            |surface, terminal_id| {
                enter_calls.set(enter_calls.get() + 1);
                assert_eq!(surface.tab_id, "tab-id");
                assert_eq!(surface.window_id, "window-id");
                assert_eq!(terminal_id, "terminal-uuid");
                Ok(())
            },
            |_, _| {
                cleanup_calls.set(cleanup_calls.get() + 1);
                Ok(())
            },
            |_| pauses.set(pauses.get() + 1),
        )
        .unwrap();

        assert_eq!(queue_calls.get(), 3);
        assert_eq!(pauses.get(), 2);
        assert_eq!(enter_calls.get(), 1);
        assert_eq!(cleanup_calls.get(), 0);
        assert_eq!(session.id, "terminal-uuid");
        assert_eq!(session.tab_id.as_deref(), Some("tab-id"));
        assert_eq!(session.window_id.as_deref(), Some("window-id"));
    }

    #[test]
    fn queue_bound_exhaustion_cleans_the_exact_discovered_composite() {
        let queue_calls = Cell::new(0);
        let enter_calls = Cell::new(0);
        let cleanup_calls = Cell::new(0);
        let pauses = Cell::new(0);

        let error = open_tab_with_operations(
            "bridge command",
            || {
                Ok(CreatedSurface {
                    tab_id: "reused-tab-id".to_owned(),
                    window_id: "window-id".to_owned(),
                })
            },
            |_| Ok(Discovery::Ready("terminal-uuid".to_owned())),
            |_, _, _| {
                queue_calls.set(queue_calls.get() + 1);
                Ok(QueueOutcome::NotReady)
            },
            |_, _| {
                enter_calls.set(enter_calls.get() + 1);
                Ok(())
            },
            |surface, terminal_id| {
                cleanup_calls.set(cleanup_calls.get() + 1);
                assert_eq!(surface.tab_id, "reused-tab-id");
                assert_eq!(surface.window_id, "window-id");
                assert_eq!(terminal_id, "terminal-uuid");
                Ok(())
            },
            |duration| {
                assert_eq!(duration, QUEUE_DELAY);
                pauses.set(pauses.get() + 1);
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("did not accept command input"));
        assert_eq!(queue_calls.get(), QUEUE_ATTEMPTS);
        assert_eq!(pauses.get(), QUEUE_ATTEMPTS - 1);
        assert_eq!(enter_calls.get(), 0);
        assert_eq!(cleanup_calls.get(), 1);
    }

    #[test]
    fn enter_failure_never_requeues_and_cleans_the_exact_composite() {
        let queue_calls = Cell::new(0);
        let enter_calls = Cell::new(0);
        let cleanup_calls = Cell::new(0);

        let error = open_tab_with_operations(
            "bridge command",
            || {
                Ok(CreatedSurface {
                    tab_id: "reused-tab-id".to_owned(),
                    window_id: "window-id".to_owned(),
                })
            },
            |_| Ok(Discovery::Ready("terminal-uuid".to_owned())),
            |_, _, _| {
                queue_calls.set(queue_calls.get() + 1);
                Ok(QueueOutcome::Queued)
            },
            |_, _| {
                enter_calls.set(enter_calls.get() + 1);
                bail!("enter failed")
            },
            |surface, terminal_id| {
                cleanup_calls.set(cleanup_calls.get() + 1);
                assert_eq!(surface.tab_id, "reused-tab-id");
                assert_eq!(surface.window_id, "window-id");
                assert_eq!(terminal_id, "terminal-uuid");
                Ok(())
            },
            |_| {},
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("enter failed"));
        assert_eq!(queue_calls.get(), 1);
        assert_eq!(enter_calls.get(), 1);
        assert_eq!(cleanup_calls.get(), 1);
    }
}
