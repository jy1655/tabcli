//! One Session's recorded facts, partial read failures, and timed judgments.
use super::request::{RequestObservation, RequestState, observed_elapsed};
use super::*;
use crate::native::reopen::ResumedFrom;
use crate::native::session;
use crate::native::session::origin;
use crate::native::session::schema::{EVENT_READ_LIMIT, SessionEvent, SessionManifest};
use judgment::{Cancel, CancelState, Hold, Judgments, OwnerJudgment, Surface, SurfacePresence};
use serde::Serialize;
use std::cell::OnceCell;
use std::fs::File;

pub(in crate::native) mod judgment;

pub(in crate::native) struct PartialObservation {
    pub(in crate::native) manifest: Option<SessionManifest>,
    pub(in crate::native) evidence: SessionEvidence,
    pub(in crate::native) cancel: Cancel,
    pub(in crate::native) surface: Surface,
    pub(in crate::native) hold: Hold,
}

impl PartialObservation {
    pub(in crate::native) fn read(reader: &Reader, id: &str) -> Self {
        Self::read_with_lock(reader, id, || reader.lock_shared())
    }

    fn read_with_lock(
        reader: &Reader,
        id: &str,
        mut lock_shared: impl FnMut() -> Result<Option<File>>,
    ) -> Self {
        let cancel =
            lock_shared().and_then(|lock| session::cancel::observe(reader, lock.is_some()));
        let cancel = judgment::cancel_record(&cancel);
        let evidence = match lock_shared() {
            Ok(lock) => {
                SessionEvidence::read(reader, session::hold::observe(reader, lock.is_some()))
            }
            Err(error) => SessionEvidence::read(reader, Err(error)),
        };
        let surface = judgment::surface(&evidence.surface, id);
        let hold = judgment::hold(&evidence.held, evidence.close_tombstone.as_ref(), None);
        Self {
            manifest: reader.manifest().ok(),
            evidence,
            cancel,
            surface,
            hold,
        }
    }
}

/// Hold, Owner and Surface records are separate observations from the four lifecycle records.
/// They remain readable when the lifecycle records cannot form a consistent Observation.
pub(in crate::native) struct SessionEvidence {
    /// The Owner record judged against the session whose directory was read.
    pub(in crate::native) owner: session::owner::OwnerRecord,
    pub(in crate::native) held: Result<bool>,
    /// The session close tombstone, read for a held session only: the hold release gate
    /// is its one reader. None when it was not read.
    pub(in crate::native) close_tombstone: Option<Result<Option<SessionStatus>>>,
    pub(in crate::native) surface: SurfaceRecord,
    pub(in crate::native) copy: Option<terminal::foreign::CopyEvidence>,
    /// Made on the first request and kept for this Observation (D21).
    pub(in crate::native) owner_judgment: OnceCell<OwnerJudgment>,
    /// Judged on the first request and kept for this Observation (D21).
    pub(in crate::native) surface_presence:
        OnceCell<(SurfacePresence, Option<terminal::foreign::CopyEvidence>)>,
}

pub(in crate::native) enum SurfaceRecord {
    Closed(Result<Value>),
    Closing(Result<terminal::TerminalSession>),
    Active(Result<Option<terminal::TerminalSession>>),
}

impl SurfaceRecord {
    #[cfg(test)]
    pub(in crate::native) fn read(reader: &Reader) -> Self {
        Self::from_records(reader, terminal::foreign::SurfaceRecords::read(reader))
    }

    pub(super) fn from_records(
        reader: &Reader,
        records: terminal::foreign::SurfaceRecords,
    ) -> Self {
        match records.marker {
            Ok(Some(marker)) => Self::Closed(Ok(marker.value)),
            Err(error) => Self::Closed(Err(error)),
            Ok(None) => match reader
                .record(CoreRecord::TerminalClosing)
                .optional_json_with(session::record::terminal)
            {
                Ok(Some(value)) => Self::Closing(Ok(value)),
                Err(error) => Self::Closing(Err(error)),
                Ok(None) => Self::Active(records.active.map(|slot| slot.map(|(handle, _)| handle))),
            },
        }
    }
}

