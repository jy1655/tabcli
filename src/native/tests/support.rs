use super::*;
use crate::native::session::primitives::write_json_atomic;
use crate::native::session::test_support::{
    CLOSED_STATUS_FILE, LEGACY_RESUME_PENDING_FILE, PROVIDER_PROCESS_FILE, SESSION_OWNER_FILE,
    TERMINAL_HANDLE_FILE, TURN_COMPLETION_FILE,
};
use crate::native::session::turn::{JournaledEventState, PendingTurnCompletion};
use crate::native::session::{SessionState, SessionStatus, update_status};
use crate::native::sessions::sessions_in;
#[cfg(target_os = "macos")]
use crate::native::terminal::macos::process::macos_process_start;
#[cfg(target_os = "macos")]
use crate::native::terminal::ownership::MacTerminalAppIdentity;
use crate::native::terminal::ownership::NativeSessionOwner;
use crate::native::{NativeCommand, parse_args, query, terminal};
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

pub(in crate::native) fn reaped_child_pid() -> u32 {
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

pub(super) fn write_owned_terminal_state(directory: &Path, state: &str, owner_pid: u32) {
    fs::create_dir(directory.join("events")).unwrap();
    update_status(directory, state.parse().unwrap(), None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    claim.retain();
    let windows_process_identity = test_windows_process_identity(owner_pid).or_else(|| {
        cfg!(windows).then(|| terminal::WindowsProcessIdentity {
            creation_time: 0,
            executable_path: String::new(),
        })
    });
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: if cfg!(windows) {
                terminal::TerminalKind::WindowsConsole
            } else {
                terminal::TerminalKind::AppleTerminal
            },
            id: if cfg!(windows) {
                owner_pid.to_string()
            } else {
                "/dev/ttys999".to_owned()
            },
            tab_id: None,
            window_id: (!cfg!(windows)).then(|| "1001".to_owned()),
            managed_session_id: Some("session-owner123".to_owned()),
            wezterm_mux: None,
            windows_process_identity: windows_process_identity.clone(),
        },
    )
    .unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: owner_pid,
            managed_session_id: Some("session-owner123".to_owned()),
            terminal_tty: (!cfg!(windows)).then(|| "/dev/ttys999".to_owned()),
            windows_process_identity,
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
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

#[cfg(windows)]
pub(in crate::native) fn test_windows_process_identity(
    pid: u32,
) -> Option<terminal::WindowsProcessIdentity> {
    terminal::windows_process_identity(pid).ok()
}

#[cfg(not(windows))]
pub(in crate::native) fn test_windows_process_identity(
    _pid: u32,
) -> Option<terminal::WindowsProcessIdentity> {
    None
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

pub(super) fn sample_completion(claim_token: &str, message: &str) -> PendingTurnCompletion {
    PendingTurnCompletion::new(
        claim_token,
        SessionEvent {
            cancelled: false,
            provider: FirstPartyCli::Codex.as_str().to_owned(),
            message: message.to_owned(),
            error: None,
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            created_unix_ms: Some(1),
        },
        None,
    )
    .unwrap()
}

pub(super) fn delivery_uncertain_failure(message: &str) -> terminal::TerminalSendFailure {
    terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!("{message}"))
}

/// After recovery no journal remains. Temporary files an injected fault left behind (as an
/// abrupt stop would) are best-effort records: observation must ignore them.
pub(super) fn assert_journal_settled(directory: &Path) {
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    query::observe_snapshot(&Reader::open_unchecked(directory)).unwrap();
}

pub(super) fn close_test_terminal() -> terminal::TerminalSession {
    terminal::TerminalSession {
        kind: if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::AppleTerminal
        },
        id: "4242".to_owned(),
        tab_id: None,
        window_id: None,
        managed_session_id: Some("session-close123".to_owned()),
        wezterm_mux: None,
        windows_process_identity: None,
    }
}

pub(super) fn write_test_manifest(directory: &Path) {
    let id = directory.file_name().unwrap().to_str().unwrap().to_owned();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id,
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("/opt/codex"),
            provider_version: "codex-cli 0.147.0".to_owned(),
            workspace: directory.to_path_buf(),
            title: "review round 2".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
}

/// A completion that stopped right after writing its event: journal and event exist, the
/// claim is still held, and the status was never updated. Returns the request address and
/// the journal's event path. `event_message` lets the written event disagree with the journal.
pub(super) fn seed_event_written_completion(
    directory: &Path,
    event_message: &str,
) -> (String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}

pub(super) fn request_state(directory: &Path, request_id: &str) -> (String, String) {
    let value = query::request_result(&Reader::open_unchecked(directory), request_id).unwrap();
    (
        value["request_state"].as_str().unwrap().to_owned(),
        value["session_state"].as_str().unwrap().to_owned(),
    )
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

// The query commands exactly as the CLI runs them, parsed from argv and evaluated over
// an explicit state root instead of the process environment.
pub(super) fn cli_result(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Result(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a result command")
    };
    query::result_value_in(root, &request).unwrap()
}

pub(super) fn cli_search(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Search(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a search command")
    };
    query::search_value_in(root, &request).unwrap()
}

pub(super) fn cli_sessions(root: &Path) -> Vec<serde_json::Value> {
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!("not a sessions command")
    };
    sessions_in(root, &request).unwrap()
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

pub(super) fn oversized_sample_event(message: &str) -> SessionEvent {
    SessionEvent {
        cancelled: false,
        provider: FirstPartyCli::Codex.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: Some(1),
    }
}

/// A working session with a held claim and no completion yet: what a provider completion
/// commits into. Returns the request address, the claim token, and the receipt's event path.
pub(super) fn seed_claimed_session(directory: &Path) -> (String, String, PathBuf) {
    let (request, claimed, path) = seed_claimed_session_with_claim(directory);
    let token = claimed.token().to_owned();
    claimed.retain();
    (request, token, path)
}

pub(super) fn seed_claimed_session_with_claim(directory: &Path) -> (String, turn::Claim, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let event_path = directory.join("events").join(&claim.receipt().event_file);
    (request_id, claim, event_path)
}

/// Points `link` at `target` as a directory symlink, or on Windows a junction when
/// symlinks need a privilege. Returns false where neither can be created.
pub(super) fn link_directory(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link).is_ok()
        || Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|output| output.status.success());
    created && fs::symlink_metadata(link).is_ok_and(|metadata| metadata.file_type().is_symlink())
}
