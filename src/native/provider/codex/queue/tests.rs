#[cfg(unix)]
use super::super::ADAPTER;
#[cfg(unix)]
use super::super::test_support::QUEUE_TIMEOUT_READY;
use super::super::test_support::write_established_thread;
use super::super::{PENDING_TURN_FILE, read_pending_turn};
use super::*;
#[cfg(unix)]
use crate::native::native_delegation_prompt;
#[cfg(unix)]
use crate::native::provider::NativeProviderAdapter;
#[cfg(unix)]
use crate::native::session::SessionState;
#[cfg(unix)]
use crate::native::session::primitives::read_json;
use crate::native::session::primitives::{event_paths, write_json_atomic};
#[cfg(unix)]
use crate::native::session::schema::SessionEvent;
use crate::native::session::schema::{SESSION_SCHEMA, SessionManifest};
use crate::native::session::test_support::TURN_CLAIM_FILE;
#[cfg(unix)]
use crate::native::session::{SessionStatus, update_status};
use crate::native::session::{Store, turn};
#[cfg(unix)]
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
fn outcome_error(outcome: &Outcome) -> &anyhow::Error {
    match outcome {
        Outcome::NotSent(error) | Outcome::Uncertain(error) => error,
        Outcome::Sent => panic!("expected failure"),
    }
}

#[test]
fn queue_keeps_the_inherited_environment_and_its_explicit_session_directory() {
    let directory = std::path::Path::new(".");
    let command =
        super::queue_command(std::path::Path::new("unused"), directory, directory, vec![]).unwrap();
    crate::native::process_env::tests::assert_provider_environment(
        &command,
        &[],
        &[(
            crate::native::session::primitives::SESSION_DIR_ENV,
            Some(directory.as_os_str()),
        )],
    );
}

#[test]
fn recovery_hint_uses_the_public_command() {
    let hint = super::unaddressed_follow_up();
    assert!(hint.contains("`tabcli doctor <session> --probe`"));
    assert!(hint.contains(&format!(
        "`{} doctor <session> --probe`",
        agent_bridge::PUBLIC_COMMAND
    )));
    assert!(!hint.contains(&format!("{} doctor", "agent-bridge")));
}

#[cfg(unix)]
fn write_successful_queue_provider(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let provider = directory.join("fake-codex");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\0' "$@" > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-argv.bin"
  pwd > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-cwd.txt"
  printf '%s\n' '{"status":"running","appServerVersion":"0.153.2"}'
  exit 0
fi
if [ "$1" = "queue" ]; then
  printf '%s\0' "$@" > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-argv.bin"
  pwd > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-cwd.txt"
  printf 'Queued message queued-id for thread %s.\n' "$3"
  exit 0
fi
exit 91
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    provider
}

fn write_queue_manifest(directory: &Path, provider: &Path, version: &str) {
    write_queue_manifest_for_workspace(directory, provider, version, directory);
}

