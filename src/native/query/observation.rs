//! One Session's recorded facts, partial read failures, and timed judgments.
use super::*;
use crate::native::reopen::ResumedFrom;
use crate::native::session;
use crate::native::session::origin;
use crate::native::session::schema::{EVENT_READ_LIMIT, SessionEvent};
use crate::native::terminal::ownership::NativeSessionOwner;
use judgment::OwnerJudgment;
use std::cell::OnceCell;

pub(in crate::native) mod judgment;

/// Hold, Owner and Surface records are separate observations from the four lifecycle records.
/// They remain readable when the lifecycle records cannot form a consistent Observation.
pub(in crate::native) struct SessionEvidence {
    pub(in crate::native) owner: Result<Option<NativeSessionOwner>>,
    pub(in crate::native) held: Result<bool>,
    pub(in crate::native) hold_release_allowed: bool,
    pub(in crate::native) surface: SurfaceRecord,
    /// Made on the first request and kept for this Observation (D21).
    pub(in crate::native) owner_judgment: OnceCell<OwnerJudgment>,
    pub(in crate::native) surface_presence: Option<Result<bool>>,
}

pub(in crate::native) enum SurfaceRecord {
    Closed(Result<Value>),
    Closing(Result<terminal::TerminalSession>),
    Active(Result<Option<terminal::TerminalSession>>),
}

impl SessionEvidence {
    /// The raw Owner record read, shared with `wait`'s per-poll judgment.
    pub(super) fn read_owner(reader: &Reader) -> Result<Option<NativeSessionOwner>> {
        RecordReader::at(reader.record(CoreRecord::Owner).path()).optional_json()
    }

    pub(in crate::native) fn read(reader: &Reader, held: Result<bool>) -> Self {
        let owner = Self::read_owner(reader);
        let surface = match RecordReader::at(reader.record(CoreRecord::TerminalClosed).path())
            .optional_json::<Value>()
        {
            Ok(Some(value)) => SurfaceRecord::Closed(Ok(value)),
            Err(error) => SurfaceRecord::Closed(Err(error)),
            Ok(None) => match RecordReader::at(reader.record(CoreRecord::TerminalClosing).path())
                .optional_json::<terminal::TerminalSession>()
            {
                Ok(Some(value)) => SurfaceRecord::Closing(Ok(value)),
                Err(error) => SurfaceRecord::Closing(Err(error)),
                Ok(None) => SurfaceRecord::Active(
                    RecordReader::at(reader.record(CoreRecord::Terminal).path()).optional_json(),
                ),
            },
        };
        // An interrupted close can have its tombstone before status.json catches up.
        let hold_release_allowed =
            matches!(held, Ok(true)) && matches!(reader.closed_if_present(), Ok(None));
        Self {
            hold_release_allowed,
            held,
            owner,
            surface,
            owner_judgment: OnceCell::new(),
            surface_presence: None,
        }
    }

    pub(in crate::native) fn observe_surface(
        &mut self,
        reader: &Reader,
        deadline: Instant,
    ) -> &Result<bool> {
        let directory = reader.directory();
        self.surface_presence.insert((|| -> Result<bool> {
            let session = RecordReader::at(
                Reader::open_unchecked(directory)
                    .record(CoreRecord::Terminal)
                    .path(),
            )
            .optional_json::<terminal::TerminalSession>()?
            .context("no active terminal handle is recorded")?;
            session.verify_managed_session(
                directory
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("invalid session path")?,
            )?;
            let budget = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(2));
            if budget.is_zero() {
                bail!("diagnostic probe deadline exhausted")
            }
            terminal::surface_present(directory, &session, budget)
        })())
    }

    /// The owner judgment for `session`, made on the first request. One Observation
    /// describes one session, so later requests return the first judgment.
    pub(in crate::native) fn judge_owner(&self, session: &str) -> &OwnerJudgment {
        self.judge_owner_with(session, &judgment::Observers::SYSTEM)
    }

    pub(in crate::native) fn judge_owner_with(
        &self,
        session: &str,
        observers: &judgment::Observers<'_>,
    ) -> &OwnerJudgment {
        self.owner_judgment
            .get_or_init(|| judgment::owner(&self.owner, session, observers))
    }
}

#[derive(Serialize)]
pub(in crate::native) struct CancelObservation {
    pub(in crate::native) request_id: Option<String>,
    pub(in crate::native) requested_unix_ms: Option<u128>,
    pub(in crate::native) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip)]
    pub(in crate::native) active: bool,
}