/// The OS probe of a recorded surface. Tests replace it.
pub(in crate::native) type SurfaceProbe<'a> =
    &'a dyn Fn(&Path, &terminal::TerminalSession, Duration) -> Result<bool>;

impl SessionEvidence {
    pub(in crate::native) fn read(reader: &Reader, held: Result<bool>) -> Self {
        let owner = session::owner::observe(reader);
        let records = terminal::foreign::SurfaceRecords::read(reader);
        let copy = records.evidence(reader, &owner);
        let surface = SurfaceRecord::from_records(reader, records);
        // An interrupted close can have its tombstone before status.json catches up.
        let close_tombstone = matches!(held, Ok(true)).then(|| reader.closed_if_present());
        Self {
            copy,
            close_tombstone,
            held,
            owner,
            surface,
            owner_judgment: OnceCell::new(),
            surface_presence: OnceCell::new(),
        }
    }

    /// Whether the recorded active surface is present, judged on the first request with
    /// the caller's deadline and kept for this Observation. It reads the current active
    /// handle of the supplied Reader's directory and probes nothing else.
    pub(in crate::native) fn judge_surface_presence(
        &self,
        reader: &Reader,
        deadline: Instant,
    ) -> &SurfacePresence {
        self.judge_surface_presence_with(reader, deadline, &terminal::surface_present)
    }

    pub(in crate::native) fn judge_surface_presence_with(
        &self,
        reader: &Reader,
        deadline: Instant,
        probe: SurfaceProbe<'_>,
    ) -> &SurfacePresence {
        &self
            .surface_presence
            .get_or_init(|| {
                let directory = reader.directory();
                let records = terminal::foreign::SurfaceRecords::read(reader);
                let copy = records.evidence(reader, &session::owner::observe(reader));
                let presence = SurfacePresence::of((|| -> Result<bool> {
                    let (session, _) = records
                        .active?
                        .context("no active terminal handle is recorded")?;
                    session.verify_managed_session(
                        directory
                            .file_name()
                            .and_then(|s| s.to_str())
                            .context("invalid session path")?,
                    )?;
                    if let Some(copy) = copy {
                        return Err(copy.observation_refusal());
                    }
                    let budget = deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(2));
                    if budget.is_zero() {
                        bail!("diagnostic probe deadline exhausted")
                    }
                    probe(directory, &session, budget)
                })());
                (presence, copy)
            })
            .0
    }

    /// The owner judgment, made on the first request. One Observation describes one
    /// session, so later requests return the first judgment.
    pub(in crate::native) fn judge_owner(&self) -> &OwnerJudgment {
        self.judge_owner_with(&judgment::Observers::SYSTEM)
    }

    /// A copied session's records supply no local liveness verdict to Repair.
    pub(in crate::native) fn repair_owner_with(
        &self,
        observers: &judgment::Observers<'_>,
    ) -> Option<&OwnerJudgment> {
        self.copy
            .is_none()
            .then(|| self.judge_owner_with(observers))
    }

    pub(in crate::native) fn judge_owner_with(
        &self,
        observers: &judgment::Observers<'_>,
    ) -> &OwnerJudgment {
        self.owner_judgment.get_or_init(|| match self.copy {
            Some(copy) => judgment::owner(
                &self.owner,
                &judgment::Observers {
                    attest: &|_| session::owner::OwnerAttestation {
                        process: crate::native::process_identity::ProcessObservation::Unknown(
                            copy.observation_refusal().to_string(),
                        ),
                        control: session::owner::ControlAttestation::NotRecorded,
                        pid_alive: None,
                    },
                },
            ),
            None => judgment::owner(&self.owner, observers),
        })
    }
}

