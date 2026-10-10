//! The user's request to interrupt one claimed turn.
use super::{CoreRecord, Reader, Store, requests, turn, unix_ms};
use crate::native::{SessionState, provider, valid_turn_claim_token};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Deserialize, Serialize)]
pub(in crate::native) struct Cancel {
    pub(in crate::native) schema: u32,
    pub(in crate::native) request_id: String,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) created_unix_ms: u128,
}

impl Cancel {
    pub(in crate::native) fn validate(&self) -> Result<()> {
        if self.created_unix_ms > u128::from(u64::MAX)
            || self.schema != 1
            || !requests::valid_id(&self.request_id)
            || !valid_turn_claim_token(&self.claim_token)
        {
            bail!("invalid cancel record");
        }
        Ok(())
    }
}

pub(in crate::native) fn read(reader: &Reader) -> Result<Option<Cancel>> {
    let record: Option<Cancel> = reader.record(CoreRecord::Cancel).optional_json()?;
    if let Some(record) = &record {
        record.validate()?;
    }
    Ok(record)
}

pub(in crate::native) fn observe(reader: &Reader, locked: bool) -> Result<Option<Cancel>> {
    if locked {
        return read(reader);
    }
    let record = reader.record(CoreRecord::Cancel);
    let before = record.bytes()?;
    let cancel = read(reader)?;
    if record.bytes()? != before || reader.lock_shared()?.is_some() {
        bail!("cancel record changed during observation; retry the query");
    }
    Ok(cancel)
}

#[derive(Debug)]
pub(in crate::native) struct Refusal(String);
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refusal {}

pub(in crate::native) fn request(store: &Store, id: &str) -> Result<Cancel> {
    request_with_support(store, id, || {
        let manifest = store.manifest()?;
        provider::cancel_support(
            FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
            store.directory(),
        )
    })
}

