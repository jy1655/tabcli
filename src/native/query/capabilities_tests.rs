//! `inspect --json` capabilities.cancel against the `cancel` action's own admission: every
//! refusal, provider support, unreadable evidence and the legacy lock-free snapshot, with no
//! record changed by the observation.
use super::*;
use crate::native::reopen::tests::snapshot_directory;
use crate::native::session::cancel::{self, Refusal};
use crate::native::session::test_support::{
    REQUESTS_DIRECTORY, TURN_CLAIM_FILE, TURN_CLAIM_LOCK_FILE, write_test_manifest,
};
use crate::native::session::{CoreRecord, SessionState, Store, turn, update_status};

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
