use super::*;
use crate::native::session::primitives::write_json_atomic;
#[cfg(target_os = "macos")]
use crate::native::session::test_support::SESSION_OWNER_FILE;
pub(in crate::native) use crate::native::session::test_support::lifecycle::{
    assert_journal_settled, cli_result, cli_search, cli_sessions, close_test_terminal,
    link_directory, oversized_sample_event, reaped_child_pid, request_state, sample_completion,
    seed_claimed_session, seed_claimed_session_with_claim, seed_event_written_completion,
    test_windows_process_identity, write_owned_terminal_state, write_test_manifest,
};
use crate::native::session::test_support::{
    CLOSED_STATUS_FILE, LEGACY_RESUME_PENDING_FILE, PROVIDER_PROCESS_FILE, TERMINAL_HANDLE_FILE,
    TURN_COMPLETION_FILE,
};
use crate::native::session::turn::JournaledEventState;
use crate::native::session::{SessionState, SessionStatus, update_status};
use crate::native::terminal;
#[cfg(target_os = "macos")]
use crate::native::terminal::macos::process::macos_process_start;
#[cfg(target_os = "macos")]
use crate::native::terminal::ownership::MacTerminalAppIdentity;
#[cfg(target_os = "macos")]
use crate::native::terminal::ownership::NativeSessionOwner;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn write_prune_test_session(
    root: &Path,
    id: &str,
    state: &str,
    closed_unix_ms: Option<u128>,
) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: id.to_owned(),
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("/opt/codex"),
            provider_version: "codex-cli 0.147.0".to_owned(),
            workspace: root.to_owned(),
            title: id.to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
    let status = SessionStatus {
        state: state.parse().unwrap(),
        generation: 1,
        updated_unix_ms: closed_unix_ms.unwrap_or(1_000),
        exit_code: None,
        error: None,
        residual_surface: None,
    };
    write_json_atomic(&directory.join("status.json"), &status).unwrap();
    if let Some(updated_unix_ms) = closed_unix_ms {
        write_json_atomic(
            &directory.join(CLOSED_STATUS_FILE),
            &SessionStatus {
                state: SessionState::Closed,
                generation: 1,
                updated_unix_ms,
                exit_code: None,
                error: None,
                residual_surface: None,
            },
        )
        .unwrap();
    }
    directory
}

// An owner that passed the live Terminal.app verification has its whole identity recorded.
#[cfg(target_os = "macos")]
pub(in crate::native) fn write_attested_apple_terminal_state(
    directory: &Path,
    state: &str,
    owner_pid: u32,
) -> NativeSessionOwner {
    write_owned_terminal_state(directory, state, owner_pid);
    let owner = NativeSessionOwner {
        pid: owner_pid,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: Some("/dev/ttys999".to_owned()),
        terminal_tty_device: Some(7),
        process_start_seconds: Some(1_790_000_000),
        process_start_microseconds: Some(42),
        process_group: Some(owner_pid),
        terminal_process_group: Some(owner_pid),
        terminal_app: Some(MacTerminalAppIdentity {
            pid: std::process::id(),
            start_seconds: macos_process_start(std::process::id()).unwrap().unwrap().0,
            start_microseconds: macos_process_start(std::process::id()).unwrap().unwrap().1,
        }),
        ..NativeSessionOwner::default()
    };
    write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner).unwrap();
    owner
}

// The provider process record as the launch wrapper writes it, for a process the test
// controls. Pid 0 stands in for a provider that is verified dead.
pub(in crate::native) fn write_provider_process_record(directory: &Path, session: &str, pid: u32) {
    write_json_atomic(
        &directory.join(PROVIDER_PROCESS_FILE),
        &ProviderProcessRecord {
            schema: 1,
            managed_session_id: session.to_owned(),
            pid,
            windows_process_identity: test_windows_process_identity(pid),
            spawned_unix_ms: 1,
        },
    )
    .unwrap();
}

// A process that stays alive until the test ends it: it stands in for a provider process
// that outlived its launch wrapper and its console.
pub(in crate::native) fn spawn_surviving_process() -> std::process::Child {
    #[cfg(windows)]
    let mut command = Command::new("cmd.exe");
    #[cfg(windows)]
    command.args(["/c", "pause"]);
    #[cfg(not(windows))]
    let mut command = Command::new("/bin/sh");
    #[cfg(not(windows))]
    command.args(["-c", "read _"]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

pub(super) fn delivery_uncertain_failure(message: &str) -> terminal::TerminalSendFailure {
    terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!("{message}"))
}

pub(in crate::native) struct CloseFixture {
    pub(super) root: tempfile::TempDir,
    pub(in crate::native) directory: PathBuf,
    pub(super) request_id: String,
}

/// A working session under `session-fault` with a held claim, a journaled completion
/// whose event is absent, committed, or mismatched, a terminal handle, and a legacy
/// resume marker: everything an explicit close has to settle.
pub(in crate::native) fn seed_close_fixture(journaled_event: JournaledEventState) -> CloseFixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-fault");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    match journaled_event {
        JournaledEventState::Absent => {}
        JournaledEventState::Committed => {
            write_json_atomic(&event_path, &pending.event).unwrap();
        }
        JournaledEventState::Mismatched => {
            let mut event = pending.event.clone();
            event.message = "different result".to_owned();
            write_json_atomic(&event_path, &event).unwrap();
        }
        JournaledEventState::Oversized(_) => unreachable!("fixtures never exceed the read limit"),
    }
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &close_test_terminal(),
    )
    .unwrap();
    fs::write(directory.join(LEGACY_RESUME_PENDING_FILE), "{}").unwrap();
    CloseFixture {
        root,
        directory,
        request_id,
    }
}

/// Like [`seed_event_written_completion`], with the journal's own message chosen too.
pub(super) fn seed_event_written_completion_with(
    directory: &Path,
    journal_message: &str,
    event_message: &str,
) -> (String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), journal_message);
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}
