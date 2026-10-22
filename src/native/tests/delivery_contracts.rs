use super::support::*;
use super::*;
use crate::native::delivery::{remaining_turn_timeout, terminal_input_bytes};
use crate::native::session::primitives::read_json;
use crate::native::session::test_support::{TURN_CLAIM_FILE, write_test_manifest};
use crate::native::session::turn::Report;
use crate::native::session::{SessionState, SessionStatus, update_status};
use crate::native::{delivery, provider, terminal};
use agent_bridge::provider_effort_args;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn remaining_turn_timeout_rejects_an_exhausted_total_budget() {
    let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let error = remaining_turn_timeout(expired, Duration::from_secs(5)).unwrap_err();
    assert!(format!("{error:#}").contains("timed out after 5 seconds"));

    let future = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
    let remaining = remaining_turn_timeout(future, Duration::from_secs(5)).unwrap();
    assert!(remaining > Duration::ZERO);
    assert!(remaining <= Duration::from_secs(5));
}

#[test]
fn effort_uses_each_provider_native_session_option() {
    assert_eq!(
        provider_effort_args(FirstPartyCli::Codex, "xhigh").unwrap(),
        ["-c", "model_reasoning_effort=\"xhigh\""]
    );
    assert_eq!(
        provider_effort_args(FirstPartyCli::Claude, "max").unwrap(),
        ["--effort", "max"]
    );
    assert_eq!(
        provider_effort_args(FirstPartyCli::Agy, "high").unwrap(),
        ["--effort", "high"]
    );
    assert_eq!(
        provider_effort_args(FirstPartyCli::Pi, "minimal").unwrap(),
        ["--thinking", "minimal"]
    );
}

#[test]
fn follow_up_prompts_only_enter_a_completed_live_cli_turn() {
    assert!(SessionState::Ready.accepts_prompt());
    for state in [
        "launching",
        "running",
        "working",
        "exited",
        "failed",
        "closed",
    ] {
        assert!(
            !state.parse::<SessionState>().unwrap().accepts_prompt(),
            "accepted {state}"
        );
    }
}

#[test]
fn every_provider_declares_initial_and_follow_up_transports() {
    use delivery::{Readiness, Transport};
    for (provider, name, count, delay, surface) in [
        (FirstPartyCli::Codex, "provider-native-queue", 2, 12, false),
        (
            FirstPartyCli::Claude,
            "provider-cross-session-message",
            1,
            2,
            false,
        ),
        (FirstPartyCli::Agy, "terminal-paste-fallback", 1, 0, true),
        (FirstPartyCli::Pi, "terminal-paste-fallback", 1, 2, true),
    ] {
        let follow = provider::follow_up_transport(provider).facts();
        assert_eq!(follow.name, name);
        assert_eq!(follow.surface.is_some(), surface);
        if let Some(input) = follow.surface {
            assert_eq!(input.submit_count, count);
        }
        let initial = provider::initial_transport(provider).facts();
        if cfg!(windows) {
            assert_eq!(
                initial.readiness,
                Readiness::AfterLaunch {
                    ready_delay: Duration::from_secs(delay)
                }
            );
            assert_eq!(initial.surface.is_some(), provider != FirstPartyCli::Claude);
            assert_eq!(initial.surface.map_or(1, |input| input.submit_count), count);
        } else {
            assert_eq!(initial.name, delivery::LAUNCH_ARGUMENT.facts().name);
            assert_eq!(initial.readiness, Readiness::LaunchArgument);
            assert!(initial.surface.is_none());
        }
    }
}

#[test]
fn terminal_input_framing_matches_each_adapter_contract() {
    use terminal::{TerminalKind::*, facts::PasteEnvelope};
    for (kind, envelope, guarded) in [
        (Iterm2, PasteEnvelope::Bracketed, true),
        (AppleTerminal, PasteEnvelope::Bracketed, true),
        (Ghostty, PasteEnvelope::Raw, false),
        (Warp, PasteEnvelope::Bracketed, false),
        (WezTerm, PasteEnvelope::Bracketed, false),
        (WindowsConsole, PasteEnvelope::Raw, true),
    ] {
        let facts = terminal::facts(kind);
        assert_eq!(facts.paste_envelope, envelope);
        assert_eq!(facts.guards_dialog_input, guarded);
        assert_eq!(terminal::guards_dialog_input(kind), guarded);
        let expected: &[u8] = match envelope {
            PasteEnvelope::Raw => b"line one\nline two",
            PasteEnvelope::Bracketed => b"\x1b[200~line one\nline two\x1b[201~",
        };
        assert_eq!(terminal_input_bytes(kind, "line one\nline two"), expected);
    }
}

