//! Query waits: one consistent read per session per pass, without lifecycle changes.
use super::request::RequestState;
use super::*;
use crate::native::args::{option_value, parse_timeout, set_flag_once, set_once};
use crate::native::clock::{Clock, SystemClock};
use crate::native::failure_kind::FailureKind;
use crate::native::session::primitives::valid_session_id;
use observation::{SessionEvidence, judgment};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Address {
    session: String,
    request: String,
}

impl Address {
    fn parse(value: &str) -> Result<Self> {
        let invalid = || {
            anyhow::anyhow!(
                "invalid wait address {value:?}; expected SESSION/REQUEST with a Bridge request id"
            )
        };
        let (session, request) = value.split_once('/').ok_or_else(invalid)?;
        if !valid_session_id(session) || !requests::valid_id(request) {
            return Err(invalid());
        }
        Ok(Self {
            session: session.to_owned(),
            request: request.to_owned(),
        })
    }

    fn text(&self) -> String {
        format!("{}/{}", self.session, self.request)
    }
}

#[derive(Debug)]
pub(crate) struct WaitRequest {
    addresses: Vec<Address>,
    timeout: Duration,
    json: bool,
    questions: bool,
    after_questions: Vec<QuestionCursor>,
}

/// A caller-held observation position, never an answer or a provider acknowledgement.
#[derive(Debug)]
struct QuestionCursor {
    address: Address,
    id: String,
}

