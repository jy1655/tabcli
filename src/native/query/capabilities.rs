//! What a session's own action admissions would decide at this Snapshot. An observation is
//! not a reservation: the action rechecks every gate under its own lock, and an admissible
//! cancel records intent only, never interruption or completion.
use super::*;
use crate::native::session::cancel::{self, Refusal, RefusalReason};

pub(super) const SCHEMA_VERSION: u32 = 1;

/// Called only while the caller retains the Snapshot's lifecycle lock; it takes no lock,
/// writes nothing and starts no provider process. `observed_unix_ms` is the time of this
/// observation, not of any record.
pub(super) fn value(
    reader: &Reader,
    records: &Snapshot,
    id: &str,
    observed_unix_ms: u128,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "observed_unix_ms": observed_unix_ms,
        "cancel": cancel_value(reader, records, id),
    })
}

fn cancel_value(reader: &Reader, records: &Snapshot, id: &str) -> Value {
    // A legacy session without a lifecycle lock has no Snapshot that excludes writers.
    if records._lock.is_none() {
        return json!({"state": "unknown", "reason": "lifecycle_lock_absent"});
    }
    let support = || {
        provider::cancel_support(
            FirstPartyCli::from_str(&records.manifest.provider).map_err(anyhow::Error::msg)?,
            reader.directory(),
        )
    };
    match cancel::admission(reader, id, support) {
        Ok(admitted) => json!({
            "state": "admissible", "reason": "admission_passed",
            "request_id": admitted.request_id,
        }),
        Err(error) => match error.downcast_ref::<Refusal>() {
            Some(refusal) => {
                let mut value = json!({
                    "state": if refusal.reason() == RefusalReason::ProviderUnsupported {
                        "unsupported"
                    } else {
                        "refused"
                    },
                    "reason": refusal.reason().code(),
                    "detail": refusal.to_string(),
                });
                if let Some(request) = refusal.request_id() {
                    value["request_id"] = json!(request);
                }
                value
            }
            None => json!({
                "state": "unknown", "reason": "evidence_unreadable",
                "detail": format!("{error:#}"),
            }),
        },
    }
}