impl CancelObservation {
    pub(super) fn from_record(
        record: &session::cancel::Cancel,
        records: &Snapshot,
        read_event: impl FnOnce(&str) -> Result<Option<SessionEvent>>,
    ) -> Self {
        let selector = Selector::Request(record.request_id.clone());
        let outcome = (|| {
            let receipt = records
                .receipts
                .iter()
                .find(|r| r.request_id == record.request_id)
                .context("cancel record has no readable request receipt")?;
            if receipt.claim_token != record.claim_token {
                bail!("cancel record claim does not match its request receipt");
            }
            records.observe_result_with(&selector, read_event)
        })();
        let (state, error) = match outcome {
            Ok(result) if result.state == RequestState::Cancelled => ("applied", None),
            Ok(result) if result.event.is_some() => ("not_applied", None),
            Ok(_) => ("requested", None),
            Err(error) => ("unreadable", Some(format!("{error:#}"))),
        };
        Self {
            request_id: Some(record.request_id.clone()),
            requested_unix_ms: Some(record.created_unix_ms),
            state,
            error,
            active: state == "requested"
                && records.claim.as_deref() == Some(record.claim_token.as_str()),
        }
    }

    pub(in crate::native) fn read(reader: &Reader, records: &Snapshot) -> Option<Self> {
        match session::cancel::observe(reader, records._lock.is_some()) {
            Ok(None) => None,
            Ok(Some(record)) => Some(Self::from_record(&record, records, |name| {
                records.event(reader, name)
            })),
            Err(error) => Some(Self {
                request_id: None,
                requested_unix_ms: None,
                state: "unreadable",
                error: Some(format!("{error:#}")),
                active: false,
            }),
        }
    }
}

/// Required lifecycle failures fail the read. Auxiliary failures stay with their part.
/// Ordinary reads release the lifecycle lock before returning; inspect explicitly keeps
/// it until its requested elapsed reads finish. Historical request elapsed values are
/// read only by inspect's rendering.
pub(in crate::native) struct Observation {
    pub(in crate::native) records: Snapshot,
    pub(in crate::native) cancel: Option<CancelObservation>,
    pub(in crate::native) evidence: SessionEvidence,
    pub(in crate::native) resumed_from: Result<Option<ResumedFrom>>,
    pub(in crate::native) workspace_consent: Value,
    pub(in crate::native) judgments: Judgments,
    /// The Origin as this Snapshot's launch record and initial receipt name it.
    pub(in crate::native) origin: origin::OriginReference,
}

pub(in crate::native) struct Judgments {
    pub(in crate::native) active_state: Option<Result<RequestState>>,
    pub(in crate::native) launch_failure: Option<(&'static str, String)>,
}

/// A Result reference rendered by the same builder as `result`, without its body.
/// Construct it only when requested, while the caller retains its Snapshot lock.
pub(in crate::native) struct ResultReference(Value);

impl ResultReference {
    pub(in crate::native) fn latest(records: &Snapshot, reader: &Reader) -> Result<Self> {
        let mut value = records.observe_result(reader, &Selector::Latest)?.value();
        if let Value::Object(fields) = &mut value {
            fields.remove("result");
        }
        Ok(Self(value))
    }

    pub(in crate::native) fn value(self) -> Value {
        self.0
    }
}

impl Observation {
    pub(in crate::native) fn read(reader: &Reader) -> Result<Self> {
        let mut observation = Self::read_locked(reader)?;
        observation.records._lock.take();
        Ok(observation)
    }

    pub(super) fn read_locked(reader: &Reader) -> Result<Self> {
        let records = observe_snapshot(reader)?;
        let active_state = records.active_receipt().map(|r| {
            records
                .observe_result(reader, &Selector::Request(r.request_id.clone()))
                .map(|observed| observed.state)
        });
        Ok(Self::from_records(reader, records, active_state))
    }

    fn from_records(
        reader: &Reader,
        records: Snapshot,
        active_state: Option<Result<RequestState>>,
    ) -> Self {
        let launch_failure = launch::diagnostic(
            records.launch.as_ref(),
            &records.status,
            records.claim.as_deref(),
        );
        let held = session::hold::observe(reader, records._lock.is_some());
        let cancel = CancelObservation::read(reader, &records);
        let origin = origin::reference(&records);
        Self {
            origin,
            cancel,
            records,
            evidence: SessionEvidence::read(reader, held),
            resumed_from: read_resumed_from(reader.directory()),
            workspace_consent: consent::observe(reader.directory()),
            judgments: Judgments {
                active_state,
                launch_failure,
            },
        }
    }

