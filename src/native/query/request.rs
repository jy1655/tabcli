//! Typed Request observation shared by read-only queries.
use super::Selector;
use super::observation::judgment::OwnerJudgment;
use crate::native::envelope;
use crate::native::session::schema::{SessionEvent, USAGE_OMITTED_SIZE};
use crate::native::session::snapshot::Snapshot;
use crate::native::session::{Reader, SessionState, launch, requests};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum RequestState {
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
    Pending,
    Unresolved,
    Unavailable,
}

impl RequestState {
    pub(in crate::native) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
            Self::Pending => "pending",
            Self::Unresolved => "unresolved",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One Request's meaning, independent of its command's JSON representation. The
/// snapshot retains the lifecycle read lock; each caller keeps its own read policy.
pub(in crate::native) struct RequestObservation<'a> {
    pub(super) snapshot: &'a Snapshot,
    pub(super) receipt: Option<&'a requests::Receipt>,
    pub(super) event_id: Option<&'a str>,
    pub(super) event: Option<SessionEvent>,
    pub(super) state: RequestState,
    pub(super) error: Option<String>,
}

impl RequestObservation<'_> {
    pub(super) fn with_owner(mut self, owner: &OwnerJudgment, session_ended: bool) -> Self {
        if owner.gone() {
            self.state = RequestState::Unresolved;
            self.error = Some(
                "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request".to_owned()
            );
        } else if session_ended {
            self.state = RequestState::Unresolved;
        }
        self
    }

    pub(in crate::native) fn state(&self) -> RequestState {
        self.state
    }

    pub(in crate::native) fn event_id(&self) -> Option<&str> {
        self.event_id
    }

    pub(super) fn value(&self) -> Value {
        let snapshot = self.snapshot;
        let event = self.event.as_ref();
        let (elapsed, elapsed_reason) = observed_elapsed(self.receipt, event);
        let (usage, usage_reason, usage_error) = reported_usage(event);
        envelope::ok(json!({
            "bridge_observed_elapsed_ms": elapsed, "bridge_observed_elapsed_reason": elapsed_reason,
            "usage": usage, "usage_reason": usage_reason, "usage_error": usage_error,
            "session": snapshot.manifest.id,
            "provider": snapshot.manifest.provider, "workspace": snapshot.manifest.workspace,
            "request_id": self.receipt.map(|r| &r.request_id), "event_id": self.event_id,
            "context_sources": self.receipt.map(|r| r.context_sources.as_slice()).unwrap_or_default(),
            "request_state": self.state, "session_state": snapshot.status.state,
            "result": event.map(|e| &e.message), "error": self.error,
            "session_error": snapshot.status.error,
            "provider_session_id": event.and_then(|e| e.provider_session_id.as_ref()),
            "turn_id": event.and_then(|e| e.turn_id.as_ref()),
            "created_unix_ms": event.map(|e| e.created_unix_ms),
            "recovery_required": snapshot.pending.is_some(),
            "unreadable_requests": snapshot.unreadable_requests,
            "request_index_error": snapshot.request_index_error,
        }).as_object().unwrap().clone())
    }

    fn attachment(
        &self,
        reader: &Reader,
        session: &str,
    ) -> std::result::Result<(requests::ContextSource, String), AttachmentFailure> {
        let refuse = |state, detail| AttachmentFailure { state, detail };
        let unreadable = |detail| refuse("unreadable", Some(detail));
        if self.state != RequestState::Completed || self.event.is_none() || self.error.is_some() {
            return Err(refuse(self.state.as_str(), None));
        }
        let event_id = self
            .event_id
            .ok_or_else(|| unreadable("no event id".to_owned()))?;
        let request_id = self.receipt.map(|r| r.request_id.clone());
        let snapshot = self.snapshot;
        // An unmapped event is legacy only when the entire Receipt index was readable.
        if request_id.is_none()
            && (snapshot.unreadable_requests > 0 || snapshot.request_index_error.is_some())
        {
            let mut detail = format!(
                "the request index has {} unreadable receipt(s), so the event may still belong to an active request",
                snapshot.unreadable_requests
            );
            if let Some(error) = &snapshot.request_index_error {
                detail = format!("{detail}; {error}");
            }
            return Err(refuse("unverifiable", Some(detail)));
        }
        // A case-insensitive filesystem must not silently select a different event name.
        let exact = reader
            .events()
            .map_err(|e| unreadable(format!("{e:#}")))?
            .iter()
            .any(|path| path.file_name().and_then(|name| name.to_str()) == Some(event_id));
        if !exact {
            return Err(unreadable(format!(
                "recorded event filename {event_id} does not match an events/ entry exactly"
            )));
        }
        // Attachments deliberately re-read strictly; ordinary queries and the timeline
        // retain their distinct readers. Do not substitute the lossy event above.
        let event = reader.event_strict(event_id).map_err(unreadable)?;
        if event.cancelled || event.error.is_some() {
            return Err(refuse("failed", None));
        }
        let source = requests::ContextSource {
            session: session.to_owned(),
            request_id,
            event_id: event_id.to_owned(),
            provider: snapshot.manifest.provider.clone(),
            created_unix_ms: event
                .created_unix_ms
                .ok_or_else(|| unreadable("no event creation time".to_owned()))?,
        };
        requests::validate_context_source(&source)
            .map_err(|e| unreadable(format!("invalid recorded provenance: {e:#}")))?;
        Ok((source, event.message))
    }
}

