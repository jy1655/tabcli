//! Mechanical lifecycle fixtures shared by close and native command tests.
//! Partial-state writes deliberately bypass recovery and retain their original order.
use crate::native::session::names::{
    SESSION_OWNER_FILE, TERMINAL_HANDLE_FILE, TURN_COMPLETION_FILE,
};
use crate::native::session::primitives::write_json_atomic;
use crate::native::session::schema::{SESSION_SCHEMA, SessionEvent, SessionManifest};
use crate::native::session::turn::PendingTurnCompletion;
use crate::native::session::{Reader, SessionState, Store, turn, update_status};
use crate::native::sessions::sessions_in;
use crate::native::terminal::ownership::NativeSessionOwner;
use crate::native::{NativeCommand, parse_args, query, terminal};
use agent_bridge::FirstPartyCli;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

pub(in crate::native) fn write_owned_terminal_state(directory: &Path, state: &str, owner_pid: u32) {
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

pub(in crate::native) fn sample_completion(
    claim_token: &str,
    message: &str,
) -> PendingTurnCompletion {
    PendingTurnCompletion::new(
        claim_token,
        SessionEvent {
            provenance: None,
            cancelled: false,
            provider: FirstPartyCli::Codex.as_str().to_owned(),
            message: message.to_owned(),
            error: None,
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        },
        None,
    )
    .unwrap()
}

/// After recovery no journal remains. Temporary files an injected fault left behind (as an
/// abrupt stop would) are best-effort records: observation must ignore them.
pub(in crate::native) fn assert_journal_settled(directory: &Path) {
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    query::observe_snapshot(&Reader::open_unchecked(directory)).unwrap();
}

pub(in crate::native) fn close_test_terminal() -> terminal::TerminalSession {
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

pub(in crate::native) fn write_test_manifest(directory: &Path) {
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
pub(in crate::native) fn seed_event_written_completion(
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

pub(in crate::native) fn request_state(directory: &Path, request_id: &str) -> (String, String) {
    let value = query::request_result(&Reader::open_unchecked(directory), request_id).unwrap();
    (
        value["request_state"].as_str().unwrap().to_owned(),
        value["session_state"].as_str().unwrap().to_owned(),
    )
}

// The query commands exactly as the CLI runs them, parsed from argv and evaluated over
// an explicit state root instead of the process environment.
pub(in crate::native) fn cli_result(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Result(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a result command")
    };
    query::result_value_in(root, &request).unwrap()
}

pub(in crate::native) fn cli_search(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Search(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a search command")
    };
    query::search::search_value_in(root, &request).unwrap()
}

pub(in crate::native) fn cli_sessions(root: &Path) -> Vec<serde_json::Value> {
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!("not a sessions command")
    };
    sessions_in(root, &request).unwrap()
}

pub(in crate::native) fn oversized_sample_event(message: &str) -> SessionEvent {
    SessionEvent {
        provenance: None,
        cancelled: false,
        provider: FirstPartyCli::Codex.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: Some(1),
        usage: None,
        usage_error: None,
    }
}

/// A working session with a held claim and no completion yet: what a provider completion
/// commits into. Returns the request address, the claim token, and the receipt's event path.
pub(in crate::native) fn seed_claimed_session(directory: &Path) -> (String, String, PathBuf) {
    let (request, claimed, path) = seed_claimed_session_with_claim(directory);
    let token = claimed.token().to_owned();
    claimed.retain();
    (request, token, path)
}

/// Points `link` at `target` as a directory symlink, or on Windows a junction when
/// symlinks need a privilege. Returns false where neither can be created.
pub(in crate::native) fn link_directory(target: &Path, link: &Path) -> bool {
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

pub(in crate::native) fn seed_claimed_session_with_claim(
    directory: &Path,
) -> (String, turn::Claim, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let event_path = directory.join("events").join(&claim.receipt().event_file);
    (request_id, claim, event_path)
}
