//! Terminal-script, adapter-behaviour and ownership-proof tests of the macOS terminal
//! adapters. They compile AppleScript and inspect real processes; moved here from
//! `native/tests/macos.rs` beside the adapters they exercise.

use crate::native::terminal;
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
            "iTerm2 session presence",
            terminal::macos::iterm2::PRESENCE_SCRIPT,
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