#[test]
fn initial_prompt_failures_distinguish_safe_abort_from_uncertain_delivery() {
    for delivery_started in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        write_test_manifest(directory.path(), "codex");
        update_status(
            directory.path(),
            SessionState::AwaitingInitialInput,
            None,
            None,
        )
        .unwrap();
        let mut claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        if delivery_started {
            update_status(directory.path(), SessionState::Working, None, None).unwrap();
        }

        {
            let delivery_error = &anyhow::anyhow!("terminal delivery failed");
            let _ = claim.settle_delivery(if delivery_started {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_started {
            assert_eq!(status.state.as_str(), "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        } else {
            assert_eq!(status.state.as_str(), "failed");
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        }
        assert_eq!(status.error.as_deref(), Some("terminal delivery failed"));
    }
}

#[test]
fn follow_up_terminal_send_failure_releases_only_confirmed_not_started_claims() {
    for delivery_may_have_occurred in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        write_test_manifest(directory.path(), "codex");
        update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        let (mut claim, _) = turn::claim_ready(
            &Store::open_unchecked(directory.path()),
            "session-test",
            &[],
        )
        .unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let failure = if delivery_may_have_occurred {
            terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
                "terminal delivery uncertain"
            ))
        } else {
            terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "terminal delivery did not start"
            ))
        };

        {
            let delivery_error = failure.error();
            let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_may_have_occurred {
            assert_eq!(status.state.as_str(), "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(status.error.as_deref(), Some("terminal delivery uncertain"));
        } else {
            assert_eq!(status.state.as_str(), "ready");
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(
                status.error.as_deref(),
                Some("terminal delivery did not start")
            );
        }
    }
}

#[test]
fn follow_up_cross_session_uncertainty_keeps_the_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (mut claim, _) = turn::claim_ready(
        &Store::open_unchecked(directory.path()),
        "session-test",
        &[],
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    {
        let _ = claim.settle_delivery(turn::Delivery::Uncertain(
            &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
        ));
    };
    drop(claim);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
    assert_eq!(
        status.error.as_deref(),
        Some("delivery unconfirmed: executed input was not reported")
    );
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn late_cross_session_uncertainty_cannot_write_into_a_newer_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (mut delivered, _) = turn::claim_ready(
        &Store::open_unchecked(directory.path()),
        "session-test",
        &[],
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    // The target completed the delivered turn while its sender was still settling.
    Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Codex,
        Some(delivered.token()),
    )
    .complete("completed", None, None)
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (newer, _) = turn::claim_ready(
        &Store::open_unchecked(directory.path()),
        "session-test",
        &[],
    )
    .unwrap();

    {
        let _ = delivered.settle_delivery(turn::Delivery::Uncertain(&anyhow::anyhow!(
            "late report for the completed turn"
        )));
    };
    drop(delivered);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "claimed");
    assert_eq!(status.error, None);
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap(),
        Some(newer.token().to_owned())
    );
    newer.retain();
}

#[test]
fn delayed_terminal_delivery_failure_cannot_overwrite_a_replacement_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut stale_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    stale_claim.settle_delivery(turn::Delivery::Sent).unwrap();
    turn::Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Codex,
        Some(stale_claim.token()),
    )
    .complete(
        "turn A result",
        Some("codex-session".to_owned()),
        Some("codex-turn-a".to_owned()),
    )
    .unwrap();
    let replacement = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    replacement.retain();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    let failure = delivery_uncertain_failure("turn A paste timed out");
    stale_claim
        .settle_delivery(turn::Delivery::Uncertain(failure.error()))
        .unwrap();

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state.as_str(), "working");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn late_initial_cross_session_uncertainty_cannot_write_into_a_newer_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut initial = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    initial.settle_delivery(turn::Delivery::Sent).unwrap();
    // The target completed the initial turn and a tell claimed the replacement turn before
    // the initial messenger reported that its delivery could not be confirmed.
    turn::Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Claude,
        Some(initial.token()),
    )
    .complete("initial result", None, Some("claude-turn-1".to_owned()))
    .unwrap();
    let (replacement, _) = turn::claim_ready(
        &Store::open_unchecked(directory.path()),
        "session-test",
        &[],
    )
    .unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(before.state.as_str(), "claimed");

    {
        let _ = initial.settle_delivery(turn::Delivery::Uncertain(&anyhow::anyhow!(
            "late initial report"
        )));
    };
    drop(initial);

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state.as_str(), "claimed");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap(),
        Some(replacement.token().to_owned())
    );
    replacement.retain();
}

#[test]
fn initial_cross_session_uncertainty_keeps_its_own_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut initial = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();

    {
        let _ = initial.settle_delivery(turn::Delivery::Uncertain(
            &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
        ));
    };
    let token = initial.token().to_owned();
    drop(initial);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
    assert_eq!(
        status.error.as_deref(),
        Some("delivery unconfirmed: executed input was not reported")
    );
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap(),
        Some(token)
    );
}

#[cfg(not(any(target_os = "macos", windows)))]
#[test]
fn unsupported_platform_send_is_not_sent() {
    let root = tempfile::tempdir().unwrap();
    let failure = terminal::send_file(
        &close_test_terminal(),
        &root.path().join("absent-prompt"),
        close_deadline(),
    )
    .unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
}
