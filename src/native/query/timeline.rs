//! A read-only projection of retained evidence, not a history of inferred transitions.
use super::*;
use crate::native::session::record::{self, Cached, CachedRecords as Records};
use crate::native::session::schema::{EVENT_READ_LIMIT, SessionEvent};
use crate::native::session::{CoreRecord, Reader};
use crate::native::{EventsDirectory, valid_turn_claim_token};
use agent_bridge::PUBLIC_COMMAND;
use serde::de::DeserializeOwned;

// Auxiliary launch diagnostics retain at most 1 MiB; all other reads use
// the existing 64 MiB event limit. Oversized files are evidence gaps.
const LOG_READ_LIMIT: u64 = 1024 * 1024;

fn records(reader: &Reader, request: Option<&str>) -> Result<Records> {
    let directory = reader.directory();
    let mut records = Records::new();
    for name in [
        CoreRecord::Cancel.name(),
        CoreRecord::Manifest.name(),
        CoreRecord::Status.name(),
        CoreRecord::TurnClaim.name(),
        CoreRecord::Completion.name(),
        CoreRecord::Launch.name(),
        launch::LOG,
        CoreRecord::Closed.name(),
        CoreRecord::TerminalClosing.name(),
        CoreRecord::TerminalClosed.name(),
    ] {
        records.insert(
            name.to_owned(),
            reader
                .private(name)
                .timeline(if name == launch::LOG {
                    LOG_READ_LIMIT
                } else {
                    EVENT_READ_LIMIT
                })
                .map_err(|e| format!("{e:#}")),
        );
    }
    let root = Reader::open_unchecked(directory)
        .record(CoreRecord::Requests)
        .path()
        .to_owned();
    let index = (|| -> Result<Option<Vec<u8>>> {
        let m = match fs::symlink_metadata(&root) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if m.file_type().is_symlink() || !m.is_dir() {
            bail!("refusing non-directory or linked requests directory");
        }
        for item in fs::read_dir(&root)? {
            let name = item?.file_name();
            if let Some(name) = name.to_str()
                && name
                    .strip_suffix(".json")
                    .is_some_and(valid_turn_claim_token)
            {
                records.insert(
                    format!("requests/{name}"),
                    reader
                        .record(CoreRecord::Requests)
                        .child(name)
                        .timeline(EVENT_READ_LIMIT)
                        .map_err(|e| format!("{e:#}")),
                );
            }
        }
        Ok(Some(vec![1]))
    })();
    records.insert("requests/".to_owned(), index.map_err(|e| format!("{e:#}")));
    let receipts = receipt_index(&records);
    let present = matches!(
        Reader::open_unchecked(directory).events_directory_state()?,
        EventsDirectory::Present
    );
    for path in if present {
        reader.events()?
    } else {
        Vec::new()
    } {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if receipts
                .receipts
                .iter()
                .any(|r| r.event_file == name && request.is_some_and(|id| r.request_id != id))
            {
                continue;
            }
            records.insert(
                format!("events/{name}"),
                reader
                    .event(name)
                    .timeline(EVENT_READ_LIMIT)
                    .map_err(|e| format!("{e:#}")),
            );
        }
    }
    records.insert("events/".to_owned(), Ok(present.then(|| vec![1])));
    Ok(records)
}

/// The timeline's own strict parser of a retained record that is not a core Record (the
/// cancel record, the terminal records, an event) or a deserialize-only diagnostic: strict
/// UTF-8, the `_unix_ms` range rule, then the shape, with the `invalid JSON record`
/// context. A retained read fault is the error it was retained as.
fn parsed<T: DeserializeOwned>(records: &Records, source: &str) -> Result<Option<T>> {
    let bytes = records
        .get(source)
        .context("record not selected")?
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    bytes
        .as_ref()
        .map(|b| {
            let text = std::str::from_utf8(b).context("invalid UTF-8")?;
            record::timestamps_in_range(&serde_json::from_str::<Value>(text)?)?;
            serde_json::from_str(text).context("invalid JSON record")
        })
        .transpose()
}

