//! `inspect --json` capabilities against the actions' own admissions. `cancel`: every
//! refusal, provider support, unreadable evidence and the legacy lock-free snapshot. `tell`:
//! each record gate in the action's order, with the action's own text, and the gates it
//! names instead of evaluating. `questions`: each adapter's configuration. No record is
//! changed by the observation.
use super::*;
use crate::native::reopen::tests::snapshot_directory;
use crate::native::session::cancel::{self, Refusal};
use crate::native::session::test_support::lifecycle::{
    reaped_child_pid, test_windows_process_identity,
};
use crate::native::session::test_support::{
    REQUESTS_DIRECTORY, SESSION_OWNER_FILE, TERMINAL_HANDLE_FILE, TERMINAL_TOMBSTONE_FILE,
    TURN_CLAIM_FILE, TURN_CLAIM_LOCK_FILE, write_test_manifest,
};
use crate::native::session::{
    CoreRecord, SessionState, Store, TURN_ALREADY_CLAIMED, hold, turn, update_status,
};
use crate::native::terminal::ownership::NativeSessionOwner;

const SESSION: &str = "session-capability";

struct Fixture {
    _root: tempfile::TempDir,
    directory: PathBuf,
    token: String,
    request_id: String,
}

/// A working session of `provider` with one active claim and its receipt.
fn working(provider: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(SESSION);
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory, provider);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let token = claim.token().to_owned();
    let request_id = claim.receipt().request_id.clone();
    claim.retain();
    Fixture {
        _root: root,
        directory,
        token,
        request_id,
    }
}

fn pi_ready(directory: &Path, schema: u32, token: &str) {
    fs::write(
        directory.join("pi-startup-ready.json"),
        serde_json::to_vec(
            &json!({"schema": schema, "claim_token": token, "session_id": "native"}),
        )
        .unwrap(),
    )
    .unwrap();
}

/// The observation, with every session file byte-for-byte unchanged by it.
fn observe(directory: &Path) -> Value {
    let before = snapshot_directory(directory);
    let value = inspect_value(&Reader::open_unchecked(directory), SESSION).unwrap();
    assert_eq!(
        snapshot_directory(directory),
        before,
        "inspect changed a record"
    );
    let capabilities = &value["capabilities"];
    assert_eq!(capabilities["schema_version"], 1);
    assert!(capabilities["observed_unix_ms"].as_u64().unwrap() > 0);
    capabilities["cancel"].clone()
}

/// The action's refusal for the same records: same reason code, same rendered message.
fn assert_action_refuses(directory: &Path, observed: &Value) {
    let error = cancel::request(&Store::open_unchecked(directory), SESSION).unwrap_err();
    let refusal = error
        .downcast_ref::<Refusal>()
        .expect("the action refuses with a Refusal");
    assert_eq!(observed["reason"], refusal.reason().code());
    assert_eq!(observed["detail"], refusal.to_string());
    assert_eq!(
        observed.get("request_id").and_then(Value::as_str),
        refusal.request_id()
    );
    assert!(
        !Reader::open_unchecked(directory)
            .record(CoreRecord::Cancel)
            .path()
            .exists()
            || observed["reason"] == "cancel_already_recorded"
    );
}

fn assert_no_token(observed: &Value, token: &str) {
    assert!(
        !observed.to_string().contains(token),
        "claim token exposed: {observed}"
    );
}

#[test]
fn pi_current_claim_is_admissible_and_the_action_then_records_once() {
    let fixture = working("pi");
    pi_ready(&fixture.directory, 2, &fixture.token);
    let observed = observe(&fixture.directory);
    assert_eq!(
        observed,
        json!({"state": "admissible", "reason": "admission_passed", "request_id": fixture.request_id})
    );
    assert_no_token(&observed, &fixture.token);
    let recorded = cancel::request(&Store::open_unchecked(&fixture.directory), SESSION).unwrap();
    assert_eq!(recorded.request_id, fixture.request_id);

    let observed = observe(&fixture.directory);
    assert_eq!(observed["state"], "refused");
    assert_eq!(observed["reason"], "cancel_already_recorded");
    assert_eq!(observed["request_id"], fixture.request_id.as_str());
    assert_no_token(&observed, &fixture.token);
    assert_action_refuses(&fixture.directory, &observed);
}