fn write_queue_manifest_for_workspace(
    directory: &Path,
    provider: &Path,
    version: &str,
    workspace: &Path,
) {
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: "session-codexqueue".to_owned(),
            provider: "codex".to_owned(),
            provider_path: provider.to_owned(),
            provider_version: version.to_owned(),
            workspace: workspace.to_owned(),
            title: "Codex queue test".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn unavailable_queue_never_selects_unaddressed_terminal_input() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session-codexqueue");
    std::fs::create_dir(&path).unwrap();
    let directory = path.as_path();
    let provider = directory.join("fake-codex");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\n' '{"status":"running","appServerVersion":"0.159.3"}'
  exit 0
fi
printf '%s\n' 'Error: failed to queue session message: thread/queue/add failed: user message queue is unavailable (code -32600)' >&2
exit 1
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    write_queue_manifest(directory, &provider, "codex-cli 0.159.3");
    // The stored result belongs to A. It says nothing about whether the TUI
    // still shows A, has switched to B, or is displaying the agent picker.
    let managed_thread = "018f0000-0000-7000-8000-000000000001";
    write_established_thread(directory, managed_thread);
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (claim, _) =
        turn::claim_ready(&Store::open_unchecked(directory), "session-codexqueue", &[]).unwrap();
    update_status(directory, SessionState::Working, None, None).unwrap();
    let failure = send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory,
        provider_path: &provider,
        correlation_id: claim.token(),
        prompt: "request addressed only to A",
        deadline: Instant::now() + Duration::from_secs(2),
    })
    .unwrap_err();

    assert_eq!(
        ADAPTER.follow_up_transport().facts().name,
        "provider-native-queue",
        "a queue rejection must not select a paste into an unverified active thread"
    );
    assert!(matches!(failure, Outcome::NotSent(_)));
    assert!(!directory.join(PENDING_TURN_FILE).exists());
    let reason = format!("{:#}", outcome_error(&failure));
    assert!(reason.contains("the active thread cannot be verified"));
    assert!(reason.contains("user message queue is unavailable"));
    let receipt = crate::native::session::requests::for_claim(
        &crate::native::session::Reader::open_unchecked(directory),
        claim.token(),
    )
    .unwrap()
    .unwrap();
    let token = claim.token().to_owned();
    drop(claim);
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
    assert_eq!(event_paths(directory).unwrap().len(), 1);
    assert_eq!(
        established_codex_thread(directory).unwrap().as_deref(),
        Some(managed_thread)
    );
    assert_eq!(
        crate::native::session::requests::for_claim(
            &crate::native::session::Reader::open_unchecked(directory),
            &token
        )
        .unwrap()
        .unwrap()
        .request_id,
        receipt.request_id
    );
    // A known pre-send refusal releases the claim and allows a later,
    // independently requested addressed turn; it never retries this prompt.
    let (next, _) =
        turn::claim_ready(&Store::open_unchecked(directory), "session-codexqueue", &[]).unwrap();
    assert_ne!(next.token(), token);
}

#[test]
fn codex_native_queue_uses_the_authoritative_thread_and_claim_marker() {
    let pending = PendingCodexTurn::new("1-2-3").unwrap();
    assert_eq!(
        codex_queue_arguments(
            "018f0000-0000-7000-8000-000000000001",
            "follow up",
            &pending
        ),
        vec![
            OsString::from("queue"),
            OsString::from("--thread"),
            OsString::from("018f0000-0000-7000-8000-000000000001"),
            OsString::from("--message"),
            OsString::from(correlated_prompt("follow up", &pending)),
        ]
    );
}

#[test]
fn codex_queue_accepts_only_the_expected_thread_confirmation() {
    let thread_id = "018f0000-0000-7000-8000-000000000001";
    assert!(
        classify_codex_queue_output(
            true,
            format!("Queued message queued-id for thread {thread_id}.\n").as_bytes(),
            b"",
            false,
            thread_id,
        )
        .is_ok()
    );

    for (stdout, truncated) in [
        (b"unexpected success".as_slice(), false),
        (b"".as_slice(), true),
    ] {
        let failure =
            classify_codex_queue_output(true, stdout, b"", truncated, thread_id).unwrap_err();
        assert!(matches!(failure, Outcome::Uncertain(_)));
    }
}

