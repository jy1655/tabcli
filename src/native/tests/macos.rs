use super::support::*;
use super::*;

#[cfg(target_os = "macos")]
#[test]
fn iterm_script_keeps_dynamic_values_in_argv() {
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("review this"));
    assert!(
        terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("set bridgeCommand to item 2 of argv")
    );
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("write text"));
    assert!(
        terminal::macos::iterm2::OPEN_TAB_SCRIPT
            .contains("with default profile command bridgeCommand")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn iterm_follow_up_sends_an_explicit_carriage_return() {
    assert!(terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("set carriageReturn to return"));
    assert!(terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("newline false"));
    assert!(!terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("write text \"\""));
}

#[cfg(target_os = "macos")]
#[test]
fn iterm_new_window_returns_selection_before_starting_the_provider() {
    let script = terminal::macos::iterm2::OPEN_TAB_SCRIPT;
    // The new-window path previously remembered nothing: a real self-test
    // selected its window for all 28 seconds and brought iTerm2 to the front.
    assert!(script.contains("set keyboardWindow to my itermCurrentWindow()"));
    assert!(script.contains(
        "set newSessionId to my itermCreateWindow(bridgeCommand)\n        try\n            my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)\n        end try"
    ));
    // iTerm2 activates itself after the creation and then makes the new window
    // key again. The return waits for iTerm2's own word that this has happened,
    // selects the earlier window only while the new session is the selected one,
    // and looks at both again before it gives the foreground back.
    let (_, handler) = script
        .split_once("on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)")
        .unwrap();
    let (handler, _) = handler.split_once("end returnFromNewWindow").unwrap();
    let mut rest = handler;
    for step in [
        "repeat until my itermIsActive()",
        "if looks > activationLooks then return",
        "end repeat\n    if (my itermSelectedSessionId()) is not newSessionId then return",
        "my itermSelectWindow(keyboardWindow)",
        "if not (my isEarlierOrITerm(my foregroundApplication(), earlierApplication)) then return",
        "if earlierApplication's isTerminated() as boolean then return",
        "if not (my itermIsActive()) then return\n    if (my itermSelectedSessionId()) is not expectedSessionId then return\n    earlierApplication's activateWithOptions:2\n",
    ] {
        let (_, after) = rest
            .split_once(step)
            .unwrap_or_else(|| panic!("the return from a new window lost or moved {step:?}"));
        rest = after;
    }
    assert!(rest.trim().is_empty(), "{rest}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_cold_start_never_adopts_an_app_restored_surface() {
    // Closing the key window while iTerm2 is in the background can leave other
    // windows but no current window. That is not authority to adopt any of them.
    // The current window is read once, and only from an iTerm2 that was running.
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains(
        "if my itermIsRunning() then\n        try\n            set keyboardWindow to my itermCurrentWindow()"
    ));
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains(
        "if forceNewWindow or keyboardWindow is missing value then\n        set newSessionId to my itermCreateWindow(bridgeCommand)"
    ));
    assert!(terminal::macos::ghostty::CREATE_SURFACE_SCRIPT.contains(
        "if wantedWindowId is \"-\" then\n   set targetWindow to new window with configuration cfg"
    ));
    assert!(!terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains(
        "set targetTab to do script bridgeCommand\n            set targetWindow to front window"
    ));
    for restored_surface in ["front window", "current window", "selected tab"] {
        assert!(
            !terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains(restored_surface),
            "Terminal.app cold-start path still references {restored_surface}"
        );
    }
    assert!(
        terminal::macos::apple_terminal::OPEN_TAB_SCRIPT
            .contains("set targetWindowId to my windowIdForTty(targetTty, priorWindowIds)")
    );
}