#[test]
fn providers_without_cancel_report_unsupported_with_the_request() {
    for provider in ["codex", "claude", "agy"] {
        let fixture = working(provider);
        let observed = observe(&fixture.directory);
        assert_eq!(observed["state"], "unsupported", "{provider}");
        assert_eq!(observed["reason"], "provider_unsupported");
        assert_eq!(observed["request_id"], fixture.request_id.as_str());
        assert_action_refuses(&fixture.directory, &observed);
    }
    // Pi readiness that does not offer cancel for the current Claim follows the adapter:
    // schema 1, another Claim, an absent record and an unusable record are unsupported.
    let fixture = working("pi");
    for record in [
        Some(json!({"schema": 1, "claim_token": fixture.token, "session_id": "native"})),
        Some(json!({"schema": 2, "claim_token": "1-2-3", "session_id": "native"})),
        None,
    ] {
        let path = fixture.directory.join("pi-startup-ready.json");
        let _ = fs::remove_file(&path);
        if let Some(record) = record {
            fs::write(&path, record.to_string()).unwrap();
        }
        let observed = observe(&fixture.directory);
        assert_eq!(observed["state"], "unsupported");
        assert_eq!(observed["reason"], "provider_unsupported");
        assert_action_refuses(&fixture.directory, &observed);
    }
    fs::write(fixture.directory.join("pi-startup-ready.json"), "not-json").unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(observed["state"], "unsupported");
    assert_action_refuses(&fixture.directory, &observed);
}

#[test]
fn state_claim_and_receipt_refusals_match_the_action() {
    let fixture = working("pi");
    pi_ready(&fixture.directory, 2, &fixture.token);
    update_status(&fixture.directory, SessionState::Ready, None, None).unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(observed["state"], "refused");
    assert_eq!(observed["reason"], "state_not_running");
    assert!(observed.get("request_id").is_none());
    assert_action_refuses(&fixture.directory, &observed);

    // Ready -> Working is no lifecycle transition; write the record as the cancel tests do.
    let store = Store::open_unchecked(&fixture.directory);
    let mut working = store.status().unwrap();
    working.state = SessionState::Working;
    store.write_status(&working).unwrap();
    fs::remove_file(fixture.directory.join(TURN_CLAIM_FILE)).unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(observed["reason"], "no_active_claim");
    assert!(observed.get("request_id").is_none());
    assert_action_refuses(&fixture.directory, &observed);

    fs::write(
        fixture.directory.join(TURN_CLAIM_FILE),
        format!("{}\n", fixture.token),
    )
    .unwrap();
    fs::remove_file(
        fixture
            .directory
            .join(REQUESTS_DIRECTORY)
            .join(format!("{}.json", fixture.token)),
    )
    .unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(observed["state"], "refused");
    assert_eq!(observed["reason"], "no_active_receipt");
    assert!(observed.get("request_id").is_none());
    assert_action_refuses(&fixture.directory, &observed);
}

#[test]
fn a_published_result_refuses_before_provider_support() {
    let fixture = crate::native::tests::seed_close_fixture(turn::JournaledEventState::Committed);
    // The close fixture names its own session directory.
    let reader = Reader::open_unchecked(&fixture.directory);
    let before = snapshot_directory(&fixture.directory);
    let value = inspect_value(&reader, "session-fault").unwrap();
    assert_eq!(snapshot_directory(&fixture.directory), before);
    let observed = &value["capabilities"]["cancel"];
    assert_eq!(observed["state"], "refused");
    assert_eq!(observed["reason"], "result_already_published");
    assert_eq!(observed["request_id"], value["requests"][0]["request_id"]);
    assert!(observed["request_id"].as_str().is_some());
    let error =
        cancel::request(&Store::open_unchecked(&fixture.directory), "session-fault").unwrap_err();
    let refusal = error.downcast_ref::<Refusal>().unwrap();
    assert_eq!(observed["reason"], refusal.reason().code());
    assert_eq!(observed["detail"], refusal.to_string());
}

#[test]
fn unreadable_evidence_is_unknown_not_unsupported() {
    let fixture = working("pi");
    pi_ready(&fixture.directory, 2, &fixture.token);
    fs::write(fixture.directory.join("cancel.json"), "{").unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(observed["state"], "unknown");
    assert_eq!(observed["reason"], "evidence_unreadable");
    assert!(observed["detail"].as_str().is_some());
    assert!(observed.get("request_id").is_none());
    assert_no_token(&observed, &fixture.token);
    // The action fails on the same evidence without refusing or writing.
    let error = cancel::request(&Store::open_unchecked(&fixture.directory), SESSION).unwrap_err();
    assert!(error.downcast_ref::<Refusal>().is_none());
    assert_eq!(
        fs::read(fixture.directory.join("cancel.json")).unwrap(),
        b"{"
    );
}

