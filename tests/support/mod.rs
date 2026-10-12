// Each integration test is a separate crate and uses a different subset of these helpers.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Output,
};

pub fn manifest(
    id: &str,
    provider: &str,
    provider_path: &Path,
    version: &str,
    workspace: &Path,
    title: &str,
) -> Value {
    json!({
        "schema": 1, "id": id, "provider": provider, "provider_path": provider_path,
        "provider_version": version, "workspace": workspace, "title": title,
        "model": null, "effort": null, "yolo": false, "created_unix_ms": 1
    })
}

pub fn status(state: &str, generation: u64, error: Option<&str>) -> Value {
    json!({"state": state, "generation": generation, "updated_unix_ms": generation,
        "exit_code": null, "error": error})
}

pub fn event(
    provider: &str,
    session_id: &str,
    turn_id: &str,
    message: &str,
    error: Option<&str>,
    created_unix_ms: u64,
) -> Value {
    json!({"provider": provider, "message": message, "error": error,
        "provider_session_id": session_id, "turn_id": turn_id, "created_unix_ms": created_unix_ms})
}

pub fn receipt(request: &str, claim: &str, event: &str) -> Value {
    json!({"schema": 1, "request_id": request, "claim_token": claim,
        "event_file": event, "created_unix_ms": 3})
}

pub fn write(path: &Path, value: &Value) {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temporary
        .write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
    temporary.persist(path).unwrap();
}

/// Every path under the state root: directories carry no content, files carry their bytes,
/// so an added directory is as visible as an added file.
pub type Entry = (PathBuf, Option<Vec<u8>>);

pub fn entries(directory: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            entries.push((entry.path(), None));
            entries.extend(self::entries(&entry.path()));
        } else {
            entries.push((entry.path(), Some(fs::read(entry.path()).unwrap())));
        }
    }
    entries.sort();
    entries
}

pub fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
