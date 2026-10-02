// Native Ghostty scripting (installed dictionary; pinned upstream v1.3.1).
// Creation runs only a clean shell, never Bridge/provider input. An official returned
// tab plus exclusion from the pre-snapshot and an exact post-snapshot prove ownership.
// Surface UUID metadata alone is insufficient: empty input must reach a live model
// before binding. Keep the new selected tab until then; guarded restoration never
// overrides another selection. No activate/focus command is issued. Foreground/model
// behavior remains a manual runtime gate, not a version promise (upstream #12730).
use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    applescript,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    thread,
    time::{Duration, Instant},
};
#[cfg(test)]
pub(in crate::native) const VERSION_SCRIPT: &str = r#"on run
 tell application "Ghostty" to return version
end run"#;
pub(in crate::native) const SNAPSHOT_SCRIPT: &str = r#"
on run
 if application "Ghostty" is not running then return "none"
 tell application "Ghostty"
  set rows to {}
  set frontId to "-"
  if (count of windows) > 0 then set frontId to id of front window
  -- Ghostty defines a `tab` class, so the bare word is not AppleScript's delimiter.
  set end of rows to "front" & (ASCII character 9) & frontId
  repeat with w in windows
   set end of rows to "window" & (ASCII character 9) & (id of w) & (ASCII character 9) & (id of selected tab of w)
   repeat with t in tabs of w
    set end of rows to "tab" & (ASCII character 9) & (id of w) & (ASCII character 9) & (id of t)
    repeat with term in terminals of t
     set end of rows to "terminal" & (ASCII character 9) & (id of w) & (ASCII character 9) & (id of t) & (ASCII character 9) & (id of term)
    end repeat
   end repeat
  end repeat
  set AppleScript's text item delimiters to linefeed
  return rows as text
 end tell
end run
"#;
pub(in crate::native) const CREATE_SURFACE_SCRIPT: &str = r#"
on run argv
 set wantedWindowId to item 1 of argv
 tell application "Ghostty"
  set cfg to new surface configuration
  set command of cfg to "/bin/zsh -f"
  if wantedWindowId is "-" then
   set targetWindow to new window with configuration cfg
   set targetTab to selected tab of targetWindow
  else
   set matches to every window whose id is wantedWindowId
   if (count of matches) is not 1 then error "Ghostty tab target is no longer unique; nothing created"
   set targetWindow to item 1 of matches
   set targetTab to new tab in targetWindow with configuration cfg
  end if
  set createdWindowId to id of targetWindow
  set createdTabId to id of targetTab
  set terminalId to "-"
  if (count of terminals of targetTab) is 1 then set terminalId to id of focused terminal of targetTab
  -- Selection can initialize the view. Never take a selection the user changed.
  if (id of selected tab of targetWindow) is createdTabId then
   if (id of front window) is createdWindowId then select tab targetTab
  end if
  return createdTabId & linefeed & createdWindowId & linefeed & terminalId
 end tell
end run
"#;
pub(in crate::native) const DISCOVER_TERMINAL_SCRIPT: &str = r#"
on run argv
 set wantedTabId to item 1 of argv
 set wantedWindowId to item 2 of argv
 set expectedTerminalId to item 3 of argv
 if application "Ghostty" is not running then return "missing"
 tell application "Ghostty"
  set ws to every window whose id is wantedWindowId
  if (count of ws) is 0 then return "missing"
  if (count of ws) is not 1 then error "Ghostty window identity is ambiguous"
  set targetWindow to item 1 of ws
  set ts to every tab of targetWindow whose id is wantedTabId
  if (count of ts) is 0 then return "missing"
  if (count of ts) is not 1 then error "Ghostty tab identity is ambiguous"
  set targetTab to item 1 of ts
  if (count of terminals of targetTab) is 0 then return "not-ready"
  if (count of terminals of targetTab) is not 1 then error "Created Ghostty tab has sibling terminals"
  set targetTerminal to focused terminal of targetTab
  set terminalId to id of targetTerminal
  if expectedTerminalId is not "-" and terminalId is not expectedTerminalId then error "Ghostty terminal identity changed"
  try
   input text "" to targetTerminal
  on error errorText number errorNumber
   if errorNumber is -10000 and errorText contains "Terminal surface model is not available" then return "not-ready"
   error errorText number errorNumber
  end try
  return "ready" & linefeed & terminalId
 end tell
end run
"#;
pub(in crate::native) const RESTORE_SELECTION_SCRIPT: &str = r#"
on run argv
 set wantedTabId to item 1 of argv
 set wantedWindowId to item 2 of argv
 set previousWindowId to item 3 of argv
 set previousTabId to item 4 of argv
 if previousWindowId is "-" or previousTabId is "-" then return "unchanged"
 if application "Ghostty" is not running then return "unchanged"
 tell application "Ghostty"
  if (count of windows) is 0 then return "unchanged"
  if (id of front window) is not wantedWindowId then return "unchanged"
  if (id of selected tab of front window) is not wantedTabId then return "unchanged"
  set ws to every window whose id is previousWindowId
  if (count of ws) is not 1 then return "unchanged"
  set ts to every tab of item 1 of ws whose id is previousTabId
  if (count of ts) is not 1 then return "unchanged"
  select tab (item 1 of ts)
  return "restored"
 end tell