impl QuestionCursor {
    fn parse(value: &str) -> Result<Self> {
        let mut parts = value.splitn(3, '/');
        let session = parts.next().unwrap_or_default();
        let request = parts.next().unwrap_or_default();
        let id = parts.next().filter(|id| !id.is_empty()).with_context(|| {
            format!("invalid question cursor {value:?}; expected SESSION/REQUEST/EMISSION_ID")
        })?;
        Ok(Self {
            address: Address::parse(&format!("{session}/{request}"))?,
            id: id.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WakeReason {
    RequestEnded,
    QuestionEmitted,
}

fn error_value(error: &anyhow::Error) -> Value {
    envelope::error(
        error,
        json!({"ended": null,
        "remaining": [], "timed_out": false, })
        .as_object()
        .unwrap()
        .clone(),
    )
}

pub(in crate::native) fn parse(args: &[String]) -> Result<NativeCommand> {
    match parse_options(args) {
        Ok(request) => Ok(NativeCommand::Wait(request)),
        Err(error) => {
            envelope::parse_failure(
                args,
                &error,
                json!({"ended": null, "remaining": [], "timed_out": false})
                    .as_object()
                    .unwrap()
                    .clone(),
            )?;
            Err(error)
        }
    }
}

fn parse_options(args: &[String]) -> Result<WaitRequest> {
    let mut addresses = Vec::new();
    let mut timeout = None;
    let mut json = false;
    let mut questions = false;
    let mut after_questions: Vec<QuestionCursor> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => set_flag_once(&mut json, "--json")?,
            "--questions" => set_flag_once(&mut questions, "--questions")?,
            "--after-question" => {
                let cursor =
                    QuestionCursor::parse(option_value(args, &mut index, "--after-question")?)?;
                if after_questions
                    .iter()
                    .any(|prior| prior.address == cursor.address)
                {
                    bail!("duplicate --after-question for {}", cursor.address.text());
                }
                after_questions.push(cursor);
            }
            "--timeout-secs" => {
                let value = option_value(args, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            other if other.starts_with('-') => bail!("unknown wait option: {other}"),
            other => {
                let address = Address::parse(other)?;
                if addresses.contains(&address) {
                    bail!("duplicate wait address: {other}")
                }
                addresses.push(address);
            }
        }
        index += 1;
    }
    if addresses.is_empty() {
        bail!("wait requires at least one SESSION/REQUEST address")
    }
    if !questions && !after_questions.is_empty() {
        bail!("--after-question requires --questions");
    }
    for cursor in &after_questions {
        if !addresses.contains(&cursor.address) {
            bail!(
                "--after-question address {} is not being waited on",
                cursor.address.text()
            );
        }
    }
    Ok(WaitRequest {
        addresses,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        json,
        questions,
        after_questions,
    })
}

impl RequestState {
    fn ends_wait(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Cancelled
                | Self::Failed
                | Self::Unresolved
                | Self::RecoveryRequired
        )
    }
}

/// Completed observations precede owner checks and therefore have no owner fields.
fn observe(
    snapshot: &Snapshot,
    reader: &Reader,
    selector: &Selector,
    session: &str,
    questions: bool,
    after: Option<&str>,
) -> Result<(Value, Option<WakeReason>)> {
    observe_with_owner(
        snapshot,
        reader,
        selector,
        session,
        &judgment::Observers::SYSTEM,
        questions,
        after,
    )
}

/// Each poll judges the Owner record afresh. Only a verified-gone owner ends the wait:
/// a changed control identity, an Unknown attestation and a foreign record keep it (B2).
fn observe_with_owner(
    snapshot: &Snapshot,
    reader: &Reader,
    selector: &Selector,
    session: &str,
    observers: &judgment::Observers<'_>,
    questions: bool,
    after: Option<&str>,
) -> Result<(Value, Option<WakeReason>)> {
    let mut observation = snapshot.observe_result(reader, selector)?;
    let mut owner = None;
    if !observation.state().ends_wait() {
        let judgment = judgment::owner(&SessionEvidence::read_owner(reader), session, observers);
        owner = Some(judgment.observation());
        observation = observation.with_owner(&judgment, snapshot.status.state.ended());
    }
    let mut value = observation.value();
    if let Some(owner) = owner {
        value["owner_process_alive"] = json!(owner.process_alive);
        value["owner"] = json!(owner);
    }
    let mut reason = observation
        .state()
        .ends_wait()
        .then_some(WakeReason::RequestEnded);
    // A pending poll without --questions reads no question record; an ending reads it once.
    let emissions = (questions || reason.is_some())
        .then(|| observation.questions(reader))
        .flatten();
    if let Some(emissions) = &emissions {
        question_presence(&mut value, emissions);
    }
    if questions {
        // A supplied cursor must name a retained item, even when completion is visible.
        // Guessing past lost evidence could silently skip a question on the next poll.
        let start = match after {
            Some(id) => emissions.as_ref()
                .and_then(|emissions| emissions.items.iter().position(|item| item.id == id))
                .map(|index| index + 1)
                .with_context(|| format!("--after-question emission {id:?} is not in the selected request's readable retained evidence"))?,
            None => 0,
        };
        if let Some(emissions) = emissions {
            if let Some(last) = emissions.items.last() {
                let receipt = observation
                    .receipt
                    .context("question observation has no Receipt")?;
                value["after_question"] =
                    json!(format!("{session}/{}/{}", receipt.request_id, last.id));
            }
            if start < emissions.items.len() {
                value["question_emissions"] = json!(&emissions.items[start..]);
                if reason.is_none() && observation.state() == RequestState::Pending {
                    reason = Some(WakeReason::QuestionEmitted);
                }
            }
        }
    }
    Ok((value, reason))
}

struct Outcome {
    selected: Option<(usize, WakeReason)>,
    last: Vec<Value>,
}

struct SessionGroup {
    directory: PathBuf,
    indices: Vec<usize>,
}

/// Validates directories before polling. Receipts are checked only under a consistent
/// Snapshot, on every readable pass. Busy means no observation, never a pending result.
fn poll(
    root: &Path,
    addresses: &[Address],
    timeout: Duration,
    questions: bool,
    after_questions: &[QuestionCursor],
    clock: &mut impl Clock,
) -> Result<Outcome> {
    let mut groups: Vec<SessionGroup> = Vec::new();
    for (index, address) in addresses.iter().enumerate() {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| addresses[group.indices[0]].session == address.session)
        {
            group.indices.push(index);
        } else {
            groups.push(SessionGroup {
                directory: Reader::session_directory_in(root, &address.session)?,
                indices: vec![index],
            });
        }
    }
    let deadline = checked_deadline_from(clock.now(), timeout)?;
    // One selector per address, built before polling: the loop only borrows them.
    let selectors: Vec<Selector> = addresses
        .iter()
        .map(|address| Selector::Request(address.request.clone()))
        .collect();
    let mut outcome = Outcome {
        selected: None,
        last: addresses.iter().map(|address| envelope::ok(json!({
            "session": address.session,
            "request_id": address.request, "request_state": "busy", "result": null,
            "bridge_observed_elapsed_ms": null, "bridge_observed_elapsed_reason": "no_published_result",
            "usage": null, "usage_reason": "no_published_result", "usage_error": null
        }).as_object().unwrap().clone())).collect(),
    };
    loop {
        for group in &groups {
            let reader = Reader::open_unchecked(&group.directory);
            let snapshot = match reader.snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) if error.is::<SnapshotBusy>() => continue,
                Err(error) => return Err(error),
            };
            for &index in &group.indices {
                let after = after_questions
                    .iter()
                    .find(|cursor| cursor.address == addresses[index])
                    .map(|cursor| cursor.id.as_str());
                let (value, reason) = observe(
                    &snapshot,
                    &reader,
                    &selectors[index],
                    &addresses[index].session,
                    questions,
                    after,
                )?;
                outcome.last[index] = value;
                if let Some(reason) = reason
                    && outcome
                        .selected
                        .is_none_or(|(previous, _)| index < previous)
                {
                    outcome.selected = Some((index, reason));
                }
            }
            // Drop this session's shared lock before reading the next session.
        }
        // Validate every readable address in the pass before selecting a success.
        if outcome.selected.is_some() {
            return Ok(outcome);
        }
        let remaining = deadline.saturating_duration_since(clock.now());
        if remaining.is_zero() {
            return Ok(outcome);
        }
        clock.sleep(remaining.min(Duration::from_millis(100)));
    }
}

