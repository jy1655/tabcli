use super::super::diagnostics::workspace_trust_check;
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
// A session directory with the status record the shared wait reads; no manifest, which
// the production entry reads before the wait.
fn session_with_status(root: &Path, name: &str, state: &str) -> std::path::PathBuf {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    write_status(&directory, state);
    directory
}

fn write_status(directory: &Path, state: &str) {
    fs::write(
        directory.join("status.json"),
        format!(
            r#"{{"state":"{state}","generation":2,"updated_unix_ms":2,"exit_code":null,"error":null}}"#
        ),
    )
    .unwrap();
}

#[test]
fn the_trust_wait_ends_when_the_session_has_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let start = Instant::now();
    let wait = Duration::from_secs(60);

    // Issue #60: the launcher of a closed session went on waiting until its
    // deadline. The session is looked at before every look at the trust evidence:
    // the session closes during the second poll's look at the evidence, and the
    // third poll ends the wait before it looks again.
    let closing = session_with_status(&root, "session-closing", "awaiting-initial-input");
    let mut clock = FakeClock::new(start);
    let mut polls = 0;
    let error = wait_for_workspace_trust_evidence(&closing, start + wait, &mut clock, &mut || {
        polls += 1;
        if polls == 2 {
            write_status(&closing, "closed");
        }
        Ok(Some(TRUST_STORE_MISSING))
    })
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "the session is closed and no longer waits for Agy workspace trust, so the prompt was not pasted"
    );
    assert_eq!(polls, 2);
    assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 2);

    // An ended session is not pasted into even when its trust is evident: the
    // evidence is not even looked at.
    for state in ["closed", "failed", "exited"] {
        let ended = session_with_status(&root, &format!("session-{state}"), state);
        let mut clock = FakeClock::new(start);
        let mut looked = false;
        let error =
            wait_for_workspace_trust_evidence(&ended, start + wait, &mut clock, &mut || {
                looked = true;
                Ok(None)
            })
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!(
                "the session is {state} and no longer waits for Agy workspace trust, so the prompt was not pasted"
            )
        );
        assert!(!looked, "{state}");
        assert_eq!(clock.slept, Duration::ZERO);
    }
}

#[test]
fn a_paste_is_withheld_until_the_store_and_this_sessions_log_show_trust() {
    use super::super::super::super::consent;
    use super::super::super::super::diagnostics::Availability;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let directory = root.join("session-trust1");
    fs::create_dir(&directory).unwrap();
    let log_path = directory.join(AGY_LOG_FILE);
    let env = consent::Environment::at_home(&root);
    fs::create_dir_all(root.join(".gemini/antigravity-cli")).unwrap();
    let store = root.join(".gemini/antigravity-cli/settings.json");
    let trust_store = |entries: &[&Path]| {
        let keys: Vec<String> = entries
            .iter()
            .map(|path| consent::native_key(path).unwrap())
            .collect();
        crate::native::session::primitives::write_json_atomic(
            &store,
            &serde_json::json!({ "trustedWorkspaces": keys }),
        )
        .unwrap();
    };
    let start = Instant::now();
    let wait = Duration::from_secs(30);
    let refused = |expected: &str, availability: Availability, reason_code: &str| {
        let mut clock = FakeClock::new(start);
        let error =
            wait_for_workspace_trust_evidence(&directory, start + wait, &mut clock, &mut || {
                workspace_trust_missing(&workspace, &env, &directory)
            })
            .unwrap_err()
            .to_string();
        assert_eq!(clock.slept, wait, "the wait ends only at the deadline");
        assert_eq!(
            error,
            format!(
                "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {expected}. {TRUST_RECOVERY}"
            )
        );
        let check = workspace_trust_check(&workspace, &env, &directory);
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
        wait_for_workspace_trust_evidence(&directory, start + wait, &mut clock, &mut || {
            workspace_trust_missing(&workspace, &env, &directory)
        })
        .unwrap();
        assert_eq!(clock.slept, Duration::ZERO, "{what}");
        let check = workspace_trust_check(&workspace, &env, &directory);
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
    wait_for_workspace_trust_evidence(&directory, start + wait, &mut clock, &mut || {
        answers.next().unwrap()
    })
    .unwrap();
    assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 3);

    // A later poll's reason replaces an earlier one: the deadline reports the
    // missing evidence of its own poll, not the error of the poll before it.
    let mut answers = [
        Err(anyhow::anyhow!("store busy")),
        Ok(Some(TRUST_STORE_MISSING)),
    ]
    .into_iter();
    let mut clock = FakeClock::new(start);
    let error = wait_for_workspace_trust_evidence(
        &directory,
        start + STARTUP_POLL_INTERVAL,
        &mut clock,
        &mut || answers.next().unwrap(),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        format!(
            "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {TRUST_STORE_MISSING}. {TRUST_RECOVERY}"
        )
    );
    assert_eq!(clock.slept, STARTUP_POLL_INTERVAL);

    // An unreadable store is not an approval, and the report says why.
    fs::write(&store, b"{\"trustedWorkspaces\":true}").unwrap();
    let mut clock = FakeClock::new(start);
    let error =
        wait_for_workspace_trust_evidence(&directory, start + wait, &mut clock, &mut || {
            workspace_trust_missing(&workspace, &env, &directory)
        })
        .unwrap_err()
        .to_string();
    let read_error = format!(
        "{:#}",
        workspace_trust_missing(&workspace, &env, &directory).unwrap_err()
    );
    assert!(read_error.contains("trustedWorkspaces"), "{read_error}");
    assert_eq!(
        error,
        format!(
            "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: Agy's trust evidence could not be read ({read_error}). {TRUST_RECOVERY}"
        )
    );
    assert_eq!(clock.slept, wait);
    assert_eq!(
        workspace_trust_check(&workspace, &env, &directory).availability,
        Availability::Unknown
    );
}

