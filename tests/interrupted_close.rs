//! An interrupted close must never hide a result the provider already published. These
//! fixtures are the records each stop of a close leaves behind once its tombstone exists
//! (tombstone -> status -> claim release -> journal removal), for a completion that had
//! already written its event. Every query command must report the result as `completed`
//! before `sessions` converges the close, and again afterwards.
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SESSION: &str = "session-closing";
const TARGET: &str = "session-target";
const CLAIM: &str = "7-7-7";
const REQUEST: &str = "request-late";
const EVENT: &str = "event-7.json";

/// The step after which a close stopped, for a completion whose journal and event exist.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// The tombstone was written; status, claim, and journal are untouched.
    Tombstone,
    /// The status was rewritten from the tombstone.
    Status,
    /// The turn claim was released; the journal is still in place.
    ClaimRelease,
    /// The journal was removed; only the legacy cleanup never ran.
    JournalRemoval,
}

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new(stop: Stop) -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        fixture.session(TARGET, "claude", "ready");
        fixture.session(SESSION, "codex", "working");
        let directory = fixture.directory(SESSION);
        fs::write(directory.join("events").join(EVENT), event_text()).unwrap();
        fs::create_dir(directory.join("requests")).unwrap();
        write(
            &directory.join("requests").join(format!("{CLAIM}.json")),
            &json!({"schema": 1, "request_id": REQUEST, "claim_token": CLAIM,
                "event_file": EVENT, "created_unix_ms": 3}),
        );
        write(&directory.join("closed.json"), &tombstone());
        if matches!(stop, Stop::Tombstone) {
            fs::write(directory.join("turn.claim"), CLAIM).unwrap();
            write(&directory.join("turn.completion.json"), &journal());
            return fixture;
        }
        write(&directory.join("status.json"), &tombstone());
        match stop {
            Stop::Tombstone => unreachable!(),
            Stop::Status => {
                fs::write(directory.join("turn.claim"), CLAIM).unwrap();
                write(&directory.join("turn.completion.json"), &journal());
            }
            Stop::ClaimRelease => {
                write(&directory.join("turn.completion.json"), &journal());
            }
            Stop::JournalRemoval => (),
        }
        fixture
    }

    fn directory(&self, id: &str) -> PathBuf {
        self.root.path().join(id)
    }

    fn session(&self, id: &str, provider: &str, state: &str) {
        let directory = self.directory(id);
        fs::create_dir_all(directory.join("events")).unwrap();
        write(
            &directory.join("manifest.json"),
            &json!({
                "schema": 1, "id": id, "provider": provider, "provider_path": provider,
                "provider_version": "0.0.0", "workspace": self.root.path().canonicalize().unwrap(),
                "title": "interrupted close fixture", "model": null, "effort": null,
                "yolo": false, "created_unix_ms": 1
            }),
        );
        write(
            &directory.join("status.json"),
            &json!({"state": state, "generation": 2, "updated_unix_ms": 2, "exit_code": null, "error": null}),
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
}

fn tombstone() -> Value {
    json!({"state": "closed", "generation": 3, "updated_unix_ms": 9, "exit_code": null,
        "error": "closed by the maintainer"})
}

/// The event exactly as the completion wrote it: the journal's event serialised as
/// pretty JSON in record field order, which is the byte form publication compares.
fn event_text() -> String {
    [
        "{",
        "  \"provider\": \"codex\",",
        "  \"message\": \"late result\",",
        "  \"error\": null,",
        "  \"provider_session_id\": \"thread\",",
        "  \"turn_id\": \"turn-7\",",
        "  \"created_unix_ms\": 3",
        "}",
    ]
    .join("\n")
}

fn journal() -> Value {
    json!({"schema": 1, "claim_token": CLAIM, "event_file": EVENT,
        "event": serde_json::from_str::<Value>(&event_text()).unwrap(),
        "status_error": null, "status_state": "ready"})
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_completed(fixture: &Fixture, phase: &str) {
    let result = success(fixture.run(&["result", SESSION, "--request", REQUEST, "--json"]));
    assert_eq!(result["request_state"], "completed", "{phase}: {result}");
    assert_eq!(result["result"], "late result", "{phase}");
    assert_eq!(result["event_id"], EVENT, "{phase}");

    let waited = success(fixture.run(&[
        "result",
        SESSION,
        "--request",
        REQUEST,
        "--wait",
        "--timeout-secs",
        "1",
        "--json",
    ]));
    assert_eq!(waited["ok"], true, "{phase}: {waited}");
    assert_eq!(waited["request_state"], "completed", "{phase}: {waited}");
    assert_eq!(waited["result"], "late result", "{phase}");
    assert_eq!(waited["timed_out"], Value::Null, "{phase}");

    let search = success(fixture.run(&["search", "late result", "--all-workspaces", "--json"]));
    let hits = search["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{phase}: {search}");
    assert_eq!(hits[0]["session"], SESSION, "{phase}");
    assert_eq!(hits[0]["request_id"], REQUEST, "{phase}");
    assert_eq!(hits[0]["event_id"], EVENT, "{phase}");
    assert_eq!(search["incomplete"], false, "{phase}: {search}");

    // Resolution passes; the fixture target has no terminal handle, so the tell then
    // fails on the target, not on the attachment.
    let output = fixture.run(&[
        "tell",
        TARGET,
        "--prompt",
        "continue from the attached result",
        "--context-result",
        &format!("{SESSION}/{REQUEST}"),
        "--json",
    ]);
    assert!(!output.status.success(), "{phase}");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!text.contains("cannot be attached"), "{phase}: {text}");
    assert!(!text.contains("context result"), "{phase}: {text}");
    assert!(text.contains("terminal.json"), "{phase}: {text}");
}

#[test]
fn a_published_result_stays_completed_across_every_stop_of_an_interrupted_close() {
    for stop in [
        Stop::Tombstone,
        Stop::Status,
        Stop::ClaimRelease,
        Stop::JournalRemoval,
    ] {
        let fixture = Fixture::new(stop);
        let directory = fixture.directory(SESSION);
        let tombstone_before = fs::read(directory.join("closed.json")).unwrap();
        let event_before = fs::read(directory.join("events").join(EVENT)).unwrap();

        assert_completed(&fixture, &format!("{stop:?} before sessions"));

        let listing = success(fixture.run(&["sessions", "--json"]));
        let session = listing
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == SESSION)
            .unwrap_or_else(|| panic!("{stop:?}: {listing}"));
        assert_eq!(session["state"], "closed", "{stop:?}: {listing}");
        assert_eq!(session["results"], 1, "{stop:?}: {listing}");

        assert_completed(&fixture, &format!("{stop:?} after sessions"));
        let result = success(fixture.run(&["result", SESSION, "--request", REQUEST, "--json"]));
        assert_eq!(result["session_state"], "closed", "{stop:?}");
        assert_eq!(
            result["session_error"], "closed by the maintainer",
            "{stop:?}"
        );
        assert_eq!(result["recovery_required"], false, "{stop:?}");
        assert!(!directory.join("turn.claim").exists(), "{stop:?}");
        assert!(!directory.join("turn.completion.json").exists(), "{stop:?}");
        assert_eq!(
            fs::read(directory.join("closed.json")).unwrap(),
            tombstone_before,
            "{stop:?}: the tombstone changed"
        );
        assert_eq!(
            fs::read(directory.join("events").join(EVENT)).unwrap(),
            event_before,
            "{stop:?}: the event changed"
        );
        let status: Value =
            serde_json::from_slice(&fs::read(directory.join("status.json")).unwrap()).unwrap();
        assert_eq!(status["state"], "closed", "{stop:?}");
        assert_eq!(status["generation"], 3, "{stop:?}");
    }
}