/// The legacy result caller retains its timeout envelope and last good observation.
pub(in crate::native) fn one(
    root: &Path,
    session: &str,
    request: &str,
    timeout: Duration,
) -> Result<Value> {
    one_with(root, session, request, timeout, &mut SystemClock)
}

fn one_with(
    root: &Path,
    session: &str,
    request: &str,
    timeout: Duration,
    clock: &mut impl Clock,
) -> Result<Value> {
    let outcome = poll(
        root,
        &[Address {
            session: session.to_owned(),
            request: request.to_owned(),
        }],
        timeout,
        false,
        &[],
        clock,
    )?;
    let mut last = outcome.last.into_iter().next().unwrap();
    if outcome.selected.is_none() {
        last["ok"] = json!(false);
        last["timed_out"] = json!(true);
        last["error"] = json!("waiting timed out; the request was not cancelled or resent");
        // This wait observed its own deadline; the request and its records are unchanged.
        last["failure_kind"] = json!(FailureKind::TimedOut.as_str());
    }
    Ok(last)
}

fn value_in(root: &Path, request: &WaitRequest, clock: &mut impl Clock) -> Result<Value> {
    let outcome = poll(
        root,
        &request.addresses,
        request.timeout,
        request.questions,
        &request.after_questions,
        clock,
    )?;
    let remaining: Vec<_> = request
        .addresses
        .iter()
        .enumerate()
        .filter(|(index, _)| Some((*index, WakeReason::RequestEnded)) != outcome.selected)
        .map(|(_, address)| address.text())
        .collect();
    let Some((index, reason)) = outcome.selected else {
        return Ok(envelope::error(
            &anyhow::anyhow!("waiting timed out; no request was cancelled or resent"),
            json!({"ended": null,
            "remaining": remaining, "timed_out": true,
            })
            .as_object()
            .unwrap()
            .clone(),
        ));
    };
    let mut ended = outcome.last.into_iter().nth(index).unwrap();
    ended["address"] = json!(request.addresses[index].text());
    if reason == WakeReason::QuestionEmitted {
        return Ok(envelope::ok(
            json!({
                "ended": null, "question": ended, "remaining": remaining, "timed_out": false,
            })
            .as_object()
            .unwrap()
            .clone(),
        ));
    }
    if ended["request_state"] != "completed" {
        correct_unsuccessful_result(&mut ended);
    }
    Ok(envelope::ok(
        json!({"ok": ended["ok"], "ended": ended,
        "remaining": remaining, "timed_out": false})
        .as_object()
        .unwrap()
        .clone(),
    ))
}

