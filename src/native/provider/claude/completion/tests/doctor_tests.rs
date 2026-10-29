use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

// Every file with its bytes and every directory, so a created lock, record or plugin
// directory shows as a difference.
fn snapshot(path: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            result.insert(entry.path(), None);
            result.extend(snapshot(&entry.path()));
        } else {
            result.insert(entry.path(), Some(std::fs::read(entry.path()).unwrap()));
        }
    }
    result
}

// The rendered rows exactly as doctor's JSON places them in `checks`.
fn rows(directory: Option<&Path>) -> [Value; 2] {
    let before = directory.map(snapshot);
    let rows = doctor_checks(directory).map(|check| serde_json::to_value(check).unwrap());
    assert_eq!(directory.map(snapshot), before, "doctor changed records");
    assert_eq!(rows[0]["id"], "claude_completion");
    assert_eq!(rows[1]["id"], "claude_completion_diagnostic");
    rows
}

fn put(directory: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(directory.join(name), bytes).unwrap();
}

fn mode_record(schema: u32, mode: Mode) -> Vec<u8> {
    serde_json::to_vec(&LaunchMode {
        schema,
        mode,
        reason: "macos_stable_2_1_302_or_newer".into(),
        claude_version: Some("2.1.309 (Claude Code)".into()),
    })
    .unwrap()
}

fn admitted(f: &Fixture) -> bool {
    let now = Instant::now();
    admit(
        f.root.path(),
        &f.token,
        now + Duration::from_millis(100),
        false,
        &mut FakeClock::new(now),
    )
    .is_ok()
}

#[test]
fn doctor_without_a_session_scope_reads_no_record_and_says_so() {
    let [completion, diagnostic] = rows(None);
    for row in [&completion, &diagnostic] {
        assert_eq!(row["availability"], "unknown");
        assert_eq!(row["reason_code"], "session_required");
        assert_eq!(row["evidence"]["source"], "no_session_scope");
    }
    for field in ["mode", "readiness"] {
        assert_eq!(
            completion["evidence"][field],
            json!({"observation": "not_read"})
        );
    }
    assert_eq!(
        diagnostic["evidence"]["latest_diagnostic"],
        json!({"observation": "not_read"})
    );
}