#[test]
fn a_snapshot_without_the_lifecycle_lock_is_unknown() {
    let fixture = working("pi");
    pi_ready(&fixture.directory, 2, &fixture.token);
    fs::remove_file(fixture.directory.join(TURN_CLAIM_LOCK_FILE)).unwrap();
    let observed = observe(&fixture.directory);
    assert_eq!(
        observed,
        json!({"state": "unknown", "reason": "lifecycle_lock_absent"})
    );
    assert!(!fixture.directory.join(TURN_CLAIM_LOCK_FILE).exists());
}

/// The terminal kind this platform supports. Linux supports none, so a handle of this kind
/// is judged unsupported there.
fn supported_kind() -> terminal::TerminalKind {
    if cfg!(windows) {
        terminal::TerminalKind::WindowsConsole
    } else {
        terminal::TerminalKind::AppleTerminal
    }
}

fn write_handle(directory: &Path, kind: terminal::TerminalKind, session: Option<&str>) {
    crate::native::session::primitives::write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind,
            id: "4242".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: session.map(str::to_owned),
            windows_process_identity: None,
            wezterm_mux: None,
        },
    )
    .unwrap();
}

fn write_owner(directory: &Path, pid: u32, session: Option<&str>) {
    Store::open_unchecked(directory)
        .write_owner(&NativeSessionOwner {
            pid,
            managed_session_id: session.map(str::to_owned),
            windows_process_identity: test_windows_process_identity(pid),
            ..NativeSessionOwner::default()
        })
        .unwrap();
}

/// A ready session of `provider` without a claim: the lifecycle lock, a bound live owner
/// (this test process) and a terminal handle bound to the session.
fn ready(provider: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(SESSION);
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory, provider);
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    drop(Store::open_unchecked(&directory).lock().unwrap());
    write_owner(&directory, std::process::id(), Some(SESSION));
    write_handle(&directory, supported_kind(), Some(SESSION));
    Fixture {
        _root: root,
        directory,
        token: String::new(),
        request_id: String::new(),
    }
}

/// The whole capabilities object, with every session file byte-for-byte unchanged.
fn observe_all(directory: &Path) -> Value {
    let before = snapshot_directory(directory);
    let value = inspect_value(&Reader::open_unchecked(directory), SESSION).unwrap();
    assert_eq!(
        snapshot_directory(directory),
        before,
        "inspect changed a record"
    );
    value["capabilities"].clone()
}

/// Renders an Observation the test adjusted, as inspect renders it, with every session file
/// byte-for-byte unchanged.
fn render(directory: &Path, adjust: impl FnOnce(&mut observation::Observation)) -> Value {
    let before = snapshot_directory(directory);
    let reader = Reader::open_unchecked(directory);
    let mut observed = observation::Observation::read_locked(&reader).unwrap();
    adjust(&mut observed);
    let mut clock = SystemClock;
    let value = super::inspect::value(
        observed,
        &reader,
        SESSION,
        &mut crate::native::session::origin::OriginEnvironment::production(&mut clock),
    )
    .unwrap();
    assert_eq!(
        snapshot_directory(directory),
        before,
        "inspect changed a record"
    );
    value["capabilities"].clone()
}

/// For rows past the surface gate. The terminals a platform supports differ (Linux supports
/// none), so these rows replace that one judgment with a bound, supported handle, as
/// inspect's own tests replace judgments; every other gate, the owner included, is judged
/// from the records by the production observers.
fn observe_past_surface(directory: &Path) -> Value {
    render(directory, |observed| {
        observed.judgments.surface = observation::judgment::Surface::Active {
            kind: supported_kind(),
            supported: true,
            bound: true,
            empty_id: false,
        };
    })
}

fn refused(reason: &str, detail: &str) -> Value {
    json!({"state": "refused", "reason": reason, "detail": detail})
}

fn admissible(unevaluated: &[&str]) -> Value {
    json!({"state": "admissible", "reason": "record_gates_passed", "unevaluated": unevaluated})
}