#[test]
fn codex_queue_reports_not_sent_only_for_explicit_pre_delivery_unavailability() {
    let thread_id = "018f0000-0000-7000-8000-000000000001";
    for stderr in [
        format!(
            "Error: failed to queue session message: thread/queue/add failed: thread not found: {thread_id} (code -32600)"
        ),
        "Error: the local app-server daemon does not support thread/queue/add; update or restart the local app-server daemon: failed to queue session message: thread/queue/add failed: Method not found (code -32601)".to_owned(),
        "Error: failed to queue session message: thread/queue/add failed: user message queue is unavailable (code -32600)".to_owned(),
        format!(
            "Error: failed to queue session message: thread/queue/add failed: ephemeral thread does not support queued submissions: {thread_id} (code -32600)"
        ),
        format!(
            "Error: failed to queue session message: thread/queue/add failed: session {thread_id} is archived. Run `codex unarchive {thread_id}` to unarchive it first. (code -32600)"
        ),
        "Error: failed to queue session message: thread/queue/add failed: direct app-server input is not allowed for multi-agent v2 sub-agents (code -32600)".to_owned(),
        "Error: failed to queue session message: thread/queue/add failed: direct app-server input is not allowed for unloaded spawned sub-agents (code -32600)".to_owned(),
        "Error: failed to queue session message: thread/queue/add failed: queue cannot contain more than 100 submissions (code -32600)".to_owned(),
        "Error: failed to queue session message: thread/queue/add failed: Input exceeds the maximum length of 1048576 characters. (code -32602)".to_owned(),
        "Error: failed to queue session message: thread/queue/add failed: failed to read thread: database unavailable (code -32603)".to_owned(),
    ] {
        let failure =
            classify_codex_queue_output(false, b"", stderr.as_bytes(), false, thread_id)
                .unwrap_err();
        assert!(outcome_error(&failure).to_string().contains(&unaddressed_follow_up()), "{stderr}");
        assert!(matches!(failure, Outcome::NotSent(_)), "{stderr}");
    }

    let failure = classify_codex_queue_output(
        false,
        b"",
        b"Error: failed to queue session message: thread/queue/add transport error: transport closed after request",
        false,
        thread_id,
    )
    .unwrap_err();
    assert!(matches!(failure, Outcome::Uncertain(_)));
}

#[cfg(unix)]
#[test]
fn codex_native_queue_runs_the_provider_command_with_the_claim_marker() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    let provider = write_successful_queue_provider(&directory);
    write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
    let thread_id = "018f0000-0000-7000-8000-000000000001";
    write_established_thread(&directory, thread_id);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    // The prompt as `tell` hands it to the adapter: already framed.
    let prompt = native_delegation_prompt("external", "follow up");

    send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory: &directory,
        provider_path: &provider,
        correlation_id: &claim_token,
        prompt: &prompt,
        deadline: Instant::now() + Duration::from_secs(2),
    })
    .unwrap();

    let arguments = std::fs::read(directory.join("queue-argv.bin")).unwrap();
    let arguments = arguments
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8(value.to_vec()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        &arguments[..4],
        ["queue", "--thread", thread_id, "--message"]
    );
    assert_eq!(
        arguments[4],
        format!(
            "[Agent Bridge native delegation]\nSource: external\n\nfollow up\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n<!-- agent-bridge-codex-turn:{claim_token} -->"
        )
    );
    assert!(!directory.join("daemon-argv.bin").exists());
    assert!(directory.join(PENDING_TURN_FILE).is_file());
    assert!(directory.join(TURN_CLAIM_FILE).is_file());
    assert_eq!(event_paths(&directory).unwrap().len(), 1);

    // Codex reports the message it was given: replay the argument that was queued.
    let completion = serde_json::json!({
        "type": "agent-turn-complete",
        "thread-id": thread_id,
        "turn-id": "queued-turn",
        "input-messages": [arguments[4]],
        "last-assistant-message": "queued result",
    });
    ADAPTER.handle_hook(&directory, &completion).unwrap();
    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 2);
    let event: SessionEvent = read_json(paths.last().unwrap()).unwrap();
    assert_eq!(event.message, "queued result");
    assert_eq!(event.provider_session_id.as_deref(), Some(thread_id));
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");

    ADAPTER.handle_hook(&directory, &completion).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn codex_queue_runs_in_the_original_workspace() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    let workspace = root.path().join("original-workspace");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let provider = write_successful_queue_provider(&directory);
    write_queue_manifest_for_workspace(&directory, &provider, "codex-cli 0.153.2", &workspace);
    write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();

    send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory: &directory,
        provider_path: &provider,
        correlation_id: &claim_token,
        prompt: "follow up",
        deadline: Instant::now() + Duration::from_secs(2),
    })
    .unwrap();

    let expected = format!("{}\n", workspace.canonicalize().unwrap().display());
    assert!(!directory.join("daemon-cwd.txt").exists());
    assert_eq!(
        std::fs::read_to_string(directory.join("queue-cwd.txt")).unwrap(),
        expected
    );
}