pub(in crate::native) struct AttachmentFailure {
    pub(in crate::native) state: &'static str,
    pub(in crate::native) detail: Option<String>,
}

// Bridge wall-clock time from receipt creation to the published completion event's
// timestamp, including dispatch and delivery waits; never model or billing time.
pub(in crate::native) fn observed_elapsed(
    receipt: Option<&requests::Receipt>,
    event: Option<&SessionEvent>,
) -> (Option<u128>, Option<&'static str>) {
    let Some(event) = event else {
        return (None, Some("no_published_result"));
    };
    let Some(receipt) = receipt else {
        return (None, Some("missing_receipt"));
    };
    let Some(start) = receipt.created_unix_ms else {
        return (None, Some("missing_receipt_time"));
    };
    let Some(end) = event.created_unix_ms else {
        return (None, Some("missing_result_time"));
    };
    match end.checked_sub(start) {
        Some(elapsed) => (Some(elapsed), None),
        None => (None, Some("inverted_time")),
    }
}

// The usage the provider itself reported for the request, as the event recorded it, and
// why there is none: `not_reported` (no usage fields on the event: legacy events and
// providers that report nothing), `malformed` (the bounded `usage_error` the writer recorded),
// `omitted_size` (dropped by the event size policy) or `no_published_result`.
pub(in crate::native) fn reported_usage(
    event: Option<&SessionEvent>,
) -> (
    Option<&crate::native::session::schema::ProviderUsage>,
    Option<&'static str>,
    Option<&str>,
) {
    let Some(event) = event else {
        return (None, Some("no_published_result"), None);
    };
    if let Some(usage) = &event.usage {
        return (Some(usage), None, None);
    }
    match event.usage_error.as_deref() {
        None => (None, Some("not_reported"), None),
        Some(USAGE_OMITTED_SIZE) => (None, Some("omitted_size"), Some(USAGE_OMITTED_SIZE)),
        Some(error) => (None, Some("malformed"), Some(error)),
    }
}

/// The human line for a result that carries the provider's own usage; none otherwise.
pub(super) fn usage_text(value: &Value) -> Option<String> {
    let usage = value["usage"].as_object()?;
    let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    let runs = count("runs");
    let provider = value["provider"].as_str().unwrap_or("provider");
    let mut provider_name = provider.chars();
    let provider_name = match provider_name.next() {
        Some(first) => first.to_uppercase().collect::<String>() + provider_name.as_str(),
        None => String::new(),
    };
    Some(format!(
        "usage: {provider_name} reported {} input, {} output, {} cache read, {} cache write, {} total tokens over {runs} run{} (coverage {})",
        count("input_tokens"),
        count("output_tokens"),
        count("cache_read_tokens"),
        count("cache_write_tokens"),
        count("total_tokens"),
        if runs == 1 { "" } else { "s" },
        usage
            .get("coverage")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    ))
}

