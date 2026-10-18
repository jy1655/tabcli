use crate::native::session::primitives::write_json_atomic;
use crate::native::session::schema::SessionEvent;
use std::path::Path;

pub(super) fn write_established_thread(directory: &Path, thread_id: &str) {
    std::fs::create_dir_all(directory.join("events")).unwrap();
    write_json_atomic(
        &directory.join("events/event-1.json"),
        &SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "initial result".to_owned(),
            error: None,
            provider_session_id: Some(thread_id.to_owned()),
            turn_id: Some("initial-turn".to_owned()),
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        },
    )
    .unwrap();
}

// A private session fixture shared by launch and real-shell hook tests.
pub(super) fn launch_session(root: &Path, version: Option<&str>) -> std::path::PathBuf {
    use crate::native::session::{Store, turn};
    let directory = root.join("session-hooks");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    if let Some(version) = version {
        Store::open_unchecked(&directory)
            .private("manifest.json")
            .write_json(&serde_json::json!({
                "schema": 1, "id": "session-hooks", "provider": "codex", "provider_path": "codex",
                "provider_version": version, "workspace": root, "title": "hooks fixture",
                "yolo": false, "created_unix_ms": 1
            }))
            .unwrap();
    }
    directory
}