/// The receipt index over the retained receipts, as the Cached source presents it, so
/// that the request filter of the event collection matches the assembled Snapshot.
fn receipt_index(records: &Records) -> requests::Index {
    let mut source = Cached::new(records, None);
    use crate::native::session::record::Source as _;
    let tokens = match source.receipt_names() {
        record::ReceiptNames::Absent => Vec::new(),
        record::ReceiptNames::Present(tokens) | record::ReceiptNames::Fault { tokens, .. } => {
            tokens
        }
    };
    record::receipt_index(&tokens, |token| source.receipt(token))
}

fn event(records: &Records, snapshot: &Snapshot, name: &str) -> Result<Option<SessionEvent>> {
    for source in [
        CoreRecord::Status.name(),
        CoreRecord::TurnClaim.name(),
        CoreRecord::Completion.name(),
        CoreRecord::Launch.name(),
    ] {
        if let Err(error) = &records[source] {
            bail!("{source}: {error}");
        }
    }
    if !snapshot.published(name) {
        return Ok(None);
    }
    let source = format!("events/{name}");
    if !records.contains_key(&source) {
        return Ok(None);
    }
    parsed(records, &source)
}

struct Entry<'a> {
    stage: &'a str,
    time: Option<u128>,
    source: &'a str,
    state: &'a str,
    detail: Value,
}

fn entry(
    id: &str,
    receipt: Option<&requests::Receipt>,
    event: Option<&str>,
    entry: Entry<'_>,
) -> Value {
    let Entry {
        stage,
        time,
        source,
        state,
        detail,
    } = entry;
    json!({"session": id, "request_id": receipt.map(|r| &r.request_id),
        "event_id": event, "stage": stage, "observed_unix_ms": time.and_then(|t| u64::try_from(t).ok()),
        "source": source, "record_state": state, "detail": detail})
}

fn unreadable_entry(id: &str, stage: &str, source: &str, error: Value) -> Value {
    entry(
        id,
        None,
        None,
        Entry {
            stage,
            time: None,
            source,
            state: "unreadable",
            detail: error,
        },
    )
}

fn sort(entries: &mut [Value]) {
    // Null times are explicitly unknown and are placed after timed evidence. Stable
    // insertion order breaks ties; it does not assert an order between equal times.
    entries.sort_by_key(|e| {
        e["observed_unix_ms"]
            .as_u64()
            .map(u128::from)
            .unwrap_or(u128::MAX)
    });
}

fn unreadable_receipts(before: &Records, id: &str, session_entries: &mut Vec<Value>) {
    for source in before
        .keys()
        .filter(|s| s.starts_with("requests/") && s.ends_with(".json"))
    {
        if let Err(error) = parsed::<requests::Receipt>(before, source) {
            session_entries.push(unreadable_entry(
                id,
                "request_receipt",
                source,
                json!(format!("{error:#}")),
            ));
        }
    }
}

fn render_cancel(
    before: &Records,
    snapshot: &Snapshot,
    id: &str,
    request: Option<&str>,
    entries: &mut Vec<Value>,
    session_entries: &mut Vec<Value>,
    incomplete: &mut bool,
) {
    let source = CoreRecord::Cancel.name();
    let cancel =
        parsed::<crate::native::session::cancel::Cancel>(before, source).and_then(|record| {
            if let Some(record) = &record {
                record.validate()?;
            }
            Ok(record)
        });
    match cancel {
        Ok(Some(cancel)) if request.is_none_or(|r| r == cancel.request_id) => {
            let receipt = snapshot
                .receipts
                .iter()
                .find(|r| r.request_id == cancel.request_id && r.claim_token == cancel.claim_token);
            let observed = observation::CancelObservation::from_record(&cancel, snapshot, |name| {
                event(before, snapshot, name)
            });
            *incomplete |= observed.state == "unreadable";
            let mut value = entry(
                id,
                receipt,
                receipt.map(|r| r.event_file.as_str()),
                Entry {
                    stage: "cancel_request",
                    time: Some(cancel.created_unix_ms),
                    source,
                    state: "observed",
                    detail: json!({"claim_token":cancel.claim_token}),
                },
            );
            value["request_id"] = json!(cancel.request_id);
            value["cancel"] = json!({"state": observed.state, "derived_from": "result"});
            entries.push(value);
        }
        Err(error) => {
            *incomplete = true;
            session_entries.push(unreadable_entry(
                id,
                "cancel_request",
                source,
                json!(format!("{error:#}")),
            ));
        }
        _ => (),
    }
}