pub(super) fn elapsed_text(value: &Value) -> String {
    match &value["bridge_observed_elapsed_ms"] {
        Value::Number(ms) => format!("Bridge observed elapsed: {ms} ms"),
        _ => format!(
            "Bridge observed elapsed: not computable ({})",
            value["bridge_observed_elapsed_reason"]
                .as_str()
                .unwrap_or("unknown")
        ),
    }
}

impl Snapshot {
    pub(in crate::native) fn observe_result<'a>(
        &'a self,
        reader: &Reader,
        selector: &'a Selector,
    ) -> Result<RequestObservation<'a>> {
        self.observe_result_with(selector, |name| self.event(reader, name))
    }

    pub(in crate::native) fn attachable_result(
        &self,
        reader: &Reader,
        selector: &Selector,
        session: &str,
    ) -> std::result::Result<(requests::ContextSource, String), AttachmentFailure> {
        self.observe_result(reader, selector)
            .map_err(|e| AttachmentFailure {
                state: "unreadable",
                detail: Some(format!("{e:#}")),
            })?
            .attachment(reader, session)
    }

    pub(super) fn observe_result_with<'a>(
        &'a self,
        selector: &'a Selector,
        read_event: impl FnOnce(&str) -> Result<Option<SessionEvent>>,
    ) -> Result<RequestObservation<'a>> {
        let receipt = match selector {
            Selector::Request(id) => Some(
                self.receipts
                    .iter()
                    .find(|receipt| &receipt.request_id == id)
                    .with_context(|| {
                        if self.unreadable_requests > 0 || self.request_index_error.is_some() {
                            format!("no readable receipt for request {id}; the request index is incomplete")
                        } else {
                            format!("no such Bridge request: {id}; legacy events have no request mapping")
                        }
                    })?,
            ),
            _ => None,
        };
        let name = match selector {
            Selector::Request(_) => receipt.map(|r| r.event_file.as_str()),
            Selector::Event(name) => Some(name.as_str()),
            Selector::Latest => self.latest_event_id(),
            Selector::List => bail!("list is not a single result selector"),
        };
        let receipt = receipt.or_else(|| name.and_then(|name| self.receipt_for_event(name)));
        let event = name.map(read_event).transpose()?.flatten();
        if matches!(selector, Selector::Event(_))
            && event.is_none()
            && name.is_some_and(|name| self.published(name))
        {
            bail!("no such recorded event: {}", name.unwrap_or_default())
        }
        let launch_failure = receipt
            .filter(|r| {
                self.launch
                    .as_ref()
                    .is_some_and(|l| l.claim_token == r.claim_token)
            })
            .and_then(|_| {
                launch::diagnostic(self.launch.as_ref(), &self.status, self.claim.as_deref())
            });
        let state = if let Some(event) = &event {
            if event.cancelled {
                RequestState::Cancelled
            } else if event.error.is_some() {
                RequestState::Failed
            } else {
                RequestState::Completed
            }
        } else if name
            .is_some_and(|name| self.pending.as_ref().is_some_and(|p| p.event_file == name))
        {
            RequestState::RecoveryRequired
        } else if launch_failure.is_some() {
            if self.status.state == SessionState::Failed {
                RequestState::Failed
            } else {
                RequestState::Unresolved
            }
        } else if receipt.is_some_and(|r| self.is_active(r)) {
            RequestState::Pending
        } else if receipt.is_some() {
            RequestState::Unresolved
        } else {
            RequestState::Unavailable
        };
        let error = event.as_ref().and_then(|e| e.error.clone()).or_else(|| {
            if event.is_none() {
                launch_failure.map(|(_, detail)| detail)
            } else {
                None
            }
        });
        Ok(RequestObservation {
            snapshot: self,
            receipt,
            event_id: name,
            event,
            state,
            error,
        })
    }
}

#[cfg(test)]
mod elapsed_tests {
    use super::{elapsed_text, reported_usage, usage_text};
    use crate::native::session::schema::{SessionEvent, USAGE_OMITTED_SIZE};
    use serde_json::json;