// Each mode record maps to its own reason, and only an observed native_v1 record reaches
// the readiness verdict. Unavailable follows what the live reader concludes from the record.
#[test]
fn doctor_keeps_absent_legacy_unknown_malformed_and_unreadable_modes_apart() {
    let oversized = format!(
        "{{\"schema\":1,\"mode\":\"native_v1\",\"reason\":\"{}\",\"claude_version\":null}}",
        "x".repeat(super::super::diagnose::record_limit() as usize)
    );
    let cases = [
        (
            "absent",
            None,
            "claude_completion_mode_absent",
            "unavailable",
            "not_observed",
        ),
        (
            "legacy_stop",
            Some(mode_record(1, Mode::LegacyStop)),
            "claude_completion_legacy_stop",
            "unavailable",
            "observed",
        ),
        (
            "native_v1",
            Some(mode_record(1, Mode::NativeV1)),
            "claude_completion_readiness_not_observed",
            "unknown",
            "observed",
        ),
        (
            "schema",
            Some(mode_record(2, Mode::NativeV1)),
            "claude_completion_mode_unrecognized",
            "unavailable",
            "unrecognized",
        ),
        (
            "variant",
            Some(br#"{"schema":1,"mode":"native_v2","reason":"r","claude_version":null}"#.to_vec()),
            "claude_completion_mode_unrecognized",
            "unavailable",
            "unrecognized",
        ),
        (
            "fields",
            Some(br#"{"schema":1,"mode":"native_v1"}"#.to_vec()),
            "claude_completion_mode_malformed",
            "unavailable",
            "malformed",
        ),
        (
            "syntax",
            Some(b"{".to_vec()),
            "claude_completion_mode_malformed",
            "unavailable",
            "malformed",
        ),
        (
            "oversized",
            Some(oversized.into_bytes()),
            "claude_completion_mode_unreadable",
            "unknown",
            "unreadable",
        ),
    ];
    let mut reasons = BTreeSet::new();
    for (case, record, reason, availability, observation) in cases {
        let directory = tempfile::tempdir().unwrap();
        if let Some(record) = &record {
            put(directory.path(), MODE_FILE, record);
        }
        let [completion, _] = rows(Some(directory.path()));
        assert_eq!(completion["reason_code"], reason, "{case}");
        assert_eq!(completion["availability"], availability, "{case}");
        let mode = &completion["evidence"]["mode"];
        assert_eq!(mode["observation"], observation, "{case}");
        assert_eq!(mode["file"], MODE_FILE, "{case}");
        // The live reader refuses every record that doctor does not report as observed
        // (an oversized record aside: doctor's bound is its own and only says unknown).
        if case != "oversized" {
            assert_eq!(
                native_mode(directory.path()).is_ok(),
                matches!(observation, "observed" | "not_observed"),
                "{case}"
            );
        }
        reasons.insert((reason, observation));
    }
    // native_v1 and legacy_stop share `observed` but not their reasons; malformed records
    // share a reason with each other only.
    assert_eq!(reasons.len(), 6);
}

#[test]
fn doctor_mode_evidence_echoes_only_bounded_sanitized_text() {
    let directory = tempfile::tempdir().unwrap();
    let mode = format!("\u{1b}[2Jnative_{}", "v".repeat(200));
    put(
        directory.path(),
        MODE_FILE,
        json!({"schema": 1, "mode": mode, "reason": "r", "claude_version": null})
            .to_string()
            .as_bytes(),
    );
    let [completion, _] = rows(Some(directory.path()));
    let echoed = completion["evidence"]["mode"]["mode"].as_str().unwrap();
    assert!(!echoed.contains('\u{1b}'));
    assert!(echoed.starts_with("\\u{1b}[2Jnative_"));
    assert_eq!(echoed.chars().count(), 64);

    let directory = tempfile::tempdir().unwrap();
    put(directory.path(), MODE_FILE, &mode_record(1, Mode::NativeV1));
    let [completion, _] = rows(Some(directory.path()));
    assert_eq!(
        completion["evidence"]["mode"],
        json!({"file": MODE_FILE, "observation": "observed", "schema": 1, "mode": "native_v1",
            "reason": "macos_stable_2_1_302_or_newer", "claude_version_at_launch": "2.1.309 (Claude Code)"})
    );
}

// Record validity matches admission, while availability remains unknown for historical
// readiness. A recorded version alone never establishes readiness.
#[test]
fn doctor_readiness_matches_admission_and_never_follows_the_version() {
    let ready = |value: Value| Some(value.to_string().into_bytes());
    let oversized = json!({"schema": 1, "revision": REVISION, "conversation": CONVERSATION,
        "pad": "x".repeat(super::super::diagnose::record_limit() as usize)});
    let large: u64 = 1 << 40;
    let cases = [
        ("absent", None, "claude_completion_readiness_not_observed", "unknown"),
        (
            "valid",
            ready(json!({"schema": 1, "revision": REVISION, "conversation": CONVERSATION})),
            "claude_completion_readiness_recorded",
            "unknown",
        ),
        (
            "revision",
            ready(json!({"schema": 1, "revision": 2, "conversation": CONVERSATION})),
            "claude_completion_readiness_unrecognized",
            "unavailable",
        ),
        (
            "large_revision",
            ready(json!({"schema": 1, "revision": large, "conversation": CONVERSATION})),
            "claude_completion_readiness_unrecognized",
            "unavailable",
        ),
        (
            "schema",
            ready(json!({"schema": 2, "revision": REVISION, "conversation": CONVERSATION})),
            "claude_completion_readiness_unrecognized",
            "unavailable",
        ),
        (
            "conversation",
            ready(json!({"schema": 1, "revision": REVISION, "conversation": "not-a-uuid"})),
            "claude_completion_readiness_malformed",
            "unavailable",
        ),
        (
            "missing",
            ready(json!({"schema": 1, "revision": REVISION})),
            "claude_completion_readiness_malformed",
            "unavailable",
        ),
        (
            "duplicate",
            Some(
                format!(
                    "{{\"schema\":1,\"schema\":1,\"revision\":1,\"conversation\":\"{CONVERSATION}\"}}"
                )
                .into_bytes(),
            ),
            "claude_completion_readiness_malformed",
            "unavailable",
        ),
        (
            "syntax",
            Some(b"not json".to_vec()),
            "claude_completion_readiness_malformed",
            "unavailable",
        ),
        (
            "oversized",
            ready(oversized),
            "claude_completion_readiness_unreadable",
            "unknown",
        ),
    ];
    for (case, record, reason, availability) in cases {
        let f = Fixture::new();
        if let Some(record) = &record {
            put(f.root.path(), READY_FILE, record);
        }
        let [completion, _] = rows(Some(f.root.path()));
        assert_eq!(completion["reason_code"], reason, "{case}");
        assert_eq!(completion["availability"], availability, "{case}");
        assert_eq!(
            completion["evidence"]["mode"]["mode"], "native_v1",
            "{case}"
        );
        assert_eq!(
            completion["evidence"]["mode"]["claude_version_at_launch"], "2.1.302",
            "{case}"
        );
        // Doctor's own bound makes an oversized record unknown, though admission reads it.
        let expected =
            completion["evidence"]["readiness"]["observation"] == "observed" || case == "oversized";
        assert_eq!(admitted(&f), expected, "{case}");
    }
}

#[test]
fn doctor_reads_the_module_writers_own_readiness_and_rejection_records() {
    let f = Fixture::new();
    f.send("session.start", json!({}));
    // The real receive path writes the latest rejection.
    f.send(
        "turn.start",
        json!({"turnId": TURN, "text": "not the addressed frame"}),
    );
    f.retained();
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(completion["availability"], "unknown");
    assert_eq!(
        completion["reason_code"],
        "claude_completion_readiness_recorded"
    );
    assert_eq!(
        completion["evidence"]["readiness"],
        json!({"file": READY_FILE, "observation": "observed", "schema": 1,
            "revision": REVISION, "session_start_conversation": CONVERSATION})
    );
    assert_eq!(diagnostic["availability"], "unknown");
    assert_eq!(
        diagnostic["reason_code"],
        "claude_completion_diagnostic_recorded"
    );
    // No request id, claim, turn or time is attached to the rejection.
    assert_eq!(
        diagnostic["evidence"],
        json!({"source": "session_records", "latest_diagnostic": {
            "file": DIAGNOSTIC_FILE, "observation": "observed", "schema": 1,
            "event": "turn.start",
            "reason": "Claude turn.start did not match the pending request's entire cross-session frame",
            "truncated": false}})
    );
    let text = diagnostic.to_string();
    assert!(!text.contains(REQUEST) && !text.contains(&f.token));
}

// A rejection never decides availability, and its untrusted text is sanitized and cut.
#[test]
fn doctor_diagnostic_is_history_without_authority_over_the_completion_row() {
    let f = Fixture::new();
    f.send("session.start", json!({}));
    let [baseline, absent] = rows(Some(f.root.path()));
    assert_eq!(
        absent["reason_code"],
        "claude_completion_diagnostic_not_recorded"
    );
    assert_eq!(absent["availability"], "unknown");
    assert!(
        absent["detail"]
            .as_str()
            .unwrap()
            .contains("Absence does not show that no signal was rejected")
    );
    let reason = format!("\u{1b}]0;title\u{7}{}\nnext", "r".repeat(3000));
    put(
        f.root.path(),
        DIAGNOSTIC_FILE,
        json!({"schema": 1, "event": "turn.complete\u{1b}[2J", "reason": reason})
            .to_string()
            .as_bytes(),
    );
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(completion, {
        let mut expected = baseline.clone();
        expected["observed_unix_ms"] = completion["observed_unix_ms"].clone();
        expected
    });
    let recorded = &diagnostic["evidence"]["latest_diagnostic"];
    assert_eq!(recorded["truncated"], true);
    for field in ["event", "reason"] {
        let text = recorded[field].as_str().unwrap();
        assert!(!text.chars().any(char::is_control), "{field}: {text:?}");
        assert!(text.chars().count() <= 1024);
    }
    assert_eq!(recorded["event"], "turn.complete\\u{1b}[2J");
    assert!(
        diagnostic["detail"]
            .as_str()
            .unwrap()
            .contains("cannot identify which request was rejected")
    );

    for (record, reason, observation) in [
        (
            json!({"schema": 2, "event": "e", "reason": "secret"}).to_string(),
            "claude_completion_diagnostic_unrecognized",
            "unrecognized",
        ),
        (
            json!({"schema": 1, "event": "e"}).to_string(),
            "claude_completion_diagnostic_malformed",
            "malformed",
        ),
        (
            "[]".to_owned(),
            "claude_completion_diagnostic_malformed",
            "malformed",
        ),
        (
            json!({"schema": 1, "event": "e", "reason": "x".repeat(20_000)}).to_string(),
            "claude_completion_diagnostic_unreadable",
            "unreadable",
        ),
    ] {
        put(f.root.path(), DIAGNOSTIC_FILE, record.as_bytes());
        let [completion, diagnostic] = rows(Some(f.root.path()));
        assert_eq!(completion["reason_code"], baseline["reason_code"]);
        assert_eq!(diagnostic["reason_code"], reason);
        assert_eq!(diagnostic["availability"], "unknown");
        let recorded = &diagnostic["evidence"]["latest_diagnostic"];
        assert_eq!(recorded["observation"], observation);
        // Text that was not validated is never echoed; a refused read names its bound.
        if observation == "unreadable" {
            assert_eq!(
                recorded["limit_bytes"],
                super::super::diagnose::record_limit()
            );
        } else {
            assert!(recorded.get("reason").is_none() && recorded.get("event").is_none());
        }
        assert!(!diagnostic.to_string().contains("secret"));
    }
}

#[test]
fn doctor_reports_a_non_regular_record_as_unreadable_without_following_it() {
    let f = Fixture::new();
    std::fs::create_dir(f.root.path().join(READY_FILE)).unwrap();
    std::fs::create_dir(f.root.path().join(DIAGNOSTIC_FILE)).unwrap();
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(
        completion["reason_code"],
        "claude_completion_readiness_unreadable"
    );
    assert_eq!(completion["availability"], "unknown");
    assert!(
        completion["evidence"]["readiness"]["reason"]
            .as_str()
            .unwrap()
            .contains("non-regular")
    );
    assert_eq!(
        diagnostic["reason_code"],
        "claude_completion_diagnostic_unreadable"
    );
}

#[test]
fn doctor_keeps_independently_damaged_completion_records_visible() {
    let f = Fixture::new();
    put(
        f.root.path(),
        MODE_FILE,
        br#"{"schema":1,"mode":"future_mode"}"#,
    );
    put(
        f.root.path(),
        READY_FILE,
        json!({"schema":1,"revision":999,"conversation":CONVERSATION})
            .to_string()
            .as_bytes(),
    );
    put(
        f.root.path(),
        DIAGNOSTIC_FILE,
        &vec![b'x'; super::super::diagnose::record_limit() as usize + 1],
    );
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(
        completion["evidence"]["mode"]["observation"],
        "unrecognized"
    );
    assert_eq!(
        completion["evidence"]["readiness"]["observation"],
        "unrecognized"
    );
    assert_eq!(completion["evidence"]["readiness"]["revision"], 999);
    assert_eq!(
        diagnostic["evidence"]["latest_diagnostic"]["observation"],
        "unreadable"
    );
    assert_eq!(diagnostic["availability"], "unknown");
    f.retained();
}

#[test]
fn doctor_does_not_echo_malformed_numeric_record_fields() {
    let f = Fixture::new();
    put(
        f.root.path(),
        READY_FILE,
        json!({"schema":1,"revision":"SENTINEL_PRIVATE_TEXT","conversation":CONVERSATION})
            .to_string()
            .as_bytes(),
    );
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(
        completion["evidence"]["readiness"]["fault"],
        "schema_mismatch"
    );
    for row in [completion, diagnostic] {
        assert!(!row.to_string().contains("SENTINEL_PRIVATE_TEXT"));
    }
}

// Doctor neither duplicates nor disturbs inspect's candidate observation.
#[test]
fn doctor_leaves_candidate_inspection_to_inspect() {
    let f = Fixture::new();
    f.send("session.start", json!({}));
    f.start();
    f.complete("private interim answer");
    f.retained();
    let before = observe_candidate(&f.store(), &f.token).unwrap();
    assert_eq!(before["state"], "unmarked_bound_turn");
    let [completion, diagnostic] = rows(Some(f.root.path()));
    assert_eq!(
        completion["reason_code"],
        "claude_completion_readiness_recorded"
    );
    assert!(
        diagnostic["evidence"]["latest_diagnostic"]["reason"]
            .as_str()
            .unwrap()
            .contains("without the final marker")
    );
    for row in [&completion, &diagnostic] {
        assert!(!row.to_string().contains("private interim answer"));
    }
    assert_eq!(observe_candidate(&f.store(), &f.token).unwrap(), before);
}