#[cfg(unix)]
#[test]
fn codex_queue_refuses_before_start_for_old_versions_or_missing_threads() {
    for (version, include_thread) in [("codex-cli 0.148.9", true), ("codex-cli 0.153.2", false)] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-codexqueue");
        std::fs::create_dir(&directory).unwrap();
        let provider = directory.join("provider-must-not-run");
        write_queue_manifest(&directory, &provider, version);
        if include_thread {
            write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
        }
        let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        let claim_token = claim.token().to_owned();
        claim.retain();

        let failure = send_native_queue_message(CrossSessionMessageContext {
            bridge_executable: Path::new("/unused/agent-bridge"),
            directory: &directory,
            provider_path: &provider,
            correlation_id: &claim_token,
            prompt: "follow up",
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .unwrap_err();

        assert!(matches!(failure, Outcome::NotSent(_)), "{version}");
        assert!(!directory.join(PENDING_TURN_FILE).exists(), "{version}");
    }
}

#[cfg(unix)]
#[test]
fn codex_queue_never_treats_a_non_uuid_provider_identity_as_a_session_name() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    let provider = directory.join("provider-must-not-run");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
: > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/provider-ran"
exit 91
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
    write_established_thread(&directory, "human-readable-session-name");
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();

    let failure = send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory: &directory,
        provider_path: &provider,
        correlation_id: &claim_token,
        prompt: "follow up",
        deadline: Instant::now() + Duration::from_secs(1),
    })
    .unwrap_err();

    assert!(matches!(failure, Outcome::NotSent(_)));
    assert!(
        !directory.join("provider-ran").exists(),
        "native queue attempted to resolve a non-UUID target as an exact session name"
    );
}

#[test]
fn codex_queue_uses_the_provider_backend_without_a_daemon_probe() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    #[cfg(unix)]
    let provider = directory.join("fake-codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        crate::native::session::primitives::write_private(
            &provider,
            br#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/daemon-probed"
  printf '%s\n' 'daemon socket is missing' >&2
  exit 1
fi
if [ "$1" = "queue" ]; then
  : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-ran"
  printf 'Queued message queued-id for thread %s.\n' "$3"
  exit 0
fi
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    let provider = directory.join("fake-codex.ps1");
    #[cfg(windows)]
    crate::native::session::primitives::write_private(
        &provider,
        br#"if ($args[0] -eq 'app-server') {
  [IO.File]::WriteAllText((Join-Path $env:AGENT_BRIDGE_NATIVE_SESSION_DIR 'daemon-probed'), '')
  [Console]::Error.WriteLine('daemon socket is missing')
  exit 1
}
if ($args[0] -eq 'queue') {
  [IO.File]::WriteAllText((Join-Path $env:AGENT_BRIDGE_NATIVE_SESSION_DIR 'queue-ran'), '')
  [Console]::WriteLine(('Queued message queued-id for thread {0}.' -f $args[2]))
  exit 0
}
exit 91
"#,
    )
    .unwrap();
    write_queue_manifest(&directory, &provider, "codex-cli 0.160.0");
    write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();

    send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory: &directory,
        provider_path: &provider,
        correlation_id: &claim_token,
        prompt: "follow up",
        deadline: Instant::now() + Duration::from_secs(10),
    })
    .unwrap();

    assert!(!directory.join("daemon-probed").exists());
    assert!(directory.join("queue-ran").exists());
    assert!(directory.join(PENDING_TURN_FILE).exists());
    assert!(directory.join(TURN_CLAIM_FILE).exists());
    assert_eq!(
        read_pending_turn(&directory).unwrap().unwrap().claim_token,
        claim_token
    );
    assert_eq!(event_paths(&directory).unwrap().len(), 1);
}