#[derive(Serialize)]
pub(in crate::native) struct CancelObservation {
    pub(in crate::native) request_id: Option<String>,
    pub(in crate::native) requested_unix_ms: Option<u128>,
    pub(in crate::native) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The typed judgment the state and error present.
    #[serde(skip)]
    pub(in crate::native) judgment: Cancel,
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
        let (state, error, judgment) = match outcome {
            Ok(result) if result.state == RequestState::Cancelled => (
                "applied",
                None,
                Cancel::Recorded(Some(CancelState::Applied)),
            ),
            Ok(result) if result.event.is_some() => (
                "not_applied",
                None,
                Cancel::Recorded(Some(CancelState::NotApplied)),
            ),
            Ok(_) => (
                "requested",
                None,
                Cancel::Recorded(Some(
                    if records.claim.as_deref() == Some(record.claim_token.as_str()) {
                        CancelState::RequestedActive
                    } else {
                        CancelState::RequestedStale
                    },
                )),
            ),
            Err(error) => {
                let reason = format!("{error:#}");
                (
                    "unreadable",
                    Some(reason.clone()),
                    Cancel::Unreadable(reason),
                )
            }
        };
        Self {
            request_id: Some(record.request_id.clone()),
            requested_unix_ms: Some(record.created_unix_ms),
            state,
            error,
            judgment,
        }
    }

    pub(in crate::native) fn read(reader: &Reader, records: &Snapshot) -> Option<Self> {
        match session::cancel::observe(reader, records._lock.is_some()) {
            Ok(None) => None,
            Ok(Some(record)) => Some(Self::from_record(&record, records, |name| {
                records.event(reader, name)
            })),
            Err(error) => {
                let reason = format!("{error:#}");
                Some(Self {
                    request_id: None,
                    requested_unix_ms: None,
                    state: "unreadable",
                    error: Some(reason.clone()),
                    judgment: Cancel::Unreadable(reason),
                })
            }
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
    pub(in crate::native) workspace_consent: consent::SessionConsentObservation,
    /// Decided once from the Records above; renderers map them (judgment.rs).
    pub(in crate::native) judgments: Judgments,
    /// The Origin as this Snapshot's launch record and initial receipt name it.
    pub(in crate::native) origin: origin::OriginReference,
    /// The launch host record attributed to this Snapshot's launch receipt (#376), read
    /// under the retained lock.
    pub(in crate::native) launch_host: launch::host::HostObservation,
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

/// The session id a Reader's directory names; the surface binding is judged against it.
fn session_id_of(reader: &Reader) -> &str {
    reader
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
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
        // The diagnostic reads the clock and is made under the retained lock (D20).
        let launch_failure = launch::diagnostic(
            records.launch.as_ref(),
            &records.status,
            records.claim.as_deref(),
        );
        let held = session::hold::observe(reader, records._lock.is_some());
        let cancel = CancelObservation::read(reader, &records);
        let origin = origin::reference(&records);
        let evidence = SessionEvidence::read(reader, held);
        let resumed_from = read_resumed_from(reader.directory());
        let judgments = Judgments::of(
            &records,
            &evidence,
            cancel.as_ref(),
            &resumed_from,
            launch_failure,
            active_state,
            session_id_of(reader),
        );
        let launch_host = launch::host::observe(reader, records.launch.as_ref());
        Self {
            origin,
            cancel,
            records,
            evidence,
            resumed_from,
            workspace_consent: consent::observe(reader.directory()),
            judgments,
            launch_host,
        }
    }

    /// Status decodes the latest published event and, only when different, the
    /// active request's event. Both use the ordinary Result observation.
    pub(super) fn read_for_status(
        reader: &Reader,
        deadline: Instant,
        include_closed: bool,
        clock: &mut dyn Clock,
    ) -> Result<Option<StatusObservation>> {
        let records = observe_snapshot_until(
            reader,
            PublicationRead::Within(EVENT_READ_LIMIT),
            deadline,
            clock,
        )?;
        if !include_closed && records.status.state == SessionState::Closed {
            return Ok(None);
        }
        if clock.now() >= deadline {
            bail!("status time budget exhausted");
        }
        let latest_event_id = records.latest_event_id();
        let latest = records.observe_result(reader, &Selector::Latest);
        let active_summary = |observed: &RequestObservation<'_>| {
            let (elapsed, reason) = observed_elapsed(observed.receipt, observed.event.as_ref());
            ActiveRequestStatus {
                state: observed.state,
                elapsed_ms: elapsed,
                elapsed_reason: reason,
            }
        };
        let active = records.active_receipt().map(|receipt| {
            if latest_event_id == Some(receipt.event_file.as_str()) {
                latest
                    .as_ref()
                    .map(active_summary)
                    .map_err(|error| anyhow::anyhow!("{error:#}"))
            } else {
                if clock.now() >= deadline {
                    bail!("status time budget exhausted");
                }
                records
                    .observe_result(reader, &Selector::Request(receipt.request_id.clone()))
                    .map(|observed| active_summary(&observed))
            }
        });
        let focus = if records.claim.is_some() {
            records
                .active_receipt()
                .map(|receipt| (QuestionFocus::ActiveRequest, receipt))
        } else {
            latest
                .as_ref()
                .ok()
                .and_then(|observed| observed.receipt)
                .map(|receipt| (QuestionFocus::LatestResult, receipt))
        };
        if clock.now() >= deadline {
            bail!("status time budget exhausted");
        }
        let questions = focus.and_then(|(focus, receipt)| {
            question_emissions(reader, &records, receipt)
                .map(|emissions| FocusedQuestions { focus, emissions })
        });
        let latest = latest.map(|observed| {
            observed.event.as_ref().map(|event| LatestResultStatus {
                event_id: observed.event_id.map(str::to_owned),
                request_id: observed.receipt.map(|receipt| receipt.request_id.clone()),
                state: observed.state,
                created_unix_ms: event.created_unix_ms,
            })
        });
        let active_state = active.as_ref().map(|result| {
            result
                .as_ref()
                .map(|active| active.state)
                .map_err(|error| anyhow::anyhow!("{error:#}"))
        });
        let mut observation = Self::from_records(reader, records, active_state);
        observation.records._lock.take();
        Ok(Some(StatusObservation {
            observation,
            active,
            latest,
            questions,
        }))
    }

    /// The Origin's own records, read within one Origin budget that ends no later than
    /// `outer`. Read-only; the Origin is looked up in this session's state root only.
    pub(in crate::native) fn observe_origin(
        &self,
        reader: &Reader,
        outer: Option<Instant>,
        environment: &mut origin::OriginEnvironment<'_>,
    ) -> origin::OriginObservation {
        let mut budget = origin::ReadBudget::for_query(outer, environment.clock);
        origin::observe(
            &state_root_of(reader),
            self.origin.clone(),
            &mut budget,
            environment,
        )
    }

    pub(in crate::native) fn active_request(&self) -> Option<&requests::Receipt> {
        self.records.active_receipt()
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
    pub(super) active: Option<Result<ActiveRequestStatus>>,
    pub(super) latest: Result<Option<LatestResultStatus>>,
    pub(super) questions: Option<FocusedQuestions>,
}

/// Status retains only the observed facts it renders, never a Result's body or a
/// borrow that extends the lifecycle lock into owner observation or rendering.
pub(super) struct ActiveRequestStatus {
    pub(super) state: RequestState,
    pub(super) elapsed_ms: Option<u128>,
    pub(super) elapsed_reason: Option<&'static str>,
}

pub(super) struct LatestResultStatus {
    pub(super) event_id: Option<String>,
    pub(super) request_id: Option<String>,
    pub(super) state: RequestState,
    pub(super) created_unix_ms: Option<u128>,
}

pub(super) enum QuestionFocus {
    ActiveRequest,
    LatestResult,
}

/// The target selected under the Snapshot's lock travels with its question evidence.
pub(super) struct FocusedQuestions {
    pub(super) focus: QuestionFocus,
    pub(super) emissions: QuestionEmissions,
}

#[cfg(test)]
mod tests;