fn unreadable_core_records(before: &Records, id: &str, session_entries: &mut Vec<Value>) {
    for (source, stage) in [
        (CoreRecord::TurnClaim.name(), "turn_claim"),
        (CoreRecord::Completion.name(), "completion_journal"),
        (CoreRecord::Launch.name(), "launch_phase"),
        (CoreRecord::Status.name(), "status"),
    ] {
        if let Err(error) = &before[source] {
            session_entries.push(unreadable_entry(id, stage, source, json!(error)));
        }
    }
}

fn render_receipts(snapshot: &Snapshot, id: &str, request: Option<&str>, entries: &mut Vec<Value>) {
    for receipt in snapshot
        .receipts
        .iter()
        .filter(|r| request.is_none_or(|id| r.request_id == id))
    {
        let source = format!("requests/{}.json", receipt.claim_token);
        entries.push(entry(
            id,
            Some(receipt),
            Some(&receipt.event_file),
            Entry {
                stage: "request_receipt",
                time: receipt.created_unix_ms,
                source: &source,
                state: "observed",
                detail: json!({"source": receipt.source,
                "context_sources": receipt.context_sources}),
            },
        ));
    }
}

fn request_summaries(
    before: &Records,
    snapshot: &Snapshot,
    id: &str,
    request: Option<&str>,
    incomplete: &mut bool,
) -> Vec<Value> {
    let mut summaries = Vec::new();
    for receipt in snapshot
        .receipts
        .iter()
        .filter(|r| request.is_none_or(|id| r.request_id == id))
    {
        let event_source = format!("events/{}", receipt.event_file);
        let mut completion_recorded = false;
        let result = match before.get(&event_source) {
            Some(Err(error)) if snapshot.published(&receipt.event_file) => {
                Err(anyhow::anyhow!("{error}"))
            }
            _ => snapshot.result_with(&Selector::Request(receipt.request_id.clone()), |name| {
                let event = event(before, snapshot, name)?;
                completion_recorded = event.is_some();
                Ok(event)
            }),
        };
        let summary = match result {
            Ok(mut result) => {
                result.as_object_mut().unwrap().remove("result");
                result["result_command"] = json!(format!(
                    "{PUBLIC_COMMAND} result {id} --request {} --json",
                    receipt.request_id
                ));
                result["delivery"] = json!(if completion_recorded {
                    "completion_recorded"
                } else {
                    "unknown"
                });
                result
            }
            Err(error) => {
                *incomplete = true;
                json!({"request_id": receipt.request_id, "event_id": receipt.event_file,
                    "request_state": "unknown", "delivery": "unknown", "error": format!("{error:#}"),
                    "result_command": format!("{PUBLIC_COMMAND} result {id} --request {} --json", receipt.request_id)})
            }
        };
        let mut summary = summary;
        summary["derived_from"] = json!("result");
        summaries.push(summary);
    }
    summaries
}

