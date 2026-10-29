use super::{GuardedDialogInput, StaleDialogClaim};
use crate::native::session::{Store, turn};
use anyhow::{Result, bail};
use std::{path::Path, time::Instant};

pub(super) fn dispatch(
    directory: &Path,
    input: &GuardedDialogInput,
    deadline: Instant,
    automation: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    precheck(directory, input, deadline).map_err(|error| error.context(StaleDialogClaim))?;
    // The lock protects this observation only. AppleScript runs after it is released:
    // retirement or a repaint can still occur before the terminal consumes the key.
    automation()
}

fn precheck(directory: &Path, input: &GuardedDialogInput, deadline: Instant) -> Result<()> {
    let store = Store::open_unchecked(directory);
    // A busy lifecycle must not extend the response deadline. No input has been sent.
    let verdict = (turn::ClaimCurrency {
        expected: input.expected_claim.as_deref(),
        state: turn::ClaimState::NotEnded,
        tombstone: turn::ClaimTombstone::Ignored,
        expires: input.expires_unix_ms,
    })
    .try_run(&store, || Ok(Instant::now() < deadline))?;
    match verdict {
        Ok(true) => Ok(()),
        Err(turn::StaleClaim::Changed) => {
            bail!("workspace trust response claim changed or retired")
        }
        Err(turn::StaleClaim::State(_) | turn::StaleClaim::Tombstone) => {
            bail!("session ended before workspace trust response")
        }
        Ok(false) | Err(turn::StaleClaim::Expired) => {
            bail!("workspace trust response deadline expired")
        }
        Err(turn::StaleClaim::Busy) => bail!("session lifecycle is busy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::{Store, primitives::write_private, test_support::*};
    use crate::native::terminal::DialogKey;
    use std::{cell::Cell, time::Duration};

    fn fixture(directory: &Path, state: &str, claim: Option<&str>) {
        let store = Store::open_unchecked(directory);
        store
            .private(STATUS_FILE)
            .write_json(&serde_json::json!({
                "state": state, "updated_unix_ms": 1
            }))
            .unwrap();
        if let Some(claim) = claim {
            write_private(&directory.join(TURN_CLAIM_FILE), claim.as_bytes()).unwrap();
        }
    }

    #[test]
    fn guarded_dialog_refuses_retired_replaced_ended_and_expired_requests() {
        let mut dispatched = Vec::new();
        for (case, state, claim, expires, expired_deadline) in [
            ("retired", "launching", None, u128::MAX, false),
            ("replaced", "launching", Some("successor"), u128::MAX, false),
            ("closed", "closed", Some("expected"), u128::MAX, false),
            ("failed", "failed", Some("expected"), u128::MAX, false),
            ("exited", "exited", Some("expected"), u128::MAX, false),
            ("expired payload", "launching", Some("expected"), 0, false),
            (
                "expired operation",
                "launching",
                Some("expected"),
                u128::MAX,
                true,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            fixture(temp.path(), state, claim);
            let input = GuardedDialogInput {
                expected_claim: Some("expected".into()),
                expires_unix_ms: Some(expires),
                screen: "trust this directory?".into(),
                key: DialogKey::Enter,
            };
            let deadline = Instant::now()
                + if expired_deadline {
                    Duration::ZERO
                } else {
                    Duration::from_secs(10)
                };
            let result = dispatch(temp.path(), &input, deadline, || {
                dispatched.push(case);
                Ok(true)
            });
            assert!(result.unwrap_err().is::<StaleDialogClaim>(), "{case}");
        }
        assert!(
            dispatched.is_empty(),
            "stale dialog requests reached automation: {dispatched:?}"
        );
    }

    #[test]
    fn guarded_dialog_refuses_busy_or_unreadable_state_without_automation() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), "launching", Some("expected"));
        let store = Store::open_unchecked(temp.path());
        let input = GuardedDialogInput {
            expected_claim: Some("expected".into()),
            expires_unix_ms: Some(u128::MAX),
            screen: "exact screen".into(),
            key: DialogKey::Enter,
        };
        let lock = store.lock().unwrap();
        let run = || {
            dispatch(
                temp.path(),
                &input,
                Instant::now() + Duration::from_secs(10),
                || panic!("unverified state must not reach automation"),
            )
            .unwrap_err()
        };
        assert!(format!("{:#}", run()).contains("lifecycle is busy"));
        drop(lock);
        store.private(STATUS_FILE).write_json(&"broken").unwrap();
        assert!(run().is::<StaleDialogClaim>());
    }

    #[test]
    fn guarded_dialog_automation_error_is_not_a_prewrite_refusal() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), "launching", Some("expected"));
        let input = GuardedDialogInput {
            expected_claim: Some("expected".into()),
            expires_unix_ms: Some(u128::MAX),
            screen: "exact screen".into(),
            key: DialogKey::Enter,
        };
        let calls = Cell::new(0);
        let error = dispatch(
            temp.path(),
            &input,
            Instant::now() + Duration::from_secs(10),
            || {
                calls.set(calls.get() + 1);
                bail!("automation failed after dispatch")
            },
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(!error.is::<StaleDialogClaim>());
        assert_eq!(error.to_string(), "automation failed after dispatch");
    }

    #[test]
    fn guarded_dialog_dispatches_once_without_holding_the_lifecycle_lock() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), "launching", Some("expected\n"));
        let calls = Cell::new(0);
        let input = GuardedDialogInput {
            expected_claim: Some("expected".into()),
            expires_unix_ms: Some(u128::MAX),
            screen: "exact screen".into(),
            key: DialogKey::DownEnter,
        };
        let result = dispatch(
            temp.path(),
            &input,
            Instant::now() + Duration::from_secs(10),
            || {
                let store = Store::open_unchecked(temp.path());
                let _lock = store
                    .try_lock()?
                    .expect("precheck must release its lock before automation");
                calls.set(calls.get() + 1);
                Ok(false)
            },
        )
        .unwrap();
        assert!(!result, "changed screen remains the adapter's answer");
        assert_eq!(calls.get(), 1);
    }
}
