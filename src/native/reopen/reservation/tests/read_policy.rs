//! What Reopen's readers of the launch refusal and the reopen marker make of a link at the
//! record's path and of bytes that are not UTF-8.
use super::*;

/// Puts a `0xff` byte after the first `after` in the record, inside a JSON string.
fn corrupt_in_place(path: &Path, after: &str) {
    let text = fs::read_to_string(path).unwrap();
    let (before, rest) = text.split_once(after).unwrap();
    let mut bytes = before.as_bytes().to_vec();
    bytes.extend_from_slice(after.as_bytes());
    bytes.push(0xff);
    bytes.extend_from_slice(rest.as_bytes());
    fs::write(path, bytes).unwrap();
}

fn refused_session() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    // The request receipt of the launch's Claim makes the directory that launch's
    // (`launch_owns_directory`).
    fs::create_dir(directory.path().join("requests")).unwrap();
    fs::write(
        directory.path().join("requests").join("1-2-3.json"),
        r#"{"schema":1,"request_id":"request-launch","claim_token":"1-2-3","event_file":"event-1.json","created_unix_ms":1}"#,
    )
    .unwrap();
    let _ = record_reopen_refusal(
        directory.path(),
        ReopenGate::StartFailed,
        "start failed".to_owned(),
    );
    directory
}

#[test]
fn launch_refusal_readers_over_a_record_that_is_not_utf8() {
    let directory = refused_session();
    corrupt_in_place(
        &directory.path().join(REOPEN_REFUSAL_FILE),
        "\"detail\": \"start",
    );
    // A refusal that is not UTF-8 is unreadable: no launch refusal is read from it, and a
    // failed launch cannot be recorded over it.
    assert_eq!(read_reopen_refusal_gate(directory.path()), None);
    let error = format!(
        "{:#}",
        record_failed_launch(directory.path(), "1-2-3", &anyhow::anyhow!("launch failed"))
            .unwrap_err()
    );
    assert!(
        error.contains("stream did not contain valid UTF-8"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn launch_refusal_readers_over_a_link() {
    let directory = refused_session();
    let outside = tempfile::tempdir().unwrap();
    let path = directory.path().join(REOPEN_REFUSAL_FILE);
    let target = outside.path().join(REOPEN_REFUSAL_FILE);
    fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(read_reopen_refusal_gate(directory.path()), None);
    assert!(
        format!(
            "{:#}",
            record_failed_launch(directory.path(), "1-2-3", &anyhow::anyhow!("launch failed"))
                .unwrap_err()
        )
        .contains("refusing non-regular session file")
    );
}

#[test]
fn reopen_marker_judgment_over_a_record_that_is_not_utf8() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(REOPEN_MARKER_FILE);
    fs::write(
        &path,
        r#"{"schema":1,"claim":"c","provider_session_id":"p","created_unix_ms":1}"#,
    )
    .unwrap();
    corrupt_in_place(&path, "\"claim\":\"c");
    // A marker that is not UTF-8 is unreadable, and the read's error reaches the command.
    let Err(error) = marker::observe(directory.path()) else {
        panic!("a marker that is not UTF-8 must be unreadable");
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("stream did not contain valid UTF-8"),
        "{error}"
    );
}
