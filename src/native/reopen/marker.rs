//! Optional, read-only Reopen marker Observation. The command re-observes under its
//! source lock before changing a marker; doctor only renders the same Judgment.
use super::{
    RecordedReopenRefusal, RefusedLaunchCleanup, ReopenMarker, read_reopen_launch_refusal,
    refused_launch_cleanup, reopen_refusal, valid_session_id,
};
use crate::native::diagnostics::{Availability, Check};
use crate::native::session::{CoreRecord, Reader};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[derive(Debug)]
pub(super) struct StaleReopenMarker {
    pub(super) refused_session: String,
    pub(super) gate: String,
    pub(super) cleanup: RefusedLaunchCleanup,
}

pub(super) enum Judgment {
    Absent,
    Malformed(anyhow::Error),
    InProgress { claim: String },
    Target { id: String, judgment: Target },
}

pub(super) enum Target {
    InvalidId,
    MissingRecords,
    NoLaunchRefusal,
    Refused {
        record: Box<RecordedReopenRefusal>,
        cleanup: Cleanup,
    },
}

pub(super) enum Cleanup {
    Reconcilable(RefusedLaunchCleanup),
    Retained(RefusedLaunchCleanup),
    Unverified(anyhow::Error),
}

// Reading and decoding remain distinct: an unreadable record propagates its original
// error to the command, whereas malformed JSON refuses with already-reopened.
pub(super) fn observe(directory: &Path) -> Result<Judgment> {
    let reader = Reader::open_unchecked(directory);
    let record = reader.record(CoreRecord::ReopenMarker);
    let Some(text) = record.text()? else {
        return Ok(Judgment::Absent);
    };
    let marker: ReopenMarker = match serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in {}", record.path().display()))
    {
        Ok(marker) => marker,
        Err(error) => return Ok(Judgment::Malformed(error)),
    };
    let Some(id) = marker.reopened_by else {
        return Ok(Judgment::InProgress {
            claim: marker.claim,
        });
    };
    let judgment = if !valid_session_id(&id) {
        Target::InvalidId
    } else {
        let target = directory
            .parent()
            .context("session directory has no state root")?
            .join(&id);
        if !fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
            Target::MissingRecords
        } else if let Some(record) = read_reopen_launch_refusal(&target) {
            let cleanup = match refused_launch_cleanup(&target, &record) {
                Ok(cleanup) if cleanup.releases_marker() => Cleanup::Reconcilable(cleanup),
                Ok(cleanup) => Cleanup::Retained(cleanup),
                Err(error) => Cleanup::Unverified(error),
            };
            Target::Refused {
                record: Box::new(record),
                cleanup,
            }
        } else {
            Target::NoLaunchRefusal
        }
    };
    Ok(Judgment::Target { id, judgment })
}

impl Judgment {
    pub(super) fn into_stale(self, source: &str) -> Result<Option<StaleReopenMarker>> {
        let detail = match self {
            Self::Absent => return Ok(None),
            Self::Malformed(_) => format!(
                "session {source} carries a reopen marker that cannot be read; it is treated as consumed"
            ),
            Self::InProgress { .. } => {
                format!("a reopen of session {source} is already in progress")
            }
            Self::Target { id, judgment } => {
                let already = format!("session {source} was already reopened as {id}");
                match judgment {
                    Target::InvalidId => format!(
                        "{already}; the marker names an invalid session id, so it is treated as consumed"
                    ),
                    Target::MissingRecords => format!(
                        "{already}; the records of {id} are missing, so the marker is treated as consumed"
                    ),
                    Target::NoLaunchRefusal => already,
                    Target::Refused { record, cleanup } => match cleanup {
                        Cleanup::Reconcilable(cleanup) => {
                            return Ok(Some(StaleReopenMarker {
                                refused_session: id,
                                gate: record.gate,
                                cleanup,
                            }));
                        }
                        Cleanup::Retained(cleanup) => format!(
                            "{already}; that launch was refused ({}) but {cleanup}",
                            record.gate
                        ),
                        Cleanup::Unverified(error) => format!(
                            "{already}; that launch was refused ({}) but its records cannot be verified: {error:#}",
                            record.gate
                        ),
                    },
                }
            }
        };
        Err(reopen_refusal("already-reopened", detail))
    }
}