#[test]
fn tell_record_gates_follow_the_actions_order_and_report_its_own_text() {
    let fixture = ready("codex");
    let directory = &fixture.directory;
    let store = Store::open_unchecked(directory);
    let reader = Reader::open_unchecked(directory);
    assert_eq!(
        observe_past_surface(directory)["tell"],
        admissible(&[
            "terminal_surface_probe",
            "provider_delivery_readiness",
            "codex_native_queue_version",
            "codex_thread",
        ])
    );

    // tell checks the Hold first, so a held session that is not ready reports the Hold.
    hold::change(&store, false).unwrap();
    let mut status = store.status().unwrap();
    status.state = SessionState::Working;
    store.write_status(&status).unwrap();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(
        observed,
        refused(
            "session_held",
            &format!("session {SESSION} is held; release the hold before sending a follow-up"),
        )
    );
    let action = hold::permit(&reader, SESSION).unwrap_err();
    assert_eq!(observed["detail"], format!("{action:#}"));
    assert_eq!(crate::native::failure_kind::classify(&action), None);

    store.record(CoreRecord::Hold).remove().unwrap();
    store
        .record(CoreRecord::Hold)
        .write_private(b"malformed")
        .unwrap();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(
        observed,
        refused(
            "hold_unreadable",
            &format!(
                "session {SESSION} has an unreadable hold record; inspect it before sending a follow-up"
            ),
        )
    );
    assert_eq!(
        observed["detail"],
        format!("{:#}", hold::permit(&reader, SESSION).unwrap_err())
    );

    // Without a Hold, the stored state is decided as follow_up_admission decides it.
    hold::change(&store, true).unwrap();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(
        observed,
        refused(
            "state_not_ready",
            &format!("session {SESSION} is working; tell requires the ready state"),
        )
    );
    let action = hold::follow_up_admission(&reader, SESSION).unwrap_err();
    assert_eq!(observed["detail"], format!("{action:#}"));
    assert_eq!(crate::native::failure_kind::classify(&action), None);
    let claimed = turn::claim_ready(&store, SESSION, &[]).err().unwrap();
    assert_eq!(format!("{claimed:#}"), format!("{action:#}"));

    // A ready session that still has a claim refuses another one.
    status.state = SessionState::Ready;
    store.write_status(&status).unwrap();
    turn::claim(&store, &[]).unwrap().retain();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(observed, refused("turn_claimed", TURN_ALREADY_CLAIMED));
    let action = turn::claim_ready(&store, SESSION, &[]).err().unwrap();
    assert!(
        format!("{action:#}").starts_with(TURN_ALREADY_CLAIMED),
        "{action:#}"
    );
}

