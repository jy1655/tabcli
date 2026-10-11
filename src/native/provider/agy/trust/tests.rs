use super::super::diagnostics::workspace_trust_check_with;
use super::super::readiness::STARTUP_COMPLETED_MARKER;
use super::super::test_support::{
    FakeClock, HOOKS_LOADED, REAL_FAILURE_STARTUP, REAL_MACOS_INITIAL_TURN, REAL_QUIET_STARTUP,
    REAL_STARTUP_RELOAD_AFTER_STARTUP, REAL_SUCCESS_STARTUP, REAL_TRUST_RELOAD_AFTER_PASTE, glog,
    settled_startup_log,
};
use super::super::{AGY_LOG_FILE, STARTUP_POLL_INTERVAL, TRUST_RECOVERY, TRUST_STORE_MISSING};
use super::*;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

// What tells a trusted Agy process from one whose dialog is still open, in the
// recorded logs of both platforms: a `hooks_manager.go` line from a goroutine
// other than the main one. The two delivered Windows pastes (session-udT6uY,
// session-M8QFPp) had it at startup; every lost paste logged it only after the
// paste's own Enter approved the dialog; session-IQHEwf, which was never
// approved, never logged it.
#[test]
fn workspace_customization_load_is_read_from_the_sessions_own_log() {
    let macos_before_approval = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
        .rfind("I0924 21:32:19.637013")
        .unwrap()];
    for (log, loaded, what) in [
        (
            REAL_SUCCESS_STARTUP,
            true,
            "session-udT6uY, trusted at startup",
        ),
        (
            REAL_STARTUP_RELOAD_AFTER_STARTUP,
            true,
            "session-M8QFPp, trusted at startup",
        ),
        (
            REAL_FAILURE_STARTUP,
            false,
            "session-fMqSQc before its paste",
        ),
        (REAL_QUIET_STARTUP, false, "session-IQHEwf, never approved"),
        (
            macos_before_approval,
            false,
            "session-QMFk6F before its paste",
        ),
        (
            REAL_MACOS_INITIAL_TURN,
            true,
            "session-QMFk6F after the paste approved the dialog",
        ),
        (
            REAL_TRUST_RELOAD_AFTER_PASTE,
            true,
            "session-U2yPxX trust reload",
        ),
    ] {
        assert_eq!(
            workspace_customizations_loaded(log.as_bytes()),
            loaded,
            "{what}"
        );
    }
    assert!(
        workspace_customizations_loaded(settled_startup_log().as_bytes()),
        "session-fMqSQc after the paste approved the dialog"
    );
    // The main goroutine's line while the store manager is built is not the load,
    // and a line still being written is not evidence yet.
    let main_only = glog("16:41:07.819578", 1, "hooks_manager.go:53", HOOKS_LOADED);
    assert!(!workspace_customizations_loaded(main_only.as_bytes()));
    let partial = glog("16:41:20.814059", 406, "hooks_manager.go:53", HOOKS_LOADED);
    assert!(!workspace_customizations_loaded(
        partial.trim_end().as_bytes()
    ));
    assert!(workspace_customizations_loaded(partial.as_bytes()));
    // A field that is not a goroutine id is not evidence either.
    let not_an_id = partial.replace("     406 ", "     4o6 ");
    assert!(!workspace_customizations_loaded(not_an_id.as_bytes()));
}

// The fix for that state: no paste, before any terminal input, until Agy's own
// trust store lists the exact workspace and this session's own log shows the
// workspace customization load. A parent entry does not count, a dialog approved
// in another session leaves this session's dialog open, a log without the load
// line is reported as unverified rather than as an open dialog, an unreadable
// store proves nothing, and an approval during the wait releases the paste.
#[test]
fn the_trust_wait_ends_when_the_session_has_ended() {
    let start = Instant::now();
    let wait = Duration::from_secs(60);

    // Issue #60: the launcher of a closed session went on waiting until its
    // deadline. The session is looked at before every look at the trust evidence.
    let mut clock = FakeClock::new(start);
    let mut polls = 0;
    let error = wait_for_workspace_trust_with(
        &mut || Ok(Some(TRUST_STORE_MISSING)),
        &mut || {
            polls += 1;
            (polls == 3).then(|| "closed".to_owned())
        },
        start + wait,
        STARTUP_POLL_INTERVAL,
        &mut clock,
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "the session is closed and no longer waits for Agy workspace trust, so the prompt was not pasted"
    );
    assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 2);

    // An ended session is not pasted into even when its trust is evident.
    let mut clock = FakeClock::new(start);
    let mut looked = false;
    assert!(
        wait_for_workspace_trust_with(
            &mut || {
                looked = true;
                Ok(None)
            },
            &mut || Some("failed".to_owned()),
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .is_err()
    );
    assert!(!looked);
    assert_eq!(clock.slept, Duration::ZERO);
}