fn render_events(
    before: &Records,
    snapshot: &Snapshot,
    id: &str,
    request: Option<&str>,
    entries: &mut Vec<Value>,
    session_entries: &mut Vec<Value>,
    incomplete: &mut bool,
) {
    let mut event_names = snapshot
        .paths
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
        .chain(snapshot.receipts.iter().map(|r| r.event_file.clone()))
        .collect::<Vec<_>>();
    event_names.sort();
    event_names.dedup();
    for name in &event_names {
        let name = name.as_str();
        let receipt = snapshot.receipt_for_event(name);
        if receipt.is_some_and(|r| request.is_some_and(|id| r.request_id != id)) {
            continue;
        }
        let source = format!("events/{name}");
        let event = match before.get(&source) {
            Some(Err(error)) => Err(anyhow::anyhow!("{error}")),
            _ => event(before, snapshot, name),
        };
        let e = match event {
            Ok(Some(event)) => entry(
                id,
                receipt,
                Some(name),
                Entry {
                    stage: "completion",
                    time: event.created_unix_ms,
                    source: &source,
                    state: "observed",
                    detail: json!(event),
                },
            ),
            Ok(None) => {
                *incomplete = true;
                entry(
                    id,
                    receipt,
                    Some(name),
                    Entry {
                        stage: "completion",
                        time: None,
                        source: &source,
                        state: if before
                            .get(&source)
                            .is_some_and(|r| matches!(r, Ok(Some(_))))
                        {
                            "unpublished"
                        } else {
                            "missing"
                        },
                        detail: Value::Null,
                    },
                )
            }
            Err(error) => {
                *incomplete = true;
                entry(
                    id,
                    receipt,
                    Some(name),
                    Entry {
                        stage: "completion",
                        time: None,
                        source: &source,
                        state: "unreadable",
                        detail: json!(format!("{error:#}")),
                    },
                )
            }
        };
        if receipt.is_some() {
            entries.push(e);
        } else {
            session_entries.push(e);
        }
    }
}

fn render_event_index(
    before: &Records,
    id: &str,
    session_entries: &mut Vec<Value>,
    incomplete: &mut bool,
) {
    let events_present = before["events/"].as_ref().is_ok_and(|v| v.is_some());
    *incomplete |= !events_present;
    session_entries.push(entry(
        id,
        None,
        None,
        Entry {
            stage: "event_index",
            time: None,
            source: "events/",
            state: if events_present {
                "observed"
            } else {
                "missing"
            },
            detail: Value::Null,
        },
    ));
}

fn request_index_entry(id: &str, snapshot: &Snapshot, state: &str) -> Value {
    entry(
        id,
        None,
        None,
        Entry {
            stage: "request_index",
            time: None,
            source: "requests/",
            state,
            detail: json!({"unreadable_requests": snapshot.unreadable_requests, "error": snapshot.request_index_error}),
        },
    )
}

fn render_lifecycle(
    before: &Records,
    snapshot: &Snapshot,
    id: &str,
    request: Option<&str>,
    entries: &mut Vec<Value>,
    session_entries: &mut Vec<Value>,
) {
    if snapshot.launch.is_none() && before[CoreRecord::Launch.name()].is_ok() {
        session_entries.push(entry(
            id,
            None,
            None,
            Entry {
                stage: "launch_phase",
                time: None,
                source: CoreRecord::Launch.name(),
                state: "missing",
                detail: Value::Null,
            },
        ));
    }
    if let Some(launch) = &snapshot.launch {
        let receipt = snapshot
            .receipts
            .iter()
            .find(|r| r.claim_token == launch.claim_token);
        let e = entry(
            id,
            receipt,
            receipt.map(|r| r.event_file.as_str()),
            Entry {
                stage: "launch_phase",
                time: None,
                source: CoreRecord::Launch.name(),
                state: "observed",
                detail: json!({"phase": launch.phase, "deadline_unix_ms": launch.deadline_unix_ms}),
            },
        );
        if let Some(receipt) = receipt {
            if request.is_none_or(|id| receipt.request_id == id) {
                entries.push(e);
            }
        } else {
            session_entries.push(e);
        }
    }
    if let Some(pending) = &snapshot.pending {
        let receipt = snapshot.receipt_for_event(&pending.event_file);
        let e = entry(
            id,
            receipt,
            Some(&pending.event_file),
            Entry {
                stage: "completion_journal",
                time: None,
                source: CoreRecord::Completion.name(),
                state: "observed",
                detail: json!({"published": snapshot.published(&pending.event_file)}),
            },
        );
        if let Some(receipt) = receipt {
            if request.is_none_or(|id| receipt.request_id == id) {
                entries.push(e);
            }
        } else {
            session_entries.push(e);
        }
    }
    if before[CoreRecord::Status.name()].is_ok() {
        session_entries.push(entry(
            id,
            None,
            None,
            Entry {
                stage: "status",
                time: Some(snapshot.status.updated_unix_ms),
                source: CoreRecord::Status.name(),
                state: "observed",
                detail: json!(snapshot.status),
            },
        ));
    }
}

