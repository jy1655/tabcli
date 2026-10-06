use crate::native::session::SessionState;
use crate::native::session::{CoreRecord, Reader, RecordStore};
// Native Ghostty scripting (installed dictionary; pinned upstream v1.3.1).
// Creation runs only the launch host in a clean shell, never Bridge/provider input and
// never a typed shell line: a key typed into the new surface must not edit the launch.
// The shell sources the launch command once the host has read its own launch from the
// bound terminal (`run_host`). An official returned
// tab plus exclusion from the pre-snapshot and an exact post-snapshot prove ownership.
// Surface UUID metadata alone is insufficient: empty input must reach a live model
// before binding. Keep the new selected tab until then; guarded restoration never
// overrides another selection. No activate/focus command is sent to Ghostty: it takes
// the foreground by itself for every surface it creates, and FOREGROUND_SCRIPT gives it
// back to the application that had it, best effort. Real keyboard routing and the model
// remain a manual runtime gate, not a version promise (upstream #12730).
use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    applescript,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
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
// Ghostty activates itself for every tab and window it creates (v1.3.1,
// TerminalController.newTab and .newWindow) and its dictionary offers no creation in the
// background, so the application that was in front gets the foreground back afterwards.
// The script asks AppKit and sends that application no Apple Event: an Automation consent
// for every application the user may work in is not acceptable.
// Without arguments: pid, bundle identifier and start time of the application in front,
// "-" when that is Ghostty or has no bundle identifier. With the window and tab that
// Ghostty must show and such an identity: activates that application only while Ghostty is
// in front with exactly that selection and the pid is still that process. Option 2 is
// NSApplicationActivateIgnoringOtherApps, which macOS 14 and later ignore.
// The process is named by its pid and the kernel's start time (`ps`, one second), not by
// AppKit's launch date: a process started without LaunchServices, as a private WezTerm
// GUI is, has none. The start time is read while the application object is held, and
// the pid is resolved once more afterwards: an equal object proves that the pid was this
// process's the whole time. The object that is activated is the one that was checked,
// so a pid that changes hands later cannot receive the foreground.
// Delete this when Ghostty can create a tab or window without activating itself.
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
// What the launcher writes to the bound terminal, and only there, once the surface is
// bound: `agent-bridge-ghostty-host:<claim token>:<length>:<session directory>`.
const HOST_FRAME: &str = "agent-bridge-ghostty-host:";
// A launch receipt lives for at most 30 s (`launch::begin`). The host cannot read it
// before the frame has named its session, so it waits that long for the frame.
const HOST_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(30)
};
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
// The creation command. Ghostty runs it through `bash -c "exec -l <command>"`
// (Exec.zig, v1.3.1): it is a program with arguments, not a line that a key can edit.
// `-f` reads no startup file, as before. `-i` keeps job control, so the wrapper is a
// foreground job of this shell exactly as when the launch line was typed there. The
// host prints the script to source only after it has proven its terminal and launch;
// when it fails the shell ends and nothing was run.
fn bootstrap(executable: &OsStr) -> Result<String> {
    crate::native::validate_shell_command_component(executable, "Agent Bridge executable")?;
    Ok(format!(
        "/bin/zsh -f -i -c {}",
        crate::native::shell_quote(OsStr::new(&format!(
            "start=$({} native-ghostty-host) || exit; . \"$start\"",
            crate::native::shell_quote(executable)
        )))
    ))
}
fn create_with_runner(
    runner: &mut dyn Runner,
    force_new_window: bool,
    deadline: Instant,
) -> Result<TerminalSession> {
    runner.capable()?;
    let command = bootstrap(
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
// One write to the terminal of the handle, and no Enter: what arrives is data for the
// host, which no shell reads.
fn start_with_runner(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    frame: &str,
    deadline: Instant,
) -> Result<()> {
    let (tab, window) = ownership_proof(session)?;
    let response = runner.run(
        QUEUE_COMMAND_SCRIPT,
        &[&session.id, tab, window, frame],
        deadline,
    )?;
    if response != "queued" {
        bail!("Ghostty command input was not confirmed: {response:?}; no paste retry");
    }
    Ok(())
}
fn host_frame(directory: &Path, token: &str) -> Result<String> {
    let directory = directory
        .to_str()
        .context("Ghostty session directory is not UTF-8")?;
    let frame = format!("{HOST_FRAME}{token}:{}:{directory}", directory.len());
    // The frame can arrive while the terminal is still in canonical mode, whose input
    // queue holds 1024 bytes.
    if frame.len() >= 1024 || !frame_token(token.as_bytes()) {
        bail!("Ghostty launch frame cannot be written to its terminal");
    }
    Ok(frame)
}
fn frame_token(token: &[u8]) -> bool {
    !token.is_empty()
        && token
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}
// After the binding: the script that the host hands to its shell, then the frame that
// releases exactly the host on the bound terminal.
fn start_bound(
    runner: &mut dyn Runner,
    session: &TerminalSession,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<()> {
    launch_command(directory, command, std::env::var_os("PATH").as_deref())?;
    let receipt = crate::native::launch::read(&Reader::open_unchecked(directory))?
        .context("Ghostty launch has no pending receipt")?;
    start_with_runner(
        runner,
        session,
        &host_frame(directory, &receipt.claim_token)?,
        deadline,
    )
}
// The process of the creation command (`native-ghostty-host`). Ghostty gives a child no
// identity of its terminal: Exec.zig (v1.3.1) sets TERM, COLORTERM, TERMINFO,
// TERM_PROGRAM, TERM_PROGRAM_VERSION, GHOSTTY_RESOURCES_DIR, GHOSTTY_BIN_DIR and the
// variables of the creator's own configuration, and none names the surface. The proof
// is the PTY itself: the launcher addresses the bound terminal by its id, and only the
// process on that terminal reads the frame. A host on any other terminal gets none and
// ends. Everything else that was typed is discarded, so no key reaches the launch
// command or the provider's first dialog. Replace the frame by an identity from the
// environment when Ghostty provides one.
pub(in crate::native) fn run_host() -> Result<()> {
    let start = {
        let _raw = RawInput::enter()?;
        let (token, directory) = read_frame(Instant::now() + HOST_WAIT)?;
        let start = verify_launch(&directory, &token)?;
        // Only this terminal's queue: keys typed while Ghostty showed the surface.
        if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot discard Ghostty startup input");
        }
        start
    };
    let mut out = std::io::stdout().lock();
    out.write_all(start.as_os_str().as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
// Every byte as it was typed: no line editing, no echo, no signal or flow-control key.
// The terminal gets its settings back before the launch command inherits it.
struct RawInput(libc::termios);
impl RawInput {
    fn enter() -> Result<Self> {
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, saved.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("the Ghostty launch host has no terminal");
        }
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !libc::IXON;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // Not TCSAFLUSH: a frame that is already queued must stay.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot read the Ghostty terminal");
        }
        Ok(Self(saved))
    }
}
impl Drop for RawInput {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}
fn read_frame(deadline: Instant) -> Result<(String, PathBuf)> {
    let mut typed = Vec::new();
    loop {
        let mut bytes = [0u8; 1024];
        let asked = Instant::now();
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len()) };
        match usize::try_from(count) {
            // The timed read returns nothing after a tenth of a second, and at once
            // only when the terminal is gone.
            Ok(0) if asked.elapsed() < Duration::from_millis(20) => {
                bail!("the Ghostty terminal closed before its launch arrived")
            }
            Ok(count) => typed.extend_from_slice(&bytes[..count]),
            Err(_) => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("cannot read the Ghostty terminal");
                }
            }
        }
        if let Some(frame) = parse_frame(&typed)? {
            return Ok(frame);
        }
        if Instant::now() >= deadline {
            bail!("no launch reached this Ghostty terminal");
        }
        // A frame is shorter than 1024 bytes; older keys cannot belong to one.
        if typed.len() > 8192 {
            typed.drain(..typed.len() - 2048);
        }
    }
}
// `None` while the frame is incomplete. What precedes it was typed by somebody else.
fn parse_frame(typed: &[u8]) -> Result<Option<(String, PathBuf)>> {
    let Some(at) = typed
        .windows(HOST_FRAME.len())
        .position(|window| window == HOST_FRAME.as_bytes())
    else {
        return Ok(None);
    };
    let mut fields = typed[at + HOST_FRAME.len()..].splitn(3, |byte| *byte == b':');
    let (Some(token), Some(length), Some(directory)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return Ok(None);
    };
    let length = std::str::from_utf8(length)
        .ok()
        .and_then(|length| length.parse::<usize>().ok())
        .filter(|length| (1..1024).contains(length));
    let (true, Some(length)) = (frame_token(token), length) else {
        bail!("malformed Ghostty launch frame");
    };
    if directory.len() < length {
        return Ok(None);
    }
    Ok(Some((
        String::from_utf8_lossy(token).into_owned(),
        PathBuf::from(OsStr::from_bytes(&directory[..length])),
    )))
}
// The frame names a launch; the records decide whether it is this one and still open:
// the launcher's private session directory, its pending receipt and claim, the atomic
// binding of a Ghostty surface to this session, and the script written for the host.
fn verify_launch(directory: &Path, token: &str) -> Result<PathBuf> {
    use crate::native::{SessionStatus, launch, unix_ms};
    let private = |path: &Path, directory: bool| -> Result<bool> {
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(metadata.is_dir() == directory
            && (directory || metadata.is_file())
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0)
    };
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid Ghostty launch directory")?;
    crate::native::require_valid_session_id(id)?;
    if !directory.is_absolute() || !private(directory, true)? {
        bail!("Ghostty launch directory is not private to the current user");
    }
    let receipt = launch::read(&Reader::open_unchecked(directory))?
        .context("missing Ghostty launch receipt")?;
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if receipt.phase != launch::Phase::Pending
        || receipt.claim_token != token
        || unix_ms() >= receipt.deadline_unix_ms
        || status.state != SessionState::Launching
        || crate::native::session::turn::current_claim_token(
            &crate::native::session::Reader::open_unchecked(directory),
        )?
        .as_deref()
            != Some(token)
    {
        bail!("Ghostty launch was cancelled, timed out or is not the one sent to this terminal");
    }
    let surface: TerminalSession = serde_json::from_str(
        &Reader::open_unchecked(directory)
            .record(CoreRecord::Terminal)
            .text()?
            .context("Ghostty surface is not bound")?,
    )
    .context("invalid Ghostty surface binding")?;
    surface.verify_managed_session(id)?;
    if surface.kind != TerminalKind::Ghostty {
        bail!("the bound surface is not a Ghostty terminal");
    }
    let start = directory.join("ghostty-start.sh");
    if !private(&start, false)? {
        bail!("Ghostty start script is not private to the current user");
    }
    Ok(start)
}
pub(super) fn start_session(
    session: &TerminalSession,
    command: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<()> {
    start_bound(&mut Installed, session, command, directory, deadline)
}
fn launch_command(
    directory: &Path,
    command: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<String> {
    // The clean shell does not load the user's startup files. Carry only the
    // invoking PATH so /usr/bin/env interpreters of resolved provider executables
    // remain available. The host hands this private script to its shell, which sources
    // it; the returned line is what that shell then runs.
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
    RecordStore::at(&script).write_private(format!("{export}{command}\n").as_bytes())?;
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
        // The application in front, as the foreground script reports it. Empty: not
        // modelled. Ghostty activates itself for every tab and window it creates.
        foreground: String,
        switch_application: bool,
        earlier_application_gone: bool,
        // "read" or "give": the call of the foreground script that fails.
        foreground_error: &'static str,
        // A private PTY in place of the surface: the process Ghostty runs for the
        // creation command, and the input that reaches it.
        pty: Option<Pty>,
        // The creation command and what the start wrote to the terminal.
        command: String,
        typed: String,
    }
    #[derive(Default)]
    struct Pty {
        master: Option<std::fs::File>,
        child: Option<std::process::Child>,
        output: Vec<u8>,
        // Typed by somebody while the surface is selected: before its process runs,
        // and directly before and after the launch arrives.
        keys_before: &'static [u8],
        keys_with: &'static [u8],
        keys_after: &'static [u8],
    }
    impl Pty {
        // Ghostty runs the configured command through `bash -c "exec -l <command>"`
        // (Exec.zig, v1.3.1). The test executable stands in for the installed one.
        fn spawn(&mut self, configured: &str) {
            use std::os::{fd::FromRawFd, unix::process::CommandExt};
            let probe = " --exact native::terminal::macos::ghostty::tests::ghostty_host_probe --nocapture --test-threads=1 3>&1 >/dev/null";
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
        fn write(&mut self, bytes: &[u8]) {
            use std::io::Write;
            self.master.as_mut().unwrap().write_all(bytes).unwrap();
        }
        // The exit status of the surface's process, `None` when `done` held first or
        // the time ran out; then the process is ended.
        fn run(&mut self, done: impl Fn() -> bool, limit: Duration) -> Option<i32> {
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
        fn shown(&self) -> String {
            String::from_utf8_lossy(&self.output).into_owned()
        }
        // The host reads every key as a byte. Before that, the terminal itself acts on
        // an interrupt key.
        fn wait_until_raw(&mut self) {
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
    // A session directory as the launcher leaves it before it opens the surface:
    // launching, claimed, with a pending launch receipt.
    fn launch_fixture() -> tempfile::TempDir {
        use crate::native::{acquire_turn_claim, launch, update_status};
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .prefix("session-ghostty-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        launch::begin(
            &crate::native::session::Store::open_unchecked(directory.path()),
            &token,
            Instant::now() + Duration::from_secs(8),
        )
        .unwrap();
        directory
    }
    // The launcher's atomic binding of the surface that the creation returned.
    fn bind_fixture(directory: &Path, handle: &TerminalSession) {
        let mut bound = handle.clone();
        bound.managed_session_id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        crate::native::write_json_atomic(
            &directory.join(crate::native::TERMINAL_HANDLE_FILE),
            &bound,
        )
        .unwrap();
    }
    // Issue #58, reproduced by hand on 2026-10-03 in iTerm2 and Terminal.app: a key typed
    // while the new surface was selected arrived before the launch command, the shell
    // ran `a. '/.../launch.sh'`, and the provider never started. Replayed on a private
    // PTY with the process Ghostty runs for the creation command; no terminal
    // application and no keyboard is involved.
    #[test]
    fn regression_startup_keys_cannot_change_the_launch_command() {
        let directory = launch_fixture();
        let marker = directory.path().join("started");
        let launch = directory.path().join("launch.sh");
        std::fs::write(
            &launch,
            format!(
                "printf started > {}; exit\n",
                crate::native::shell_quote(marker.as_os_str())
            ),
        )
        .unwrap();
        let mut f = existing();
        f.pty = Some(Pty {
            keys_before: b"a",
            ..Pty::default()
        });
        let handle = create_with_runner(&mut f, false, soon()).unwrap();
        bind_fixture(directory.path(), &handle);
        start_bound(
            &mut f,
            &handle,
            &format!(". {}", crate::native::shell_quote(launch.as_os_str())),
            directory.path(),
            soon(),
        )
        .unwrap();
        let pty = f.pty.as_mut().unwrap();
        pty.run(|| marker.exists(), Duration::from_secs(5));
        assert!(
            marker.exists(),
            "a typed key changed the launch command: {}",
            pty.shown()
        );
    }
    const PROBE: &str = "--exact native::terminal::macos::ghostty::tests::ghostty_host_probe --nocapture --test-threads=1";
    // Runs only as a process on the private PTY of the tests below, where the test
    // executable stands in for the installed one: `host` is the creation command,
    // `owner` the launch command.
    #[test]
    fn ghostty_host_probe() {
        let Ok(mode) = std::env::var("AB_GHOSTTY_PROBE") else {
            return;
        };
        if mode == "host" {
            // The test harness writes to standard output; the surface keeps the real
            // one, which the shell reads, on descriptor 3.
            assert_eq!(unsafe { libc::dup2(3, 1) }, 1);
            if let Err(error) = run_host() {
                eprintln!("{error:#}");
                std::process::exit(7);
            }
            std::process::exit(0);
        }
        let directory = PathBuf::from(std::env::var_os("AB_GHOSTTY_PROBE_DIR").unwrap());
        let id = directory.file_name().unwrap().to_str().unwrap();
        let owner = crate::native::current_native_session_owner(id).unwrap();
        let mut queued: libc::c_int = -1;
        assert_eq!(unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) }, 0);
        crate::native::write_json_atomic(
            &directory.join("owner-probe.json"),
            &serde_json::json!({
                "group": owner.process_group,
                "terminal_group": owner.terminal_process_group,
                "shell": owner.terminal_shell.map(|shell| shell.pid),
                "queued_input": queued,
            }),
        )
        .unwrap();
    }
    #[test]
    fn creation_runs_only_the_host_and_the_start_writes_one_frame_and_no_enter() {
        let directory = launch_fixture();
        let token = crate::native::launch::read(&crate::native::session::Reader::open_unchecked(
            directory.path(),
        ))
        .unwrap()
        .unwrap()
        .claim_token;
        let mut f = existing();
        let handle = create_with_runner(&mut f, false, soon()).unwrap();
        assert_eq!(
            f.command,
            bootstrap(std::env::current_exe().unwrap().as_os_str()).unwrap()
        );
        assert!(f.command.starts_with("/bin/zsh -f -i -c 'start=$("));
        assert!(
            f.command
                .ends_with(" native-ghostty-host) || exit; . \"$start\"'")
        );
        assert!(!f.command.contains(directory.path().to_str().unwrap()));
        assert!(CREATE_SURFACE_SCRIPT.contains("set command of cfg to item 2 of argv"));
        bind_fixture(directory.path(), &handle);
        let created = f.calls.len();
        start_bound(&mut f, &handle, ". '/launch.sh'", directory.path(), soon()).unwrap();
        assert_eq!(f.calls[created..], ["input"], "one write and no Enter");
        assert_eq!(
            f.typed,
            format!(
                "agent-bridge-ghostty-host:{token}:{}:{}",
                directory.path().as_os_str().len(),
                directory.path().display()
            )
        );
        assert!(
            std::fs::read_to_string(directory.path().join("ghostty-start.sh"))
                .unwrap()
                .ends_with("\n. '/launch.sh'\n")
        );
        // Without a launch receipt there is nothing to hand to a host.
        let stray = tempfile::tempdir().unwrap();
        assert!(start_bound(&mut f, &handle, ". '/launch.sh'", stray.path(), soon()).is_err());
        assert_eq!(f.calls.len(), created + 1, "a frame without a launch");
    }
    #[test]
    fn host_takes_a_frame_from_among_typed_keys_and_only_for_its_pending_bound_launch() {
        let surface = handle(&CreatedSurface {
            tab_id: "t2".into(),
            window_id: "w1".into(),
            terminal_id: "u2".into(),
        });
        let prepared = || {
            let directory = launch_fixture();
            bind_fixture(directory.path(), &surface);
            launch_command(directory.path(), ". '/launch.sh'", None).unwrap();
            let token = crate::native::launch::read(
                &crate::native::session::Reader::open_unchecked(directory.path()),
            )
            .unwrap()
            .unwrap()
            .claim_token;
            (directory, token)
        };
        let (directory, token) = prepared();
        let path = directory.path();
        // Keys before and after the frame are somebody else's; a frame that has not
        // arrived completely is waited for.
        let frame = host_frame(path, &token).unwrap();
        let mut typed = b"aaaa\nmore\x03\x15".to_vec();
        typed.extend_from_slice(frame.as_bytes());
        for cut in [10, typed.len() - frame.len() + 30, typed.len() - 1] {
            assert_eq!(parse_frame(&typed[..cut]).unwrap(), None, "{cut}");
        }
        typed.extend_from_slice(b"zz\n");
        assert_eq!(
            parse_frame(&typed).unwrap(),
            Some((token.clone(), path.to_path_buf()))
        );
        for malformed in [
            "bad token:3:/ab",
            ":3:/ab",
            "token:0:",
            "token:x:/ab",
            "token:4096:/",
        ] {
            assert!(
                parse_frame(format!("{HOST_FRAME}{malformed}").as_bytes()).is_err(),
                "{malformed}"
            );
        }
        assert_eq!(
            verify_launch(path, &token).unwrap(),
            path.join("ghostty-start.sh")
        );
        use crate::native::{TERMINAL_HANDLE_FILE, launch, update_status, write_json_atomic};
        use std::os::unix::fs::PermissionsExt;
        for case in [
            "the token of another launch",
            "the receipt of another launch",
            "not bound",
            "bound to another session",
            "bound to another terminal",
            "cancelled",
            "expired",
            "already spawned",
            "claim released",
            "directory open to others",
            "no start script",
            "start script is a link",
            "relative directory",
        ] {
            let (directory, token) = prepared();
            let path = directory.path();
            let binding = path.join(TERMINAL_HANDLE_FILE);
            let mut receipt = launch::read(&crate::native::session::Reader::open_unchecked(path))
                .unwrap()
                .unwrap();
            let mut sent = (path.to_path_buf(), token.clone());
            match case {
                "the token of another launch" => sent.1 = "1-2-3".into(),
                "the receipt of another launch" => receipt.claim_token = "1-2-3".into(),
                "not bound" => std::fs::remove_file(&binding).unwrap(),
                "bound to another session" => write_json_atomic(
                    &binding,
                    &serde_json::json!({ "terminal": "ghostty", "session_id": "u2", "tab_id": "t2", "window_id": "w1", "managed_session_id": "session-other" }),
                )
                .unwrap(),
                "bound to another terminal" => write_json_atomic(
                    &binding,
                    &serde_json::json!({ "terminal": "iterm2", "session_id": "u2", "managed_session_id": path.file_name().unwrap().to_str().unwrap() }),
                )
                .unwrap(),
                "cancelled" => update_status(path, SessionState::Closed, None, None).unwrap(),
                "expired" => receipt.deadline_unix_ms = 0,
                "already spawned" => receipt.phase = launch::Phase::Spawned,
                "claim released" => {
                    std::fs::remove_file(path.join(crate::native::TURN_CLAIM_FILE)).unwrap()
                }
                "directory open to others" => {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap()
                }
                "no start script" => std::fs::remove_file(path.join("ghostty-start.sh")).unwrap(),
                "start script is a link" => {
                    std::fs::remove_file(path.join("ghostty-start.sh")).unwrap();
                    std::os::unix::fs::symlink(path.join("launch.json"), path.join("ghostty-start.sh"))
                        .unwrap();
                }
                "relative directory" => sent.0 = PathBuf::from(path.file_name().unwrap()),
                _ => unreachable!(),
            }
            write_json_atomic(&path.join(launch::FILE), &receipt).unwrap();
            assert!(verify_launch(&sent.0, &sent.1).is_err(), "{case}");
        }
    }
    // The whole start on a private PTY: the creation command as Ghostty runs it, keys
    // typed before, with and after the launch, and the launch command as the shell job
    // that the owner record describes.
    #[test]
    fn host_on_a_private_pty_starts_only_its_bound_launch_and_no_key_reaches_it() {
        for case in ["launched", "cancelled", "never addressed"] {
            let directory = launch_fixture();
            let path = directory.path();
            let owner = path.join("owner-probe.json");
            let probe = format!(
                "AB_GHOSTTY_PROBE=owner AB_GHOSTTY_PROBE_DIR={} {} {PROBE}; exit $?",
                crate::native::shell_quote(path.as_os_str()),
                crate::native::shell_quote(std::env::current_exe().unwrap().as_os_str())
            );
            let mut f = existing();
            f.pty = Some(Pty {
                keys_before: b"aaaa\n",
                keys_with: b"more\x03\x15",
                keys_after: b"zz\n",
                ..Pty::default()
            });
            let handle = create_with_runner(&mut f, case == "cancelled", soon()).unwrap();
            f.pty.as_mut().unwrap().wait_until_raw();
            assert!(!owner.exists(), "{case}: the launch ran before the binding");
            bind_fixture(path, &handle);
            if case == "cancelled" {
                crate::native::update_status(path, SessionState::Closed, None, None).unwrap();
            }
            if case != "never addressed" {
                start_bound(&mut f, &handle, &probe, path, soon()).unwrap();
            }
            let pty = f.pty.as_mut().unwrap();
            let status = pty.run(|| false, Duration::from_secs(10));
            if case != "launched" {
                assert!(!owner.exists(), "{case}: {}", pty.shown());
                assert!(
                    matches!(status, Some(code) if code != 0),
                    "{case}: {status:?}"
                );
                continue;
            }
            assert_eq!(status, Some(0), "{}", pty.shown());
            let owner: serde_json::Value =
                serde_json::from_slice(&std::fs::read(owner).unwrap()).unwrap();
            assert_eq!(
                owner["queued_input"], 0,
                "a typed key reached the launch: {owner}"
            );
            assert_eq!(owner["group"], owner["terminal_group"], "{owner}");
            assert!(owner["shell"].is_u64(), "{owner}");
            assert!(
                !pty.shown().contains("more"),
                "typed keys were echoed: {}",
                pty.shown()
            );
        }
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