// A session launched with Agy's session-only setting needs no entry in Agy's store, and it
// still needs its own log line: the setting is TAB's configuration, the line is Agy's evidence
// that this process took it. Only this session's own record of the Agy setting counts.
#[test]
fn the_session_only_setting_stands_for_the_store_entry_but_not_for_the_log_line() {
    use super::super::super::super::consent;
    use super::super::super::super::diagnostics::Availability;
    use super::super::{APPLIED_MARKER, SESSION_SETTINGS_FILE};
    use crate::native::session::primitives::write_json_atomic;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let directory = session_with_status(&root, "session-trust2", "running");
    let log_path = directory.join(AGY_LOG_FILE);
    let env = consent::Environment::at_home(&root);
    let consent_record = directory.join("workspace-consent.json");
    let applied = |marker: serde_json::Value| {
        write_json_atomic(
            &consent_record,
            &serde_json::json!({"state": "verified", "applied": marker}),
        )
        .unwrap();
    };
    let start = Instant::now();
    let wait = Duration::from_secs(30);
    let gate = || {
        let mut clock = FakeClock::new(start);
        let outcome =
            wait_for_workspace_trust_evidence(&directory, start + wait, &mut clock, &mut || {
                workspace_trust_missing(&workspace, &env, &directory)
            })
            .map_err(|error| error.to_string());
        (outcome, workspace_trust_check(&workspace, &env, &directory))
    };
    let refused = |reason: &str| {
        Err(format!(
            "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {reason}. {TRUST_RECOVERY}"
        ))
    };

    // No Agy store at all: the setting alone with this process's load line opens the gate.
    applied(serde_json::json!(APPLIED_MARKER));
    fs::write(&log_path, REAL_SUCCESS_STARTUP).unwrap();
    let (outcome, check) = gate();
    assert_eq!(outcome, Ok(()));
    assert_eq!(check.availability, Availability::Available);
    assert_eq!(check.reason_code, "agy_workspace_trusted");
    let row = serde_json::to_value(&check).unwrap();
    assert!(
        row["detail"]
            .as_str()
            .unwrap()
            .contains("session-only setting"),
        "{row}"
    );
    assert_eq!(
        row["evidence"]["session_settings"],
        serde_json::json!(directory.join(SESSION_SETTINGS_FILE))
    );
    // Nor does an unreadable store matter then: it is not read.
    fs::create_dir_all(root.join(".gemini/antigravity-cli")).unwrap();
    let store = root.join(".gemini/antigravity-cli/settings.json");
    fs::write(&store, b"{\"trustedWorkspaces\":true}").unwrap();
    assert_eq!(gate().0, Ok(()));

    // Without the load line Agy may have ignored the setting: no paste, and the report says
    // which fact is missing.
    fs::write(&log_path, REAL_FAILURE_STARTUP).unwrap();
    let (outcome, check) = gate();
    assert_eq!(outcome, refused(TRUST_SETTING_UNVERIFIED));
    assert_eq!(check.availability, Availability::Unknown);
    assert_eq!(check.reason_code, "agy_session_trust_unverified");
    fs::remove_file(&log_path).unwrap();
    assert_eq!(gate().0, refused(TRUST_SETTING_UNVERIFIED));

    // Another adapter's marker, the dialog's own markers, a marker that is not a string and an
    // unreadable record are not this setting: the store rule applies, here an absent entry.
    crate::native::session::primitives::write_private(&log_path, REAL_SUCCESS_STARTUP.as_bytes())
        .unwrap();
    fs::remove_file(&store).unwrap();
    write_json_atomic(&store, &serde_json::json!({"trustedWorkspaces": []})).unwrap();
    for marker in [
        serde_json::json!("codex-config-override"),
        serde_json::json!("pi-approve-once"),
        serde_json::json!("provider-workspace-dialog"),
        serde_json::json!("workspace-dialog-response-pending"),
        serde_json::json!(true),
        serde_json::Value::Null,
    ] {
        applied(marker.clone());
        let (outcome, check) = gate();
        assert_eq!(outcome, refused(TRUST_STORE_MISSING), "{marker}");
        assert_eq!(check.reason_code, "agy_workspace_untrusted", "{marker}");
        assert_eq!(
            serde_json::to_value(&check).unwrap()["evidence"]["session_settings"],
            serde_json::Value::Null,
            "{marker}"
        );
    }
    fs::write(&consent_record, b"{").unwrap();
    assert_eq!(gate().0, refused(TRUST_STORE_MISSING));
}
