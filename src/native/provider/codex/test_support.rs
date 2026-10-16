use crate::native::session::primitives::write_json_atomic;
use crate::native::session::schema::SessionEvent;
use std::path::Path;

pub(super) fn write_established_thread(directory: &Path, thread_id: &str) {
    std::fs::create_dir_all(directory.join("events")).unwrap();
    write_json_atomic(
        &directory.join("events/event-1.json"),
        &SessionEvent {
            cancelled: false,
            provider: "codex".to_owned(),
            message: "initial result".to_owned(),
            error: None,
            provider_session_id: Some(thread_id.to_owned()),
            turn_id: Some("initial-turn".to_owned()),
            created_unix_ms: Some(1),
        },
    )
    .unwrap();
}