#[test]
fn the_surface_gate_judges_the_recorded_handle_and_leaves_the_probe_unevaluated() {
    let fixture = ready("pi");
    let directory = &fixture.directory;
    let platform_supports_terminals = cfg!(any(windows, target_os = "macos"));
    let unbound = if platform_supports_terminals {
        "surface_unbound"
    } else {
        "surface_unsupported"
    };
    let other_platform = if cfg!(windows) {
        terminal::TerminalKind::AppleTerminal
    } else {
        terminal::TerminalKind::WindowsConsole
    };
    for (kind, session, reason) in [
        (other_platform, Some(SESSION), "surface_unsupported"),
        (supported_kind(), Some("session-other"), unbound),
        (supported_kind(), None, unbound),
    ] {
        write_handle(directory, kind, session);
        let observed = observe_all(directory)["tell"].clone();
        assert_eq!(observed["state"], "refused", "{kind:?} {session:?}");
        assert_eq!(observed["reason"], reason, "{kind:?} {session:?}");
    }
    fs::remove_file(directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    assert_eq!(
        observe_all(directory)["tell"],
        refused("surface_missing", "no active terminal handle is recorded")
    );
    // tell reads the same handle before its probe and fails without it.
    assert!(Reader::open_unchecked(directory).terminal().is_err());
    crate::native::session::primitives::write_json_atomic(
        &directory.join(TERMINAL_TOMBSTONE_FILE),
        &json!({"consumed": true}),
    )
    .unwrap();
    assert_eq!(
        observe_all(directory)["tell"],
        refused(
            "surface_closed",
            "the terminal surface is closed or being closed"
        )
    );
    fs::remove_file(directory.join(TERMINAL_TOMBSTONE_FILE)).unwrap();
    fs::write(directory.join(TERMINAL_HANDLE_FILE), "{").unwrap();
    let observed = observe_all(directory)["tell"].clone();
    assert_eq!(observed["state"], "unknown");
    assert_eq!(observed["reason"], "evidence_unreadable");
    assert!(observed["detail"].as_str().is_some());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_ready_session_passes_every_record_gate_through_the_production_judgments() {
    let fixture = ready("agy");
    assert_eq!(
        observe_all(&fixture.directory)["tell"],
        admissible(&[
            "terminal_surface_probe",
            "provider_delivery_readiness",
            "agy_trust_store",
            "agy_log_readiness",
        ])
    );
}

#[test]
fn the_owner_gate_is_what_dead_owner_repair_finds_without_repairing() {
    let fixture = ready("codex");
    let directory = &fixture.directory;
    let ended = refused(
        "owner_ended",
        "the recorded session owner is no longer running",
    );
    let unattested = json!({"state": "unknown", "reason": "owner_unattested"});
    let dead = reaped_child_pid();
    let live = std::process::id();
    for (pid, session, expected) in [
        (dead, Some(SESSION), &ended),
        // An unbound legacy record observed gone is the death repair finds too.
        (dead, None, &ended),
        (live, None, &unattested),
        (live, Some("session-other"), &unattested),
    ] {
        write_owner(directory, pid, session);
        assert_eq!(
            &observe_past_surface(directory)["tell"],
            expected,
            "{pid} {session:?}"
        );
    }
    fs::remove_file(directory.join(SESSION_OWNER_FILE)).unwrap();
    assert_eq!(observe_past_surface(directory)["tell"], unattested);
    fs::write(directory.join(SESSION_OWNER_FILE), "{").unwrap();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(observed["state"], "unknown");
    assert_eq!(observed["reason"], "evidence_unreadable");
    assert!(observed["detail"].as_str().is_some());

    // Repair leaves a closed session's owner unexamined; the stored state refuses it.
    write_owner(directory, dead, Some(SESSION));
    update_status(directory, SessionState::Closed, None, None).unwrap();
    assert_eq!(
        observe_past_surface(directory)["tell"],
        refused(
            "state_not_ready",
            &format!("session {SESSION} is closed; tell requires the ready state"),
        )
    );
}

#[test]
fn an_owner_whose_process_cannot_be_inspected_is_unverified_not_ended() {
    use crate::native::process_identity::ProcessObservation;
    use crate::native::session::owner::ControlAttestation::NotRecorded;
    let fixture = ready("codex");
    let observed = render(&fixture.directory, |observed| {
        observed.evidence.judge_owner_with(
            SESSION,
            &observation::judgment::Observers {
                attest: &|_| {
                    observation::judgment::tests::attestation(
                        ProcessObservation::Unknown("access denied".into()),
                        NotRecorded,
                        Some(true),
                    )
                },
                pid_alive: &|_| panic!("a bound record is attested"),
            },
        );
    });
    assert_eq!(
        observed["tell"],
        json!({"state": "unknown", "reason": "owner_unverified", "detail": "access denied"})
    );
}

#[test]
fn the_adapter_gate_decides_last_and_names_the_waits_it_does_not_evaluate() {
    for (provider, adapter) in [
        ("codex", &["codex_native_queue_version", "codex_thread"][..]),
        ("agy", &["agy_trust_store", "agy_log_readiness"][..]),
        ("pi", &["pi_startup_readiness"][..]),
    ] {
        let fixture = ready(provider);
        let mut expected = vec!["terminal_surface_probe", "provider_delivery_readiness"];
        expected.extend_from_slice(adapter);
        assert_eq!(
            observe_past_surface(&fixture.directory)["tell"],
            admissible(&expected),
            "{provider}"
        );
    }

    // Claude decides from the completion-mode record, with admit's own text.
    let fixture = ready("claude");
    let directory = &fixture.directory;
    let store = Store::open_unchecked(directory);
    assert_eq!(
        observe_past_surface(directory)["tell"],
        refused(
            "claude_legacy_completion",
            if cfg!(windows) {
                "Claude session was launched before native per-turn completion; close and reopen it before sending a new request"
            } else {
                "Claude session was launched before native per-turn completion; inspect its result and terminal, close it when appropriate, then start a fresh ask with the needed context; no message was sent"
            },
        )
    );
    let native = json!({"schema": 1, "mode": "native_v1",
        "reason": "windows_stable_2_1_302_or_newer", "claude_version": "2.1.302"});
    store
        .private("claude-completion-mode.json")
        .write_json(&native)
        .unwrap();
    assert_eq!(
        observe_past_surface(directory)["tell"],
        admissible(&[
            "terminal_surface_probe",
            "provider_delivery_readiness",
            "claude_native_readiness",
        ])
    );
    store
        .private("claude-completion-mode.json")
        .remove()
        .unwrap();
    store
        .private("claude-completion-mode.json")
        .write_private(b"{")
        .unwrap();
    let observed = observe_past_surface(directory)["tell"].clone();
    assert_eq!(observed["state"], "unknown");
    assert_eq!(observed["reason"], "evidence_unreadable");
    assert!(observed["detail"].as_str().is_some());

    // A reopened session names the exclusivity check it cannot evaluate from records.
    store
        .private("claude-completion-mode.json")
        .write_json(&native)
        .unwrap();
    let mut manifest: Value = store.record(CoreRecord::Manifest).json().unwrap();
    manifest["resumed_from"] = json!({"session": "session-source",
        "provider_session_id": "conversation-1", "event_id": "event-1-1.json"});
    store
        .record(CoreRecord::Manifest)
        .write_json(&manifest)
        .unwrap();
    assert_eq!(
        observe_past_surface(directory)["tell"],
        admissible(&[
            "terminal_surface_probe",
            "reopened_conversation_exclusivity",
            "provider_delivery_readiness",
            "claude_native_readiness",
        ])
    );
}

#[test]
fn questions_report_each_adapters_configuration_and_no_answer_path() {
    let answer = json!({"state": "unsupported", "reason": "no_addressed_answer_path"});
    for (provider, emission) in [
        (
            "codex",
            json!({"state": "unavailable", "reason": "hooks_not_injected"}),
        ),
        (
            "claude",
            json!({"state": "unavailable", "reason": "legacy_completion_mode"}),
        ),
        (
            "agy",
            json!({"state": "unsupported", "reason": "no_question_plugin"}),
        ),
        (
            "pi",
            json!({"state": "unsupported", "reason": "no_question_plugin"}),
        ),
    ] {
        let fixture = working(provider);
        assert_eq!(
            observe_all(&fixture.directory)["questions"],
            json!({"emission_observation": emission, "response": answer}),
            "{provider}"
        );
    }
    let fixture = working("codex");
    let store = Store::open_unchecked(&fixture.directory);
    store
        .private("codex-launch-mode.json")
        .write_json(
            &json!({"schema": 1, "lifecycle_hooks": "injected", "reason": null,
            "codex_version": "codex-cli 0.163.0", "vector_codex_version": "0.163.0"}),
        )
        .unwrap();
    assert_eq!(
        observe_all(&fixture.directory)["questions"],
        json!({"emission_observation":
            {"state": "configured", "reason": "hooks_injected_unverified"}, "response": answer})
    );
    store.private("codex-launch-mode.json").remove().unwrap();
    store
        .private("codex-launch-mode.json")
        .write_private(b"{")
        .unwrap();
    let observed = observe_all(&fixture.directory)["questions"].clone();
    assert_eq!(observed["emission_observation"]["state"], "unknown");
    assert_eq!(
        observed["emission_observation"]["reason"],
        "evidence_unreadable"
    );
    assert!(
        observed["emission_observation"]["detail"]
            .as_str()
            .is_some()
    );
    assert_eq!(observed["response"], answer);
}

#[test]
fn a_snapshot_without_the_lifecycle_lock_leaves_every_capability_unknown() {
    let fixture = ready("claude");
    Store::open_unchecked(&fixture.directory)
        .private("claude-completion-mode.json")
        .write_json(&json!({"schema": 1, "mode": "native_v1",
            "reason": "windows_stable_2_1_302_or_newer", "claude_version": "2.1.302"}))
        .unwrap();
    fs::remove_file(fixture.directory.join(TURN_CLAIM_LOCK_FILE)).unwrap();
    let unknown = json!({"state": "unknown", "reason": "lifecycle_lock_absent"});
    let capabilities = observe_all(&fixture.directory);
    assert_eq!(capabilities["schema_version"], 1);
    assert_eq!(capabilities["cancel"], unknown);
    assert_eq!(capabilities["tell"], unknown);
    assert_eq!(
        capabilities["questions"],
        json!({"emission_observation": unknown, "response": unknown})
    );
    assert!(!fixture.directory.join(TURN_CLAIM_LOCK_FILE).exists());
}