fn render_close_and_log(
    before: &Records,
    id: &str,
    session_entries: &mut Vec<Value>,
    incomplete: &mut bool,
) -> Result<()> {
    for (source, stage) in [
        (CoreRecord::Closed.name(), "closed_status"),
        (CoreRecord::TerminalClosing.name(), "terminal_closing"),
        (CoreRecord::TerminalClosed.name(), "terminal_closed"),
        (launch::LOG, "launch_log"),
    ] {
        let read = before.get(source).expect("fixed source");
        match read {
            Ok(Some(text)) if source == launch::LOG => {
                let text = match std::str::from_utf8(text) {
                    Ok(text) => text,
                    Err(error) => {
                        *incomplete = true;
                        session_entries.push(unreadable_entry(
                            id,
                            stage,
                            source,
                            json!(error.to_string()),
                        ));
                        continue;
                    }
                };
                for (index, line) in text.lines().enumerate() {
                    let time = line
                        .split_once(' ')
                        .and_then(|(time, _)| time.parse::<u64>().ok().map(u128::from));
                    session_entries.push(entry(
                        id,
                        None,
                        None,
                        Entry {
                            stage,
                            time,
                            source,
                            state: "observed",
                            detail: json!({"line": index + 1, "text": line}),
                        },
                    ));
                }
            }
            Ok(Some(_)) => {
                let parsed = if source == CoreRecord::Closed.name() {
                    match read {
                        Ok(Some(bytes)) => record::status(bytes)
                            .map(|status| Some(json!(status)))
                            .map_err(anyhow::Error::from),
                        _ => unreachable!("matched a present record"),
                    }
                } else if source == CoreRecord::TerminalClosing.name() {
                    parsed::<terminal::TerminalSession>(before, source).map(|v| v.map(|v| json!(v)))
                } else {
                    parsed::<Value>(before, source)
                };
                match parsed {
                    Ok(Some(detail)) => {
                        let time = detail["updated_unix_ms"].as_u64().map(u128::from);
                        session_entries.push(entry(
                            id,
                            None,
                            None,
                            Entry {
                                stage,
                                time,
                                source,
                                state: "observed",
                                detail,
                            },
                        ));
                    }
                    Ok(None) => return Err(SnapshotBusy.into()),
                    Err(error) => {
                        *incomplete = true;
                        session_entries.push(unreadable_entry(
                            id,
                            stage,
                            source,
                            json!(format!("{error:#}")),
                        ));
                    }
                }
            }
            Ok(None) => session_entries.push(entry(
                id,
                None,
                None,
                Entry {
                    stage,
                    time: None,
                    source,
                    state: "missing",
                    detail: Value::Null,
                },
            )),
            Err(error) => {
                *incomplete = true;
                session_entries.push(unreadable_entry(id, stage, source, json!(error)));
            }
        }
    }
    Ok(())
}