// Issue #58: a new managed session must not take the keyboard. Neither host can
// create a surface without selecting it, so each script gives the keyboard back to
// the surface that had it, only while the new surface still holds it, and never
// returns that surface. The scripts are pinned line by line where it matters: every
// use of the remembered surface and every assignment of the owned one is listed, so
// an added activation, a second use of the remembered surface, or another source
// for the owned surface fails here.
#[cfg(target_os = "macos")]
#[test]
fn macos_open_scripts_give_the_keyboard_back_and_never_activate_the_app() {
    fn lines_with<'a>(script: &'a str, needle: &str) -> Vec<&'a str> {
        script
            .lines()
            .map(str::trim)
            .filter(|line| line.contains(needle))
            .collect()
    }
    let position = |script: &str, needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("open script lost {needle:?}"))
    };

    let iterm = terminal::macos::iterm2::OPEN_TAB_SCRIPT;
    // The comments of the script speak of activation; its statements are listed.
    let iterm_statements = |needle: &str| -> Vec<&str> {
        lines_with(iterm, needle)
            .into_iter()
            .filter(|line| !line.starts_with("--"))
            .collect()
    };
    // iTerm2 is never told to activate. The one activation in the script gives
    // the foreground back to another application, and only the creation of a
    // window leads to it.
    assert_eq!(
        iterm_statements("activate"),
        ["earlierApplication's activateWithOptions:2"]
    );
    assert_eq!(
        iterm_statements("returnFromNewWindow"),
        [
            "on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
            "end returnFromNewWindow",
            "my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
        ]
    );
    let window_return = position(
        iterm,
        "on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
    );
    let activation = position(iterm, "earlierApplication's activateWithOptions:2");
    assert!(window_return < activation && activation < position(iterm, "end returnFromNewWindow"));
    assert_eq!(
        iterm_statements("returnFromNewTab"),
        [
            "on returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
            "end returnFromNewTab",
            "my returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
        ]
    );
    // A tab is left only while its window still shows it, a window only while
    // its session is the selected one.
    assert_eq!(
        iterm_statements("my itermSelectTab("),
        [
            "if (my itermSessionIdOfWindow(keyboardWindow)) is newSessionId then my itermSelectTab(keyboardTab)"
        ]
    );
    assert_eq!(
        iterm_statements("my itermSelectWindow("),
        ["my itermSelectWindow(keyboardWindow)"]
    );
    // The owned session comes from a creation, its id is read once there, and
    // a tab is created in the window that was remembered.
    assert_eq!(
        iterm_statements("create "),
        [
            "set newWindow to (create window with default profile command bridgeCommand)",
            "set newTab to (create tab with default profile command bridgeCommand)",
        ]
    );
    assert_eq!(
        iterm_statements("unique ID of current session of new"),
        [
            "return unique ID of current session of newWindow",
            "return unique ID of current session of newTab",
        ]
    );
    assert_eq!(
        iterm_statements("set newSessionId to"),
        [
            "set newSessionId to my itermCreateWindow(bridgeCommand)",
            "set newSessionId to my itermCreateTab(keyboardWindow, bridgeCommand)",
        ]
    );
    assert_eq!(
        iterm_statements("set keyboardWindow to"),
        [
            "set keyboardWindow to missing value",
            "set keyboardWindow to my itermCurrentWindow()"
        ]
    );
    assert_eq!(
        iterm_statements("set keyboardTab to"),
        [
            "set keyboardTab to missing value",
            "if keyboardWindow is not missing value then set keyboardTab to my itermCurrentTabOf(keyboardWindow)"
        ]
    );
    let remembered = position(iterm, "set keyboardWindow to my itermCurrentWindow()");
    let created = position(
        iterm,
        "set newSessionId to my itermCreateWindow(bridgeCommand)",
    );
    let window_returned = position(
        iterm,
        "my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
    );
    let tab_created = position(
        iterm,
        "set newSessionId to my itermCreateTab(keyboardWindow, bridgeCommand)",
    );
    let tab_returned = position(
        iterm,
        "my returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
    );
    assert!(
        remembered < created
            && created < window_returned
            && window_returned < tab_created
            && tab_created < tab_returned
    );
    assert!(iterm.ends_with("    return newSessionId\nend run\n"));

    let terminal_app = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(!terminal_app.contains("activate"));
    assert_eq!(
        lines_with(terminal_app, "keyboardWindowId"),
        [
            "set keyboardWindowId to missing value",
            "if (count of windows) > 0 then set keyboardWindowId to id of window 1",
            "if keyboardWindowId is not missing value and keyboardWindowId is not targetWindowId then",
            "set frontmost of (first window whose id is keyboardWindowId and visible is true) to true",
        ]
    );
    assert_eq!(
        lines_with(terminal_app, "set target"),
        [
            "set targetTab to do script ((character id 21) & bridgeCommand)",
            "set targetTty to tty of targetTab",
            "set targetWindowId to my windowIdForTty(targetTty, priorWindowIds)",
            "set targetWindow to first window whose id is targetWindowId",
        ]
    );
    assert_eq!(
        lines_with(terminal_app, "window 1"),
        [
            "if (count of windows) > 0 then set keyboardWindowId to id of window 1",
            "if (id of window 1) is targetWindowId then",
        ]
    );
    let remembered = position(
        terminal_app,
        "try\n                if (count of windows) > 0 then set keyboardWindowId to id of window 1\n            end try",
    );
    let created = position(
        terminal_app,
        "set targetTab to do script ((character id 21) & bridgeCommand)",
    );
    let restored = position(
        terminal_app,
        "try\n                if (id of window 1) is targetWindowId then\n                    set frontmost of (first window whose id is keyboardWindowId and visible is true) to true\n                end if\n            end try",
    );
    assert!(remembered < created && created < restored);
    assert!(terminal_app.ends_with(
        "return targetTty & linefeed & (targetWindowId as text)\n    end tell\nend run\n"
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_window_discovery_snapshots_and_skips_stale_window_references() {
    let script = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(script.contains("set candidateWindows to get windows"));
    assert!(script.contains("repeat with candidateWindow in candidateWindows"));
    assert!(script.contains("set candidateTabs to get tabs of candidateWindow"));
    assert!(script.contains("repeat with candidateTab in candidateTabs"));
    assert!(script.contains("try\n                set candidateTabs"));
}

// The Terminal.app ownership decision, executed rather than read: the handler is
// plain AppleScript, so `osascript` runs it without talking to Terminal. The first
// case is the launch that failed on 2026-10-01 (session-y5Wpkl): window 8332 had
// lost its shell while a close confirmation was pending and still reported
// `/dev/ttys003`, the name the new window 8335 received.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_ownership_proof_ignores_windows_that_existed_before_the_launch() {
    const UNPROVEN: &str = "Agent Bridge could not prove the newly created Terminal.app window";
    const DRIVER: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set priorWindowIds to missing value
    if item 2 of argv is not "unknown" then
        set priorWindowIds to {}
        repeat with priorId in (words of (item 2 of argv))
            set end of priorWindowIds to (priorId as integer)
        end repeat
    end if
    set windowTtys to {}
    repeat with argIndex from 3 to (count of argv) by 2
        set end of windowTtys to {(item argIndex of argv) as integer, item (argIndex + 1) of argv}
    end repeat
    return my soleNewWindowWithTty(windowTtys, priorWindowIds, wantedTty)
end run
"#;
    let script = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    let start = script.find("on soleNewWindowWithTty(").unwrap();
    let end = script.find("end soleNewWindowWithTty").unwrap() + "end soleNewWindowWithTty".len();
    let handler = &script[start..end];
    assert!(
        !handler.contains("tell application"),
        "the decision must not need Terminal"
    );
    let prove = |prior: &str, windows: &[(&str, &str)]| {
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("{handler}\n{DRIVER}"))
            .arg("/dev/ttys003")
            .arg(prior)
            .args(windows.iter().flat_map(|(id, tty)| [*id, *tty]))
            .output()
            .unwrap();
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    };
    let recorded = [("8335", "/dev/ttys003"), ("8332", "/dev/ttys003")];

    // The stale window existed before the launch, so only the new one is a candidate.
    assert_eq!(prove("8332 8316 8329", &recorded), Ok("8335".to_owned()));
    // Without the list of earlier windows the former rule applies: the recorded failure.
    assert!(prove("unknown", &recorded).unwrap_err().contains(UNPROVEN));
    // A cold start has no earlier windows; a restored window with another tty is ignored.
    assert_eq!(
        prove("", &[("10", "/dev/ttys001"), ("11", "/dev/ttys003")]),
        Ok("11".to_owned())
    );
    // The new window is not in the list, or two new windows report the tty: no proof.
    assert!(
        prove("8332", &[("8332", "/dev/ttys003")])
            .unwrap_err()
            .contains(UNPROVEN)
    );
    assert!(
        prove(
            "8332",
            &[("8335", "/dev/ttys003"), ("8336", "/dev/ttys003")]
        )
        .unwrap_err()
        .contains(UNPROVEN)
    );
    // A window that existed before is never returned, even as the only match.
    assert!(
        prove("8332 8335", &recorded)
            .unwrap_err()
            .contains(UNPROVEN),
        "an earlier window must not be adopted"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_actions_require_the_recorded_window_and_tty() {
    for script in [
        terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
    ] {
        assert!(script.contains("wantedWindowId"));
        assert!(script.contains("wantedTty"));
    }
    let wait = terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT;
    assert!(wait.contains("wantedWindowId"));
    assert!(!wait.contains("wantedTty"));
    // The start is not an action on a recorded tab: Terminal types it into the tab
    // that the same `do script` creates, and no script types a start afterwards.
    let open = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(open.contains("set targetTab to do script ((character id 21) & bridgeCommand)"));
    assert!(!open.contains("do script \"\""));
    assert!(!open.contains(" in targetTab"));
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_close_waits_for_attested_process_group_shutdown_without_key_injection() {
    let script = terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT;
    assert!(script.contains("repeat 60 times"));
    assert!(!script.contains("character id 3"));
    assert!(!script.contains("do script controlC"));
}

#[cfg(target_os = "macos")]
#[test]
fn stable_iterm_and_ghostty_ids_do_not_depend_on_mutable_display_titles() {
    for script in [
        terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
        terminal::macos::iterm2::SEND_FILE_SCRIPT,
        terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
    ] {
        assert!(script.contains("unique ID of targetSession is wantedId"));
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetSession"));
    }
    fn assert_ordered(script: &str, statements: &[&str]) {
        let mut remaining = script;
        for statement in statements {
            let offset = remaining.find(statement).unwrap_or_else(|| {
                panic!("missing or out-of-order Ghostty ownership check: {statement}")
            });
            remaining = &remaining[offset + statement.len()..];
        }
    }
    assert_ordered(
        terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
        &[
            "if id of w is item 3 of argv then",
            "repeat with t in tabs of w",
            "if id of t is item 2 of argv then",
            "repeat with term in terminals of t",
            "if id of term is item 1 of argv then set matchCount to matchCount + 1",
            "if matchCount is 1 then return \"present\"",
            "if matchCount is 0 then return \"missing\"",
            "error \"Ghostty composite ownership is ambiguous\"",
        ],
    );
    assert_ordered(
        terminal::macos::ghostty::SEND_FILE_SCRIPT,
        &[
            "set wantedTerminalId to item 1 of argv",
            "set wantedTabId to item 2 of argv",
            "set wantedWindowId to item 3 of argv",
            "set targetWindow to first window whose id is wantedWindowId",
            "repeat with candidateTab in tabs of targetWindow",
            "if id of candidateTab is wantedTabId then",
            "if targetTab is missing value then error",
            "repeat with candidateTerminal in terminals of targetTab",
            "if id of candidateTerminal is wantedTerminalId then",
            "if targetTerminal is missing value then error",
            "input text promptText to targetTerminal",
        ],
    );
    assert_ordered(
        terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
        &[
            "set ws to every window whose id is item 3 of argv",
            "if (count of ws) is not 1 then error",
            "set ts to every tab of item 1 of ws whose id is item 2 of argv",
            "if (count of ts) is not 1 then error",
            "set targetTab to item 1 of ts",
            "set terms to every terminal of targetTab whose id is item 1 of argv",
            "if (count of terms) is not 1 then error",
            "if (count of terminals of targetTab) is 1 then",
            "close tab targetTab",
            "else",
            "close (item 1 of terms)",
        ],
    );
    for script in [
        terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
        terminal::macos::ghostty::SEND_FILE_SCRIPT,
        terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
    ] {
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetTab"));
        assert!(!script.contains("name of targetWindow"));
        assert!(!script.contains("name of targetTerminal"));
        assert!(!script.contains("title"));
        assert!(!script.contains("focused terminal"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_terminal_adapters_never_set_or_verify_display_titles() {
    for (name, script) in [
        ("iTerm2 open", terminal::macos::iterm2::OPEN_TAB_SCRIPT),
        (
            "iTerm2 verify",
            terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
        ),
        ("iTerm2 send", terminal::macos::iterm2::SEND_FILE_SCRIPT),
        (
            "iTerm2 close",
            terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
        ),
        ("Ghostty version", terminal::macos::ghostty::VERSION_SCRIPT),
        (
            "Ghostty create",
            terminal::macos::ghostty::CREATE_SURFACE_SCRIPT,
        ),
        (
            "Ghostty discover",
            terminal::macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
        ),
        (
            "Ghostty queue",
            terminal::macos::ghostty::QUEUE_COMMAND_SCRIPT,
        ),
        (
            "Ghostty verify",
            terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
        ),
        ("Ghostty send", terminal::macos::ghostty::SEND_FILE_SCRIPT),
        ("Ghostty close", terminal::macos::ghostty::CLOSE_TAB_SCRIPT),
        (
            "Terminal.app open",
            terminal::macos::apple_terminal::OPEN_TAB_SCRIPT,
        ),
        (
            "Terminal.app send",
            terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        ),
        (
            "Terminal.app verify",
            terminal::macos::apple_terminal::VERIFY_TAB_SCRIPT,
        ),
        (
            "Terminal.app close",
            terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
        ),
        (
            "Terminal.app wait",
            terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
        ),
    ] {
        for forbidden in [
            "tabTitle",
            "set name",
            "set_tab_title",
            "custom title",
            "title displays custom title",
            "wantedOwnershipTitle",
        ] {
            assert!(
                !script.contains(forbidden),
                "{name} still depends on visible title fragment {forbidden:?}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_terminal_applescripts_compile_without_opening_a_tab() {
    for (name, script, application, path) in [
        (
            "iTerm2 guarded dialog",
            terminal::macos::iterm2::SCREEN_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "Terminal.app guarded dialog",
            terminal::macos::apple_terminal::SCREEN_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "iTerm2 open tab",
            terminal::macos::iterm2::OPEN_TAB_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "iTerm2 send file",
            terminal::macos::iterm2::SEND_FILE_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "iTerm2 verify session",
            terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "iTerm2 close session",
            terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "Terminal.app open tab",
            terminal::macos::apple_terminal::OPEN_TAB_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app send file",
            terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app verify tab",
            terminal::macos::apple_terminal::VERIFY_TAB_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app close tab",
            terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app wait for close",
            terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Ghostty version",
            terminal::macos::ghostty::VERSION_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty create surface",
            terminal::macos::ghostty::CREATE_SURFACE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty foreground",
            terminal::macos::ghostty::FOREGROUND_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty discover terminal",
            terminal::macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty queue command",
            terminal::macos::ghostty::QUEUE_COMMAND_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty send file",
            terminal::macos::ghostty::SEND_FILE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty verify surface",
            terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty close tab",
            terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty close created tab",
            terminal::macos::ghostty::CLOSE_CREATED_TAB_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        // iTerm2 scripts address the application by bundle identifier so that a launch
        // finds it while it is not running (#99); the compiler is given the bundle path.
        let script = script
            .replace(
                &format!("tell application \"{application}\""),
                &format!("tell application \"{path}\""),
            )
            .replace(
                "tell application id \"com.googlecode.iterm2\"",
                &format!("tell application \"{path}\""),
            );
        let source = directory.path().join("bridge.applescript");
        std::fs::write(&source, script).unwrap();
        let output = std::process::Command::new("/usr/bin/osacompile")
            .arg("-o")
            .arg(directory.path().join("bridge.scpt"))
            .arg(source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name} AppleScript did not compile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// The first attempt of an explicit Terminal.app close, up to its failure: the owner and the
// surface were verified, the intent was recorded as the teardown records it, the teardown
// ended the owner (it is already gone here), and the adapter close failed.
#[cfg(target_os = "macos")]
fn fail_apple_terminal_close_after_teardown(directory: &Path, owner: &NativeSessionOwner) {
    let failure = close_session_state_with_error(
        directory,
        None,
        |session| {
            record_terminal_close_intent(directory, "session-owner123", session, owner)?;
            Err(anyhow::anyhow!(
                "Terminal.app automation failed: AppleEvent timed out."
            ))
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert!(failure.to_string().contains("timed out"));
}

#[cfg(target_os = "macos")]
fn assert_closed_without_terminal_records(directory: &Path) {
    for record in [
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
        TERMINAL_CLOSE_INTENT_FILE,
    ] {
        assert!(!directory.join(record).exists(), "{record} was kept");
    }
    assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

// The partial transition of an explicit Terminal.app close: the teardown ended the owner and
// the adapter close failed. The retry must reach the adapter instead of reporting the
// surface closed because the owner is gone.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_after_teardown_reaches_the_adapter() {
    for outcome in [
        terminal::CloseOutcome::Closed,
        terminal::CloseOutcome::Missing,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

        // A later command's repair does not report the surface closed.
        assert!(!repair_dead_native_owner(directory.path()).unwrap());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        // The close that recorded the intent may finish without signalling anything. The
        // first assertion keeps a regression from reaching the Terminal.app probe.
        assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
        assert!(
            verify_terminal_close_authority_with_evidence(
                directory.path(),
                "session-owner123",
                &handle,
                || panic!("a matching intent does not need a surface query"),
                #[cfg(target_os = "macos")]
                terminal::close_evidence::with_apple(
                    handle.kind,
                    &terminal::macos::apple_terminal::close_evidence::with_observations(
                        |_| {
                            let app = owner.terminal_app.as_ref().unwrap();
                            Ok(Some((app.start_seconds, app.start_microseconds)))
                        },
                        || Ok(vec![owner.terminal_app.clone().unwrap()])
                    )
                ),
                crate::native::session::close::budget::close_deadline(),
            )
            .unwrap()
                == TerminalCloseAuthority::SurfaceOnly
        );

        let mut adapter_calls = 0;
        close_repaired_session_state(directory.path(), |session| {
            adapter_calls += 1;
            assert_eq!(session, &handle);
            Ok(outcome)
        })
        .unwrap();
        assert_eq!(adapter_calls, 1, "the retried close must reach the adapter");
        assert_closed_without_terminal_records(directory.path());
    }
}

// A legacy handle has only window/tty identity. Those names can belong to a new
// Terminal process after a restart; a prior close intent must not authorize it. The
// retry never reaches the adapter: only a window that is proven gone ends it.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_without_app_incarnation_never_reaches_the_adapter() {
    for present in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        owner.terminal_app = None;
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let path = directory.path().join(TERMINAL_HANDLE_FILE);
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession = read_json(&path).unwrap();
        assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
        let result = close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_evidence(
                directory.path(),
                "session-owner123",
                session,
                || Ok(present),
                #[cfg(target_os = "macos")]
                terminal::close_evidence::with_apple(
                    session.kind,
                    &terminal::macos::apple_terminal::close_evidence::with_observations(
                        |_| panic!("no app incarnation is recorded"),
                        || Ok(Vec::new()),
                    ),
                ),
                crate::native::session::close::budget::close_deadline(),
            )?;
            assert_eq!(
                authority,
                TerminalCloseAuthority::Absent,
                "a legacy retry reached an adapter without app-incarnation proof"
            );
            Ok(terminal::CloseOutcome::Missing)
        });
        assert_eq!(result.is_ok(), !present, "{result:?}");
        if present {
            assert!(path.exists());
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        } else {
            assert_closed_without_terminal_records(directory.path());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_incarnation_gates_retry_and_read_only_absence() {
    for prior_intent in [false, true] {
        for mode in [
            "same-present",
            "same-absent",
            "dead",
            "reused",
            "unreadable",
            "missing",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
            let birth = (100, 42);
            owner.terminal_app = (mode != "missing").then_some(MacTerminalAppIdentity {
                pid: 1234,
                start_seconds: birth.0,
                start_microseconds: birth.1,
            });
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if prior_intent {
                fail_apple_terminal_close_after_teardown(directory.path(), &owner);
            }
            let observations = std::cell::Cell::new(0);
            let result = close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_evidence(
                    directory.path(),
                    "session-owner123",
                    session,
                    || {
                        observations.set(observations.get() + 1);
                        Ok(mode == "same-present")
                    },
                    #[cfg(target_os = "macos")]
                    terminal::close_evidence::with_apple(
                        session.kind,
                        &terminal::macos::apple_terminal::close_evidence::with_observations(
                            |pid| {
                                assert_eq!(pid, 1234);
                                match mode {
                                    "dead" => Ok(None),
                                    "reused" => Ok(Some((101, 42))),
                                    "unreadable" => bail!("injected OS observation error"),
                                    "missing" => panic!("missing app identity must not query"),
                                    _ => Ok(Some(birth)),
                                }
                            }, // A record without an incarnation is not settled by a reply that a
                            // second Terminal process could have given.
                            || {
                                let mut instances = vec![terminal_instance(1234, 100)];
                                if mode == "missing" {
                                    instances.push(terminal_instance(5678, 200));
                                }
                                Ok(instances)
                            },
                        ),
                    ),
                    crate::native::session::close::budget::close_deadline(),
                )?;
                if authority == TerminalCloseAuthority::Absent {
                    Ok(terminal::CloseOutcome::Missing)
                } else {
                    assert!(prior_intent);
                    assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
                    bail!("injected adapter failure retains retry")
                }
            });
            let absent = mode == "dead" || (!prior_intent && mode == "same-absent");
            assert_eq!(result.is_ok(), absent, "{prior_intent}/{mode}: {result:?}");
            if absent {
                assert_closed_without_terminal_records(directory.path());
            } else {
                assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
            }
            assert_eq!(
                observations.get(),
                usize::from(!prior_intent && mode.starts_with("same-")),
                "{prior_intent}/{mode}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_app_capture_requires_stable_verified_shell_ancestry() {
    let shell = MacTerminalShellIdentity {
        pid: 4,
        process_group: 4,
        terminal_tty_device: 7,
        process_start_seconds: 100,
        process_start_microseconds: 42,
    };
    for mode in [
        "same",
        "reused-shell",
        "changed-parent",
        "unreadable",
        "foreign-app",
        "wrong-pid",
    ] {
        let mut visits = std::collections::HashMap::new();
        let result = terminal_app_process_with(&shell, |pid| {
            let count = visits.entry(pid).or_insert(0);
            *count += 1;
            if mode == "unreadable" && pid == 3 {
                bail!("injected unreadable ancestor");
            }
            let seconds = if mode == "reused-shell" && pid == 4 {
                101
            } else {
                100
            };
            let parent = if mode == "changed-parent" && *count > 1 && pid == 3 {
                1
            } else {
                pid - 1
            };
            let path = if pid == 2 && mode != "foreign-app" {
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"
            } else {
                "/bin/zsh"
            };
            Ok((
                MacTerminalAppIdentity {
                    pid: if mode == "wrong-pid" { 99 } else { pid },
                    start_seconds: seconds,
                    start_microseconds: 42,
                },
                parent,
                path.into(),
            ))
        });
        assert_eq!(result.is_ok(), mode == "same", "{mode}: {result:?}");
        if let Ok(app) = result {
            assert_eq!(app.pid, 2);
            assert_eq!(visits.get(&4), Some(&2));
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_uses_only_proven_read_only_absence() {
    for mode in [
        "no-owner",
        "no-app",
        "live-owner",
        "app-dead",
        "window-absent",
        "present",
        "reused",
        "unreadable",
        "wrong-window",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
        owner.terminal_app = (mode != "no-app").then_some(MacTerminalAppIdentity {
            pid: 1234,
            start_seconds: 100,
            start_microseconds: 42,
        });
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        if mode == "no-owner" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
        if mode == "wrong-window" {
            handle.window_id = Some("9999".into());
        }
        let observations = std::cell::Cell::new(0);
        let result = apple_terminal_startup_absent_with(
            directory.path(),
            &handle,
            |_| mode == "live-owner",
            |_| match mode {
                "app-dead" => Ok(None),
                "reused" => Ok(Some((101, 42))),
                "unreadable" => bail!("injected OS error"),
                _ => Ok(Some((100, 42))),
            },
            || {
                let mut instances = vec![terminal_instance(1234, 100)];
                if matches!(mode, "no-owner" | "no-app") {
                    instances.push(terminal_instance(5678, 200));
                }
                Ok(instances)
            },
            || {
                observations.set(observations.get() + 1);
                Ok(mode != "window-absent")
            },
        );
        assert_eq!(
            matches!(result, Ok(true)),
            matches!(mode, "app-dead" | "window-absent"),
            "{mode}: {result:?}"
        );
        assert_eq!(
            observations.get(),
            usize::from(matches!(mode, "window-absent" | "present")),
            "{mode}"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_failure_or_interruption_keeps_the_intent_and_the_handle() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    fail_apple_terminal_close_after_teardown(directory.path(), &owner);

    close_repaired_session_state(directory.path(), |_| {
        Err(anyhow::anyhow!(
            "Agent Bridge Terminal.app window no longer holds its tab"
        ))
    })
    .unwrap_err();
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");

    // A closer that stopped after claiming the handle leaves it claimed; repair leaves it
    // and the next close resumes the claim.
    fs::rename(
        directory.path().join(TERMINAL_HANDLE_FILE),
        directory.path().join(TERMINAL_CLOSING_FILE),
    )
    .unwrap();
    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    assert!(directory.path().join(TERMINAL_CLOSING_FILE).exists());
    let mut adapter_calls = 0;
    close_repaired_session_state(directory.path(), |_| {
        adapter_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(adapter_calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

// A close interrupted after its intent but before its teardown left the owner alive: the
// intent grants nothing, and the original live-owner rules apply.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_while_the_incarnation_is_live() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", std::process::id());
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner).unwrap();
    assert!(
        terminal_close_intent_owner(directory.path(), &handle)
            .unwrap()
            .is_some()
    );
    assert!(
        !session::close::terminal_close_resumable_with(
            &Reader::open_unchecked(directory.path()),
            &handle,
            true,
            |_| session::owner::OwnerAttestation {
                process: crate::native::process_identity::ProcessObservation::Live(
                    crate::native::process_identity::Attested::Incarnation
                ),
                control: session::owner::ControlAttestation::Verified,
                pid_alive: Some(true),
            },
        )
        .unwrap()
    );
}

// Warp can close the owned tab (ending its owner) while its dedicated window remains.
// A second close must retain authority to verify the window, not consume the handle
// merely because the owner died during the first requested close.
#[cfg(target_os = "macos")]
#[test]
fn warp_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Warp;
    handle.id = "instance-test".into();
    handle.tab_id = Some("tab-test".into());
    handle.window_id = Some("window-test".into());
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(
        directory.path(),
        None,
        |session| {
            record_terminal_close_intent(directory.path(), "session-owner123", session, &owner)?;
            bail!("managed Warp tab is closed but its dedicated empty window remains")
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();

    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
    assert!(
        verify_terminal_close_authority(
            directory.path(),
            "session-owner123",
            &handle,
            crate::native::session::close::budget::close_deadline()
        )
        .unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    close_repaired_session_state(directory.path(), |session| {
        assert_eq!(session, &handle);
        calls += 1;
        Ok(terminal::CloseOutcome::Missing)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

#[cfg(target_os = "macos")]
#[test]
fn wezterm_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::WezTerm;
    handle.id = "0".into();
    // As a handle is stored, so that the record keeps whatever scope it was created with.
    handle.wezterm_mux = Some(
        serde_json::from_value(serde_json::json!({
            "socket": "/tmp/owned-wezterm/gui-sock-123",
            "pid": 123,
            "start_seconds": 100,
            "start_microseconds": 1,
        }))
        .unwrap(),
    );
    handle.tab_id = None;
    handle.window_id = None;
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(
        directory.path(),
        None,
        |session| {
            record_terminal_close_intent(directory.path(), "session-owner123", session, &owner)?;
            bail!("managed WezTerm pane is closed but its process remains")
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();

    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
    assert!(
        verify_terminal_close_authority(
            directory.path(),
            "session-owner123",
            &handle,
            crate::native::session::close::budget::close_deadline()
        )
        .unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    close_repaired_session_state(directory.path(), |session| {
        assert_eq!(session, &handle);
        calls += 1;
        Ok(terminal::CloseOutcome::Missing)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

#[cfg(target_os = "macos")]
type CloseIntentMutation = fn(&Path, &terminal::TerminalSession, &NativeSessionOwner);

// An invalid intent grants no close authority, but is not evidence that the surface
// disappeared. An owner that died before any explicit close also leaves cleanup unverified.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_unless_it_names_this_handle_and_owner() {
    let record = |directory: &Path, id: &str, handle, owner| {
        record_terminal_close_intent(directory, id, handle, owner).unwrap()
    };
    let cases: [(&str, CloseIntentMutation); 6] = [
        ("no intent", |_, _, _| {}),
        ("corrupt intent", |directory, _, _| {
            fs::write(directory.join(TERMINAL_CLOSE_INTENT_FILE), "{").unwrap()
        }),
        ("another session", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.managed_session_id = Some("session-other456".to_owned());
            record_terminal_close_intent(directory, "session-other456", &handle, owner).unwrap()
        }),
        ("another handle", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.window_id = Some("1002".to_owned());
            record_terminal_close_intent(directory, "session-owner123", &handle, owner).unwrap()
        }),
        ("another owner", |directory, handle, owner| {
            let mut owner = owner.clone();
            owner.process_start_microseconds = Some(43);
            record_terminal_close_intent(directory, "session-owner123", handle, &owner).unwrap()
        }),
        ("owner without its identity", |directory, handle, owner| {
            let owner = NativeSessionOwner {
                pid: owner.pid,
                managed_session_id: owner.managed_session_id.clone(),
                terminal_tty: owner.terminal_tty.clone(),
                ..NativeSessionOwner::default()
            };
            write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner).unwrap();
            record_terminal_close_intent(directory, "session-owner123", handle, &owner).unwrap()
        }),
    ];
    for (case, mutate) in cases {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        mutate(directory.path(), &handle, &owner);
        assert!(
            !terminal_close_resumable(directory.path(), &handle).unwrap(),
            "{case}"
        );
        assert!(
            !repair_dead_native_owner_with_terminal_close(directory.path(), |_| {
                panic!("{case}: absent or invalid intent grants no adapter authority")
            })
            .unwrap(),
            "{case}: missing close evidence must not become cleanup success"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        if case != "no intent" {
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        }
        assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready", "{case}");
    }

    // Only a Terminal.app handle can carry the intent.
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Iterm2;
    record(directory.path(), "session-owner123", &handle, &owner);
    assert!(!terminal_close_resumable(directory.path(), &handle).unwrap());
}

#[cfg(target_os = "macos")]
fn assert_internal_wait_reports_dead_owner(status_wait: bool) {
    let dead_pid = reaped_child_pid();
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Ghostty,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Warp,
    ] {
        for retained in [
            TERMINAL_HANDLE_FILE,
            TERMINAL_CLOSING_FILE,
            TERMINAL_CLOSE_INTENT_FILE,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let owner = write_attested_apple_terminal_state(directory.path(), "working", dead_pid);
            let handle_path = directory.path().join(TERMINAL_HANDLE_FILE);
            let mut handle: terminal::TerminalSession = read_json(&handle_path).unwrap();
            handle.kind = kind;
            write_json_atomic(&handle_path, &handle).unwrap();
            if retained == TERMINAL_CLOSING_FILE {
                fs::rename(&handle_path, directory.path().join(retained)).unwrap();
            } else if retained == TERMINAL_CLOSE_INTENT_FILE {
                record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner)
                    .unwrap();
            }
            let records = ["status.json", SESSION_OWNER_FILE, TURN_CLAIM_FILE, retained];
            let before: Vec<_> = records
                .iter()
                .map(|name| fs::read(directory.path().join(name)).unwrap())
                .collect();
            // An expired deadline makes the priority deterministic, without timing a live app.
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                turn::wait(
                    &Store::open_unchecked(directory.path()),
                    0,
                    None,
                    None,
                    agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO)
                        .unwrap(),
                    Duration::ZERO,
                )
                .unwrap_err()
            };
            for (name, bytes) in records.iter().zip(before) {
                assert_eq!(
                    fs::read(directory.path().join(name)).unwrap(),
                    bytes,
                    "{kind:?}/{retained}/{name}"
                );
            }
            assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            assert!(
                format!("{error:#}").contains("no longer running"),
                "{kind:?}/{retained}: {error:#}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(false);
}

#[cfg(target_os = "macos")]
#[test]
fn internal_status_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(true);
    // Also cover a retained, stale ready status in this internal helper. Current
    // macOS launches pass input as a provider argument and do not call this wait.
    let directory = tempfile::tempdir().unwrap();
    write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let error = wait_for_status(
        directory.path(),
        SessionState::Ready,
        Instant::now(),
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("no longer running"),
        "{error:#}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn internal_wait_does_not_call_live_or_unverified_owner_dead() {
    for mode in ["live", "missing"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), "working", std::process::id());
        if mode == "missing" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        // PID-only compatibility and a missing owner remain non-terminal observations.
        // Incarnation reuse and unknown inspection are covered with injected observations.
        require_running_wait_owner(directory.path()).unwrap();
        for status_wait in [false, true] {
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                turn::wait(
                    &Store::open_unchecked(directory.path()),
                    0,
                    None,
                    None,
                    agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO)
                        .unwrap(),
                    Duration::ZERO,
                )
                .unwrap_err()
            };
            assert!(
                !format!("{error:#}").contains("no longer running"),
                "{mode}: {error:#}"
            );
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_recovers_completion_before_reporting_dead_owner() {
    for published in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write_attested_apple_terminal_state(directory.path(), "working", reaped_child_pid());
        let handle = fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        let token = fs::read_to_string(directory.path().join(TURN_CLAIM_FILE)).unwrap();
        let pending = sample_completion(token.trim(), "completed before owner exit");
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if published {
            recover_pending_completion(directory.path()).unwrap();
        }
        let event = turn::wait(
            &Store::open_unchecked(directory.path()),
            0,
            Some("provider-turn"),
            Some(token.trim()),
            agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO).unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(event.message, "completed before owner exit");
        assert_eq!(
            fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap(),
            handle
        );
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
    }
}

#[cfg(target_os = "macos")]
fn assert_dead_owner_preserves_unverified_surface(kind: terminal::TerminalKind) {
    for state in ["ready", "exited", "failed"] {
        for claimed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_attested_apple_terminal_state(directory.path(), state, reaped_child_pid());
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.kind = kind;
            write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
            if claimed {
                fs::rename(
                    directory.path().join(TERMINAL_HANDLE_FILE),
                    directory.path().join(TERMINAL_CLOSING_FILE),
                )
                .unwrap();
            }
            assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
            let result = close_repaired_session_state(directory.path(), |session| {
                verify_terminal_close_authority_with_evidence(
                    directory.path(),
                    "session-owner123",
                    session,
                    || Ok(true),
                    #[cfg(target_os = "macos")]
                    terminal::close_evidence::with_apple(
                        session.kind,
                        &terminal::macos::apple_terminal::close_evidence::with_observations(
                            |_| {
                                let app = owner.terminal_app.as_ref().unwrap();
                                Ok(Some((app.start_seconds, app.start_microseconds)))
                            },
                            || Ok(vec![owner.terminal_app.clone().unwrap()]),
                        ),
                    ),
                    crate::native::session::close::budget::close_deadline(),
                )?;
                panic!("a dead owner without intent must grant no adapter authority")
            });
            assert!(
                result.is_err(),
                "{kind:?}/{state}/{claimed}: no adapter proof, but close reported success"
            );
            assert!(
                directory.path().join(TERMINAL_HANDLE_FILE).exists(),
                "{kind:?}: last surface evidence consumed"
            );
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_ne!(status.state.as_str(), "closed");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::AppleTerminal);
}

#[cfg(target_os = "macos")]
#[test]
fn warp_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Warp);
}

#[cfg(target_os = "macos")]
#[test]
fn wezterm_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::WezTerm);
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Ghostty);
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_exact_absence_can_finish_without_adapter_or_signal() {
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Ghostty,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = kind;
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        let mut observations = 0;
        close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_evidence(
                directory.path(),
                "session-owner123",
                session,
                || {
                    observations += 1;
                    Ok(false)
                },
                #[cfg(target_os = "macos")]
                terminal::close_evidence::with_apple(
                    session.kind,
                    &terminal::macos::apple_terminal::close_evidence::with_observations(
                        |_| {
                            let app = owner.terminal_app.as_ref().unwrap();
                            Ok(Some((app.start_seconds, app.start_microseconds)))
                        },
                        || Ok(vec![owner.terminal_app.clone().unwrap()]),
                    ),
                ),
                crate::native::session::close::budget::close_deadline(),
            )?;
            assert_eq!(authority, TerminalCloseAuthority::Absent);
            Ok(terminal::CloseOutcome::Missing)
        })
        .unwrap();
        assert_eq!(observations, 1);
        assert_closed_without_terminal_records(directory.path());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_absence_errors_and_reused_or_incomplete_owners_retain_handle() {
    for mode in ["read-error", "present", "reused", "incomplete", "foreign"] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        if mode == "reused" {
            owner.pid = std::process::id();
        }
        if mode == "incomplete" {
            owner.process_start_seconds = None;
        }
        if mode == "foreign" {
            owner.managed_session_id = Some("foreign".to_owned());
        }
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let mut observed = 0;
        let result = close_repaired_session_state(directory.path(), |session| {
            verify_terminal_close_authority_with_evidence(
                directory.path(),
                "session-owner123",
                session,
                || {
                    observed += 1;
                    match mode {
                        "read-error" => bail!("injected unreadable target"),
                        "present" => Ok(true),
                        _ => panic!("unproven owner must not inspect a surface"),
                    }
                },
                #[cfg(target_os = "macos")]
                terminal::close_evidence::with_apple(
                    session.kind,
                    &terminal::macos::apple_terminal::close_evidence::with_observations(
                        |_| {
                            let app = owner.terminal_app.as_ref().unwrap();
                            Ok(Some((app.start_seconds, app.start_microseconds)))
                        },
                        || Ok(vec![owner.terminal_app.clone().unwrap()]),
                    ),
                ),
                crate::native::session::close::budget::close_deadline(),
            )?;
            panic!("unproven absence must never close or signal")
        });
        assert!(result.is_err(), "{mode}");
        assert_eq!(
            observed,
            usize::from(matches!(mode, "read-error" | "present")),
            "{mode}"
        );
        assert!(
            directory.path().join(TERMINAL_HANDLE_FILE).exists(),
            "{mode}"
        );
        assert!(
            !directory.path().join(TERMINAL_TOMBSTONE_FILE).exists(),
            "{mode}"
        );
    }
}

#[cfg(target_os = "macos")]
fn terminal_instance(pid: u32, start_seconds: u64) -> MacTerminalAppIdentity {
    MacTerminalAppIdentity {
        pid,
        start_seconds,
        start_microseconds: 42,
    }
}

// A live process of this test with a controlling TTY of its own (a private pty): what a
// recorded owner PID is once another process has taken it.
#[cfg(target_os = "macos")]
fn spawn_on_private_pty() -> (std::process::Child, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let (mut master, mut slave) = (0, 0);
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
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (command.spawn().unwrap(), master)
}

// A launch that failed after its wrapper recorded itself, as `ask`, the wrapper and its
// finalization leave the records: the claim, the launch receipt, the bound handle, the
// complete owner and the failed status. `Pending`: the wrapper failed before the spawn
// (the provider version check). `Spawning`: the spawn is uncertain and keeps the claim.
#[cfg(target_os = "macos")]
fn write_failed_launch_with_owner(
    directory: &Path,
    kind: terminal::TerminalKind,
    phase: launch::Phase,
    owner_pid: u32,
) -> NativeSessionOwner {
    let owner = write_attested_apple_terminal_state(directory, "launching", owner_pid);
    let token = turn::current_claim_token(&Reader::open_unchecked(directory))
        .unwrap()
        .unwrap();
    launch::begin(
        &Store::open_unchecked(directory),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    if phase == launch::Phase::Spawning {
        let mut record = launch::read(&Reader::open_unchecked(directory))
            .unwrap()
            .unwrap();
        record.phase = phase;
        write_json_atomic(&directory.join(launch::FILE), &record).unwrap();
    }
    launch::finalize(
        &Store::open_unchecked(directory),
        &Err(anyhow::anyhow!("provider version check failed")),
    )
    .unwrap();
    let mut handle: terminal::TerminalSession =
        read_json(&directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = kind;
    write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    owner
}

// A failed launch is no close intent. A surface that can outlive its owner follows the
// dead-owner rules whatever the launch phase: the whole owner identity, then the read-only
// proof of absence, and never a close or a signal.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_with_a_recorded_owner_gets_no_close_without_a_prior_intent() {
    let (mut other_process, _pty) = spawn_on_private_pty();
    let mut failures = Vec::new();
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in [
                "absent",
                "present",
                "read-error",
                "incomplete",
                "foreign",
                "reused",
            ] {
                let case = format!("{kind:?}/{phase:?}/{mode}");
                let directory = tempfile::tempdir().unwrap();
                let owner_pid = if mode == "reused" {
                    other_process.id()
                } else {
                    reaped_child_pid()
                };
                let mut owner =
                    write_failed_launch_with_owner(directory.path(), kind, phase, owner_pid);
                if mode == "incomplete" {
                    owner.process_start_seconds = None;
                }
                if mode == "foreign" {
                    owner.managed_session_id = Some("session-foreign".to_owned());
                }
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                let app = owner.terminal_app.clone().unwrap();
                let claim =
                    turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap();
                assert_eq!(claim.is_some(), phase == launch::Phase::Spawning, "{case}");
                assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

                let (mut observations, mut adapter_calls) = (0, 0);
                let result = close_repaired_session_state(directory.path(), |session| {
                    let authority = verify_terminal_close_authority_with_evidence(
                        directory.path(),
                        "session-owner123",
                        session,
                        || {
                            observations += 1;
                            match mode {
                                "absent" => Ok(false),
                                "present" => Ok(true),
                                _ => bail!("injected unreadable target"),
                            }
                        },
                        #[cfg(target_os = "macos")]
                        terminal::close_evidence::with_apple(
                            session.kind,
                            &terminal::macos::apple_terminal::close_evidence::with_observations(
                                |pid| {
                                    assert_eq!(pid, app.pid);
                                    Ok(Some((app.start_seconds, app.start_microseconds)))
                                },
                                || Ok(vec![app.clone()]),
                            ),
                        ),
                        crate::native::session::close::budget::close_deadline(),
                    )?;
                    if authority == TerminalCloseAuthority::Absent {
                        return Ok(terminal::CloseOutcome::Missing);
                    }
                    // Where `close_session_surface` signals the owner or calls the adapter.
                    adapter_calls += 1;
                    Ok(terminal::CloseOutcome::Closed)
                });

                if adapter_calls != 0 {
                    failures.push(format!("{case}: {adapter_calls} close/signal step reached"));
                }
                let wanted_observations =
                    usize::from(matches!(mode, "absent" | "present" | "read-error"));
                if observations != wanted_observations {
                    failures.push(format!(
                        "{case}: {observations} surface observations, wanted {wanted_observations}"
                    ));
                }
                let status: SessionStatus =
                    read_json(&directory.path().join("status.json")).unwrap();
                let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
                let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
                if mode == "absent" {
                    if result.is_err()
                        || handle_kept
                        || !tombstone
                        || status.state.as_str() != "closed"
                    {
                        failures.push(format!(
                            "{case}: proven absence did not consume the handle: {result:?}"
                        ));
                    }
                } else if result.is_ok()
                    || !handle_kept
                    || tombstone
                    || status.state.as_str() != "failed"
                    || turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap()
                        != claim
                    || launch::read(&Reader::open_unchecked(directory.path()))
                        .unwrap()
                        .unwrap()
                        .phase
                        != phase
                {
                    failures.push(format!(
                        "{case}: handle, claim or state was not retained (result {result:?}, state {})",
                        status.state
                    ));
                }
            }
        }
    }
    other_process.kill().unwrap();
    other_process.wait().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[cfg(target_os = "macos")]
#[test]
fn ownerless_warp_and_wezterm_failures_only_consume_proven_absence() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in ["absent", "present", "unreadable", "foreign"] {
                let directory = tempfile::tempdir().unwrap();
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
                let claim =
                    turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap();
                let (mut observations, mut mutations) = (0, 0);
                let result = close_repaired_session_state(directory.path(), |session| {
                    let authority = verify_terminal_close_authority_with_evidence(
                        directory.path(),
                        if mode == "foreign" {
                            "session-foreign"
                        } else {
                            "session-owner123"
                        },
                        session,
                        || {
                            observations += 1;
                            match mode {
                                "absent" => Ok(false),
                                "present" => Ok(true),
                                _ => bail!("injected unreadable surface"),
                            }
                        },
                        #[cfg(target_os = "macos")]
                        terminal::close_evidence::with_apple(
                            session.kind,
                            &terminal::macos::apple_terminal::close_evidence::with_observations(
                                |_| panic!("these adapters own their process identity checks"),
                                || panic!("this is not a Terminal.app surface"),
                            ),
                        ),
                        crate::native::session::close::budget::close_deadline(),
                    )?;
                    if authority == TerminalCloseAuthority::Absent {
                        return Ok(terminal::CloseOutcome::Missing);
                    }
                    mutations += 1;
                    Ok(terminal::CloseOutcome::Closed)
                });
                assert_eq!(
                    mutations, 0,
                    "{kind:?}/{phase:?}/{mode}: mutation without owner or intent"
                );
                assert_eq!(observations, usize::from(mode != "foreign"));
                if mode == "absent" {
                    result.unwrap();
                    assert_closed_without_terminal_records(directory.path());
                } else {
                    assert!(result.is_err(), "{kind:?}/{phase:?}/{mode}");
                    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
                    assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
                    assert_eq!(
                        turn::current_claim_token(&Reader::open_unchecked(directory.path()))
                            .unwrap(),
                        claim
                    );
                    assert_eq!(
                        launch::read(&Reader::open_unchecked(directory.path()))
                            .unwrap()
                            .unwrap()
                            .phase,
                        phase
                    );
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_failed_start_native_id_recovery_does_not_need_a_prior_intent() {
    for owner_recorded in [false, true] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            let directory = tempfile::tempdir().unwrap();
            write_failed_launch_with_owner(
                directory.path(),
                terminal::TerminalKind::Ghostty,
                phase,
                reaped_child_pid(),
            );
            if !owner_recorded {
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            }
            let handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
            assert_eq!(
                verify_terminal_close_authority_with_evidence(
                    directory.path(),
                    "session-owner123",
                    &handle,
                    || panic!("the exact native-ID close adapter verifies its target"),
                    #[cfg(target_os = "macos")]
                    terminal::close_evidence::with_apple(
                        handle.kind,
                        &terminal::macos::apple_terminal::close_evidence::with_observations(
                            |_| panic!("no Terminal.app process observation"),
                            || panic!("no Terminal.app instance observation")
                        )
                    ),
                    crate::native::session::close::budget::close_deadline(),
                )
                .unwrap(),
                TerminalCloseAuthority::SurfaceOnly
            );
        }
    }
}

// The close that recorded its intent while the owner was live still finishes after a
// failed launch, and a surface with an app-unique native id keeps its startup recovery.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_keeps_the_prior_intent_retry_and_the_stable_id_recovery() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Iterm2,
        terminal::TerminalKind::Ghostty,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
            let handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            if terminal::surface_outlives_owner(&handle) {
                record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner)
                    .unwrap();
            }
            let app = owner.terminal_app.clone().unwrap();
            let mut adapter_calls = 0;
            close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_evidence(
                    directory.path(),
                    "session-owner123",
                    session,
                    || panic!("{kind:?}/{phase:?}: no surface observation is needed"),
                    #[cfg(target_os = "macos")]
                    terminal::close_evidence::with_apple(
                        session.kind,
                        &terminal::macos::apple_terminal::close_evidence::with_observations(
                            |_| Ok(Some((app.start_seconds, app.start_microseconds))),
                            || Ok(vec![app.clone()]),
                        ),
                    ),
                    crate::native::session::close::budget::close_deadline(),
                )?;
                assert_eq!(
                    authority,
                    TerminalCloseAuthority::SurfaceOnly,
                    "{kind:?}/{phase:?}"
                );
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed)
            })
            .unwrap();
            assert_eq!(adapter_calls, 1, "{kind:?}/{phase:?}");
            assert_closed_without_terminal_records(directory.path());
        }
    }
}

// The production capture on the real ancestors of this test. Under a terminal tab they
// include the root-owned /usr/bin/login, which PROC_PIDTBSDINFO does not answer for.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_reads_the_ancestors_of_any_user() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let owner = NativeSessionOwner {
        pid,
        terminal_shell: Some(MacTerminalShellIdentity {
            pid,
            process_group: 0,
            terminal_tty_device: 0,
            process_start_seconds: seconds,
            process_start_microseconds: microseconds,
        }),
        ..NativeSessionOwner::default()
    };
    match terminal_app_process(&owner) {
        // The suite itself runs in a Terminal.app tab.
        Ok(app) => assert_eq!(
            macos_process_start(app.pid).unwrap(),
            Some((app.start_seconds, app.start_microseconds))
        ),
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!("R2_LIFECYCLE terminal_app_process on this test's lineage: {error}");
            assert!(
                error.contains("has no system Terminal.app ancestor"),
                "an ancestor could not be read: {error}"
            );
        }
    }
}

// The capture's own walk and recheck, with the production observation, up to the
// ancestor that launchd started; that ancestor stands for Terminal.app, which no test
// may start. Under a terminal tab the walk crosses the root-owned /usr/bin/login.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_walks_this_tests_real_ancestors_to_the_app() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let shell = MacTerminalShellIdentity {
        pid,
        process_group: 0,
        terminal_tty_device: 0,
        process_start_seconds: seconds,
        process_start_microseconds: microseconds,
    };
    let mut lineage = Vec::new();
    let app = terminal_app_process_with(&shell, |pid| {
        let (identity, parent, path) = terminal_app_ancestor(pid)?;
        if !lineage.contains(&path) {
            lineage.push(path.clone());
        }
        let path = if parent == 1 {
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".to_owned()
        } else {
            path
        };
        Ok((identity, parent, path))
    })
    .unwrap();
    eprintln!("R2_LIFECYCLE real lineage of this test: {lineage:#?}");
    let (identity, parent, _) = terminal_app_ancestor(app.pid).unwrap();
    assert_eq!((identity, parent), (app, 1));
}

// launchd is an ancestor of every process and belongs to root. The record of a process of
// this user carries the same birth and parent as the query that verifies an owner, so an
// incarnation captured here is the one a later close compares.
#[cfg(target_os = "macos")]
#[test]
fn process_records_answer_for_a_root_owned_ancestor_and_agree_with_the_owner_query() {
    let (launchd, parent, path) = terminal_app_ancestor(1).unwrap();
    assert_eq!(
        (launchd.pid, parent, path.as_str()),
        (1, 0, "/sbin/launchd")
    );
    assert!(launchd.start_seconds > 0 && launchd.start_microseconds < 1_000_000);

    let pid = std::process::id();
    let (own, parent, path) = terminal_app_ancestor(pid).unwrap();
    let info = macos_process_info(pid).unwrap().unwrap();
    assert_eq!(
        (own.start_seconds, own.start_microseconds, parent),
        (
            info.process_start_seconds,
            info.process_start_microseconds,
            info.parent_pid
        )
    );
    let executable = std::env::current_exe().unwrap();
    assert_eq!(Path::new(&path).file_name(), executable.file_name());

    // The command name is the executable's, cut to the kernel's 16 bytes.
    let name = executable.file_name().unwrap().as_encoded_bytes();
    let named = macos_processes_named(&name[..name.len().min(16)]).unwrap();
    assert!(named.contains(&own), "{named:?} lacks {own:?}");
    assert!(named.windows(2).all(|pair| pair[0].pid < pair[1].pid));
    assert!(
        macos_processes_named(b"no-such-command")
            .unwrap()
            .is_empty()
    );

    let error = terminal_app_ancestor(reaped_child_pid()).unwrap_err();
    assert!(error.to_string().contains("ended during attestation"));
}

// A Terminal.app record that names no app incarnation: a start that failed before the
// wrapper recorded itself, an owner record of 0.0.10 or earlier, and such a record whose
// close had begun. Nothing ties a reply to the app that created the window, so the window
// is gone only when one stable instance, or none, can have answered and its list lacks
// the window. Everything else keeps the handle, and nothing is ever closed.
#[cfg(target_os = "macos")]
#[test]
fn terminal_record_without_app_incarnation_is_consumed_only_by_proven_absence() {
    let mut failures = Vec::new();
    for shape in [
        "ownerless-failed-start",
        "legacy-dead-owner",
        "legacy-intent-retry",
    ] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "none-running-but-listed",
            "two-instances",
            "restarted",
            "started-meanwhile",
            "listing-error",
            "presence-error",
        ] {
            let case = format!("{shape}/{mode}");
            let directory = tempfile::tempdir().unwrap();
            if shape == "ownerless-failed-start" {
                write_failed_launch_with_owner(
                    directory.path(),
                    terminal::TerminalKind::AppleTerminal,
                    launch::Phase::Pending,
                    reaped_child_pid(),
                );
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            } else {
                let mut owner = write_attested_apple_terminal_state(
                    directory.path(),
                    "ready",
                    reaped_child_pid(),
                );
                owner.terminal_app = None;
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                if shape == "legacy-intent-retry" {
                    fail_apple_terminal_close_after_teardown(directory.path(), &owner);
                }
            }
            let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let listings = std::cell::Cell::new(0);
            let (mut observations, mut adapter_calls) = (0, 0);
            let result = close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_evidence(
                    directory.path(),
                    "session-owner123",
                    session,
                    || {
                        observations += 1;
                        match mode {
                            "presence-error" => bail!("injected unreadable window list"),
                            "one-stable-listed" | "none-running-but-listed" => Ok(true),
                            _ => Ok(false),
                        }
                    },
                    #[cfg(target_os = "macos")]
                    terminal::close_evidence::with_apple(
                        session.kind,
                        &terminal::macos::apple_terminal::close_evidence::with_observations(
                            |_| panic!("{case}: no app incarnation is recorded"),
                            || {
                                listings.set(listings.get() + 1);
                                let first = listings.get() == 1;
                                match mode {
                                    "listing-error" => bail!("injected process table error"),
                                    "none-running" | "none-running-but-listed" => Ok(Vec::new()),
                                    "two-instances" => Ok(vec![
                                        terminal_instance(1234, 100),
                                        terminal_instance(5678, 200),
                                    ]),
                                    "restarted" if !first => Ok(vec![terminal_instance(1234, 101)]),
                                    "started-meanwhile" if first => Ok(Vec::new()),
                                    _ => Ok(vec![terminal_instance(1234, 100)]),
                                }
                            },
                        ),
                    ),
                    crate::native::session::close::budget::close_deadline(),
                )?;
                if authority == TerminalCloseAuthority::Absent {
                    return Ok(terminal::CloseOutcome::Missing);
                }
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed)
            });

            let absent = matches!(mode, "none-running" | "one-stable");
            let wanted_observations =
                usize::from(!matches!(mode, "two-instances" | "listing-error"));
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
            let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
            let intent_kept = directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists();
            let settled = if absent {
                result.is_ok() && !handle_kept && tombstone && status.state.as_str() == "closed"
            } else {
                result.is_err()
                    && handle_kept
                    && !tombstone
                    && status.state == before.state
                    && intent_kept == (shape == "legacy-intent-retry")
            };
            if !settled || adapter_calls != 0 || observations != wanted_observations {
                failures.push(format!(
                    "{case}: result {result:?}, state {}, handle kept {handle_kept}, {adapter_calls} close steps, {observations} observations",
                    status.state
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// The rollback of a failed start asks the same question before the wrapper has recorded
// an owner or an app incarnation. It reads only, and it never touches the binding.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_without_app_incarnation_needs_one_stable_instance() {
    for shape in ["no-owner", "no-app"] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "two-instances",
            "restarted",
            "listing-error",
            "presence-error",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
            owner.terminal_app = None;
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if shape == "no-owner" {
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            }
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
            let listings = std::cell::Cell::new(0);
            let result = apple_terminal_startup_absent_with(
                directory.path(),
                &handle,
                |_| false,
                |_| panic!("{shape}/{mode}: no app incarnation is recorded"),
                || {
                    listings.set(listings.get() + 1);
                    match mode {
                        "listing-error" => bail!("injected process table error"),
                        "none-running" => Ok(Vec::new()),
                        "two-instances" => Ok(vec![
                            terminal_instance(1234, 100),
                            terminal_instance(5678, 200),
                        ]),
                        "restarted" if listings.get() > 1 => Ok(vec![terminal_instance(1234, 101)]),
                        _ => Ok(vec![terminal_instance(1234, 100)]),
                    }
                },
                || match mode {
                    "presence-error" => bail!("injected unreadable window list"),
                    "one-stable-listed" => Ok(true),
                    _ => Ok(false),
                },
            );
            match mode {
                "none-running" | "one-stable" => {
                    assert!(matches!(result, Ok(true)), "{shape}/{mode}")
                }
                "one-stable-listed" => assert!(matches!(result, Ok(false)), "{shape}/{mode}"),
                _ => assert!(result.is_err(), "{shape}/{mode}: {result:?}"),
            }
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}

// The live owner of a record of 0.0.10 or earlier: the close derives the app incarnation
// from that owner's own ancestry and records it before the intent, which is then exact for
// the updated owner record. A derivation that fails changes nothing.
#[cfg(target_os = "macos")]
#[test]
fn legacy_terminal_owner_records_its_app_incarnation_before_the_close_intent() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = write_attested_apple_terminal_state(directory.path(), "ready", 4242);
    owner.terminal_app = None;
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    let stored_app = || {
        read_json::<NativeSessionOwner>(&directory.path().join(SESSION_OWNER_FILE))
            .unwrap()
            .terminal_app
    };

    let error = record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        bail!("the verified terminal shell has no system Terminal.app ancestor")
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no system Terminal.app ancestor")
    );
    assert!(owner.terminal_app.is_none() && stored_app().is_none());

    let app = terminal_instance(1234, 100);
    record_legacy_terminal_app(directory.path(), &mut owner, |legacy| {
        assert_eq!(legacy.pid, 4242);
        Ok(app.clone())
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app.clone()));
    record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner).unwrap();
    assert_eq!(
        terminal_close_intent_owner(directory.path(), &handle)
            .unwrap()
            .and_then(|owner| owner.terminal_app),
        Some(app.clone())
    );

    record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        panic!("a recorded incarnation is never derived again")
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app));
}

// What a close may touch is the scope its session was created with, as the handle stores
// it. The lifecycle reads no opening preference: the adapter receives the stored handle
// unchanged, and the retry of a close is authorized for that exact handle only, so the
// same target read with another scope is given nothing.
#[cfg(target_os = "macos")]
#[test]
fn close_uses_the_stored_creation_scope_of_a_wezterm_session() {
    for owns_gui in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = terminal::TerminalKind::WezTerm;
        handle.id = "7".into();
        handle.tab_id = Some("3".into());
        handle.window_id = Some("1".into());
        handle.wezterm_mux = Some(
            serde_json::from_value(serde_json::json!({
                "socket": "/tmp/owned-wezterm/gui-sock-123",
                "pid": 123,
                "start_seconds": 100,
                "start_microseconds": 1,
                "owns_gui": owns_gui,
            }))
            .unwrap(),
        );
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        close_session_state_with_error(
            directory.path(),
            None,
            |session| {
                assert_eq!(session, &handle);
                record_terminal_close_intent(
                    directory.path(),
                    "session-owner123",
                    session,
                    &owner,
                )?;
                bail!("managed WezTerm pane is closed but its tab remains")
            },
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap_err();

        let mut other_scope = handle.clone();
        other_scope.wezterm_mux.as_mut().unwrap().owns_gui = !owns_gui;
        assert!(
            terminal_close_intent_owner(directory.path(), &other_scope)
                .unwrap()
                .is_none()
        );
        assert!(
            terminal_close_intent_owner(directory.path(), &handle)
                .unwrap()
                .is_some()
        );

        let mut closed = None;
        close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_evidence(
                directory.path(),
                "session-owner123",
                session,
                || panic!("the recorded intent needs no surface observation"),
                #[cfg(target_os = "macos")]
                terminal::close_evidence::with_apple(
                    session.kind,
                    &terminal::macos::apple_terminal::close_evidence::with_observations(
                        |_| panic!("a WezTerm handle names no Terminal.app incarnation"),
                        || panic!("a WezTerm handle lists no Terminal.app instances"),
                    ),
                ),
                crate::native::session::close::budget::close_deadline(),
            )?;
            assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
            closed = Some(session.clone());
            Ok(terminal::CloseOutcome::Closed)
        })
        .unwrap();
        assert_eq!(closed, Some(handle));
        assert_closed_without_terminal_records(directory.path());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn attested_terminal_absence_does_not_accept_another_app_instances_reply() {
    let app = terminal_instance(1234, 100);
    let reads = std::cell::Cell::new(0);
    let result = terminal_surface_absent(
        Some(&app),
        |_| Ok(Some((app.start_seconds, app.start_microseconds))),
        || Ok(vec![app.clone(), terminal_instance(5678, 200)]),
        || {
            reads.set(reads.get() + 1);
            Ok(false)
        },
    );
    assert!(
        result.is_err(),
        "ambiguous instance established absence: {result:?}"
    );
    assert_eq!(
        reads.get(),
        0,
        "no app is queried when its address is ambiguous"
    );
}