end run
"#;
pub(in crate::native) const VERIFY_SURFACE_SCRIPT: &str = r#"
on run argv
 if application "Ghostty" is not running then return "missing"
 tell application "Ghostty"
  set matchCount to 0
  repeat with w in windows
   if id of w is item 3 of argv then
    repeat with t in tabs of w
     if id of t is item 2 of argv then
      repeat with term in terminals of t
       if id of term is item 1 of argv then set matchCount to matchCount + 1
      end repeat
     end if
    end repeat
   end if
  end repeat
  if matchCount is 1 then return "present"
  if matchCount is 0 then return "missing"
  error "Ghostty composite ownership is ambiguous"
 end tell
end run
"#;
pub(in crate::native) const CLOSE_TAB_SCRIPT: &str = r#"
on run argv
 if application "Ghostty" is not running then return "missing"
 tell application "Ghostty"
  set ws to every window whose id is item 3 of argv
  if (count of ws) is 0 then return "missing"
  if (count of ws) is not 1 then error "Ghostty window identity is ambiguous"
  set ts to every tab of item 1 of ws whose id is item 2 of argv
  if (count of ts) is 0 then return "missing"
  if (count of ts) is not 1 then error "Ghostty tab identity is ambiguous"
  set targetTab to item 1 of ts
  set terms to every terminal of targetTab whose id is item 1 of argv
  if (count of terms) is 0 then return "missing"
  if (count of terms) is not 1 then error "Ghostty terminal identity is ambiguous"
  if (count of terminals of targetTab) is 1 then
   close tab targetTab
  else
   -- The user added splits in our tab: close only our exact terminal surface.
   close (item 1 of terms)
  end if
  return "closed"
 end tell
end run
"#;

// Retain the shared-test symbol while the manager updates obsolete tab-only
// assertions. Production cleanup always requires all three composite identities.
#[cfg(test)]
pub(in crate::native) const CLOSE_CREATED_TAB_SCRIPT: &str = CLOSE_TAB_SCRIPT;
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

