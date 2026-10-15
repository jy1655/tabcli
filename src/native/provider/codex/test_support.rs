use crate::native::session::schema::SessionEvent;
use crate::native::write_json_atomic;
#[cfg(unix)]
use anyhow::Result;
use std::path::Path;
#[cfg(unix)]
use std::{
    thread,
    time::{Duration, Instant},
};

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

#[cfg(unix)]
thread_local! {
    pub(super) static QUEUE_TIMEOUT_READY: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
pub(super) fn synchronized_queue_deadline(label: &str, deadline: Instant) -> Result<Instant> {
    if label != "Codex queue" {
        return Ok(deadline);
    }
    let Some(ready) = QUEUE_TIMEOUT_READY.with(|path| path.borrow_mut().take()) else {
        return Ok(deadline);
    };
    while !ready.is_file() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "queue fixture did not finish startup"
        );
        thread::sleep(Duration::from_millis(5));
    }
    Ok(Instant::now())
}