#[cfg(test)]
pub(in crate::native) fn test_value_in(
    root: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Value> {
    let mut request = parse_options(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())?;
    request.timeout = timeout;
    value_in(root, &request, &mut SystemClock)
}

pub(in crate::native) fn run(request: WaitRequest) -> Result<()> {
    let value =
        match Reader::state_root().and_then(|root| value_in(&root, &request, &mut SystemClock)) {
            Ok(value) => value,
            Err(error) => {
                if request.json {
                    envelope::print(&error_value(&error))?;
                }
                return Err(error);
            }
        };
    envelope::emit(request.json, &value, || {
        if !value["ended"].is_null() {
            print_result(&value["ended"], false)?;
        } else if !value["question"].is_null() {
            println!("wait returned: question emitted; request remains pending");
            print_result(&value["question"], false)?;
        } else if let Some(error) = value["error"].as_str() {
            println!("error: {error}");
        }
        for selected in [&value["ended"], &value["question"]] {
            if let Some(cursor) = selected["after_question"].as_str() {
                println!("after-question: {}", terminal_safe_text(cursor, false));
            }
        }
        println!(
            "remaining: {}",
            value["remaining"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(())
    })?;
    if value["ok"] != true {
        bail!(
            "{}",
            value["error"]
                .as_str()
                .or_else(|| value["ended"]["error"].as_str())
                .unwrap_or("request did not complete successfully")
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::clock::FakeClock;
    use crate::native::session::primitives::write_json_atomic;
    use crate::native::tests::seed_close_fixture;

    use crate::native::process_identity::{Attested, ProcessObservation};
    use crate::native::session::owner::tests::reused_attestation;
    use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
    use observation::judgment::tests::attestation;
    use std::{cell::Cell, rc::Rc};

    const UNOBSERVED: judgment::Observers<'static> = judgment::Observers {
        attest: &|_| panic!("this record is not attested"),
        pid_alive: &|_| panic!("this record is not PID-observed"),
    };

    #[test]
    fn question_options_require_unique_cursors_for_selected_addresses() {
        for args in [
            vec![
                "session-a/request-1",
                "--after-question",
                "session-a/request-1/id",
            ],
            vec!["session-a/request-1", "--questions", "--questions"],
            vec!["session-a/request-1", "--questions", "--after-question"],
            vec![
                "session-a/request-1",
                "--questions",
                "--after-question",
                "session-a/request-1/",
            ],
            vec![
                "session-a/request-1",
                "--questions",
                "--after-question",
                "session-b/request-1/id",
            ],
            vec![
                "session-a/request-1",
                "--questions",
                "--after-question",
                "session-a/request-1/id",
                "--after-question",
                "session-a/request-1/other",
            ],
        ] {
            assert!(
                parse_options(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
                    .is_err(),
                "{args:?}"
            );
        }
        let parsed = parse_options(
            &[
                "--after-question",
                "session-a/request-1/id/with/slashes",
                "--questions",
                "session-a/request-1",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert!(parsed.questions);
        assert_eq!(parsed.after_questions[0].id, "id/with/slashes");
    }

    /// Observes a pending request of `session-fault` with the given Owner record.
    fn pending_with_owner(owner: Value, observers: &judgment::Observers<'_>) -> (Value, bool) {
        let fixture = seed_close_fixture(JournaledEventState::Absent);
        let directory = &fixture.directory;
        let reader = Reader::open_unchecked(directory);
        fs::remove_file(directory.join("turn.completion.json")).unwrap();
        write_json_atomic(&directory.join("native-session.json"), &owner).unwrap();
        let snapshot = reader.snapshot().unwrap();
        let id = snapshot.receipts[0].request_id.clone();
        let before = fs::read(directory.join("status.json")).unwrap();
        let (value, ended) = observe_with_owner(
            &snapshot,
            &reader,
            &Selector::Request(id),
            "session-fault",
            observers,
            false,
            None,
        )
        .unwrap();
        assert_eq!(fs::read(directory.join("status.json")).unwrap(), before);
        assert!(directory.join("turn.claim").exists());
        (value, ended == Some(WakeReason::RequestEnded))
    }

    #[test]
    fn r1_c6_mismatch_ends_pending_wait_without_writes() {
        let (value, ended) = pending_with_owner(
            json!({"pid": 4242, "managed_session_id": "session-fault"}),
            &judgment::Observers {
                attest: &|_| reused_attestation(),
                pid_alive: &|_| panic!("a bound record is attested"),
            },
        );
        assert!(ended);
        assert_eq!(value["request_state"], "unresolved");
        assert_eq!(value["owner"]["identity_matches"], false);
    }

    #[test]
    fn only_a_verified_gone_owner_ends_a_pending_wait() {
        let bound = json!({"pid": 4242, "managed_session_id": "session-fault"});
        fn live(control: ControlAttestation) -> OwnerAttestation {
            attestation(
                ProcessObservation::Live(Attested::Incarnation),
                control,
                Some(true),
            )
        }
        fn unknown(pid_alive: bool) -> OwnerAttestation {
            attestation(
                ProcessObservation::Unknown("denied".into()),
                ControlAttestation::NotRecorded,
                Some(pid_alive),
            )
        }
        type Attest = fn() -> OwnerAttestation;
        let bound_rows: [(&str, Attest, Value); 3] = [
            // A changed control identity of a live incarnation is not a death.
            (
                "live changed",
                || live(ControlAttestation::Changed("owner TTY changed".into())),
                json!({"process_alive": true, "identity_matches": false, "error": null}),
            ),
            (
                "unknown, live hint",
                || unknown(true),
                json!({"process_alive": true, "identity_matches": null, "error": "denied"}),
            ),
            // An Unknown attestation never reads as a death, whatever its PID hint.
            (
                "unknown, dead hint",
                || unknown(false),
                json!({"process_alive": false, "identity_matches": null, "error": "denied"}),
            ),
        ];
        for (name, make, owner) in bound_rows {
            let (value, ended) = pending_with_owner(
                bound.clone(),
                &judgment::Observers {
                    attest: &|_| make(),
                    pid_alive: &|_| panic!("a bound record is attested"),
                },
            );
            assert!(!ended, "{name}");
            assert_eq!(value["request_state"], "pending", "{name}");
            assert_eq!(value["owner"], owner, "{name}");
            assert_eq!(
                value["owner_process_alive"], owner["process_alive"],
                "{name}"
            );
        }
        // A record naming another session is not observed and does not end the wait.
        let (value, ended) = pending_with_owner(
            json!({"pid": 4242, "managed_session_id": "session-other"}),
            &UNOBSERVED,
        );
        assert!(!ended);
        assert_eq!(value["request_state"], "pending");
        assert_eq!(
            value["owner"],
            json!({"process_alive": null, "identity_matches": null,
                "error": "owner record belongs to another session"})
        );
        // An unbound legacy PID observed gone ends it; observed alive keeps it.
        for (alive, ends) in [(Some(false), true), (Some(true), false), (None, false)] {
            let (value, ended) = pending_with_owner(
                json!({"pid": 4242}),
                &judgment::Observers {
                    attest: &|_| panic!("an unbound record is not attested"),
                    pid_alive: &|pid| {
                        assert_eq!(pid, 4242);
                        alive
                    },
                },
            );
            assert_eq!(ended, ends, "{alive:?}");
            assert_eq!(
                value["request_state"],
                if ends { "unresolved" } else { "pending" }
            );
            assert_eq!(value["owner_process_alive"], json!(alive));
            if ends {
                assert_eq!(
                    value["error"],
                    "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request"
                );
            }
        }
    }

    #[test]
    fn published_result_precedes_even_a_reused_owner_observation() {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        crate::native::session::turn::recover_pending_completion(&fixture.directory).unwrap();
        write_json_atomic(
            &fixture.directory.join("native-session.json"),
            &json!({"pid": 4242, "managed_session_id": "session-fault"}),
        )
        .unwrap();
        let snapshot = reader.snapshot().unwrap();
        let (value, ended) = observe_with_owner(
            &snapshot,
            &reader,
            &Selector::Request(snapshot.receipts[0].request_id.clone()),
            "session-fault",
            &judgment::Observers {
                attest: &|_| panic!("published Result precedes owner inspection"),
                pid_alive: &|_| panic!("published Result precedes owner inspection"),
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(ended, Some(WakeReason::RequestEnded));
        assert_eq!(value["request_state"], "completed");
        assert!(value.get("owner").is_none());
    }

    #[test]
    fn only_recorded_wait_end_states_end_a_wait() {
        for state in [
            RequestState::Completed,
            RequestState::Failed,
            RequestState::Unresolved,
            RequestState::RecoveryRequired,
        ] {
            assert!(state.ends_wait());
        }
        for state in [RequestState::Pending, RequestState::Unavailable] {
            assert!(!state.ends_wait());
        }
    }

    #[test]
    fn busy_after_a_good_observation_keeps_it_at_timeout() {
        let fixture = seed_close_fixture(JournaledEventState::Absent);
        let reader = Reader::open_unchecked(&fixture.directory);
        let id = reader.snapshot().unwrap().receipts[0].request_id.clone();
        fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        let value = with_snapshot_hook(
            move |directory| {
                observed.set(observed.get() + 1);
                if observed.get() == 1 {
                    // Charge the first read, then let the next pass land at the deadline.
                    now.set(now.get() + Duration::from_millis(75));
                } else {
                    let path = directory.join("status.json");
                    let mut status: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    status["generation"] = json!(100 + observed.get());
                    write_json_atomic(&path, &status).unwrap();
                }
            },
            || {
                one_with(
                    fixture.directory.parent().unwrap(),
                    "session-fault",
                    &id,
                    Duration::from_millis(150),
                    &mut clock,
                )
                .unwrap()
            },
        );
        assert_eq!(calls.get(), 2, "the busy pass must actually run");
        assert_eq!(clock.slept, Duration::from_millis(75));
        assert_eq!(clock.now(), start + Duration::from_millis(150));
        assert_eq!(value["request_state"], "pending");
        assert_eq!(value["timed_out"], true);
        assert!(value.get("owner").is_some());
        assert_eq!(value["session_state"], "working");
    }

    #[test]
    fn result_wait_timeout_adds_only_its_own_hint_and_writes_nothing() {
        use crate::native::reopen::tests::snapshot_directory;
        let fixture = seed_close_fixture(JournaledEventState::Absent);
        let reader = Reader::open_unchecked(&fixture.directory);
        let id = reader.snapshot().unwrap().receipts[0].request_id.clone();
        fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
        let root = fixture.directory.parent().unwrap();
        let before = snapshot_directory(root);
        let mut clock = FakeClock::new(Instant::now());
        let value = one_with(
            root,
            "session-fault",
            &id,
            Duration::from_millis(50),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, Duration::from_millis(50));
        assert_eq!(snapshot_directory(root), before);
        assert_eq!(value["ok"], false);
        assert_eq!(value["timed_out"], true);
        assert_eq!(
            value["error"],
            "waiting timed out; the request was not cancelled or resent"
        );
        assert_eq!(value["failure_kind"], "timed_out");
        assert_eq!(value["request_state"], "pending");
        assert_eq!(value["request_id"], json!(id));
    }

    #[test]
    fn later_receipt_damage_or_deletion_is_an_error() {
        for damage in [false, true] {
            let fixture = seed_close_fixture(JournaledEventState::Absent);
            let reader = Reader::open_unchecked(&fixture.directory);
            let snapshot = reader.snapshot().unwrap();
            let receipt = &snapshot.receipts[0];
            let id = receipt.request_id.clone();
            let path = fixture
                .directory
                .join("requests")
                .join(format!("{}.json", receipt.claim_token));
            drop(snapshot);
            fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
            let mut clock = FakeClock::new(Instant::now());
            let calls = Rc::new(Cell::new(0));
            let observed = calls.clone();
            let result = with_snapshot_hook(
                move |_| {
                    observed.set(observed.get() + 1);
                    if observed.get() == 2 {
                        if damage {
                            write_json_atomic(&path, &json!({})).unwrap();
                        } else {
                            fs::remove_file(&path).unwrap();
                        }
                    }
                },
                || {
                    one_with(
                        fixture.directory.parent().unwrap(),
                        "session-fault",
                        &id,
                        Duration::from_secs(1),
                        &mut clock,
                    )
                },
            );
            assert!(
                calls.get() >= 2,
                "the Receipt must change after the first pass"
            );
            assert!(!clock.slept.is_zero());
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains(if damage {
                "no readable receipt"
            } else {
                "no such Bridge request"
            }));
        }
    }

    #[test]
    fn completed_pass_precedes_timeout_in_both_wait_outputs() {
        use crate::native::reopen::tests::snapshot_directory;
        for timeout in [Duration::ZERO, Duration::from_millis(50)] {
            for multi in [false, true] {
                let fixture = seed_close_fixture(JournaledEventState::Committed);
                crate::native::session::turn::recover_pending_completion(&fixture.directory)
                    .unwrap();
                let request_id = Reader::open_unchecked(&fixture.directory)
                    .snapshot()
                    .unwrap()
                    .receipts[0]
                    .request_id
                    .clone();
                let root = fixture.directory.parent().unwrap();
                let before = snapshot_directory(root);
                let start = Instant::now();
                let mut clock = FakeClock::new(start);
                let now = clock.shared();
                let calls = Rc::new(Cell::new(0));
                let observed = calls.clone();
                let value = with_snapshot_hook(
                    move |_| {
                        observed.set(observed.get() + 1);
                        // The Result is decoded after this read crosses the deadline.
                        now.set(start + Duration::from_secs(1));
                    },
                    || {
                        if multi {
                            let request = WaitRequest {
                                addresses: vec![Address {
                                    session: "session-fault".into(),
                                    request: request_id.clone(),
                                }],
                                timeout,
                                json: true,
                                questions: false,
                                after_questions: vec![],
                            };
                            value_in(root, &request, &mut clock).unwrap()
                        } else {
                            one_with(root, "session-fault", &request_id, timeout, &mut clock)
                                .unwrap()
                        }
                    },
                );
                assert_eq!(calls.get(), 1, "even zero timeout observes one full pass");
                assert!(clock.now() > start + timeout);
                assert_eq!(clock.slept, Duration::ZERO);
                assert_eq!(value["ok"], true);
                assert_ne!(value["timed_out"], true);
                let ended = if multi { &value["ended"] } else { &value };
                assert_eq!(ended["request_state"], "completed");
                assert_eq!(ended["request_id"], request_id);
                assert_eq!(ended["result"], "late result");
                assert_eq!(snapshot_directory(root), before, "wait must not write");
            }
        }
    }

    #[test]
    fn owner_unknown_and_read_error_do_not_end_a_pending_wait() {
        for owner in [
            None,
            Some(json!({"pid": std::process::id()})),
            Some(json!({})),
        ] {
            let fixture = seed_close_fixture(JournaledEventState::Absent);
            let reader = Reader::open_unchecked(&fixture.directory);
            let id = reader.snapshot().unwrap().receipts[0].request_id.clone();
            fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
            if let Some(owner) = owner {
                write_json_atomic(&fixture.directory.join("native-session.json"), &owner).unwrap();
            }
            let value = one(
                fixture.directory.parent().unwrap(),
                "session-fault",
                &id,
                Duration::ZERO,
            )
            .unwrap();
            assert_eq!(value["request_state"], "pending");
            assert_eq!(value["timed_out"], true);
        }
    }

    #[test]
    fn same_session_addresses_share_one_snapshot() {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        let snapshot = reader.snapshot().unwrap();
        let mut receipt = serde_json::to_value(&snapshot.receipts[0]).unwrap();
        let first = snapshot.receipts[0].request_id.clone();
        drop(snapshot);
        receipt["request_id"] = json!("request-other");
        receipt["claim_token"] = json!("123-456-0");
        receipt["event_file"] = json!("event-other.json");
        write_json_atomic(&fixture.directory.join("requests/123-456-0.json"), &receipt).unwrap();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        let outcome = with_snapshot_hook(
            move |_| count.set(count.get() + 1),
            || {
                poll(
                    fixture.directory.parent().unwrap(),
                    &[
                        Address {
                            session: "session-fault".into(),
                            request: first,
                        },
                        Address {
                            session: "session-fault".into(),
                            request: "request-other".into(),
                        },
                    ],
                    Duration::ZERO,
                    false,
                    &[],
                    &mut FakeClock::new(Instant::now()),
                )
                .unwrap()
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(outcome.selected, Some((0, WakeReason::RequestEnded)));
    }
}