pub(in crate::native) fn timeline_value(
    reader: &Reader,
    id: &str,
    request: Option<&str>,
) -> Result<Value> {
    let directory = reader.directory();
    let lock = reader.lock_shared_with_retry(snapshot_retry_window())?;
    let before = records(reader, request)?;
    let expected_id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let snapshot = Snapshot::assemble(
        Cached::new(&before, lock),
        expected_id,
        PublicationRead::Within(EVENT_READ_LIMIT),
    )?;
    before_consistency_check(directory);
    if let Some(request) = request {
        // Keep the exact-request lookup's missing/incomplete-index error contract.
        if !snapshot.receipts.iter().any(|r| r.request_id == request) {
            snapshot.result_with(&Selector::Request(request.to_owned()), |name| {
                event(&before, &snapshot, name)
            })?;
        }
    }
    let mut entries = Vec::new();
    let mut session_entries = Vec::new();
    unreadable_receipts(&before, id, &mut session_entries);
    let mut incomplete = snapshot.unreadable_requests > 0
        || snapshot.request_index_error.is_some()
        || before.values().any(|r| r.is_err());
    render_cancel(
        &before,
        &snapshot,
        id,
        request,
        &mut entries,
        &mut session_entries,
        &mut incomplete,
    );
    unreadable_core_records(&before, id, &mut session_entries);
    render_receipts(&snapshot, id, request, &mut entries);
    let summaries = request_summaries(&before, &snapshot, id, request, &mut incomplete);
    before_consistency_check(directory);
    render_events(
        &before,
        &snapshot,
        id,
        request,
        &mut entries,
        &mut session_entries,
        &mut incomplete,
    );
    render_event_index(&before, id, &mut session_entries, &mut incomplete);
    let request_index_state =
        if snapshot.request_index_error.is_some() || snapshot.unreadable_requests > 0 {
            "unreadable"
        } else if before["requests/"].as_ref().is_ok_and(|v| v.is_some()) {
            "observed"
        } else {
            "missing"
        };
    session_entries.push(request_index_entry(id, &snapshot, request_index_state));
    render_lifecycle(
        &before,
        &snapshot,
        id,
        request,
        &mut entries,
        &mut session_entries,
    );
    render_close_and_log(&before, id, &mut session_entries, &mut incomplete)?;
    if before != records(reader, request)? {
        return Err(SnapshotBusy.into());
    }
    sort(&mut entries);
    sort(&mut session_entries);
    Ok(
        envelope::ok(json!({"session": id, "request_id": request,
        "session_state": snapshot.status.state, "session_error": snapshot.status.error,
        "recovery_required": snapshot.pending.is_some(), "incomplete": incomplete,
        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
        "requests": summaries, "entries": entries, "session_entries": session_entries,
        "doctor_command": format!("{PUBLIC_COMMAND} doctor {id} --json")}).as_object().unwrap().clone()),
    )
}

pub(super) fn run(id: &str, json: bool, request: Option<&str>) -> Result<()> {
    let value = timeline_value(
        &Reader::open_unchecked(Reader::session_directory(id)?),
        id,
        request,
    )?;
    envelope::emit(json, &value, || {
        println!(
            "session: {id}\nstate (stored): {}\nincomplete: {}\nrecovery required: {}",
            value["session_state"], value["incomplete"], value["recovery_required"]
        );
        for summary in value["requests"].as_array().into_iter().flatten() {
            println!(
                "request: {} event: {} state: {} delivery: {}",
                summary["request_id"],
                summary["event_id"],
                summary["request_state"],
                summary["delivery"]
            );
            println!(
                "  {}",
                summary["result_command"].as_str().unwrap_or("unknown")
            );
            if let Some(error) = summary["error"].as_str() {
                println!("  {}", terminal_safe_text(error, true));
            }
        }
        if let Some(error) = value["session_error"].as_str() {
            println!("error: {}", terminal_safe_text(error, true));
        }
        for group in ["entries", "session_entries"] {
            println!("{group}:");
            for e in value[group].as_array().into_iter().flatten() {
                println!(
                    "  session={} request={} event={} time={} stage={} source={} record={} {}",
                    id,
                    e["request_id"],
                    e["event_id"],
                    if e["observed_unix_ms"].is_null() {
                        "unknown".to_owned()
                    } else {
                        e["observed_unix_ms"].to_string()
                    },
                    e["stage"],
                    e["source"],
                    e["record_state"],
                    terminal_safe_text(&e["detail"].to_string(), true)
                );
            }
        }
        println!(
            "unreadable requests: {}\nrequest index error: {}\n{}",
            value["unreadable_requests"],
            value["request_index_error"],
            value["doctor_command"].as_str().unwrap_or("unknown")
        );
        Ok(())
    })
}
