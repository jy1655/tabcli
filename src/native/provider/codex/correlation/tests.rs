use super::super::{ADAPTER, PENDING_TURN_FILE, install_pending_turn};
use super::*;
use crate::native::native_delegation_prompt;
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::primitives::{event_paths, read_json};
use crate::native::session::schema::SessionEvent;
use crate::native::session::test_support::TURN_CLAIM_FILE;
use crate::native::session::{SessionState, Store, turn};
use crate::native::session::{SessionStatus, update_status};

fn claim_pending_turn(directory: &Path) -> PendingCodexTurn {
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    install_pending_turn(directory, &token).unwrap()
}

fn marked(message: &str, pending: &PendingCodexTurn) -> String {
    format!("{message}\n{}", pending.marker)
}

#[test]
fn codex_hook_owns_the_official_notify_payload_schema() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let payload = serde_json::json!({
        "thread-id": "codex-thread",
        "turn-id": "codex-turn",
        "last-assistant-message": marked("codex result", &pending),
    });

    ADAPTER.handle_hook(directory.path(), &payload).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "codex result");
    assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
    assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
    assert!(directory.path().join(PENDING_TURN_FILE).is_file());
}

#[test]
fn codex_hook_correlates_exact_output_from_the_official_input_messages() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let payload = serde_json::json!({
        "type": "agent-turn-complete",
        "thread-id": "codex-thread",
        "turn-id": "codex-turn",
        "input-messages": [correlated_prompt(
            &native_delegation_prompt(
                "external",
                "Reply with exactly EXACT_OUTPUT and nothing else.",
            ),
            &pending,
        )],
        "last-assistant-message": "EXACT_OUTPUT",
    });

    ADAPTER.handle_hook(directory.path(), &payload).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "EXACT_OUTPUT");
    assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
    assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
}

// Issue #61. The two notify payloads of one initial request, as Codex CLI 0.159.3
// sent them on 2026-10-02 (session-2qIKg5), in the order that recorded the title as
// the result. The Windows console paste joins the lines of the prompt, so the
// message that Codex reports has none.
#[test]
fn codex_hook_ignores_the_task_title_turn_that_quotes_the_prompt() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let prompt = correlated_prompt(
        &native_delegation_prompt(
            "external",
            "Reply with the single word READY and nothing else. Do not use any tools.",
        ),
        &pending,
    )
    .replace('\n', "");
    let title_turn = serde_json::json!({
        "type": "agent-turn-complete",
        "client": "codex-tui",
        "thread-id": "title-thread",
        "turn-id": "title-turn",
        "input-messages": [format!(
            "Generate a concise, single-line task title of at most 36 characters and under five words where possible. Start with an imperative verb. Capitalize only the first word unless the user's language, proper nouns, acronyms, or code terms require otherwise. Preserve ticket references exactly. Write in the user's language. Do not use quotes, markdown, or trailing punctuation. Do not answer the request.\n\nUser prompt:\n{prompt}"
        )],
        "last-assistant-message": "{\"title\":\"READY\"}",
    });

    ADAPTER.handle_hook(directory.path(), &title_turn).unwrap();

    assert!(event_paths(directory.path()).unwrap().is_empty());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "type": "agent-turn-complete",
                "client": "codex-tui",
                "thread-id": "managed-thread",
                "turn-id": "managed-turn",
                "input-messages": [prompt],
                "last-assistant-message": "READY",
            }),
        )
        .unwrap();

    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "READY");
    assert_eq!(event.provider_session_id.as_deref(), Some("managed-thread"));
    // The title turn that arrives late changes nothing either.
    ADAPTER.handle_hook(directory.path(), &title_turn).unwrap();
    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
}

// Issue #65. A message typed into the managed TUI while Bridge's turn was running is
// steered into that turn: the completion's `input-messages` carries the framed prompt
// first and the typed text last, and the answer follows both (codex-cli 0.163.0, macOS,
// 2026-10-17, session-g3V0ph; both payloads recorded with a temporary build). The title
// turn of the same session still quotes the whole prompt behind Codex's instruction.
#[test]
fn codex_hook_accepts_the_turn_steered_by_a_message_typed_into_the_tui() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let prompt = correlated_prompt(
        &native_delegation_prompt(
            "external",
            "Run the shell command `sleep 90` and wait for it to complete. Do nothing else in the meantime. When it has finished, reply with exactly the single word DONE.",
        ),
        &pending,
    );
    let typed =
        "Direct TUI follow-up typed during the turn: after DONE also write the word PINEAPPLE.";
    let title_turn = serde_json::json!({
        "client": "codex-tui",
        "cwd": "/Users/jy/Developments/ForTest/agent-bridge",
        "input-messages": [format!(
            "Generate a concise, single-line task title of at most 36 characters and under five words where possible. Start with an imperative verb. Capitalize only the first word unless the user's language, proper nouns, acronyms, or code terms require otherwise. Preserve ticket references exactly. Write in the user's language. Do not use quotes, markdown, or trailing punctuation. Do not answer the request.\n\nUser prompt:\n{prompt}"
        )],
        "last-assistant-message": "{\"title\":\"Run and await sleep command\"}",
        "thread-id": "01a14893-fbdf-7b40-807f-2d8463630186",
        "turn-id": "01a14893-fc26-7443-af17-283f9525ba7a",
        "type": "agent-turn-complete",
    });

    ADAPTER.handle_hook(directory.path(), &title_turn).unwrap();
    assert!(event_paths(directory.path()).unwrap().is_empty());

    // The typed text alone, as Codex would report a turn of its own, is not Bridge's turn.
    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "client": "codex-tui",
                "input-messages": [typed],
                "last-assistant-message": "PINEAPPLE",
                "thread-id": "01a14893-f1e3-7e40-8a10-d149b6d583bb",
                "turn-id": "another-turn",
                "type": "agent-turn-complete",
            }),
        )
        .unwrap();
    assert!(event_paths(directory.path()).unwrap().is_empty());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "client": "codex-tui",
                "cwd": "/Users/jy/Developments/ForTest/agent-bridge",
                "input-messages": [prompt, typed],
                "last-assistant-message": "DONE PINEAPPLE",
                "thread-id": "01a14893-f1e3-7e40-8a10-d149b6d583bb",
                "turn-id": "01a14893-f5b7-7051-9ea3-eb789139a896",
                "type": "agent-turn-complete",
            }),
        )
        .unwrap();

    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "DONE PINEAPPLE");
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some("01a14893-f1e3-7e40-8a10-d149b6d583bb")
    );
    assert_eq!(
        event.turn_id.as_deref(),
        Some("01a14893-f5b7-7051-9ea3-eb789139a896")
    );
    // The request is no longer pending: the claim is released and the session is ready.
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert_eq!(
        Store::open_unchecked(directory.path())
            .status()
            .unwrap()
            .state,
        SessionState::Ready
    );
}