fn request_with_support(
    store: &Store,
    id: &str,
    support: impl FnOnce() -> Result<provider::CancelSupport>,
) -> Result<Cancel> {
    let _lock = store.lock()?;
    let state = store
        .closed_if_present()?
        .map_or_else(|| store.status().map(|s| s.state), |s| Ok(s.state))?;
    if !matches!(state, SessionState::Running | SessionState::Working) {
        return Err(Refusal(format!(
            "session {id} is {state}; cancel requires the running or working state"
        ))
        .into());
    }
    let token = turn::current_claim_token(store)?
        .ok_or_else(|| Refusal(format!("session {id} has no active claim to cancel")))?;
    let receipt = requests::for_claim(store, &token)?
        .ok_or_else(|| Refusal(format!("session {id} has no receipt for its active claim")))?;
    let pending = store
        .record(CoreRecord::Completion)
        .optional_json::<turn::PendingTurnCompletion>()?;
    if let Some(pending) = &pending {
        turn::validate_pending_completion(pending)?;
    }
    let journaled = pending
        .as_ref()
        .filter(|p| p.event_file == receipt.event_file);
    let published = journaled
        .map(|p| store.journaled_event_state_within(p, crate::native::EVENT_READ_LIMIT))
        .transpose()?;
    if turn::event_published(journaled.is_some(), published.as_ref(), true) {
        return Err(Refusal(format!(
            "request {} already has a published result",
            receipt.request_id
        ))
        .into());
    }
    if read(store)?.is_some_and(|r| r.claim_token == token) {
        return Err(Refusal(format!(
            "a cancel for request {} is already recorded",
            receipt.request_id
        ))
        .into());
    }
    if let provider::CancelSupport::Unsupported(reason) = support()? {
        return Err(Refusal(reason).into());
    }
    let record = Cancel {
        schema: 1,
        request_id: receipt.request_id,
        claim_token: token,
        created_unix_ms: unix_ms(),
    };
    store
        .record(CoreRecord::Cancel)
        .write_json(&record)
        .context("failed to record cancel request")?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::*;

    fn fixture() -> (tempfile::TempDir, Store, session::turn::Claim) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("events")).unwrap();
        update_status(dir.path(), SessionState::Working, None, None).unwrap();
        let store = Store::open_unchecked(dir.path());
        let claim = acquire_turn_claim(dir.path()).unwrap();
        (dir, store, claim)
    }

    fn supported(store: &Store) -> Result<Cancel> {
        request_with_support(store, "session-test", || {
            Ok(provider::CancelSupport::Supported)
        })
    }

    #[test]
    fn cancel_admission_refusals_and_stale_record_replacement() {
        let (_dir, store, claim) = fixture();
        let token = claim.token().to_owned();
        let receipt = claim.receipt().clone();
        claim.retain();
        update_status(store.directory(), SessionState::Ready, None, None).unwrap();
        assert!(
            supported(&store)
                .unwrap_err()
                .to_string()
                .contains("requires the running or working state")
        );
        let mut running = store.status().unwrap();
        running.state = SessionState::Running;
        store.write_status(&running).unwrap();
        let error = request_with_support(&store, "session-test", || {
            Ok(provider::CancelSupport::Unsupported(
                "not supported".to_owned(),
            ))
        })
        .unwrap_err();
        assert!(error.is::<Refusal>());
        assert_eq!(error.to_string(), "not supported");
        assert!(read(&store).unwrap().is_none());
        let first = supported(&store).unwrap();
        assert_eq!(first.request_id, receipt.request_id);
        let before = store.record(CoreRecord::Cancel).bytes().unwrap();
        assert_eq!(
            supported(&store).unwrap_err().to_string(),
            format!(
                "a cancel for request {} is already recorded",
                receipt.request_id
            )
        );
        assert_eq!(store.record(CoreRecord::Cancel).bytes().unwrap(), before);
        session::turn::Report::for_claim(&store, FirstPartyCli::Pi, Some(&token))
            .complete("done", None, None)
            .unwrap();
        let next = acquire_ready_turn_claim(store.directory(), "session-test")
            .unwrap()
            .0;
        let next_token = next.token().to_owned();
        next.retain();
        update_status(store.directory(), SessionState::Working, None, None).unwrap();
        let second = supported(&store).unwrap();
        assert_eq!(second.claim_token, next_token);
        assert_ne!(second.request_id, first.request_id);
        assert_eq!(read(&store).unwrap().unwrap().claim_token, next_token);
        store.record(CoreRecord::TurnClaim).remove().unwrap();
        assert!(
            supported(&store)
                .unwrap_err()
                .to_string()
                .contains("no active claim")
        );
        store
            .record(CoreRecord::TurnClaim)
            .write_private(format!("{next_token}\n").as_bytes())
            .unwrap();
        store
            .record(CoreRecord::Requests)
            .child(&format!("{next_token}.json"))
            .remove()
            .unwrap();
        assert!(
            supported(&store)
                .unwrap_err()
                .to_string()
                .contains("no receipt")
        );
    }

    #[test]
    fn cancel_after_publication_refuses_before_claim_release() {
        let (_dir, store, claim) = fixture();
        let token = claim.token().to_owned();
        let receipt = claim.receipt().clone();
        claim.retain();
        let event: SessionEvent = serde_json::from_value(serde_json::json!({
            "provider":"pi", "message":"done", "error":null,
            "provider_session_id":null, "turn_id":null, "created_unix_ms":3
        }))
        .unwrap();
        let mut pending = PendingTurnCompletion::new(&token, event, None).unwrap();
        pending.event_file = receipt.event_file;
        store.write_completion(&pending).unwrap();
        write_pending_completion_event(store.directory(), &pending).unwrap();
        let before = crate::native::reopen::tests::snapshot_directory(store.directory());
        assert!(
            supported(&store)
                .unwrap_err()
                .to_string()
                .contains("already has a published result")
        );
        assert_eq!(
            crate::native::reopen::tests::snapshot_directory(store.directory()),
            before
        );
    }

    #[test]
    fn cancelled_journal_requires_error_and_empty_body() {
        let event: SessionEvent = serde_json::from_value(serde_json::json!({
            "provider":"pi", "message":"", "error":"cancelled: aborted", "cancelled":true,
            "provider_session_id":null, "turn_id":null, "created_unix_ms":3
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap()["cancelled"], true);
        let mut pending =
            PendingTurnCompletion::new("1-2-3", event, Some("cancelled: aborted".to_owned()))
                .unwrap();
        session::turn::validate_pending_completion(&pending).unwrap();
        pending.event.message = "success".to_owned();
        assert!(session::turn::validate_pending_completion(&pending).is_err());
        pending.event.message.clear();
        pending.event.error = None;
        pending.status_error = None;
        assert!(session::turn::validate_pending_completion(&pending).is_err());
    }
}
