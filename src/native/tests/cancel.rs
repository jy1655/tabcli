use super::*;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::TURN_CLAIM_FILE;
use crate::native::session::{SessionState, update_status};
use crate::native::{provider, session};
use std::fs;

#[test]
fn cancel_report_orders_intent_completion_and_next_claim() {
    for (requested, interrupted) in [(true, false), (true, true), (false, true)] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("events")).unwrap();
        update_status(dir.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(dir.path()), &[]).unwrap();
        let token = claim.token().to_owned();
        let receipt = claim.receipt().clone();
        claim.retain();
        if requested {
            write_json_atomic(&dir.path().join("cancel.json"), &serde_json::json!({"schema":1,"request_id":receipt.request_id,"claim_token":token,"created_unix_ms":2})).unwrap();
        }
        let store = Store::open_unchecked(dir.path());
        let report = session::turn::Report::for_claim(&store, FirstPartyCli::Pi, Some(&token));
        if interrupted {
            report.cancelled("aborted", None, None).unwrap();
        } else {
            report.complete("done", None, None).unwrap();
        }
        let event: SessionEvent =
            read_json(&dir.path().join("events").join(&receipt.event_file)).unwrap();
        assert_eq!(event.cancelled, requested && interrupted);
        assert_eq!(event.error.is_some(), interrupted);
        assert!(!dir.path().join(TURN_CLAIM_FILE).exists());
        assert_eq!(store.status().unwrap().state, SessionState::Ready);
        let next =
            turn::claim_ready(&Store::open_unchecked(dir.path()), "session-test", &[]).unwrap();
        assert_ne!(next.0.token(), token);
        assert_eq!(dir.path().join("cancel.json").exists(), requested);
    }
}

#[test]
fn every_provider_declares_session_specific_cancel_support() {
    let directory = tempfile::tempdir().unwrap();
    for provider in [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ] {
        match provider::cancel_support(provider, directory.path()).unwrap() {
            provider::CancelSupport::Unsupported(reason) => assert_eq!(
                reason,
                if provider == FirstPartyCli::Pi {
                    "this session was started by an extension without cancel support; start a new session".to_owned()
                } else {
                    format!(
                        "in this release the Bridge integration for {} does not support cancel",
                        provider.as_str()
                    )
                }
            ),
            provider::CancelSupport::Supported => {
                panic!("no session capability evidence was installed")
            }
        }
    }
}