// With `/ide` on, the TUI puts its IDE context in front of the message
// (codex-rs/tui/src/ide_context/prompt.rs, rust-v0.159.3). That turn is still the
// one Bridge started; the title turn that quotes it is not. Not observed live: the
// wrapper is taken from Codex's source.
#[test]
fn codex_hook_accepts_the_prompt_behind_the_ide_context() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let prompt = correlated_prompt(
        &native_delegation_prompt("external", "Reply READY."),
        &pending,
    );
    // The active file quotes the request heading itself.
    let with_context = format!(
        "# Context from my IDE setup:\n\n## Active file: src/prompt.rs\n\n## Active selection of the file:\nconst PROMPT_REQUEST_BEGIN: &str = \"## My request for Codex:\";\n\n## My request for Codex:\n{prompt}"
    );
    let payload = |thread: &str, input: String, answer: &str| {
        serde_json::json!({
            "type": "agent-turn-complete",
            "thread-id": thread,
            "turn-id": format!("{thread}-turn"),
            "input-messages": [input],
            "last-assistant-message": answer,
        })
    };

    for refused in [
        // The title turn quotes the whole message, context included.
        format!("Generate a concise, single-line task title.\n\nUser prompt:\n{with_context}"),
        // A context without Bridge's prompt behind any request heading.
        format!(
            "# Context from my IDE setup:\n\n## My request for Codex:\nSummarise this.\n{}",
            pending.marker
        ),
        // The headings alone, in a message that does not begin with the context.
        format!("Note\n## My request for Codex:\n{prompt}"),
    ] {
        ADAPTER
            .handle_hook(
                directory.path(),
                &payload("other-thread", refused, "{\"title\":\"READY\"}"),
            )
            .unwrap();
        assert!(event_paths(directory.path()).unwrap().is_empty());
    }

    ADAPTER
        .handle_hook(
            directory.path(),
            &payload("managed-thread", with_context, "READY"),
        )
        .unwrap();
    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "READY");
    assert_eq!(event.provider_session_id.as_deref(), Some("managed-thread"));
}

#[test]
fn codex_hook_ignores_notify_events_from_a_different_thread() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let initial_pending = claim_pending_turn(directory.path());
    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "managed-thread",
                "turn-id": "managed-turn-1",
                "last-assistant-message": marked("managed result", &initial_pending),
            }),
        )
        .unwrap();
    let _pending = claim_pending_turn(directory.path());
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "title-thread",
                "turn-id": "title-turn",
                "last-assistant-message": "{\"title\":\"Generated title\"}",
            }),
        )
        .unwrap();

    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

#[test]
fn codex_hook_requires_the_established_thread_id_on_a_queued_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let initial_pending = claim_pending_turn(directory.path());
    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "managed-thread",
                "turn-id": "managed-turn-1",
                "last-assistant-message": marked("managed result", &initial_pending),
            }),
        )
        .unwrap();
    let pending = claim_pending_turn(directory.path());
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "turn-id": "unproven-turn",
                "last-assistant-message": marked("unproven result", &pending),
            }),
        )
        .unwrap();

    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

#[test]
fn codex_hook_preserves_a_legitimate_title_shaped_result() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "managed-thread",
                "turn-id": "managed-turn",
                "last-assistant-message": marked("{\"title\":\"Requested title\"}", &pending),
            }),
        )
        .unwrap();

    let paths = event_paths(directory.path()).unwrap();
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "{\"title\":\"Requested title\"}");
}

#[test]
fn codex_hook_does_not_bind_the_first_foreign_notify_event() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let _pending = claim_pending_turn(directory.path());

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "foreign-title-thread",
                "turn-id": "foreign-title-turn",
                "last-assistant-message": "{\"title\":\"Generated title\"}",
            }),
        )
        .unwrap();

    assert!(event_paths(directory.path()).unwrap().is_empty());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn codex_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let initial_pending = claim_pending_turn(directory.path());
    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "thread-id": "managed-thread",
                "turn-id": "managed-turn-1",
                "last-assistant-message": marked("first result", &initial_pending),
            }),
        )
        .unwrap();
    let _later_pending = claim_pending_turn(directory.path());
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "type": "agent-turn-complete",
                "thread-id": "managed-thread",
                "turn-id": "delayed-old-turn-with-a-new-id",
                "input-messages": [correlated_prompt(
                    &native_delegation_prompt("external", "old prompt"),
                    &initial_pending,
                )],
                "last-assistant-message": "delayed old result",
            }),
        )
        .unwrap();

    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}