    /// Status decodes the latest published event and, only when different, the
    /// active request's event. Both use the ordinary Result observation.
    pub(super) fn read_for_status(
        reader: &Reader,
        deadline: Instant,
        include_closed: bool,
    ) -> Result<Option<StatusObservation>> {
        let records =
            observe_snapshot_until(reader, PublicationRead::Within(EVENT_READ_LIMIT), deadline)?;
        if !include_closed && records.status.state == SessionState::Closed {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            bail!("status time budget exhausted");
        }
        let latest_event_id = records.latest_event_id();
        let latest = records.observe_result(reader, &Selector::Latest);
        let active_value = |observed: &RequestObservation<'_>| {
            let (elapsed, reason) = observed_elapsed(observed.receipt, observed.event.as_ref());
            (
                observed.state,
                json!({
                    "request_state": observed.state,
                    "bridge_observed_elapsed_ms": elapsed,
                    "bridge_observed_elapsed_reason": reason,
                }),
            )
        };
        let active = records.active_receipt().map(|receipt| {
            if latest_event_id == Some(receipt.event_file.as_str()) {
                latest
                    .as_ref()
                    .map(active_value)
                    .map_err(|error| anyhow::anyhow!("{error:#}"))
            } else {
                if Instant::now() >= deadline {
                    bail!("status time budget exhausted");
                }
                records
                    .observe_result(reader, &Selector::Request(receipt.request_id.clone()))
                    .map(|observed| active_value(&observed))
            }
        });
        let latest = latest.map(|observed| {
            observed.event.as_ref().map_or(Value::Null, |event| {
                json!({
                    "event_id": observed.event_id,
                    "request_id": observed.receipt.map(|receipt| &receipt.request_id),
                    "request_state": observed.state,
                    "created_unix_ms": event.created_unix_ms,
                })
            })
        });
        let active_state = active.as_ref().map(|result| {
            result
                .as_ref()
                .map(|(state, _)| *state)
                .map_err(|error| anyhow::anyhow!("{error:#}"))
        });
        let active = active.map(|result| result.map(|(_, value)| value));
        let mut observation = Self::from_records(reader, records, active_state);
        observation.records._lock.take();
        Ok(Some(StatusObservation {
            observation,
            active,
            latest,
        }))
    }

    /// The Origin's own records, read within one Origin budget that ends no later than
    /// `outer`. Read-only; the Origin is looked up in this session's state root only.
    pub(in crate::native) fn observe_origin(
        &self,
        reader: &Reader,
        outer: Option<Instant>,
    ) -> origin::OriginObservation {
        origin::observe(
            &state_root_of(reader),
            self.origin.clone(),
            &mut origin::ReadBudget::for_query(outer),
        )
    }

    pub(in crate::native) fn active_request(&self) -> Option<&requests::Receipt> {
        self.records.active_receipt()
    }

    pub(super) fn inspect_value(self, reader: &Reader, id: &str) -> Result<Value> {
        let origin = self.observe_origin(reader, None);
        let owner = self.evidence.judge_owner(id).observation();
        let snapshot = &self.records;
        let resumed_from = self.resumed_from?;
        let latest = ResultReference::latest(snapshot, reader)?.value();
        let request_refs = snapshot
        .receipts
        .iter()
        .map(|receipt| {
            let (elapsed, elapsed_reason) = match snapshot.event(reader, &receipt.event_file) {
                Ok(event) => observed_elapsed(Some(receipt), event.as_ref()),
                Err(_) => (None, Some("unreadable_result")),
            };
            json!({
                "request_id": receipt.request_id, "created_unix_ms": receipt.created_unix_ms, "source": receipt.source,
                "event_id": receipt.event_file, "context_sources": receipt.context_sources,
                "active": snapshot.is_active(receipt),
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
            })
        })
        .collect::<Vec<_>>();
        let residual_surface = snapshot.status.residual_surface();
        let mut value = envelope::ok(json!({
            "session": id, "provider": snapshot.manifest.provider,
            "workspace": snapshot.manifest.workspace, "title": snapshot.manifest.title,
            "stored_state": snapshot.status.state, "generation": snapshot.status.generation,
            "created_unix_ms": snapshot.manifest.created_unix_ms, "updated_unix_ms": snapshot.status.updated_unix_ms,
            "error": snapshot.status.error, "exit_code": snapshot.status.exit_code,
            "configured": {"model": snapshot.manifest.model, "effort": snapshot.manifest.effort,
                "yolo": snapshot.manifest.yolo, "provider_version_at_launch": snapshot.manifest.provider_version},
            "resumed_from": resumed_from,
            "workspace_consent": self.workspace_consent,
            "cancel": self.cancel,
            "owner_process_alive": owner.process_alive, "owner_identity_verified": owner.identity_matches == Some(true), "owner": owner,
            "recovery_required": snapshot.pending.is_some(), "turn_claimed": snapshot.claim.is_some(),
            "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
            "recorded_events": snapshot.paths.len(), "latest_result": latest, "requests": request_refs,
        }).as_object().unwrap().clone());
        session::hold::add_fields(&mut value, &self.evidence.held);
        if let Some(residual) = residual_surface {
            value["residual_surface"] = serde_json::to_value(residual)?;
        }
        origin.add_fields(&mut value);
        Ok(value)
    }
}

/// The state root that holds a session directory.
pub(in crate::native) fn state_root_of(reader: &Reader) -> PathBuf {
    reader
        .directory()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

pub(super) struct StatusObservation {
    pub(super) observation: Observation,
    pub(super) active: Option<Result<Value>>,
    pub(super) latest: Result<Value>,
}

#[cfg(test)]
mod tests;