const POLL: Duration = Duration::from_millis(100);
const DISCOVERY_ATTEMPTS: usize = 50;
trait Runner {
    fn capable(&mut self) -> Result<()>;
    fn run(&mut self, script: &str, args: &[&str], deadline: Instant) -> Result<String>;
    fn pause(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= POLL {
            bail!("Ghostty surface initialization timed out");
        }
        thread::sleep(POLL);
        Ok(())
    }
}
struct Installed;
impl Runner for Installed {
    fn capable(&mut self) -> Result<()> {
        let dictionary =
            std::fs::read_to_string("/Applications/Ghostty.app/Contents/Resources/Ghostty.sdef")
                .context("Ghostty native scripting dictionary is unavailable")?;
        for command in [
            "new surface configuration",
            "new tab",
            "new window",
            "select tab",
            "input text",
            "close tab",
        ] {
            if !dictionary.contains(&format!("<command name=\"{command}\"")) {
                bail!("Ghostty native scripting lacks {command:?}");
            }
        }
        Ok(())
    }
    fn run(&mut self, script: &str, args: &[&str], deadline: Instant) -> Result<String> {
        applescript::run_until("Ghostty", script, args, deadline)
    }
}
#[derive(Clone, Debug, Default)]
struct Snapshot {
    front: Option<String>,
    windows: BTreeMap<String, String>,
    tabs: BTreeSet<(String, String)>,
    terminals: BTreeSet<(String, String, String)>,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn snapshot(runner: &mut dyn Runner, deadline: Instant) -> Result<Snapshot> {
    parse_snapshot(&runner.run(SNAPSHOT_SCRIPT, &[], deadline)?)
}
fn parse_snapshot(raw: &str) -> Result<Snapshot> {
    let mut result = Snapshot::default();
    if raw == "none" {
        return Ok(result);
    }
    let mut got_front = false;
    for row in raw.lines() {
        let columns: Vec<_> = row.split('\t').collect();
        if columns.iter().skip(1).any(|id| !valid_id(id)) {
            bail!("invalid Ghostty snapshot identity");
        }
        match columns.as_slice() {
            ["front", id] if !got_front => {
                got_front = true;
                if *id != "-" {
                    result.front = Some((*id).into());
                }
            }
            ["window", w, selected] if !result.windows.contains_key(*w) => {
                result.windows.insert((*w).into(), (*selected).into());
            }
            ["tab", w, t] if result.tabs.insert(((*w).into(), (*t).into())) => {}
            ["terminal", w, t, id]
                if result
                    .terminals
                    .insert(((*w).into(), (*t).into(), (*id).into())) => {}
            _ => bail!("unreadable or duplicate Ghostty snapshot row: {row:?}"),
        }
    }
    let tab_ids: BTreeSet<_> = result.tabs.iter().map(|(_, id)| id).collect();
    let terminal_ids: BTreeSet<_> = result.terminals.iter().map(|(_, _, id)| id).collect();
    if tab_ids.len() != result.tabs.len() || terminal_ids.len() != result.terminals.len() {
        bail!("Ghostty snapshot contains duplicate global composite identities");
    }
    if !got_front
        || result
            .front
            .as_ref()
            .is_some_and(|w| !result.windows.contains_key(w))
        || result
            .tabs
            .iter()
            .any(|(w, _)| !result.windows.contains_key(w))
        || result
            .terminals
            .iter()
            .any(|(w, t, _)| !result.tabs.contains(&(w.clone(), t.clone())))
    {
        bail!("Ghostty snapshot topology is inconsistent");
    }
    Ok(result)
}
#[derive(Clone, Debug)]
struct CreatedSurface {
    tab_id: String,
    window_id: String,
    terminal_id: String,
}
fn parse_created(
    raw: &str,
    before: &Snapshot,
    wanted_window: Option<&str>,
) -> Result<CreatedSurface> {
    let fields: Vec<_> = raw.lines().collect();
    let [tab, window, terminal] = fields.as_slice() else {
        bail!("Ghostty did not return exactly its created composite IDs");
    };
    if !valid_id(tab)
        || !valid_id(window)
        || !valid_id(terminal)
        || *tab == "-"
        || *window == "-"
        || *terminal == "-"
        || before.tabs.iter().any(|(_, id)| id == tab)
        || before.terminals.iter().any(|(_, _, id)| id == terminal)
        || wanted_window.is_some_and(|expected| expected != *window)
        || (wanted_window.is_none() && before.windows.contains_key(*window))
    {
        bail!("Ghostty returned an invalid, preexisting or wrong-window created identity");
    }
    Ok(CreatedSurface {
        tab_id: (*tab).into(),
        window_id: (*window).into(),
        terminal_id: (*terminal).into(),
    })
}
fn validate_composite(surface: &CreatedSurface, state: &Snapshot, before: &Snapshot) -> Result<()> {
    let matching: Vec<_> = state
        .terminals
        .iter()
        .filter(|(w, t, _)| *w == surface.window_id && *t == surface.tab_id)
        .collect();
    let [(_, _, id)] = matching.as_slice() else {
        bail!("created Ghostty composite did not match exactly one terminal");
    };
    if id.as_str() != surface.terminal_id
        || before
            .terminals
            .iter()
            .any(|(_, _, old)| old.as_str() == id.as_str())
    {
        bail!("created Ghostty terminal UUID is wrong or preexisting");
    }
    Ok(())
}
fn handle(surface: &CreatedSurface) -> TerminalSession {
    TerminalSession {
        kind: TerminalKind::Ghostty,
        id: surface.terminal_id.clone(),
        tab_id: Some(surface.tab_id.clone()),
        window_id: Some(surface.window_id.clone()),
        managed_session_id: None,
        wezterm_mux: None,
        windows_process_identity: None,
    }
}
fn restore(
    runner: &mut dyn Runner,
    surface: &CreatedSurface,
    before: &Snapshot,
    deadline: Instant,
) {
    let previous_window = before.front.as_deref().unwrap_or("-");
    let previous_tab = before
        .windows
        .get(previous_window)
        .map(String::as_str)
        .unwrap_or("-");
    if let Err(error) = runner.run(
        RESTORE_SELECTION_SCRIPT,
        &[
            &surface.tab_id,
            &surface.window_id,
            previous_window,
            previous_tab,
        ],
        deadline,
    ) {
        eprintln!(
            "Ghostty owned surface is ready but guarded selection restoration was unverified: {error:#}"
        );
    }
}
fn create_with_runner(
    runner: &mut dyn Runner,
    force_new_window: bool,
    deadline: Instant,
) -> Result<TerminalSession> {
    runner.capable()?;
    let before = snapshot(runner, deadline)?;
    let target = if !force_new_window && before.windows.len() == 1 {
        before.windows.keys().next().map(String::as_str)
    } else {
        None
    };
    if !force_new_window && target.is_none() {
        eprintln!(
            "Ghostty tab-first: {} verified windows; no unique existing tab target; opening a new window",
            before.windows.len()
        );
    }
    let raw = runner.run(CREATE_SURFACE_SCRIPT,&[target.unwrap_or("-")],deadline)
        .with_context(||format!("Ghostty creation may have executed; prior topology={before:?}; no retry, fallback or delta cleanup"))?;
    let surface = parse_created(&raw,&before,target)
        .with_context(||format!("Ghostty creation is uncertain; reply={raw:?}; prior topology={before:?}; no retry, fallback or delta cleanup"))?;
    let mut proven = false;
    let initialized = (|| -> Result<()> {
        for attempt in 0..DISCOVERY_ATTEMPTS {
            let current = snapshot(runner, deadline)?;
            // AppKit can return the created controller before its tab-group inventory
            // refreshes. Poll only observations; never create or select a second target.
            if !current
                .tabs
                .contains(&(surface.window_id.clone(), surface.tab_id.clone()))
            {
                if attempt + 1 < DISCOVERY_ATTEMPTS {
                    runner.pause(deadline)?;
                }
                continue;
            }
            validate_composite(&surface, &current, &before)?;
            proven = true;
            let response = runner.run(
                DISCOVER_TERMINAL_SCRIPT,
                &[&surface.tab_id, &surface.window_id, &surface.terminal_id],
                deadline,
            )?;
            if response == format!("ready\n{}", surface.terminal_id) {
                return Ok(());
            }
            if response != "not-ready" {
                bail!("Ghostty initialization returned a wrong/missing composite: {response:?}");
            }
            if attempt + 1 < DISCOVERY_ATTEMPTS {
                runner.pause(deadline)?;
            }
        }
        bail!("Ghostty terminal surface model did not become ready; metadata is not readiness")
    })();
    restore(runner, &surface, &before, deadline);
    if let Err(error) = initialized {
        if proven {
            match close_with_runner(runner, &handle(&surface), deadline) {
                Ok(_) => return Err(error),
                Err(cleanup) => bail!(
                    "{error:#}; exact Ghostty handle retained={:?}; cleanup failed: {cleanup:#}",
                    handle(&surface)
                ),
            }
        }
        bail!(
            "{error:#}; Ghostty creation unverified; retained creation evidence={surface:?}; no unproven cleanup"
        );
    }
    Ok(handle(&surface))
}
#[allow(dead_code)]
pub(super) fn create_tab(deadline: Instant) -> Result<TerminalSession> {
    create_tab_with_mode(false, deadline)
}
pub(super) fn create_tab_with_mode(
    force_new_window: bool,
    deadline: Instant,
) -> Result<TerminalSession> {
    create_with_runner(&mut Installed, force_new_window, deadline)
}
fn ownership_proof(session: &TerminalSession) -> Result<(&str, &str)> {
    let tab = session
        .tab_id
        .as_deref()
        .context("Ghostty session record is missing its tab id")?;
    let window = session
        .window_id
        .as_deref()
        .context("Ghostty session record is missing its window id")?;
    if !valid_id(&session.id) || !valid_id(tab) || !valid_id(window) || session.id == "-" {
        bail!("Ghostty composite identity is invalid");
    }
    Ok((tab, window))
}
fn start_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    command: &str,
    deadline: Instant,
) -> Result<()> {
    let (tab, window) = ownership_proof(session)?;
    let response = runner.run(
        QUEUE_COMMAND_SCRIPT,
        &[&session.id, tab, window, command],
        deadline,
    )?;
    if response != "queued" {
        bail!("Ghostty command input was not confirmed: {response:?}; no paste retry");
    }
    let response = runner.run(PRESS_ENTER_SCRIPT, &[&session.id, tab, window], deadline)?;
    if response != "pressed" {
        bail!("Ghostty Enter is uncertain: {response:?}; no retry");
    }
    Ok(())
}
pub(super) fn start_session(
    session: &TerminalSession,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<()> {
    let command = launch_command(directory, command, std::env::var_os("PATH").as_deref())?;
    start_with_runner(&mut Installed, session, &command, deadline)
}
fn launch_command(
    directory: &Path,
    command: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<String> {
    // The clean shell does not load the user's startup files. Carry only the
    // invoking PATH so /usr/bin/env interpreters of resolved provider executables
    // remain available. Source a private script in that same owned shell: placing
    // PATH on its canonical input line can exceed macOS's 1023-byte payload limit.
    let script = directory.join("ghostty-start.sh");
    crate::native::validate_shell_command_component(script.as_os_str(), "Ghostty startup path")?;
    let input = format!(". {}", crate::native::shell_quote(script.as_os_str()));
    if input.len() >= 1024 {
        bail!("Ghostty startup path exceeds the canonical input line limit");
    }
    let export = match path {
        Some(path) => {
            crate::native::validate_shell_command_component(path, "Ghostty launch PATH")?;
            format!("export PATH={}\n", crate::native::shell_quote(path))
        }
        None => String::new(),
    };
    crate::native::write_private(&script, format!("{export}{command}\n").as_bytes())?;
    Ok(input)
}
pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let (tab, window) = ownership_proof(session).map_err(TerminalSendFailure::not_sent)?;
    let path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let response = applescript::run_send_until(
        "Ghostty",
        SEND_FILE_SCRIPT,
        &[&session.id, tab, window, path],
        deadline,
    )?;
    if response != "sent" {
        return Err(TerminalSendFailure::delivery_uncertain(anyhow!(
            "unexpected Ghostty send response: {response:?}"
        )));
    }
    Ok(())
}
pub(super) fn verify_surface(session: &TerminalSession, timeout: Option<Duration>) -> Result<()> {
    let (tab, window) = ownership_proof(session)?;
    let response = applescript::run_until(
        "Ghostty",
        VERIFY_SURFACE_SCRIPT,
        &[&session.id, tab, window],
        super::timeout_deadline(timeout.unwrap_or(Duration::from_secs(10)))?,
    )?;
    if response != "present" {
        bail!("Agent Bridge Ghostty owned terminal surface is missing");
    }
    Ok(())
}
fn present(state: &Snapshot, session: &TerminalSession) -> Result<bool> {
    let (tab, window) = ownership_proof(session)?;
    let mut matches = state
        .terminals
        .iter()
        .filter(|(_, _, id)| id == &session.id);
    let Some((actual_window, actual_tab, _)) = matches.next() else {
        return Ok(false);
    };
    if matches.next().is_some() || actual_window != window || actual_tab != tab {
        bail!("Ghostty terminal UUID moved or is ambiguous; original handle retained");
    }
    Ok(true)
}
pub(super) fn surface_present(session: &TerminalSession, timeout: Duration) -> Result<bool> {
    present(
        &snapshot(&mut Installed, super::timeout_deadline(timeout)?)?,
        session,
    )
}
fn close_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let (tab, window) = ownership_proof(session)?;
    if !present(&snapshot(runner, deadline)?, session)? {
        return Ok(CloseOutcome::Missing);
    }
    // Exactly one native close. A failed reply can still have closed the terminal.
    let answer = runner.run(CLOSE_TAB_SCRIPT, &[&session.id, tab, window], deadline);
    for attempt in 0..DISCOVERY_ATTEMPTS {
        let after = snapshot(runner, deadline)?;
        if !present(&after, session)? {
            return Ok(CloseOutcome::Closed);
        }
        if attempt + 1 < DISCOVERY_ATTEMPTS {
            runner.pause(deadline)?;
        }
    }
    bail!("Ghostty owned surface remains; handle retained; close response={answer:?}")
}
pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    close_session_until(session, super::timeout_deadline(Duration::from_secs(10))?)
}
pub(super) fn close_session_until(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    close_with_runner(&mut Installed, session, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bound_start_preserves_invoking_path_for_env_interpreters() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("runtime with ' quotes");
        fs::create_dir(&bin).unwrap();
        let interpreter = bin.join("bridge-test-runtime");
        fs::write(&interpreter, "#!/bin/sh\nprintf 'provider-ready\\n'\n").unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = temp.path().join("provider");
        fs::write(&provider, "#!/usr/bin/env bridge-test-runtime\n").unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let script = temp.path().join("launch.sh");
        fs::write(
            &script,
            format!(
                "{} --version",
                crate::native::shell_quote(provider.as_os_str())
            ),
        )
        .unwrap();
        let source = format!(". {}", crate::native::shell_quote(script.as_os_str()));
        let run = |command: &str| {
            Command::new("/bin/zsh")
                .args(["-f", "-c", command])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap()
        };
        assert_eq!(run(&source).status.code(), Some(127));
        let path = std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
            .unwrap();
        let output = run(&launch_command(temp.path(), &source, Some(&path)).unwrap());
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(output.stdout, b"provider-ready\n");
        assert_eq!(
            fs::metadata(temp.path().join("ghostty-start.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(run(&source).status.code(), Some(127));
    }
    #[derive(Default)]
    struct Fake {
        state: Snapshot,
        calls: Vec<String>,
        created: usize,
        chosen: String,
        reply: Option<String>,
        lost_create: bool,
        capability_failure: bool,
        init_waits: usize,
        init_failure: bool,
        change_selection: bool,
        wrong_terminal: bool,
        lost_input: bool,
        lost_close: bool,
        stays: bool,
        unreadable: bool,
        pauses: usize,
        restored: bool,
        sibling_disappears: bool,
        move_on_close: bool,
    }
    fn existing() -> Fake {
        let mut f = Fake::default();
        f.state.front = Some("w1".into());
        f.state.windows.insert("w1".into(), "t1".into());
        f.state.tabs.insert(("w1".into(), "t1".into()));
        f.state
            .terminals
            .insert(("w1".into(), "t1".into(), "u1".into()));
        f
    }
    fn rows(s: &Snapshot) -> String {
        let mut rows = vec![format!("front\t{}", s.front.as_deref().unwrap_or("-"))];
        for (w, t) in &s.windows {
            rows.push(format!("window\t{w}\t{t}"));
        }
        for (w, t) in &s.tabs {
            rows.push(format!("tab\t{w}\t{t}"));
        }
        for (w, t, id) in &s.terminals {
            rows.push(format!("terminal\t{w}\t{t}\t{id}"));
        }
        rows.join("\n")
    }
    impl Runner for Fake {
        fn capable(&mut self) -> Result<()> {
            if self.capability_failure {
                bail!("dictionary lacks native API");
            }
            Ok(())
        }
        fn pause(&mut self, _: Instant) -> Result<()> {
            self.pauses += 1;
            Ok(())
        }
        fn run(&mut self, script: &str, args: &[&str], _: Instant) -> Result<String> {
            let name = if script == SNAPSHOT_SCRIPT {
                "snapshot"
            } else if script == CREATE_SURFACE_SCRIPT {
                "create"
            } else if script == DISCOVER_TERMINAL_SCRIPT {
                "probe"
            } else if script == RESTORE_SELECTION_SCRIPT {
                "restore"
            } else if script == QUEUE_COMMAND_SCRIPT {
                "input"
            } else if script == PRESS_ENTER_SCRIPT {
                "enter"
            } else if script == CLOSE_TAB_SCRIPT {
                "close"
            } else {
                panic!("unexpected API")
            };
            self.calls.push(name.into());
            match name {
                "snapshot" => {
                    if self.unreadable && self.created > 0 {
                        bail!("unreadable topology");
                    }
                    Ok(rows(&self.state))
                }
                "create" => {
                    self.created += 1;
                    self.chosen = args[0].into();
                    if self.sibling_disappears {
                        self.state.tabs.remove(&("w1".into(), "t1".into()));
                        self.state
                            .terminals
                            .remove(&("w1".into(), "t1".into(), "u1".into()));
                    }
                    let w = if args[0] == "-" { "w2" } else { args[0] };
                    self.state.windows.insert(w.into(), "t2".into());
                    self.state.front = Some(w.into());
                    self.state.tabs.insert((w.into(), "t2".into()));
                    self.state.terminals.insert((
                        w.into(),
                        "t2".into(),
                        if self.wrong_terminal { "wrong" } else { "u2" }.into(),
                    ));
                    if self.lost_create {
                        bail!("creation response lost");
                    }
                    Ok(self.reply.clone().unwrap_or(format!("t2\n{w}\nu2")))
                }
                "probe" => {
                    assert_eq!(args[0], "t2");
                    assert_eq!(args[2], "u2");
                    assert!(!self.restored, "restored selection before model readiness");
                    if self.change_selection {
                        self.state.front = Some("w1".into());
                        self.state.windows.insert("w1".into(), "t1".into());
                    }
                    if self.init_failure || self.init_waits > 0 {
                        self.init_waits = self.init_waits.saturating_sub(1);
                        Ok("not-ready".into())
                    } else {
                        Ok("ready\nu2".into())
                    }
                }
                "restore" => {
                    if self.state.front.as_deref() == Some(args[1])
                        && self.state.windows.get(args[1]).map(String::as_str) == Some(args[0])
                        && args[2] != "-"
                    {
                        self.state.front = Some(args[2].into());
                        self.state.windows.insert(args[2].into(), args[3].into());
                        self.restored = true;
                        Ok("restored".into())
                    } else {
                        Ok("unchanged".into())
                    }
                }
                "input" => {
                    if self.lost_input {
                        bail!("paste may have executed");
                    }
                    Ok("queued".into())
                }
                "enter" => Ok("pressed".into()),
                "close" => {
                    assert_eq!(args[0], "u2");
                    if self.move_on_close {
                        self.state.terminals.remove(&(
                            args[2].into(),
                            args[1].into(),
                            args[0].into(),
                        ));
                        self.state
                            .windows
                            .insert("moved-window".into(), "moved-tab".into());
                        self.state
                            .tabs
                            .insert(("moved-window".into(), "moved-tab".into()));
                        self.state.terminals.insert((
                            "moved-window".into(),
                            "moved-tab".into(),
                            args[0].into(),
                        ));
                        bail!("surface moved before close");
                    }
                    if !self.stays {
                        self.state.terminals.remove(&(
                            args[2].into(),
                            args[1].into(),
                            args[0].into(),
                        ));
                        if !self
                            .state
                            .terminals
                            .iter()
                            .any(|(w, t, _)| w == args[2] && t == args[1])
                        {
                            self.state.tabs.remove(&(args[2].into(), args[1].into()));
                        }
                        if !self.state.tabs.iter().any(|(w, _)| w == args[2]) {
                            self.state.windows.remove(args[2]);
                            if self.state.front.as_deref() == Some(args[2]) {
                                self.state.front = None;
                            }
                        }
                    }
                    if self.lost_close {
                        bail!("post-close window vanished (-1728)");
                    }
                    Ok("closed".into())
                }
                _ => unreachable!(),
            }
        }
    }
    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    #[test]
    fn default_creates_owned_tab_and_waits_for_model_before_guarded_restore() {
        let mut f = existing();
        f.init_waits = 2;
        let h = create_with_runner(&mut f, false, soon()).unwrap();
        assert_eq!(f.chosen, "w1");
        assert_eq!(h.tab_id.as_deref(), Some("t2"));
        assert_eq!(h.id, "u2");
        assert_eq!(f.pauses, 2);
        assert!(f.restored);
        assert!(!f.calls.iter().any(|s| s == "input" || s == "enter"));
        start_with_runner(&mut f, &h, "provider command", soon()).unwrap();
        assert_eq!(f.calls.iter().filter(|s| *s == "input").count(), 1);
        close_with_runner(&mut f, &h, soon()).unwrap();
        assert!(
            f.state
                .terminals
                .contains(&("w1".into(), "t1".into(), "u1".into()))
        );
    }
    #[test]
    fn forced_window_empty_app_and_ambiguous_windows_use_new_window_before_mutation() {
        for case in 0..3 {
            let mut f = if case == 1 {
                Fake::default()
            } else {
                existing()
            };
            if case == 2 {
                f.state.windows.insert("other".into(), "other-tab".into());
                f.state.tabs.insert(("other".into(), "other-tab".into()));
            }
            let h = create_with_runner(&mut f, case == 0, soon()).unwrap();
            assert_eq!(f.chosen, "-");
            assert_eq!(h.window_id.as_deref(), Some("w2"));
            assert_eq!(f.created, 1);
            close_with_runner(&mut f, &h, soon()).unwrap();
            assert!(!f.state.windows.contains_key("w2"));
            assert_eq!(f.state.windows.contains_key("w1"), case != 1);
        }
    }
    #[test]
    fn creation_invalid_wrong_old_or_uncertain_reply_never_retries_or_closes_a_delta() {
        for reply in ["", "t1\nw1\nu1", "t2\nwrong\nu2", "t2\nw1\nu2\nextra"] {
            let mut f = existing();
            f.reply = Some(reply.into());
            assert!(create_with_runner(&mut f, false, soon()).is_err());
            assert_eq!(f.created, 1);
            assert!(!f.calls.iter().any(|s| s == "close" || s == "input"));
        }
        let mut f = existing();
        f.lost_create = true;
        let e = create_with_runner(&mut f, false, soon()).unwrap_err();
        assert!(format!("{e:#}").contains("no retry, fallback or delta cleanup"));
        assert_eq!(f.created, 1);
        assert!(!f.calls.contains(&"close".into()));
    }
    #[test]
    fn capability_or_unreadable_pre_snapshot_never_creates() {
        let mut f = existing();
        f.capability_failure = true;
        assert!(create_with_runner(&mut f, false, soon()).is_err());
        assert_eq!(f.created, 0);
        for raw in [
            "",
            "front\tw1\nwindow\tw1\ntab\tw1\tt1",
            "front\tw1\nwindow\tw1\tt1\nwindow\tw1\tt1",
        ] {
            assert!(parse_snapshot(raw).is_err());
        }
    }
    #[test]
    fn wrong_post_composite_or_unreadable_creation_gets_no_cleanup_authority() {
        for wrong in [true, false] {
            let mut f = existing();
            f.wrong_terminal = wrong;
            f.unreadable = !wrong;
            let e = create_with_runner(&mut f, false, soon()).unwrap_err();
            assert!(format!("{e:#}").contains("no unproven cleanup"));
            assert_eq!(f.created, 1);
            assert!(!f.calls.contains(&"close".into()));
        }
    }
    #[test]
    fn initialization_failure_closes_only_the_proven_new_surface() {
        let mut f = existing();
        f.init_failure = true;
        let e = create_with_runner(&mut f, false, soon()).unwrap_err();
        assert!(format!("{e:#}").contains("model did not become ready"));
        assert_eq!(
            f.calls.iter().filter(|s| *s == "probe").count(),
            DISCOVERY_ATTEMPTS
        );
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
        assert_eq!(f.state.terminals.len(), 1);
        assert!(!f.calls.contains(&"input".into()));
    }
    #[test]
    fn user_intervening_selection_is_preserved() {
        let mut f = existing();
        f.change_selection = true;
        create_with_runner(&mut f, false, soon()).unwrap();
        assert!(!f.restored);
        assert_eq!(f.state.windows["w1"], "t1");
        assert!(RESTORE_SELECTION_SCRIPT.contains("is not wantedWindowId then return"));
        assert!(RESTORE_SELECTION_SCRIPT.contains("is not wantedTabId then return"));
    }
    #[test]
    fn uncertain_input_has_no_retry_or_enter_and_bound_handle_remains_exact() {
        let mut f = existing();
        let h = create_with_runner(&mut f, false, soon()).unwrap();
        f.lost_input = true;
        assert!(start_with_runner(&mut f, &h, "provider", soon()).is_err());
        assert_eq!(f.calls.iter().filter(|s| *s == "input").count(), 1);
        assert!(!f.calls.contains(&"enter".into()));
        assert!(present(&f.state, &h).unwrap());
    }
    #[test]
    fn close_loss_is_proved_by_absence_and_user_added_split_is_preserved() {
        let mut f = existing();
        let h = create_with_runner(&mut f, false, soon()).unwrap();
        f.state
            .terminals
            .insert(("w1".into(), "t2".into(), "user-split".into()));
        f.lost_close = true;
        assert_eq!(
            close_with_runner(&mut f, &h, soon()).unwrap(),
            CloseOutcome::Closed
        );
        assert!(
            f.state
                .terminals
                .contains(&("w1".into(), "t2".into(), "user-split".into()))
        );
        assert_eq!(
            close_with_runner(&mut f, &h, soon()).unwrap(),
            CloseOutcome::Missing
        );
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
    }
    #[test]
    fn failed_cleanup_retains_exact_ids_and_never_reports_absence() {
        let mut f = existing();
        let h = create_with_runner(&mut f, false, soon()).unwrap();
        f.stays = true;
        let e = close_with_runner(&mut f, &h, soon()).unwrap_err();
        assert!(format!("{e:#}").contains("handle retained"));
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
        f.unreadable = true;
        assert!(close_with_runner(&mut f, &h, soon()).is_err());
    }
    #[test]
    fn regression_moved_uuid_is_never_missing_or_mutated() {
        for (window, tab) in [
            ("w1", "moved-tab"),
            ("moved-window", "t2"),
            ("moved-window", "moved-tab"),
        ] {
            let mut f = existing();
            let h = create_with_runner(&mut f, false, soon()).unwrap();
            f.state
                .terminals
                .remove(&("w1".into(), "t2".into(), "u2".into()));
            f.state.windows.insert(window.into(), tab.into());
            f.state.tabs.insert((window.into(), tab.into()));
            f.state
                .terminals
                .insert((window.into(), tab.into(), "u2".into()));
            f.calls.clear();
            assert!(
                present(&f.state, &h).is_err(),
                "moved UUID treated as absence"
            );
            assert!(close_with_runner(&mut f, &h, soon()).is_err());
            assert_eq!(f.calls, ["snapshot"]);
            assert_eq!(h.tab_id.as_deref(), Some("t2"));
            assert_eq!(h.window_id.as_deref(), Some("w1"));
        }
    }
    #[test]
    fn regression_move_during_close_retains_handle_instead_of_claiming_closed() {
        let mut f = existing();
        let h = create_with_runner(&mut f, false, soon()).unwrap();
        f.move_on_close = true;
        let error = close_with_runner(&mut f, &h, soon()).expect_err("moved UUID reported closed");
        assert!(format!("{error:#}").contains("moved"));
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
    }
    #[test]
    fn regression_unrelated_sibling_disappearing_does_not_reject_fresh_creation() {
        let mut f = existing();
        f.sibling_disappears = true;
        let h = create_with_runner(&mut f, false, soon())
            .expect("native fresh reply remains authority");
        assert_eq!(h.id, "u2");
        assert_eq!(f.created, 1);
        assert!(!f.calls.contains(&"close".into()));
    }
    #[test]
    fn regression_long_path_is_not_injected_into_canonical_input_line() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("launch.sh");
        std::fs::write(&script, "true\n").unwrap();
        let source = format!(". {}", crate::native::shell_quote(script.as_os_str()));
        let path = format!("/{}:/usr/bin:/bin", "p".repeat(1300));
        let command =
            launch_command(directory.path(), &source, Some(std::ffi::OsStr::new(&path))).unwrap();
        assert!(
            command.len() < 1024,
            "injected {}-byte canonical input line",
            command.len()
        );
        assert!(!command.contains(&path));
        let output = std::process::Command::new("/bin/zsh")
            .args(["-f", "-c", &format!("{command}; printf '%s' \"$PATH\"")])
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, path.as_bytes());
    }
    #[test]
    fn scripts_use_shell_only_creation_and_never_activate_or_configure_provider_input() {
        assert!(CREATE_SURFACE_SCRIPT.contains("/bin/zsh -f"));
        assert!(!CREATE_SURFACE_SCRIPT.contains("initial input"));
        assert!(
            !CREATE_SURFACE_SCRIPT
                .lines()
                .any(|s| s.trim() == "activate")
        );
        assert!(!CREATE_SURFACE_SCRIPT.contains("bridgeCommand"));
        assert!(CLOSE_TAB_SCRIPT.contains("close (item 1 of terms)"));
    }
}
