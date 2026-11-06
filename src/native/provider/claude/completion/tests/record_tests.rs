//! Characterization of how the completion module's records are read. The live readers
//! (delivery, hooks, capability observations) take the whole file through lossy UTF-8;
//! doctor reads at most 16 KiB of strict UTF-8. These tests pin both policies and the exact
//! texts of the live refusals, so a change to who owns the records cannot change either.
use super::*;

const DOCTOR_LIMIT: u64 = 16 * 1024;

fn session_with_version(root: &Path, version: &str) -> std::path::PathBuf {
    let directory = root.join("session-records");
    std::fs::create_dir(&directory).unwrap();
    Store::open_unchecked(&directory)
        .write_manifest(&crate::native::session::schema::SessionManifest {
            schema: 1,
            id: "session-records".into(),
            provider: "claude".into(),
            provider_path: "unused".into(),
            provider_version: version.into(),
            workspace: root.into(),
            title: "record fixture".into(),
            model: None,
            effort: None,
            yolo: true,
            created_unix_ms: 1,
        })
        .unwrap();
    directory
}

fn replace(directory: &Path, file: &str, bytes: &[u8]) {
    let path = directory.join(file);
    if path.is_dir() {
        std::fs::remove_dir(&path).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn mode_row(directory: &Path) -> Value {
    let [completion, _] =
        doctor_checks(Some(directory)).map(|check| serde_json::to_value(check).unwrap());
    completion
}

fn json_error<T: serde::de::DeserializeOwned>(path: &Path, text: &str) -> String {
    let error = serde_json::from_str::<T>(text).err().unwrap();
    format!("invalid JSON in {}: {error}", path.display())
}

// A long `claude --version` output reaches the completion-mode record whole: the live reader
// still selects native completion from it, while doctor's own bound reports it unreadable.
#[test]
fn a_launch_record_past_doctors_bound_is_still_native_for_the_live_reader() {
    for banner in [20_000, 70_000] {
        let root = tempfile::tempdir().unwrap();
        let version = format!("2.1.318 (Claude Code)\n{}", "b".repeat(banner));
        let directory = session_with_version(root.path(), &version);
        prepare(&directory, false).unwrap();
        let size = std::fs::metadata(directory.join(MODE_FILE)).unwrap().len();
        assert!(size > DOCTOR_LIMIT, "{size}");
        assert!(native_mode(&directory).unwrap());
        assert!(matches!(
            follow_up_static(&directory, false),
            crate::native::provider::FollowUpStatic::Ok { .. }
        ));
        let completion = mode_row(&directory);
        assert_eq!(
            completion["reason_code"],
            "claude_completion_mode_unreadable"
        );
        assert_eq!(completion["availability"], "unknown");
        let mode = &completion["evidence"]["mode"];
        assert_eq!(mode["observation"], "unreadable");
        assert_eq!(mode["limit_bytes"], DOCTOR_LIMIT);
    }
}

// Invalid UTF-8 inside a string is replaced before the live decode and refused by doctor.
#[test]
fn live_reads_are_lossy_and_doctor_reads_are_strict() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path();
    replace(
        directory,
        MODE_FILE,
        b"{\"schema\":1,\"mode\":\"native_v1\",\"reason\":\"r\",\"claude_version\":\"2.1.302 \xff\"}",
    );
    assert!(native_mode(directory).unwrap());
    let completion = mode_row(directory);
    assert_eq!(completion["evidence"]["mode"]["observation"], "unreadable");
    assert!(
        completion["evidence"]["mode"]["reason"]
            .as_str()
            .unwrap()
            .contains("invalid UTF-8")
    );
    // A replaced character inside the mode itself names no mode the live reader knows.
    let text =
        "{\"schema\":1,\"mode\":\"native_v1\u{fffd}\",\"reason\":\"r\",\"claude_version\":null}";
    replace(
        directory,
        MODE_FILE,
        b"{\"schema\":1,\"mode\":\"native_v1\xff\",\"reason\":\"r\",\"claude_version\":null}",
    );
    assert_eq!(
        format!("{:#}", native_mode(directory).unwrap_err()),
        json_error::<LaunchMode>(&directory.join(MODE_FILE), text)
    );
}

#[test]
fn readiness_and_question_mode_reads_stay_whole_and_lossy() {
    let f = Fixture::new();
    let ready = format!(
        "{{\"schema\":1,\"revision\":{REVISION},\"conversation\":\"{CONVERSATION}\",\"pad\":\"{}\"}}",
        "p".repeat(20_000)
    );
    let mut lossy = format!(
        "{{\"schema\":1,\"revision\":{REVISION},\"conversation\":\"{CONVERSATION}\",\"pad\":\""
    )
    .into_bytes();
    lossy.extend_from_slice(b"\xff\"}");
    for record in [ready.into_bytes(), lossy] {
        replace(f.root.path(), READY_FILE, &record);
        let now = Instant::now();
        admit(
            f.root.path(),
            &f.token,
            now + Duration::from_millis(100),
            false,
            &mut FakeClock::new(now),
        )
        .unwrap();
    }
    let deflected = crate::native::provider::QuestionResponse::DeflectedToResult(
        "plugin_configured_unverified",
    );
    let padded = format!(
        "{{\"schema\":1,\"deflection\":\"on\",\"pad\":\"{}\"}}",
        "p".repeat(20_000)
    );
    for record in [
        padded.as_bytes(),
        b"{\"schema\":1,\"deflection\":\"on\",\"pad\":\"\xff\"}",
    ] {
        replace(f.root.path(), QUESTION_MODE_FILE, record);
        assert_eq!(question_response(f.root.path()), deflected);
    }
}

// The refusal texts callers show: tell's error, inspect's unknown detail, the hook's error.
#[test]
fn live_record_refusals_keep_their_texts() {
    use crate::native::provider::{EmissionObservation, FollowUpStatic, QuestionResponse};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path();
    let mode = directory.join(MODE_FILE);
    let schema_two = serde_json::to_vec(&LaunchMode {
        schema: 2,
        mode: Mode::NativeV1,
        reason: "r".into(),
        claude_version: None,
    })
    .unwrap();
    for (record, expected) in [
        (&b"{"[..], json_error::<LaunchMode>(&mode, "{")),
        (
            br#"{"schema":1,"mode":"native_v1"}"#,
            json_error::<LaunchMode>(&mode, r#"{"schema":1,"mode":"native_v1"}"#),
        ),
        (
            &schema_two[..],
            "unsupported Claude completion mode schema".to_owned(),
        ),
    ] {
        replace(directory, MODE_FILE, record);
        assert_eq!(
            format!("{:#}", native_mode(directory).unwrap_err()),
            expected
        );
        assert_eq!(
            follow_up_static(directory, false),
            FollowUpStatic::Unknown(expected.clone())
        );
        assert_eq!(
            emission_observation(directory),
            EmissionObservation::Unknown(expected.clone())
        );
        assert_eq!(
            question_response(directory),
            QuestionResponse::Unknown(expected.clone())
        );
    }
    std::fs::remove_file(&mode).unwrap();
    std::fs::create_dir(&mode).unwrap();
    assert_eq!(
        format!("{:#}", native_mode(directory).unwrap_err()),
        format!("refusing non-regular session file: {}", mode.display())
    );
    std::fs::remove_dir(&mode).unwrap();

    // Question mode is read only for native completion, with its own texts.
    replace(
        directory,
        MODE_FILE,
        &serde_json::to_vec(&LaunchMode {
            schema: 1,
            mode: Mode::NativeV1,
            reason: "r".into(),
            claude_version: None,
        })
        .unwrap(),
    );
    let question = directory.join(QUESTION_MODE_FILE);
    for (record, expected) in [
        (&b"{"[..], json_error::<QuestionMode>(&question, "{")),
        (
            br#"{"schema":1,"deflection":"maybe"}"#,
            json_error::<QuestionMode>(&question, r#"{"schema":1,"deflection":"maybe"}"#),
        ),
        (
            br#"{"schema":2,"deflection":"on"}"#,
            "unsupported Claude question mode schema".to_owned(),
        ),
    ] {
        replace(directory, QUESTION_MODE_FILE, record);
        assert_eq!(
            question_response(directory),
            QuestionResponse::Unknown(expected)
        );
    }
    replace(
        directory,
        QUESTION_MODE_FILE,
        br#"{"schema":1,"deflection":"off"}"#,
    );
    assert_eq!(
        question_response(directory),
        QuestionResponse::Unsupported(crate::native::provider::NO_ADDRESSED_ANSWER_PATH)
    );

    // Readiness refusals name that nothing was sent; a decode failure keeps the reader's text.
    let f = Fixture::new();
    let ready = f.root.path().join(READY_FILE);
    let invalid = "Claude completion module readiness record is invalid; no message was sent";
    for (record, expected) in [
        (b"{".to_vec(), json_error::<Ready>(&ready, "{")),
        (
            json!({"schema": 2, "revision": REVISION, "conversation": CONVERSATION})
                .to_string()
                .into_bytes(),
            invalid.to_owned(),
        ),
        (
            json!({"schema": 1, "revision": REVISION + 1, "conversation": CONVERSATION})
                .to_string()
                .into_bytes(),
            invalid.to_owned(),
        ),
        (
            json!({"schema": 1, "revision": REVISION, "conversation": "not-a-uuid"})
                .to_string()
                .into_bytes(),
            invalid.to_owned(),
        ),
    ] {
        replace(f.root.path(), READY_FILE, &record);
        let now = Instant::now();
        let error = admit(
            f.root.path(),
            &f.token,
            now + Duration::from_millis(100),
            false,
            &mut FakeClock::new(now),
        )
        .unwrap_err();
        assert_eq!(format!("{error:#}"), expected);
    }

    // A launch prepared again with another version keeps the first record.
    let root = tempfile::tempdir().unwrap();
    let directory = session_with_version(root.path(), "2.1.302");
    prepare(&directory, false).unwrap();
    let before = std::fs::read(directory.join(MODE_FILE)).unwrap();
    let mut manifest = Reader::open_unchecked(&directory).manifest().unwrap();
    manifest.provider_version = "2.1.318".into();
    Store::open_unchecked(&directory)
        .write_manifest(&manifest)
        .unwrap();
    assert_eq!(
        format!("{:#}", prepare(&directory, false).unwrap_err()),
        "a different Claude completion mode was already recorded for this launch"
    );
    assert_eq!(std::fs::read(directory.join(MODE_FILE)).unwrap(), before);
}

// The writers' own records, byte for byte: a schema change shows here first.
#[test]
fn the_writers_record_their_exact_shapes() {
    let root = tempfile::tempdir().unwrap();
    let directory = session_with_version(root.path(), "2.1.318 (Claude Code)");
    prepare(&directory, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.join(MODE_FILE)).unwrap(),
        "{\n  \"schema\": 1,\n  \"mode\": \"native_v1\",\n  \"reason\": \"windows_stable_2_1_302_or_newer\",\n  \"claude_version\": \"2.1.318 (Claude Code)\"\n}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join(QUESTION_MODE_FILE)).unwrap(),
        "{\n  \"schema\": 1,\n  \"deflection\": \"on\"\n}"
    );
    let f = Fixture::new();
    f.send("session.start", json!({}));
    assert_eq!(
        std::fs::read_to_string(f.root.path().join(READY_FILE)).unwrap(),
        format!(
            "{{\n  \"schema\": 1,\n  \"revision\": {REVISION},\n  \"conversation\": \"{CONVERSATION}\"\n}}"
        )
    );
}
