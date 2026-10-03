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

// iTerm2 3.7.3 selects every surface it creates; a new window also activates the
// application. Return selection only from the exact new session, then give the
// foreground back to the retained earlier application. A different application,
// tab or window the user chose keeps it. Native `command` creation plus the bound
// launch host below prevent keys in this brief exposure from editing the launch
// line or answering the provider's first dialog. Restore is best effort, never
// ownership authority; remove it when iTerm offers creation without selection.
pub(in crate::native) const OPEN_TAB_SCRIPT: &str = r#"
use framework "AppKit"

-- Keep the actual NSRunningApplication object in this script, not a PID to
-- resolve later. AppKit keeps it tied to that application even after it exits.
on currentForegroundApplication()
    -- NSWorkspace updates this property on the main run loop. Creation and
    -- selection are synchronous Apple Events; refresh pending workspace events
    -- before observing the foreground again (not a delay to guess readiness).
    current application's NSRunLoop's currentRunLoop()'s runUntilDate:(current application's NSDate's dateWithTimeIntervalSinceNow:0.001)
    return current application's NSWorkspace's sharedWorkspace()'s frontmostApplication()
end currentForegroundApplication

on mayRestoreSelection(earlierApplication)
    if earlierApplication is missing value then return false
    set frontApplication to my currentForegroundApplication()
    if frontApplication is missing value then return false
    if (frontApplication's bundleIdentifier() as text) is "com.googlecode.iterm2" then return true
    return (frontApplication's isEqual:earlierApplication) as boolean
end mayRestoreSelection

on restoreApplication(earlierApplication, expectedSessionId)
    if earlierApplication is missing value then return
    set frontApplication to my currentForegroundApplication()
    if earlierApplication's isTerminated() as boolean then return
    set earlierBundle to earlierApplication's bundleIdentifier()
    if earlierBundle is missing value then return
    if (earlierBundle as text) is "com.googlecode.iterm2" then return
    if frontApplication is missing value then return
    if (frontApplication's bundleIdentifier() as text) is not "com.googlecode.iterm2" then return
    tell application "iTerm2"
        if current window is missing value then return
        if (unique ID of current session of current window) is not expectedSessionId then return
    end tell
    earlierApplication's activateWithOptions:2
end restoreApplication

on run argv
    set forceNewWindow to (item 1 of argv) is "new-window"
    set bridgeCommand to item 2 of argv
    set earlierApplication to missing value
    try
        set earlierApplication to my currentForegroundApplication()
    end try
    set itermWasRunning to application "iTerm2" is running
    tell application "iTerm2"
        set keyboardWindow to missing value
        set keyboardTab to missing value
        if itermWasRunning then
            try
                set keyboardWindow to current window
                set keyboardTab to current tab of keyboardWindow
            end try
        end if
        if forceNewWindow or not itermWasRunning then
            set targetWindow to (create window with default profile command bridgeCommand)
            set targetSession to current session of targetWindow
        else if (count of windows) is 0 or current window is missing value then
            set targetWindow to (create window with default profile command bridgeCommand)
            set targetSession to current session of targetWindow
        else
            set targetWindow to current window
            tell targetWindow
                set targetTab to (create tab with default profile command bridgeCommand)
                set targetSession to current session of targetTab
            end tell
        end if
        set keyboardSessionId to unique ID of targetSession
        try
            if my mayRestoreSelection(earlierApplication) then
                if (unique ID of current session of current window) is (unique ID of targetSession) then
                    if keyboardWindow is not missing value and keyboardTab is not missing value then
                        if visible of keyboardWindow then
                            tell keyboardTab to select
                            if (id of keyboardWindow) is not (id of targetWindow) then
                                if (unique ID of current session of current window) is (unique ID of targetSession) then tell keyboardWindow to select
                            end if
                            set keyboardSessionId to unique ID of current session of keyboardTab
                        end if
                    end if
                    my restoreApplication(earlierApplication, keyboardSessionId)
                end if
            end if
        end try
        tell targetSession
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

pub(in crate::native) const VERIFY_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set matchedTty to missing value
    set matchCount to 0
    tell application "iTerm2"
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
    if application "iTerm2" is not running then return "missing"
    set wantedId to item 1 of argv
    tell application "iTerm2"
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
        crate::native::shell_quote(std::ffi::OsStr::new(&format!("{host} || exit; {command}")))
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
    use crate::native::{
        SessionStatus, TERMINAL_HANDLE_FILE, current_turn_claim_token, launch, read_json,
        read_regular_text_if_present, unix_ms,
    };
    let initial = launch::read(directory)?.context("missing iTerm2 launch receipt")?;
    let remaining = initial
        .deadline_unix_ms
        .saturating_sub(unix_ms())
        .min(30_000);
    let deadline = Instant::now() + Duration::from_millis(remaining as u64);
    loop {
        let record = launch::read(directory)?.context("missing iTerm2 launch receipt")?;
        let status: SessionStatus = read_json(&directory.join("status.json"))?;
        if Instant::now() >= deadline
            || unix_ms() >= record.deadline_unix_ms
            || record.phase != launch::Phase::Pending
            || record.claim_token != initial.claim_token
            || status.state != "launching"
            || current_turn_claim_token(directory)?.as_deref() != Some(initial.claim_token.as_str())
        {
            bail!("iTerm2 launch was cancelled or timed out before surface binding");
        }
        if let Some(text) = read_regular_text_if_present(&directory.join(TERMINAL_HANDLE_FILE))? {
            let surface: TerminalSession =
                serde_json::from_str(&text).context("invalid iTerm2 surface binding")?;
            surface.verify_managed_session(id)?;
            if surface.kind != TerminalKind::Iterm2 || surface.id != iterm_id {
                bail!("iTerm2 surface binding does not match this launch host");
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
    use super::*;

    // The live #58 failure was `a. '/.../launch.sh'`: the native write-text path
    // appends to an editable shell line. Replay that input on a private PTY; no
    // terminal app or global keyboard is used here.
    // Execute the shipped restoration handlers with only OS observations replaced.
    // No native app is addressed and no window or keyboard is touched by this replay.
    #[test]
    fn retained_application_and_exact_selection_bound_iterm_focus_restoration() {
        let handlers = OPEN_TAB_SCRIPT
            .split("on run argv")
            .next()
            .unwrap()
            .replace("use framework \"AppKit\"", "")
            .replace("current application's NSRunLoop's currentRunLoop()'s runUntilDate:(current application's NSDate's dateWithTimeIntervalSinceNow:0.001)", "")
            .replace(
                "current application's NSWorkspace's sharedWorkspace()'s frontmostApplication()",
                "my mockApplication(mockFront)",
            )
            .replace("tell application \"iTerm2\"", "tell me")
            .replace("current window is missing value", "mockMissingWindow")
            .replace(
                "(unique ID of current session of current window)",
                "mockSelectedSession",
            );
        assert!(!handlers.contains("current application's NSWorkspace"));
        assert!(!handlers.contains("application \"iTerm2\""));
        const MOCK: &str = r#"
on mockApplication(info)
 if info is missing value then return missing value
 script appObject
  property appInfo : info
  on bundleIdentifier()
   return bundle of appInfo
  end bundleIdentifier
  on isTerminated()
   return ended of appInfo
  end isTerminated
  on isEqual:other
   return (instanceId of appInfo) is (instanceId of appInfo of other)
  end isEqual:
  on activateWithOptions:options
   log "activated " & (instanceId of appInfo)
   return true
  end activateWithOptions:
 end script
 return appObject
end mockApplication
on run
 set earlier to my mockApplication(mockEarlier)
 set allowed to my mayRestoreSelection(earlier)
 my restoreApplication(earlier, "owned-or-restored")
 return allowed
end run
"#;
        const EDITOR: &str =
            r#"{bundle:"com.example.editor", ended:false, instanceId:"editor-original"}"#;
        const ITERM: &str = r#"{bundle:"com.googlecode.iterm2", ended:false, instanceId:"iterm"}"#;
        let replay = |earlier: &str, front: &str, selected: &str, missing: bool| {
            let script = format!(
                "property mockEarlier : {earlier}\nproperty mockFront : {front}\nproperty mockSelectedSession : \"{selected}\"\nproperty mockMissingWindow : {missing}\n{handlers}\n{MOCK}"
            );
            let output = std::process::Command::new("/usr/bin/osascript")
                .arg("-e")
                .arg(script)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{err}");
            (
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                err.contains("activated "),
            )
        };
        assert_eq!(
            replay(EDITOR, ITERM, "owned-or-restored", false),
            ("true".into(), true)
        );
        assert_eq!(
            replay(EDITOR, EDITOR, "owned-or-restored", false),
            ("true".into(), false)
        );
        assert_eq!(
            replay(
                EDITOR,
                &EDITOR.replace("editor-original", "another-instance"),
                "owned-or-restored",
                false
            ),
            ("false".into(), false)
        );
        for (earlier, front, selected, missing) in [
            (EDITOR.to_owned(), ITERM.to_owned(), "user-selected", false),
            (
                EDITOR.to_owned(),
                ITERM.to_owned(),
                "owned-or-restored",
                true,
            ),
            (
                EDITOR.replace("ended:false", "ended:true"),
                ITERM.to_owned(),
                "owned-or-restored",
                false,
            ),
            (
                EDITOR.replace("\"com.example.editor\"", "missing value"),
                ITERM.to_owned(),
                "owned-or-restored",
                false,
            ),
            (
                ITERM.to_owned(),
                ITERM.to_owned(),
                "owned-or-restored",
                false,
            ),
            (
                EDITOR.to_owned(),
                "missing value".to_owned(),
                "owned-or-restored",
                false,
            ),
            (
                "missing value".to_owned(),
                ITERM.to_owned(),
                "owned-or-restored",
                false,
            ),
        ] {
            assert!(!replay(&earlier, &front, selected, missing).1);
        }
        assert!(!OPEN_TAB_SCRIPT.contains("runningApplicationWithProcessIdentifier"));
    }

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
        use crate::native::*;
        let directory = tempfile::Builder::new()
            .prefix("session-iterm-")
            .tempdir()
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token.clone();
        claim.retain();
        launch::begin(
            directory.path(),
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
                "cancelled" => crate::native::update_status(directory, "closed", None, None).unwrap(),
                "expired" | "changed-claim" => {
                    let mut receipt = crate::native::launch::read(directory).unwrap().unwrap();
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
            let owner = crate::native::current_native_session_owner(id).unwrap();
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
        let directory = launch_fixture();
        let executable = std::env::current_exe().unwrap();
        let probe = format!(
            "{} --exact native::terminal::macos::iterm2::tests::iterm_owned_process_probe --nocapture --test-threads=1",
            crate::native::shell_quote(executable.as_os_str())
        );
        let bootstrap = shell_command(
            &format!("AB_ITERM_PROBE_MODE=host {probe}"),
            &format!("AB_ITERM_PROBE_MODE=owner {probe}; exit $?"),
        );
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
        let mut command = Command::new("/bin/zsh");
        command
            .args(["-f", "-c", &bootstrap])
            .env("AB_ITERM_PROBE_DIR", directory.path())
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
                    crate::native::update_status(directory.path(), "closed", None, None).unwrap();
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
