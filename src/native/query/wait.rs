//! Query waits: one consistent read per session per pass, without lifecycle changes.
use super::*;

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
}

fn error_value(error: &anyhow::Error) -> Value {
    json!({"schema_version": 1, "ok": false, "ended": null,
        "remaining": [], "timed_out": false, "error": format!("{error:#}")})
}

pub(in crate::native) fn parse(args: &[String]) -> Result<NativeCommand> {
    match parse_options(args) {
        Ok(request) => Ok(NativeCommand::Wait(request)),
        Err(error) => {
            if args.iter().any(|arg| arg == "--json") {
                print_json(&error_value(&error))?;
            }
            Err(error)
        }
    }
}

fn parse_options(args: &[String]) -> Result<WaitRequest> {
    let mut addresses = Vec::new();
    let mut timeout = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => set_flag_once(&mut json, "--json")?,
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
    Ok(WaitRequest {
        addresses,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        json,
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
fn observe(snapshot: &Snapshot, reader: &Reader, selector: &Selector) -> Result<(Value, bool)> {
    let mut observation = snapshot.observe_result(reader, selector)?;
    if observation.state.ends_wait() {
        return Ok((observation.value(), true));
    }
    let owner = observe_owner(reader);
    if owner.process_alive == Some(false) || owner.identity_matches == Some(false) {
        observation.state = RequestState::Unresolved;
        observation.error = Some(
            "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request".to_owned()
        );
    } else if matches!(
        snapshot.status.state,
        SessionState::Closed | SessionState::Failed | SessionState::Exited
    ) {
        observation.state = RequestState::Unresolved;
    }
    let mut value = observation.value();
    value["owner_process_alive"] = json!(owner.process_alive);
    value["owner"] = json!(owner);
    Ok((value, observation.state.ends_wait()))
}

struct Outcome {
    selected: Option<usize>,
    last: Vec<Value>,
}

struct SessionGroup {
    directory: PathBuf,
    indices: Vec<usize>,
}

/// Validates directories before polling. Receipts are checked only under a consistent
/// Snapshot, on every readable pass. Busy means no observation, never a pending result.
fn poll(root: &Path, addresses: &[Address], timeout: Duration) -> Result<Outcome> {
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
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    // One selector per address, built before polling: the loop only borrows them.
    let selectors: Vec<Selector> = addresses
        .iter()
        .map(|address| Selector::Request(address.request.clone()))
        .collect();
    let mut outcome = Outcome {
        selected: None,
        last: addresses.iter().map(|address| json!({
            "schema_version": 1, "ok": true, "session": address.session,
            "request_id": address.request, "request_state": "busy", "result": null,
            "bridge_observed_elapsed_ms": null, "bridge_observed_elapsed_reason": "no_published_result"
        })).collect(),
    };
    loop {
        for group in &groups {
            let reader = Reader::open_unchecked(&group.directory);
            let snapshot = match Snapshot::read(&reader) {
                Ok(snapshot) => snapshot,
                Err(error) if error.is::<SnapshotBusy>() => continue,
                Err(error) => return Err(error),
            };
            for &index in &group.indices {
                let (value, ended) = observe(&snapshot, &reader, &selectors[index])?;
                outcome.last[index] = value;
                if ended {
                    outcome.selected = Some(
                        outcome
                            .selected
                            .map_or(index, |previous| previous.min(index)),
                    );
                }
            }
            // Drop this session's shared lock before reading the next session.
        }
        // Validate every readable address in the pass before selecting a success.
        if outcome.selected.is_some() {
            return Ok(outcome);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(outcome);
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

/// The legacy result caller retains its timeout envelope and last good observation.
pub(super) fn one(root: &Path, session: &str, request: &str, timeout: Duration) -> Result<Value> {
    let outcome = poll(
        root,
        &[Address {
            session: session.to_owned(),
            request: request.to_owned(),
        }],
        timeout,
    )?;
    let mut last = outcome.last.into_iter().next().unwrap();
    if outcome.selected.is_none() {
        last["ok"] = json!(false);
        last["timed_out"] = json!(true);
        last["error"] = json!("waiting timed out; the request was not cancelled or resent");
    }
    Ok(last)
}

fn value_in(root: &Path, request: &WaitRequest) -> Result<Value> {
    let outcome = poll(root, &request.addresses, request.timeout)?;
    let remaining: Vec<_> = request
        .addresses
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != outcome.selected)
        .map(|(_, address)| address.text())
        .collect();
    let Some(index) = outcome.selected else {
        return Ok(json!({"schema_version": 1, "ok": false, "ended": null,
            "remaining": remaining, "timed_out": true,
            "error": "waiting timed out; no request was cancelled or resent"}));
    };
    let mut ended = outcome.last.into_iter().nth(index).unwrap();
    if ended["request_state"] != "completed" {
        correct_unsuccessful_result(&mut ended);
    }
    ended["address"] = json!(request.addresses[index].text());
    Ok(
        json!({"schema_version": 1, "ok": ended["ok"], "ended": ended,
        "remaining": remaining, "timed_out": false}),
    )
}

pub(in crate::native) fn run(request: WaitRequest) -> Result<()> {
    let value = match Reader::state_root().and_then(|root| value_in(&root, &request)) {
        Ok(value) => value,
        Err(error) => {
            if request.json {
                print_json(&error_value(&error))?;
            }
            return Err(error);
        }
    };
    if request.json {
        print_json(&value)?;
    } else {
        if !value["ended"].is_null() {
            print_result(&value["ended"], false)?;
        } else if let Some(error) = value["error"].as_str() {
            println!("error: {error}");
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
    }
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
    use crate::native::{tests::seed_close_fixture, write_json_atomic};

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
        let id = Snapshot::read(&reader).unwrap().receipts[0]
            .request_id
            .clone();
        fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
        let mut calls = 0;
        let value = with_snapshot_hook(
            move |directory| {
                calls += 1;
                if calls > 1 {
                    let path = directory.join("status.json");
                    let mut status: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    status["generation"] = json!(100 + calls);
                    write_json_atomic(&path, &status).unwrap();
                }
            },
            || {
                one(
                    fixture.directory.parent().unwrap(),
                    "session-fault",
                    &id,
                    Duration::from_millis(150),
                )
                .unwrap()
            },
        );
        assert_eq!(value["request_state"], "pending");
        assert_eq!(value["timed_out"], true);
        assert!(value.get("owner").is_some());
        assert_eq!(value["session_state"], "working");
    }

    #[test]
    fn later_receipt_damage_or_deletion_is_an_error() {
        for damage in [false, true] {
            let fixture = seed_close_fixture(JournaledEventState::Absent);
            let reader = Reader::open_unchecked(&fixture.directory);
            let snapshot = Snapshot::read(&reader).unwrap();
            let receipt = &snapshot.receipts[0];
            let id = receipt.request_id.clone();
            let path = fixture
                .directory
                .join("requests")
                .join(format!("{}.json", receipt.claim_token));
            drop(snapshot);
            fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
            let mut calls = 0;
            let result = with_snapshot_hook(
                move |_| {
                    calls += 1;
                    if calls == 2 {
                        if damage {
                            write_json_atomic(&path, &json!({})).unwrap();
                        } else {
                            fs::remove_file(&path).unwrap();
                        }
                    }
                },
                || {
                    one(
                        fixture.directory.parent().unwrap(),
                        "session-fault",
                        &id,
                        Duration::from_secs(1),
                    )
                },
            );
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains(if damage {
                "no readable receipt"
            } else {
                "no such Bridge request"
            }));
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
            let id = Snapshot::read(&reader).unwrap().receipts[0]
                .request_id
                .clone();
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
        let snapshot = Snapshot::read(&reader).unwrap();
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
                )
                .unwrap()
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(outcome.selected, Some(0));
    }
}
