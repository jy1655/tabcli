use super::OwnedSurfaceClose;
use super::{MacTerminalAdapter, MacosOpenMode};
#[cfg(test)]
use crate::native::session::CoreRecord;
#[cfg(test)]
use crate::native::session::SessionState;
use crate::native::session::close::budget::close_remaining;
// Prove returned-tab ownership and a live input model before binding; keep creation input out of shells.
// Restore focus only on matching selection; see docs/verification/2026-10-03-v0.1.0-focus.md.
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

pub(in crate::native::terminal) mod host;
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
  set command of cfg to item 2 of argv
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
// Restore via the verified AppKit object only while Ghostty owns the expected selection.
// No per-app Apple Events; remove when background creation exists. See docs/verification/2026-10-03-v0.1.0-focus.md.
pub(in crate::native) const FOREGROUND_SCRIPT: &str = r#"
use framework "AppKit"
use scripting additions

on bundleOf(candidate)
 if candidate is missing value then return ""
 set bundleId to candidate's bundleIdentifier()
 if bundleId is missing value then return ""
 return bundleId as text
end bundleOf

on identityOf(candidate)
 set bundleId to my bundleOf(candidate)
 if bundleId is "" then return "-"
 set pid to candidate's processIdentifier()
 set started to do shell script "set -- $(TZ=UTC LC_ALL=C /bin/ps -o lstart= -p " & pid & "); echo \"$*\""
 if started is "" then return "-"
 set again to current application's NSRunningApplication's runningApplicationWithProcessIdentifier:pid
 if again is missing value then return "-"
 if not ((again's isEqual:candidate) as boolean) then return "-"
 return (pid as text) & linefeed & bundleId & linefeed & started
end identityOf

on run argv
 set ghosttyId to id of application "Ghostty"
 set frontApplication to current application's NSWorkspace's sharedWorkspace()'s frontmostApplication()
 if (count of argv) is 0 then
  if (my bundleOf(frontApplication)) is ghosttyId then return "-"
  return my identityOf(frontApplication)
 end if
 set expectedWindowId to item 1 of argv
 set expectedTabId to item 2 of argv
 set earlierIdentity to item 3 of argv
 if (my bundleOf(frontApplication)) is not ghosttyId then return "unchanged"
 tell application "Ghostty"
  if (count of windows) is 0 then return "unchanged"
  if (id of front window) is not expectedWindowId then return "unchanged"
  if (id of selected tab of front window) is not expectedTabId then return "unchanged"
 end tell
 set earlier to current application's NSRunningApplication's runningApplicationWithProcessIdentifier:((paragraph 1 of earlierIdentity) as integer)
 if (my identityOf(earlier)) is not earlierIdentity then return "unchanged"
 if (earlier's activateWithOptions:2) then return "requested"
 return "refused"
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
pub(super) trait Runner {
    fn capable(&mut self) -> Result<()>;
    fn run(&mut self, script: &str, args: &[&str], deadline: Instant) -> Result<String>;
    fn run_send(
        &mut self,
        script: &str,
        args: &[&str],
        deadline: Instant,
    ) -> std::result::Result<String, TerminalSendFailure>;
    fn pause(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= POLL {
            bail!("Ghostty surface initialization timed out");
        }
        thread::sleep(POLL);
        Ok(())
    }
}
pub(super) struct Installed;
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
    fn run_send(
        &mut self,
        script: &str,
        args: &[&str],
        deadline: Instant,
    ) -> std::result::Result<String, TerminalSendFailure> {
        applescript::run_send_until("Ghostty", script, args, deadline)
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
// The application in front before the creation. `None` when it is Ghostty or nothing
// identifies it: then nothing gets the foreground back.
fn foreground(runner: &mut dyn Runner, deadline: Instant) -> Option<String> {
    match runner.run(FOREGROUND_SCRIPT, &[], deadline) {
        Ok(identity) => {
            let pid = identity.lines().next()?.parse::<u32>().ok()?;
            (pid > 0 && identity.lines().count() == 3).then_some(identity)
        }
        Err(error) => {
            eprintln!(
                "Ghostty launch could not read the application in front; it will not get the foreground back: {error:#}"
            );
            None
        }
    }
}
// Best effort, and no authority over the surface: whatever happens here, the launch and
// its handle are the same.
fn restore(
    runner: &mut dyn Runner,
    surface: &CreatedSurface,
    before: &Snapshot,
    earlier: Option<&str>,
    deadline: Instant,
) {
    let previous_window = before.front.as_deref().unwrap_or("-");
    let previous_tab = before
        .windows
        .get(previous_window)
        .map(String::as_str)
        .unwrap_or("-");
    // What Ghostty shows as long as nobody chose something else: the earlier tab that
    // was just selected again, otherwise the new surface. After a cold start there is no
    // earlier tab, so the new surface stays selected in Ghostty.
    let shown = match runner.run(
        RESTORE_SELECTION_SCRIPT,
        &[
            &surface.tab_id,
            &surface.window_id,
            previous_window,
            previous_tab,
        ],
        deadline,
    ) {
        Ok(reply) if reply == "restored" => [previous_window, previous_tab],
        Ok(_) => [surface.window_id.as_str(), surface.tab_id.as_str()],
        Err(error) => {
            eprintln!(
                "Ghostty owned surface is ready but guarded selection restoration was unverified: {error:#}"
            );
            return;
        }
    };
    // Last, so that no selection in Ghostty follows it. The launch frame goes to
    // the terminal by id and does not activate Ghostty.
    let Some(earlier) = earlier else { return };
    let outcome = runner
        .run(FOREGROUND_SCRIPT, &[shown[0], shown[1], earlier], deadline)
        .unwrap_or_else(|error| format!("{error:#}"));
    if outcome != "requested" && outcome != "unchanged" {
        eprintln!(
            "Ghostty took the foreground for this launch and the application that had it did not get it back: {outcome}"
        );
    }
}
fn create_with_runner(
    runner: &mut dyn Runner,
    force_new_window: bool,
    deadline: Instant,
) -> Result<TerminalSession> {
    runner.capable()?;
    let command = host::bootstrap(
        std::env::current_exe()
            .context("failed to resolve Agent Bridge executable")?
            .as_os_str(),
    )?;
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
    let earlier = foreground(runner, deadline);
    let raw = runner.run(CREATE_SURFACE_SCRIPT,&[target.unwrap_or("-"),&command],deadline)
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
    restore(runner, &surface, &before, earlier.as_deref(), deadline);
    if let Err(error) = initialized {
        if proven {
            match close_with_runner(runner, &handle(&surface), deadline) {
                Ok(_) => return Err(error),
                Err(cleanup) => {
                    let retained = handle(&surface);
                    let message = format!(
                        "{error:#}; exact Ghostty handle retained={retained:?}; cleanup failed: {cleanup:#}"
                    );
                    return Err(crate::native::terminal::RetainedLaunchSurface::new(
                        retained, message,
                    )
                    .into());
                }
            }
        }
        bail!(
            "{error:#}; Ghostty creation unverified; retained creation evidence={surface:?}; no unproven cleanup"
        );
    }
    Ok(handle(&surface))
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
fn send_file_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let (tab, window) = ownership_proof(session).map_err(TerminalSendFailure::not_sent)?;
    let path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let response = runner.run_send(
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
fn verify_surface_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    let (tab, window) = ownership_proof(session)?;
    let response = runner.run(
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
// The snapshot is taken before the handle is validated against it.
fn surface_present_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    present(
        &snapshot(runner, super::timeout_deadline(timeout)?)?,
        session,
    )
}
fn close_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    let (tab, window) = ownership_proof(session)?;
    let before = snapshot(runner, deadline)?;
    crate::native::session::close::budget::close_remaining(deadline)?;
    if !present(&before, session)? {
        return Ok(CloseOutcome::Missing);
    }
    // Exactly one native close. A failed reply can still have closed the terminal.
    crate::native::session::close::budget::close_remaining(deadline)?;
    let answer = runner.run(CLOSE_TAB_SCRIPT, &[&session.id, tab, window], deadline);
    for attempt in 0..DISCOVERY_ATTEMPTS {
        crate::native::session::close::budget::close_remaining(deadline)?;
        let after = snapshot(runner, deadline)?;
        crate::native::session::close::budget::close_remaining(deadline)?;
        if !present(&after, session)? {
            return Ok(CloseOutcome::Closed);
        }
        if attempt + 1 < DISCOVERY_ATTEMPTS {
            runner.pause(deadline)?;
        }
    }
    bail!("Ghostty owned surface remains; handle retained; close response={answer:?}")
}

// The runner uses `&mut self` and the adapter `&self`. The lock is taken for one
// operation at a time: `open_with` runs its create, start and cleanup callbacks one
// after another, so no callback holds the runner while another needs it.
pub(super) struct Adapter<R: Runner> {
    runner: std::sync::Mutex<R>,
}
pub(super) type InstalledAdapter = Adapter<Installed>;
impl Adapter<Installed> {
    /// The adapter over Ghostty's installed scripting dictionary and `osascript`.
    pub(super) const fn installed() -> Self {
        Self {
            runner: std::sync::Mutex::new(Installed),
        }
    }
}
#[cfg(test)]
pub(super) fn with_runner<R: Runner>(runner: R) -> Adapter<R> {
    Adapter {
        runner: std::sync::Mutex::new(runner),
    }
}
impl<R: Runner> Adapter<R> {
    fn runner(&self) -> std::sync::MutexGuard<'_, R> {
        self.runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl<R: Runner> MacTerminalAdapter for Adapter<R> {
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
                create_with_runner(
                    &mut *self.runner(),
                    mode == MacosOpenMode::NewWindow,
                    startup_deadline,
                )
            },
            |session, deadline| {
                host::start(&mut *self.runner(), session, command, directory, deadline)
            },
            |session, deadline| {
                close_with_runner(&mut *self.runner(), session, deadline).map(|_| ())
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
        send_file_with_runner(&mut *self.runner(), session, prompt_path, deadline)
    }
    fn verify_surface(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>> {
        verify_surface_with_runner(&mut *self.runner(), session, timeout).map(|()| None)
    }
    fn surface_present(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        surface_present_with_runner(&mut *self.runner(), session, timeout)
    }
    // Screen reads and guarded dialog input are declared unsupported in the registration.
    fn close_session_until(
        &self,
        _directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_with_runner(&mut *self.runner(), session, deadline)
    }
    fn close_owned_session(
        &self,
        _directory: &Path,
        _id: &str,
        session: &TerminalSession,
        close: OwnedSurfaceClose,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if close == OwnedSurfaceClose::LiveOwner {
            close_remaining(deadline)?;
        }
        close_with_runner(&mut *self.runner(), session, deadline)
    }
}
#[cfg(test)]
mod tests {
    use super::host::{bootstrap, start_with_runner};
    use super::*;
    use std::ffi::OsStr;
    #[derive(Default)]
    struct ScriptProbe {
        response: Option<Result<String>>,
        send_response: Option<std::result::Result<String, TerminalSendFailure>>,
        calls: Vec<(String, Vec<String>, Instant)>,
        send_calls: Vec<(String, Vec<String>, Instant)>,
    }

    impl Runner for ScriptProbe {
        fn capable(&mut self) -> Result<()> {
            panic!("unexpected capability check")
        }
        fn pause(&mut self, _: Instant) -> Result<()> {
            panic!("unexpected pause")
        }
        fn run(&mut self, script: &str, args: &[&str], deadline: Instant) -> Result<String> {
            self.calls.push((
                script.into(),
                args.iter().map(|s| (*s).into()).collect(),
                deadline,
            ));
            self.response.take().expect("unexpected general automation")
        }
        fn run_send(
            &mut self,
            script: &str,
            args: &[&str],
            deadline: Instant,
        ) -> std::result::Result<String, TerminalSendFailure> {
            self.send_calls.push((
                script.into(),
                args.iter().map(|s| (*s).into()).collect(),
                deadline,
            ));
            self.send_response
                .take()
                .expect("unexpected send automation")
        }
    }

    #[test]
    fn send_preflight_rejects_without_automation() {
        use std::os::unix::ffi::OsStrExt;
        let surface = handle(&CreatedSurface {
            tab_id: "t2".into(),
            window_id: "w1".into(),
            terminal_id: "u2".into(),
        });
        let invalid_path = Path::new(std::ffi::OsStr::from_bytes(b"/invalid\xff"));
        let mut no_tab = surface.clone();
        no_tab.tab_id = None;
        let mut no_window = surface.clone();
        no_window.window_id = None;
        let mut invalid_id = surface.clone();
        invalid_id.id = "-".into();
        for (session, path, message) in [
            (
                &no_tab,
                Path::new("/prompt"),
                "Ghostty session record is missing its tab id",
            ),
            (
                &no_window,
                Path::new("/prompt"),
                "Ghostty session record is missing its window id",
            ),
            (
                &invalid_id,
                Path::new("/prompt"),
                "Ghostty composite identity is invalid",
            ),
            (&surface, invalid_path, "prompt path is not valid UTF-8"),
            (
                &no_tab,
                invalid_path,
                "Ghostty session record is missing its tab id",
            ),
            (
                &invalid_id,
                invalid_path,
                "Ghostty composite identity is invalid",
            ),
        ] {
            let mut probe = ScriptProbe::default();
            let error =
                send_file_with_runner(&mut probe, session, path, Instant::now()).unwrap_err();
            assert_eq!(format!("{:#}", error.error()), message);
            assert!(!error.delivery_may_have_occurred());
            assert!(probe.calls.is_empty());
            assert!(probe.send_calls.is_empty());
            assert!(probe.response.is_none());
            assert!(probe.send_response.is_none());
        }
    }

    #[test]
    fn send_uses_one_classified_runner_call() {
        let deadline = Instant::now() + Duration::from_secs(10);
        let path = Path::new("/prompt with 'quotes' \"한글\"");
        for (tab, window) in [("t2", "w1"), ("-", "-")] {
            let surface = handle(&CreatedSurface {
                tab_id: tab.into(),
                window_id: window.into(),
                terminal_id: "u2".into(),
            });
            for (response, expected) in [
                (Ok("sent".into()), None),
                (
                    Ok(String::new()),
                    Some(("unexpected Ghostty send response: \"\"", true)),
                ),
                (
                    Ok("missing".into()),
                    Some(("unexpected Ghostty send response: \"missing\"", true)),
                ),
                (
                    Ok("sent\n".into()),
                    Some(("unexpected Ghostty send response: \"sent\\n\"", true)),
                ),
                (
                    Err(TerminalSendFailure::not_sent(anyhow!("not started"))),
                    Some(("not started", false)),
                ),
                (
                    Err(TerminalSendFailure::delivery_uncertain(
                        anyhow!("root failure").context("send context"),
                    )),
                    Some(("send context: root failure", true)),
                ),
            ] {
                let mut probe = ScriptProbe {
                    send_response: Some(response),
                    ..Default::default()
                };
                let result = send_file_with_runner(&mut probe, &surface, path, deadline);
                match expected {
                    None => result.unwrap(),
                    Some((message, uncertain)) => {
                        let error = result.unwrap_err();
                        assert_eq!(format!("{:#}", error.error()), message);
                        assert_eq!(error.delivery_may_have_occurred(), uncertain);
                    }
                }
                assert_eq!(
                    probe.send_calls,
                    vec![(
                        SEND_FILE_SCRIPT.into(),
                        vec![
                            "u2".into(),
                            tab.into(),
                            window.into(),
                            path.to_str().unwrap().into()
                        ],
                        deadline
                    )]
                );
                assert!(probe.calls.is_empty());
                assert!(probe.response.is_none());
                assert!(probe.send_response.is_none());
            }
        }
    }

    #[test]
    fn installed_send_keeps_prestart_and_script_failure_classification() {
        let surface = handle(&CreatedSurface {
            tab_id: "t2".into(),
            window_id: "w1".into(),
            terminal_id: "u2".into(),
        });
        let error = send_file_with_runner(
            &mut Installed,
            &surface,
            Path::new("/prompt"),
            Instant::now() - Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            format!("{:#}", error.error()),
            "Ghostty automation timed out before it started"
        );
        assert!(!error.delivery_may_have_occurred());
        let error = Installed
            .run_send(
                "error \"injected send failure\" number 7",
                &[],
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap_err();
        assert!(error.delivery_may_have_occurred());
        assert_eq!(
            format!("{:#}", error.error()),
            "Ghostty automation failed: 6:29: execution error: injected send failure (7)"
        );
    }

    #[test]
    fn verify_uses_runner_without_changing_validation_or_timeout() {
        let surface = handle(&CreatedSurface {
            tab_id: "t2".into(),
            window_id: "w1".into(),
            terminal_id: "u2".into(),
        });
        for timeout in [None, Some(Duration::from_secs(3))] {
            for (response, expected) in [
                (Ok("present".into()), None),
                (
                    Ok("missing".into()),
                    Some("Agent Bridge Ghostty owned terminal surface is missing"),
                ),
                (
                    Ok(String::new()),
                    Some("Agent Bridge Ghostty owned terminal surface is missing"),
                ),
                (
                    Ok("present\n".into()),
                    Some("Agent Bridge Ghostty owned terminal surface is missing"),
                ),
                (
                    Err(anyhow!("root failure").context("verify context")),
                    Some("verify context: root failure"),
                ),
            ] {
                let mut probe = ScriptProbe {
                    response: Some(response),
                    ..Default::default()
                };
                let before = Instant::now();
                let result = verify_surface_with_runner(&mut probe, &surface, timeout);
                let after = Instant::now();
                match expected {
                    None => result.unwrap(),
                    Some(message) => assert_eq!(format!("{:#}", result.unwrap_err()), message),
                }
                assert_eq!(probe.calls.len(), 1);
                let deadline = probe.calls[0].2;
                let duration = timeout.unwrap_or(Duration::from_secs(10));
                assert!(deadline >= before + duration && deadline <= after + duration);
                assert_eq!(
                    probe.calls,
                    vec![(
                        VERIFY_SURFACE_SCRIPT.into(),
                        vec!["u2".into(), "t2".into(), "w1".into()],
                        deadline
                    )]
                );
                assert!(probe.send_calls.is_empty());
                assert!(probe.response.is_none());
                assert!(probe.send_response.is_none());
            }
        }
        let mut invalid = surface.clone();
        invalid.tab_id = None;
        for (session, timeout, message) in [
            (
                &invalid,
                None,
                "Ghostty session record is missing its tab id",
            ),
            (
                &surface,
                Some(Duration::MAX),
                "terminal automation timeout is too large",
            ),
            (
                &invalid,
                Some(Duration::MAX),
                "Ghostty session record is missing its tab id",
            ),
        ] {
            let mut probe = ScriptProbe::default();
            let error = verify_surface_with_runner(&mut probe, session, timeout).unwrap_err();
            assert_eq!(format!("{error:#}"), message);
            assert!(probe.calls.is_empty());
            assert!(probe.send_calls.is_empty());
            assert!(probe.response.is_none());
            assert!(probe.send_response.is_none());
        }
    }

    #[test]
    fn close_deadline_during_disappearance_keeps_failure() {
        struct Slow {
            calls: usize,
            deadline: Instant,
        }
        impl Runner for Slow {
            fn run_send(
                &mut self,
                _: &str,
                _: &[&str],
                _: Instant,
            ) -> std::result::Result<String, TerminalSendFailure> {
                panic!("unexpected send automation")
            }
            fn capable(&mut self) -> Result<()> {
                Ok(())
            }
            fn run(&mut self, script: &str, _: &[&str], deadline: Instant) -> Result<String> {
                assert_eq!(deadline, self.deadline);
                self.calls += 1;
                match self.calls {
                    1 => {
                        assert_eq!(script, SNAPSHOT_SCRIPT);
                        Ok("front\tw1\nwindow\tw1\tt2\ntab\tw1\tt2\nterminal\tw1\tt2\tu2".into())
                    }
                    2 => {
                        assert_eq!(script, CLOSE_TAB_SCRIPT);
                        Ok("closed".into())
                    }
                    3 => {
                        assert_eq!(script, SNAPSHOT_SCRIPT);
                        std::thread::sleep(
                            deadline.saturating_duration_since(Instant::now())
                                + Duration::from_millis(1),
                        );
                        Ok("none".into())
                    }
                    _ => panic!("close must be sent once"),
                }
            }
        }
        let deadline = Instant::now() + Duration::from_millis(10);
        let mut runner = Slow { calls: 0, deadline };
        let surface = handle(&CreatedSurface {
            tab_id: "t2".into(),
            window_id: "w1".into(),
            terminal_id: "u2".into(),
        });
        let error = close_with_runner(&mut runner, &surface, deadline).unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        assert_eq!(runner.calls, 3);
    }
    #[derive(Default)]
    pub(super) struct Fake {
        state: Snapshot,
        pub(super) calls: Vec<String>,
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
        close_timeout: bool,
        stays: bool,
        unreadable: bool,
        pauses: usize,
        restored: bool,
        sibling_disappears: bool,
        move_on_close: bool,
        // The application in front, as the foreground script reports it. Empty: not
        // modelled. Ghostty activates itself for every tab and window it creates.
        foreground: String,
        switch_application: bool,
        earlier_application_gone: bool,
        // "read" or "give": the call of the foreground script that fails.
        foreground_error: &'static str,
        // A private PTY in place of the surface: the process Ghostty runs for the
        // creation command, and the input that reaches it.
        pub(super) pty: Option<Pty>,
        // The creation command and what the start wrote to the terminal.
        pub(super) command: String,
        pub(super) typed: String,
    }
    #[derive(Default)]
    pub(super) struct Pty {
        pub(super) master: Option<std::fs::File>,
        pub(super) child: Option<std::process::Child>,
        pub(super) output: Vec<u8>,
        // Typed by somebody while the surface is selected: before its process runs,
        // and directly before and after the launch arrives.
        pub(super) keys_before: &'static [u8],
        pub(super) keys_with: &'static [u8],
        pub(super) keys_after: &'static [u8],
    }
    impl Pty {
        // Ghostty runs the configured command through `bash -c "exec -l <command>"`
        // (Exec.zig, v1.3.1). The test executable stands in for the installed one.
        pub(super) fn spawn(&mut self, configured: &str) {
            use std::os::{fd::FromRawFd, unix::process::CommandExt};
            let probe = " --exact native::terminal::macos::ghostty::host::tests::ghostty_host_probe --nocapture --test-threads=1 3>&1 >/dev/null";
            let configured = configured.replace(" native-ghostty-host", probe);
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
                libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK);
                (
                    std::fs::File::from_raw_fd(master),
                    std::fs::File::from_raw_fd(slave),
                )
            };
            let mut command = std::process::Command::new("/bin/bash");
            command
                .args([
                    "--noprofile",
                    "--norc",
                    "-c",
                    &format!("exec -l {configured}"),
                ])
                .env("AB_GHOSTTY_PROBE", "host")
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(slave);
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1
                        || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            self.master = Some(master);
            self.write(self.keys_before);
            self.child = Some(command.spawn().unwrap());
        }
        pub(super) fn write(&mut self, bytes: &[u8]) {
            use std::io::Write;
            self.master.as_mut().unwrap().write_all(bytes).unwrap();
        }
        // The exit status of the surface's process, `None` when `done` held first or
        // the time ran out; then the process is ended.
        pub(super) fn run(&mut self, done: impl Fn() -> bool, limit: Duration) -> Option<i32> {
            use std::io::Read;
            let deadline = Instant::now() + limit;
            let child = self.child.as_mut().unwrap();
            loop {
                let mut bytes = [0; 4096];
                if let Ok(count) = self.master.as_mut().unwrap().read(&mut bytes) {
                    self.output.extend_from_slice(&bytes[..count]);
                }
                if let Some(status) = child.try_wait().unwrap() {
                    return status.code();
                }
                if done() || Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        pub(super) fn shown(&self) -> String {
            String::from_utf8_lossy(&self.output).into_owned()
        }
        // The host reads every key as a byte. Before that, the terminal itself acts on
        // an interrupt key.
        pub(super) fn wait_until_raw(&mut self) {
            use std::os::fd::AsRawFd;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
                let read = unsafe {
                    libc::tcgetattr(
                        self.master.as_ref().unwrap().as_raw_fd(),
                        settings.as_mut_ptr(),
                    )
                };
                if read == 0 && unsafe { settings.assume_init() }.c_lflag & libc::ICANON == 0 {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "host never read: {}",
                    self.shown()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    const GHOSTTY: &str = "47693\ncom.mitchellh.ghostty\nFri Oct 2 00:12:24 2026";
    const EDITOR: &str = "641\ncom.example.editor\nMon Sep 28 00:44:37 2026";
    const MAIL: &str = "702\ncom.example.mail\nMon Sep 28 00:45:00 2026";
    pub(super) fn existing() -> Fake {
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
        fn run_send(
            &mut self,
            _: &str,
            _: &[&str],
            _: Instant,
        ) -> std::result::Result<String, TerminalSendFailure> {
            panic!("unexpected send automation")
        }
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
            } else if script == CLOSE_TAB_SCRIPT {
                "close"
            } else if script.contains("frontmostApplication") {
                "foreground"
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
                    if !self.foreground.is_empty() {
                        self.foreground = GHOSTTY.into();
                    }
                    self.command = args.get(1).copied().unwrap_or_default().into();
                    if let Some(pty) = &mut self.pty {
                        // Before the creation command was an argument it was the
                        // literal of the script.
                        let literal = script
                            .split("set command of cfg to \"")
                            .nth(1)
                            .and_then(|rest| rest.split('"').next());
                        pty.spawn(args.get(1).copied().or(literal).unwrap());
                    }
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
                    if self.switch_application {
                        self.foreground = MAIL.into();
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
                // Without arguments the application in front, "-" when that is Ghostty.
                // With the selection Ghostty must still show and an earlier application:
                // that application gets the foreground only from Ghostty, only with
                // that selection, and only while it is the same process.
                "foreground" => {
                    if self.foreground_error == if args.is_empty() { "read" } else { "give" } {
                        bail!("AppKit is unavailable");
                    }
                    if args.is_empty() {
                        return Ok(
                            if self.foreground.is_empty() || self.foreground == GHOSTTY {
                                "-".into()
                            } else {
                                self.foreground.clone()
                            },
                        );
                    }
                    assert!(
                        !self.calls.contains(&"input".into()),
                        "foreground given back after the provider command"
                    );
                    if self.foreground == GHOSTTY
                        && self.state.front.as_deref() == Some(args[0])
                        && self.state.windows.get(args[0]).map(String::as_str) == Some(args[1])
                        && !self.earlier_application_gone
                    {
                        self.foreground = args[2].into();
                        Ok("requested".into())
                    } else {
                        Ok("unchanged".into())
                    }
                }
                "input" => {
                    if self.lost_input {
                        bail!("paste may have executed");
                    }
                    self.typed = args[3].into();
                    if let Some(pty) = &mut self.pty {
                        pty.write(pty.keys_with);
                        pty.write(args[3].as_bytes());
                        pty.write(pty.keys_after);
                    }
                    Ok("queued".into())
                }
                "close" => {
                    if self.close_timeout {
                        bail!("Ghostty automation timed out before it started");
                    }
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
    pub(super) fn soon() -> Instant {
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
            assert!(
                e.downcast_ref::<crate::native::terminal::RetainedLaunchSurface>()
                    .is_none()
            );
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
        assert!(
            e.downcast_ref::<crate::native::terminal::RetainedLaunchSurface>()
                .is_none()
        );
        assert_eq!(
            f.calls.iter().filter(|s| *s == "probe").count(),
            DISCOVERY_ATTEMPTS
        );
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
        assert_eq!(f.state.terminals.len(), 1);
        assert!(!f.calls.contains(&"input".into()));
    }
    // The production close decision over the real records and the fake Ghostty runner.
    fn production_close_authority(
        directory: &Path,
        id: &str,
        surface: &TerminalSession,
    ) -> Result<crate::native::terminal::close_authority::TerminalCloseAuthority> {
        let deadline = crate::native::session::close::budget::close_deadline();
        crate::native::terminal::close_authority::test_support::TestObservation::on_records(
            directory, id, surface, deadline,
        )
        .decide(id, surface, deadline)
    }

    #[test]
    fn failed_initialization_and_cleanup_preserve_recoverable_surface() {
        use crate::native::{
            launch,
            session::{Store, close},
        };
        let directory = launch_fixture();
        let store = Store::open_unchecked(directory.path());
        let mut f = existing();
        f.init_failure = true;
        f.close_timeout = true;
        let error = create_with_runner(&mut f, false, soon())
            .unwrap_err()
            .context("adapter creation failed");
        let typed = error
            .downcast_ref::<crate::native::terminal::RetainedLaunchSurface>()
            .expect("the handle must survive error context as data");
        assert_eq!(typed.surface.id, "u2");
        launch::terminal_failed(&store, &error).unwrap();
        assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
        assert_eq!(f.state.terminals.len(), 2);
        assert!(!f.calls.contains(&"input".into()));
        let retained = store
            .terminal()
            .expect("failed launch must persist its proven surface");
        assert_eq!(retained.id, "u2");
        assert_eq!(retained.tab_id.as_deref(), Some("t2"));
        assert_eq!(retained.window_id.as_deref(), Some("w1"));
        let id = directory.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(retained.managed_session_id.as_deref(), Some(id));
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert!(store.status().unwrap().error.unwrap().contains("u2"));
        // The ordinary explicit-close transaction must keep a failed close recoverable.
        assert!(
            close::close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |surface| {
                    production_close_authority(directory.path(), id, surface)?;
                    close_with_runner(&mut f, surface, soon()).map(Into::into)
                }
            )
            .is_err()
        );
        assert_eq!(store.terminal().unwrap(), retained);
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        f.close_timeout = false;
        close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |surface| {
                production_close_authority(directory.path(), id, surface)?;
                close_with_runner(&mut f, surface, soon()).map(Into::into)
            },
        )
        .unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert!(store.terminal().is_err());
        assert_eq!(f.state.terminals.len(), 1);
    }
    #[test]
    fn reverse_close_before_handoff_refuses_pending_creation() {
        use crate::native::{
            launch,
            session::{CoreRecord, Store, close},
        };
        let directory = launch_fixture();
        let store = Store::open_unchecked(directory.path());
        let before = std::fs::read(store.record(CoreRecord::Status).path()).unwrap();
        let claim = std::fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap();
        let refused = close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |_| panic!("no handle exists yet"),
        );
        assert!(
            refused.is_err(),
            "close during pending creation must be refused, got {refused:?}"
        );
        let message = format!("{:#}", refused.unwrap_err());
        assert!(
            message.contains("launcher is still creating the surface"),
            "{message}"
        );
        assert!(
            message.contains(
                &launch::read(&store)
                    .unwrap()
                    .unwrap()
                    .deadline_unix_ms
                    .to_string()
            )
        );
        assert_eq!(
            std::fs::read(store.record(CoreRecord::Status).path()).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap(),
            claim
        );
        assert!(store.closed_if_present().unwrap().is_none());
        assert!(!store.record(CoreRecord::TerminalClosed).path().exists());
        assert!(!store.record(CoreRecord::TerminalClosing).path().exists());
        let mut f = existing();
        f.init_failure = true;
        f.close_timeout = true;
        let error = create_with_runner(&mut f, false, soon()).unwrap_err();
        launch::terminal_failed(&store, &error).unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert_eq!(store.terminal().unwrap().id, "u2");
        f.close_timeout = false;
        let id = directory.path().file_name().unwrap().to_str().unwrap();
        close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |surface| {
                production_close_authority(directory.path(), id, surface)?;
                close_with_runner(&mut f, surface, soon()).map(Into::into)
            },
        )
        .unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert_eq!(f.state.terminals.len(), 1);
        assert!(store.terminal().is_err());
    }

    #[test]
    fn reverse_close_after_deadline_reports_unbound_residual_surface() {
        use crate::native::{
            launch,
            session::{CoreRecord, Store, close},
        };
        for (phase, expired, earlier) in [
            (launch::Phase::Pending, true, None),
            (launch::Phase::Spawning, false, Some("earlier close reason")),
            (launch::Phase::Spawned, false, Some("earlier close reason")),
        ] {
            let directory = launch_fixture();
            let store = Store::open_unchecked(directory.path());
            let mut pending = launch::read(&store).unwrap().unwrap();
            pending.phase = phase;
            if expired {
                pending.deadline_unix_ms = 0;
            }
            store
                .record(CoreRecord::Launch)
                .write_json(&pending)
                .unwrap();
            close::close(
                &store,
                earlier.map(str::to_owned),
                crate::native::session::close::budget::close_deadline(),
                |_| panic!("no handle exists yet"),
            )
            .unwrap();
            assert_eq!(store.status().unwrap().state, SessionState::Closed);
            let mut f = existing();
            f.init_failure = true;
            f.close_timeout = true;
            let error = create_with_runner(&mut f, false, soon()).unwrap_err();
            let reported = launch::terminal_failed(&store, &error);
            let status = store.status().unwrap();
            let has_handle = store.record(CoreRecord::Terminal).path().exists();
            assert!(
                !has_handle && status.error.as_deref().is_some_and(|e| e.contains("u2")),
                "closed launch must report an unbound residual surface: handle_written={has_handle}, status.error={:?}",
                status.error
            );
            assert_eq!(status.state, SessionState::Closed);
            if let Some(earlier) = earlier {
                assert!(status.error.as_deref().unwrap().starts_with(earlier));
            }
            let message = format!("{:#}", reported.unwrap_err());
            for expected in [
                "closed during the launch",
                "u2",
                "t2",
                "w1",
                "surface may remain",
                "not closed by Bridge",
            ] {
                assert!(message.contains(expected), "{message}");
                assert!(status.error.as_deref().unwrap().contains(expected));
            }
            assert!(
                std::fs::read_to_string(directory.path().join(launch::LOG))
                    .unwrap()
                    .contains(&message)
            );
            assert_eq!(
                store.closed_if_present().unwrap().unwrap().error,
                status.error
            );
            close::close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |_| panic!("a closed session grants no close authority"),
            )
            .unwrap();
            assert_eq!(store.status().unwrap().error, status.error);
            assert!(!store.record(CoreRecord::Terminal).path().exists());
            assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
            assert_eq!(f.state.terminals.len(), 2);
        }
    }

    #[test]
    fn lost_launch_claim_reports_residual_without_binding_or_state_change() {
        use crate::native::{
            launch,
            session::{Store, close},
        };
        for state in [SessionState::Launching, SessionState::Failed] {
            let directory = launch_fixture();
            let store = Store::open_unchecked(directory.path());
            store.record(CoreRecord::TurnClaim).remove().unwrap();
            let mut before = store.status().unwrap();
            before.state = state;
            before.error = Some("earlier error".to_owned());
            store.write_status(&before).unwrap();
            let mut f = existing();
            f.init_failure = true;
            f.close_timeout = true;
            let error = create_with_runner(&mut f, false, soon()).unwrap_err();
            let report = launch::terminal_failed(&store, &error).unwrap_err();
            assert!(format!("{report:#}").contains("surface may remain"));
            assert!(!store.record(CoreRecord::Terminal).path().exists());
            assert!(store.closed_if_present().unwrap().is_none());
            let after = store.status().unwrap();
            assert!(
                after
                    .error
                    .as_deref()
                    .unwrap()
                    .starts_with("earlier error; ")
            );
            assert!(after.error.as_deref().unwrap().contains("u2"));
            before.error = after.error.clone();
            assert_eq!(
                after.residual_surface,
                Some(crate::native::session::ResidualSurface::Unverified)
            );
            before.residual_surface = after.residual_surface;
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(&after).unwrap()
            );
            // Let the creation deadline pass before either subsequent close. The
            // missing claim already prevented the handoff from binding a handle.
            let mut pending = launch::read(&store).unwrap().unwrap();
            pending.deadline_unix_ms = 0;
            store
                .record(CoreRecord::Launch)
                .write_json(&pending)
                .unwrap();
            for _ in 0..2 {
                close::close(
                    &store,
                    None,
                    crate::native::session::close::budget::close_deadline(),
                    |_| panic!("an unbound residual grants no close authority"),
                )
                .unwrap();
                let closed = store.status().unwrap();
                assert_eq!(closed.state, SessionState::Closed);
                assert_eq!(
                    closed.error, after.error,
                    "handle-less close must keep the residual warning"
                );
                assert_eq!(
                    store.closed_if_present().unwrap().unwrap().error,
                    after.error
                );
                assert!(!store.record(CoreRecord::Terminal).path().exists());
            }
            assert_eq!(f.calls.iter().filter(|s| *s == "close").count(), 1);
        }
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
    // A session directory as the launcher leaves it before it opens the surface:
    // launching, claimed, with a pending launch receipt.
    pub(super) fn launch_fixture() -> tempfile::TempDir {
        super::super::test_support::launch_fixture(
            "session-ghostty-",
            std::time::Duration::from_secs(8),
            &std::env::temp_dir(),
        )
    }
    // The launcher's atomic binding of the surface that the creation returned.
    pub(super) fn bind_fixture(directory: &Path, handle: &TerminalSession) {
        let mut bound = handle.clone();
        bound.managed_session_id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        crate::native::session::primitives::write_json_atomic(
            &directory.join(crate::native::session::test_support::TERMINAL_HANDLE_FILE),
            &bound,
        )
        .unwrap();
    }
    // A private WezTerm GUI is started without LaunchServices and has no launch date
    // (issue #58 acceptance, 2026-10-03). It must get the foreground back, and a later
    // process that got its pid must not: pid and bundle identifier do not name a process.
    #[test]
    fn regression_a_reused_pid_never_gets_the_foreground() {
        let original = r#"{mockPid:9, mockBundle:"com.github.wez.wezterm", mockLaunched:missing value, mockStarted:"Sat Oct 3 06:31:41 2026", mockInstance:"original"}"#;
        let (identity, _) = replay_foreground(original, original, SHOWN, true, &[]);
        assert_ne!(
            identity, "-",
            "an application without a launch date is not identified"
        );
        let give = |running: &str| {
            replay_foreground(GHOSTTY_APP, running, SHOWN, true, &["w1", "t1", &identity]).1
        };
        assert_eq!(give(original), ["activated 9 with options 2"]);
        let reused = original
            .replace("06:31:41", "06:31:59")
            .replace("original", "another");
        assert_eq!(give(&reused), [""; 0], "a reused pid got the foreground");
        assert_eq!(give(""), [""; 0], "a process that ended got the foreground");
    }
    // Issue #58, measured 2026-10-03 with Ghostty 1.3.1: a new tab took the foreground
    // from Chrome and a new window took it from WezTerm, and nothing gave it back.
    #[test]
    fn regression_foreground_returns_to_the_application_that_was_in_front() {
        for case in ["tab", "window", "no earlier window", "not initialized"] {
            let mut f = if case == "no earlier window" {
                Fake::default()
            } else {
                existing()
            };
            f.foreground = EDITOR.into();
            f.init_failure = case == "not initialized";
            let created = create_with_runner(&mut f, case == "window", soon());
            assert_eq!(created.is_ok(), case != "not initialized", "{case}");
            assert_eq!(
                f.foreground, EDITOR,
                "{case}: Ghostty kept the foreground it took for the launch"
            );
            // The earlier application is read before the creation, and gets the
            // foreground after the guarded tab restoration, never the other way round.
            let position = |name: &str| f.calls.iter().position(|call| call == name);
            assert!(position("foreground") < position("create"), "{case}");
            assert_eq!(
                f.calls.iter().rposition(|call| call == "foreground"),
                f.calls
                    .iter()
                    .rposition(|call| call == "restore")
                    .map(|i| i + 1),
                "{case}: {:?}",
                f.calls
            );
            if let Ok(handle) = created {
                assert_eq!(f.restored, case != "no earlier window", "{case}");
                start_with_runner(&mut f, &handle, "provider command", soon()).unwrap();
                assert_eq!(f.foreground, EDITOR, "{case}");
                assert_eq!(f.calls.iter().filter(|s| *s == "foreground").count(), 2);
            }
        }
    }
    #[test]
    fn foreground_stays_with_a_choice_of_the_user_and_without_a_proven_earlier_application() {
        for (case, kept) in [
            ("the user works in Ghostty", GHOSTTY),
            ("the user went to another application", MAIL),
            ("the user chose another Ghostty tab", GHOSTTY),
            ("the earlier application is gone", GHOSTTY),
            ("the application in front is unreadable", GHOSTTY),
            ("the request for the foreground fails", GHOSTTY),
        ] {
            let mut f = existing();
            f.foreground = if case == "the user works in Ghostty" {
                GHOSTTY
            } else {
                EDITOR
            }
            .into();
            f.switch_application = case == "the user went to another application";
            f.change_selection = case == "the user chose another Ghostty tab";
            f.earlier_application_gone = case == "the earlier application is gone";
            f.foreground_error = match case {
                "the application in front is unreadable" => "read",
                "the request for the foreground fails" => "give",
                _ => "",
            };
            // Best effort: no outcome of it fails the launch or changes the handle.
            let handle = create_with_runner(&mut f, false, soon()).expect(case);
            assert_eq!(
                (handle.id.as_str(), handle.tab_id.as_deref()),
                ("u2", Some("t2"))
            );
            assert_eq!(f.foreground, kept, "{case}");
            let asked = f.calls.iter().filter(|s| *s == "foreground").count();
            match case {
                // Nothing to give back, or nothing that identifies it: one read only.
                "the user works in Ghostty" | "the application in front is unreadable" => {
                    assert_eq!(asked, 1, "{case}")
                }
                _ => assert_eq!(asked, 2, "{case}"),
            }
            close_with_runner(&mut f, &handle, soon()).unwrap();
        }
    }
    // The shipped script with AppKit and Ghostty replaced by records, as the Terminal.app
    // scripts are replayed: its own decisions run in `osascript`, nothing is activated and
    // nothing talks to Ghostty or to another application.
    const FOREGROUND_MOCK: &str = r#"
on mockApplication(info)
 if info is missing value then return missing value
 script mockApplicationObject
  property appInfo : info
  on processIdentifier()
   return mockPid of appInfo
  end processIdentifier
  on bundleIdentifier()
   return mockBundle of appInfo
  end bundleIdentifier
  on isEqual:other
   return (mockInstance of appInfo) is (mockInstance of appInfo of other)
  end isEqual:
  on activateWithOptions:options
   log "activated " & (mockPid of appInfo) & " with options " & options
   return mockGranted
  end activateWithOptions:
 end script
 return mockApplicationObject
end mockApplication

on mockProcess(wantedPid)
 repeat with info in mockRunningApplications
  if mockPid of info is wantedPid then return contents of info
 end repeat
 return missing value
end mockProcess

on mockRunning:wantedPid
 return my mockApplication(my mockProcess(wantedPid))
end mockRunning:

on mockStartOf(wantedPid)
 set info to my mockProcess(wantedPid)
 if info is missing value then return ""
 return mockStarted of info
end mockStartOf
"#;
    // The response and the activations that the script asked for.
    fn replay_foreground(
        front: &str,
        running: &str,
        windows: &str,
        granted: bool,
        arguments: &[&str],
    ) -> (String, Vec<String>) {
        let script = [
            ("use framework \"AppKit\"\n", ""),
            ("use scripting additions\n", ""),
            (
                "do shell script \"set -- $(TZ=UTC LC_ALL=C /bin/ps -o lstart= -p \" & pid & \"); echo \\\"$*\\\"\"",
                "my mockStartOf(pid)",
            ),
            ("id of application \"Ghostty\"", "mockGhosttyId"),
            (
                "current application's NSWorkspace's sharedWorkspace()'s frontmostApplication()",
                "my mockApplication(mockFront)",
            ),
            (
                "current application's NSRunningApplication's runningApplicationWithProcessIdentifier:",
                "my mockRunning:",
            ),
            ("tell application \"Ghostty\"", "tell me"),
            ("(count of windows)", "(count of mockWindows)"),
            (
                "(id of front window)",
                "(windowId of item 1 of mockWindows)",
            ),
            (
                "(id of selected tab of front window)",
                "(selectedTabId of item 1 of mockWindows)",
            ),
        ]
        .iter()
        .fold(FOREGROUND_SCRIPT.to_owned(), |script, (term, mock)| {
            assert!(script.contains(term), "the script lost {term:?}");
            script.replace(term, mock)
        });
        for term in [
            "application \"Ghostty\"",
            "NSWorkspace",
            "NSRunning",
            " window)",
            "do shell script",
        ] {
            assert!(!script.contains(term), "unmocked term {term:?}");
        }
        let state = format!(
            "property mockGhosttyId : \"com.mitchellh.ghostty\"\nproperty mockGranted : {granted}\nproperty mockFront : {front}\nproperty mockRunningApplications : {{{running}}}\nproperty mockWindows : {{{windows}}}\n"
        );
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("{state}{FOREGROUND_MOCK}{script}"))
            .args(arguments)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        (
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr.lines().map(str::to_owned).collect(),
        )
    }
    const GHOSTTY_APP: &str = r#"{mockPid:47693, mockBundle:"com.mitchellh.ghostty", mockLaunched:"2026-10-02 00:12:24 +0000", mockStarted:"Fri Oct 2 00:12:24 2026", mockInstance:"ghostty"}"#;
    const EDITOR_APP: &str = r#"{mockPid:641, mockBundle:"com.example.editor", mockLaunched:"2026-09-28 00:44:37 +0000", mockStarted:"Mon Sep 28 00:44:37 2026", mockInstance:"editor"}"#;
    const SHOWN: &str = r#"{windowId:"w1", selectedTabId:"t1"}"#;
    #[test]
    fn foreground_script_activates_only_the_proven_earlier_application_from_ghostty() {
        // Read: one process, or nothing when Ghostty is in front, no bundle names it,
        // or its pid is not provably its own.
        let unnamed = EDITOR_APP.replace("\"com.example.editor\"", "missing value");
        let taken = EDITOR_APP.replace("\"editor\"", "\"another\"");
        for (case, front, running, identity) in [
            ("in front", EDITOR_APP, EDITOR_APP, EDITOR),
            ("Ghostty", GHOSTTY_APP, GHOSTTY_APP, "-"),
            ("nothing", "missing value", "", "-"),
            ("no bundle", unnamed.as_str(), unnamed.as_str(), "-"),
            ("ended while read", EDITOR_APP, "", "-"),
            ("pid taken over while read", EDITOR_APP, taken.as_str(), "-"),
        ] {
            assert_eq!(
                replay_foreground(front, running, SHOWN, true, &[]),
                (identity.to_owned(), Vec::new()),
                "{case}"
            );
        }
        // Give back: only from Ghostty, only with the selection this launch left, only
        // to the same process.
        const AT: [&str; 2] = ["w1", "t1"];
        let give = |front: &str, running: &str, windows: &str, shown: [&str; 2]| {
            replay_foreground(front, running, windows, true, &[shown[0], shown[1], EDITOR])
        };
        assert_eq!(
            give(GHOSTTY_APP, EDITOR_APP, SHOWN, AT),
            (
                "requested".to_owned(),
                vec!["activated 641 with options 2".to_owned()]
            )
        );
        let again = EDITOR_APP.replace("Mon Sep 28 00:44:37", "Sat Oct 3 07:00:00");
        let other = EDITOR_APP.replace("com.example.editor", "com.example.other");
        for (case, outcome) in [
            (
                "another application",
                give(EDITOR_APP, EDITOR_APP, SHOWN, AT),
            ),
            (
                "nothing in front",
                give("missing value", EDITOR_APP, SHOWN, AT),
            ),
            (
                "another window",
                give(GHOSTTY_APP, EDITOR_APP, SHOWN, ["w2", "t1"]),
            ),
            (
                "another tab",
                give(GHOSTTY_APP, EDITOR_APP, SHOWN, ["w1", "t2"]),
            ),
            ("no window", give(GHOSTTY_APP, EDITOR_APP, "", AT)),
            ("ended", give(GHOSTTY_APP, "", SHOWN, AT)),
            ("started again", give(GHOSTTY_APP, &again, SHOWN, AT)),
            (
                "pid of another application",
                give(GHOSTTY_APP, &other, SHOWN, AT),
            ),
        ] {
            assert_eq!(outcome, ("unchanged".to_owned(), Vec::new()), "{case}");
        }
        let refused = replay_foreground(
            GHOSTTY_APP,
            EDITOR_APP,
            SHOWN,
            false,
            &[AT[0], AT[1], EDITOR],
        );
        assert_eq!(refused.0, "refused");
        // No Apple Event to the other application and no command that focuses Ghostty.
        for forbidden in [
            "System Events",
            "tell application id",
            "activate\n",
            "focus ",
        ] {
            assert!(!FOREGROUND_SCRIPT.contains(forbidden), "{forbidden}");
        }
    }
    #[test]
    fn scripts_use_shell_only_creation_and_never_activate_or_configure_provider_input() {
        assert!(
            bootstrap(OsStr::new("/bridge"))
                .unwrap()
                .starts_with("/bin/zsh -f -i -c ")
        );
        assert!(!CREATE_SURFACE_SCRIPT.contains("/bin/zsh"));
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