    #[test]
    fn human_usage_line_names_counts_runs_and_coverage() {
        let usage = json!({"provider": "pi", "usage": {"source": "pi_extension_agent_end",
            "scope": "request", "coverage": "complete", "runs": 1, "assistant_messages": 1,
            "input_tokens": 1234, "output_tokens": 567, "cache_read_tokens": 0,
            "cache_write_tokens": 0, "total_tokens": 1801}});
        assert_eq!(
            usage_text(&usage).unwrap(),
            "usage: Pi reported 1234 input, 567 output, 0 cache read, 0 cache write, 1801 total tokens over 1 run (coverage complete)"
        );
        let mut uncertain = usage.clone();
        uncertain["usage"]["coverage"] = json!("uncertain");
        uncertain["usage"]["runs"] = json!(2);
        assert!(
            usage_text(&uncertain)
                .unwrap()
                .ends_with("over 2 runs (coverage uncertain)")
        );
        assert_eq!(
            usage_text(&json!({"usage": null, "usage_reason": "not_reported"})),
            None
        );
    }

    #[test]
    fn reported_usage_reasons_follow_the_recorded_fields() {
        let mut event = SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "pi".to_owned(),
            message: "m".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        };
        assert_eq!(
            reported_usage(None),
            (None, Some("no_published_result"), None)
        );
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("not_reported"), None)
        );
        event.usage_error = Some(USAGE_OMITTED_SIZE.to_owned());
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("omitted_size"), Some(USAGE_OMITTED_SIZE))
        );
        event.usage_error = Some("malformed usage: runs".to_owned());
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("malformed"), Some("malformed usage: runs"))
        );
    }

    #[test]
    fn human_elapsed_distinguishes_zero_from_uncomputable() {
        assert_eq!(
            elapsed_text(&json!({"bridge_observed_elapsed_ms": 0})),
            "Bridge observed elapsed: 0 ms"
        );
        assert_eq!(
            elapsed_text(
                &json!({"bridge_observed_elapsed_ms": null, "bridge_observed_elapsed_reason": "missing_receipt"})
            ),
            "Bridge observed elapsed: not computable (missing_receipt)"
        );
    }
}

#[cfg(test)]
mod owner_tests {
    use super::RequestState;
    use crate::native::process_identity::{Attested, ProcessObservation};
    use crate::native::query::Selector;
    use crate::native::query::observation::judgment::OwnerJudgment;
    use crate::native::query::observation::judgment::tests::attestation;
    use crate::native::session::Reader;
    use crate::native::session::owner::ControlAttestation;
    use crate::native::session::owner::tests::reused_attestation;
    use crate::native::session::turn::JournaledEventState;
    use crate::native::tests::seed_close_fixture;
    use std::fs;

    #[test]
    fn owner_adjustment_preserves_wait_state_and_error_rules() {
        let fixture = seed_close_fixture(JournaledEventState::Absent);
        fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
        let reader = Reader::open_unchecked(&fixture.directory);
        let snapshot = reader.snapshot().unwrap();
        let selector = Selector::Request(snapshot.receipts[0].request_id.clone());
        let gone = OwnerJudgment::Bound(reused_attestation());
        let unknown = OwnerJudgment::Bound(attestation(
            ProcessObservation::Unknown("denied".into()),
            ControlAttestation::NotRecorded,
            Some(false),
        ));
        let changed = OwnerJudgment::Bound(attestation(
            ProcessObservation::Live(Attested::Incarnation),
            ControlAttestation::Changed("owner TTY changed".into()),
            Some(true),
        ));
        for (owner, ended, state, error) in [
            (
                &gone,
                false,
                RequestState::Unresolved,
                "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request",
            ),
            (
                &gone,
                true,
                RequestState::Unresolved,
                "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request",
            ),
            (&unknown, false, RequestState::Pending, "existing error"),
            (&changed, false, RequestState::Pending, "existing error"),
            (&unknown, true, RequestState::Unresolved, "existing error"),
        ] {
            let mut observation = snapshot.observe_result(&reader, &selector).unwrap();
            assert_eq!(observation.state(), RequestState::Pending);
            observation.error = Some("existing error".to_owned());
            let adjusted = observation.with_owner(owner, ended);
            assert_eq!(adjusted.state(), state);
            assert_eq!(adjusted.error.as_deref(), Some(error));
        }
    }
}