pub(in crate::native) fn check(reader: &Reader, source: &str, checks: &mut Vec<Check>) {
    use Availability::*;
    let observation = observe(reader.directory());
    let judgment = match observation {
        Ok(Judgment::Absent) => return,
        Ok(Judgment::Malformed(error)) | Err(error) => {
            checks.push(Check::new("reopen_marker", Unknown, "reopen_marker_unreadable", format!("{error:#}"), "reopen treats an unreadable marker as consumed; inspect the marker before any reopen."));
            return;
        }
        Ok(judgment) => judgment,
    };
    let next_action = "reopen releases a marker only when the session it names recorded a launch refusal and either no provider process was spawned or the provider process recorded in its provider-process.json is verified gone (pid dead, or alive under another identity); doctor never releases or repairs it.";
    let (id, judgment) = match judgment {
        Judgment::InProgress { claim } => {
            checks.push(Check::new("reopen_marker", Unavailable, "reopen_in_progress", format!("A reopen of {source} holds the marker and has not recorded its new session yet; a new reopen is refused with gate already-reopened."), next_action).evidence(json!({"claim": claim})));
            return;
        }
        Judgment::Target { id, judgment } => (id, judgment),
        Judgment::Absent | Judgment::Malformed(_) => unreachable!("handled above"),
    };
    let refusal = match &judgment {
        Target::Refused { record, .. } => Some(record),
        _ => None,
    };
    let (availability, reason, detail, cleanup) = match &judgment {
        Target::InvalidId | Target::MissingRecords | Target::NoLaunchRefusal => {
            let condition = match judgment {
                Target::InvalidId => "the marker names an invalid session id".to_owned(),
                Target::MissingRecords => format!("the records of {id} are missing"),
                _ => "which recorded no launch refusal".to_owned(),
            };
            (
                Unavailable,
                "reopen_marker_consumed",
                format!(
                    "Session {source} was reopened as {id}, {condition}; a new reopen is refused with gate already-reopened."
                ),
                Value::Null,
            )
        }
        Target::Refused { record, cleanup } => match cleanup {
            Cleanup::Reconcilable(cleanup) => (
                Available,
                "reopen_marker_reconcilable",
                format!(
                    "The reopen as {id} was refused at launch (gate {}) and {cleanup}; the next reopen of {source} releases the marker and proceeds.",
                    record.gate
                ),
                json!(cleanup.to_string()),
            ),
            Cleanup::Retained(cleanup) => (
                Unavailable,
                "reopen_marker_retained",
                format!(
                    "The reopen as {id} was refused at launch (gate {}) but {cleanup}; the marker stays consumed and a new reopen is refused with gate already-reopened until the provider process of {id} is verified gone. Closing its console or the exit of its launch wrapper is not that evidence on its own.",
                    record.gate
                ),
                json!(cleanup.to_string()),
            ),
            Cleanup::Unverified(error) => (
                Unknown,
                "reopen_marker_unverified",
                format!(
                    "The reopen as {id} was refused at launch (gate {}) but its records cannot be verified: {error:#}",
                    record.gate
                ),
                Value::Null,
            ),
        },
    };
    checks.push(
        Check::new("reopen_marker", availability, reason, detail, next_action).evidence(json!({
            "reopened_by": id,
            "gate": refusal.map(|refusal| &refusal.gate),
            "recorded_cleanup": refusal.and_then(|refusal| refusal.cleanup.as_ref()),
            "recorded_cleanup_detail": refusal.and_then(|refusal| refusal.cleanup_detail.as_ref()),
            "cleanup": cleanup,
        })),
    );
}