#[test]
fn a_paste_is_withheld_until_the_store_and_this_sessions_log_show_trust() {
    use super::super::super::super::consent;
    use super::super::super::super::doctor::Availability;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let directory = root.join("session-trust1");
    fs::create_dir(&directory).unwrap();
    let log_path = directory.join(AGY_LOG_FILE);
    let homes = consent::fixture_homes(&root);
    let trust_store = |entries: &[&Path]| {
        let keys: Vec<String> = entries
            .iter()
            .map(|path| consent::native_key(path).unwrap())
            .collect();
        super::super::super::super::write_json_atomic(
            &homes.agy,
            &serde_json::json!({ "trustedWorkspaces": keys }),
        )
        .unwrap();
    };
    let start = Instant::now();
    let wait = Duration::from_secs(30);
    let refused = |expected: &str, availability: Availability, reason_code: &str| {
        let mut clock = FakeClock::new(start);
        let error = wait_for_workspace_trust_with(
            &mut || workspace_trust_missing(&workspace, &homes, &log_path),
            &mut || None,
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(clock.slept, wait, "the wait ends only at the deadline");
        assert_eq!(
            error,
            format!(
                "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {expected}. {TRUST_RECOVERY}"
            )
        );
        let check = workspace_trust_check_with(&workspace, &homes, &directory);
        assert_eq!(check.availability, availability);
        assert_eq!(check.reason_code, reason_code);
        let next_action = serde_json::to_value(&check).unwrap()["next_action"].to_string();
        assert!(next_action.contains(TRUST_RECOVERY), "{next_action}");
    };
    let untrusted = || {
        refused(
            TRUST_STORE_MISSING,
            Availability::Unavailable,
            "agy_workspace_untrusted",
        )
    };
    let unverified = || {
        refused(
            TRUST_SESSION_UNVERIFIED,
            Availability::Unknown,
            "agy_session_trust_unverified",
        )
    };
    let passes = |what: &str| {
        let mut clock = FakeClock::new(start);
        wait_for_workspace_trust_with(
            &mut || workspace_trust_missing(&workspace, &homes, &log_path),
            &mut || None,
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, Duration::ZERO, "{what}");
        let check = workspace_trust_check_with(&workspace, &homes, &directory);
        assert_eq!(check.availability, Availability::Available, "{what}");
        assert_eq!(check.reason_code, "agy_workspace_trusted", "{what}");
    };

    // The workspace is not in the store: issue #48. Neither no store nor a
    // parent entry is an approval, whatever the log says.
    fs::write(&log_path, REAL_FAILURE_STARTUP).unwrap();
    untrusted();
    trust_store(&[&root]);
    untrusted();
    fs::write(&log_path, REAL_SUCCESS_STARTUP).unwrap();
    untrusted();

    // The store lists the workspace because another session approved it, and
    // this session's log shows no load: its own dialog may still be open. The
    // same holds for a log that is gone or was cut before the load line, in a
    // session that has no dialog, so the report claims neither state.
    trust_store(&[&root, &workspace]);
    fs::write(&log_path, REAL_FAILURE_STARTUP).unwrap();
    unverified();
    fs::remove_file(&log_path).unwrap();
    unverified();
    let cut = REAL_SUCCESS_STARTUP
        .find("I0924 16:42:24.081497")
        .expect("the load line of session-udT6uY");
    fs::write(&log_path, &REAL_SUCCESS_STARTUP[..cut]).unwrap();
    assert!(REAL_SUCCESS_STARTUP[..cut].contains(STARTUP_COMPLETED_MARKER));
    unverified();

    // Approved in this session, or trusted before it started.
    fs::write(&log_path, settled_startup_log()).unwrap();
    passes("the dialog was approved in this session");
    fs::write(&log_path, REAL_SUCCESS_STARTUP).unwrap();
    passes("the workspace was trusted before launch");

    // An approval during the wait releases the paste at the next poll.
    let mut answers = [
        Ok(Some(TRUST_STORE_MISSING)),
        Err(anyhow::anyhow!("store busy")),
        Ok(Some(TRUST_SESSION_UNVERIFIED)),
        Ok(None),
    ]
    .into_iter();
    let mut clock = FakeClock::new(start);
    wait_for_workspace_trust_with(
        &mut || answers.next().unwrap(),
        &mut || None,
        start + wait,
        STARTUP_POLL_INTERVAL,
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 3);

    // An unreadable store is not an approval, and the report says why.
    fs::write(&homes.agy, b"{\"trustedWorkspaces\":true}").unwrap();
    let mut clock = FakeClock::new(start);
    let error = wait_for_workspace_trust_with(
        &mut || workspace_trust_missing(&workspace, &homes, &log_path),
        &mut || None,
        start + wait,
        STARTUP_POLL_INTERVAL,
        &mut clock,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("so the prompt was not pasted"), "{error}");
    assert!(error.contains("could not be read"), "{error}");
    assert_eq!(
        workspace_trust_check_with(&workspace, &homes, &directory).availability,
        Availability::Unknown
    );
}