#[cfg(unix)]
struct QueueTimeoutFixture {
    pipe: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for QueueTimeoutFixture {
    fn drop(&mut self) {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};

        QUEUE_TIMEOUT_READY.with(|path| path.borrow_mut().take());
        // A broken containment path must fail the test without leaving the
        // fake descendant blocked forever, including on an earlier assertion.
        if let Ok(mut writer) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pipe)
        {
            let _ = writer.write_all(b"stop\n");
        }
    }
}

#[cfg(unix)]
#[test]
fn codex_queue_timeout_terminates_wrapper_descendants_and_retains_the_claim() {
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    };

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    let pipe = directory.join("descendant-pipe");
    let pipe_name = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(pipe_name.as_ptr(), 0o600) }, 0);
    let provider = directory.join("fake-codex");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
if [ "$1" = "app-server" ]; then
  printf '%s\n' '{"status":"running","appServerVersion":"0.153.2"}'
  exit 0
fi
if [ "$1" = "queue" ]; then
  # Deliberately exceed the old shared 500 ms startup budget.
  sleep 0.65
  (
    exec 3<>"$AGENT_BRIDGE_NATIVE_SESSION_DIR/descendant-pipe"
    : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/queue-started"
    read -r finish <&3
    : > "$AGENT_BRIDGE_NATIVE_SESSION_DIR/descendant-survived"
  ) &
  wait
fi
exit 91
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    write_queue_manifest(&directory, &provider, "codex-cli 0.153.2");
    write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();

    // Give setup a separate bounded budget, then trigger the real timeout path
    // only after the descendant holds the FIFO. The thread-local hook cannot
    // affect another test, the daemon probe, or a release build.
    QUEUE_TIMEOUT_READY.with(|path| {
        assert!(
            path.borrow_mut()
                .replace(directory.join("queue-started"))
                .is_none()
        );
    });
    let _fixture = QueueTimeoutFixture { pipe: pipe.clone() };
    let failure = send_native_queue_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/unused/agent-bridge"),
        directory: &directory,
        provider_path: &provider,
        correlation_id: &claim_token,
        prompt: "follow up",
        deadline: Instant::now() + Duration::from_secs(5),
    })
    .unwrap_err();

    let delivery_uncertain = matches!(failure, Outcome::Uncertain(_));
    let reason = format!("{:#}", outcome_error(&failure));
    assert!(reason.contains("Codex queue timed out"), "{reason}");
    QUEUE_TIMEOUT_READY.with(|path| {
        assert!(
            path.borrow().is_none(),
            "queue timeout hook was not consumed"
        );
    });
    assert!(delivery_uncertain, "{reason}");
    assert!(directory.join(PENDING_TURN_FILE).is_file());
    assert!(directory.join("queue-started").is_file());
    assert_eq!(
        std::fs::read_to_string(
            directory.join(crate::native::session::test_support::TURN_CLAIM_FILE)
        )
        .unwrap()
        .trim(),
        claim_token,
    );
    // A killed descendant closes the FIFO even if it remains a zombie. This
    // observes teardown directly instead of guessing from a delayed file write.
    let stopped = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pipe)
        {
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => break,
            Ok(writer) => drop(writer),
            Err(error) => panic!("failed to inspect descendant FIFO: {error}"),
        }
        assert!(
            Instant::now() < stopped,
            "timed-out descendant still holds its FIFO"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !directory.join("descendant-survived").exists(),
        "a timed-out provider wrapper left a message-delivery descendant running"
    );
}
