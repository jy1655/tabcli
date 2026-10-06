//! A read-only projection of retained evidence, not a history of inferred transitions.
use super::*;
use crate::native::session::SessionState;
use crate::native::session::{CoreRecord, Reader};
use crate::native::{EventsDirectory, SESSION_SCHEMA, valid_turn_claim_token};
use agent_bridge::PUBLIC_COMMAND;
use std::collections::BTreeMap;

type Records = BTreeMap<String, std::result::Result<Option<Vec<u8>>, String>>;

// Auxiliary launch diagnostics retain at most 1 MiB; all other reads use
// the existing 64 MiB event limit. Oversized files are evidence gaps.
const LOG_READ_LIMIT: u64 = 1024 * 1024;

fn parsed<T: for<'de> Deserialize<'de>>(records: &Records, source: &str) -> Result<Option<T>> {
    let bytes = records
        .get(source)
        .context("record not selected")?
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    bytes
        .as_ref()
        .map(|b| {
            let text = std::str::from_utf8(b).context("invalid UTF-8")?;
            // Recorded u128 values must fit the integer range supported by json!.
            fn numbers(v: &Value) -> Result<()> {
                match v {
                    Value::Array(vs) => {
                        for v in vs {
                            numbers(v)?;
                        }
                    }
                    Value::Object(vs) => {
                        for (key, v) in vs {
                            if key.ends_with("_unix_ms") && !v.is_null() && v.as_u64().is_none() {
                                bail!(
                                    "record timestamp is outside the JSON unsigned integer range"
                                );
                            }
                            numbers(v)?;
                        }
                    }
                    _ => (),
                }
                Ok(())
            }
            numbers(&serde_json::from_str::<Value>(text)?)?;
            serde_json::from_str(text).context("invalid JSON record")
        })
        .transpose()
}

fn receipt_index(records: &Records) -> requests::Index {
    let mut index = requests::Index::default();
    for source in records
        .keys()
        .filter(|s| s.starts_with("requests/") && s.ends_with(".json"))
    {
        let read = parsed::<requests::Receipt>(records, source).and_then(|r| {
            let r = r.context("missing receipt")?;
            requests::validate(&r)?;
            if source != &format!("requests/{}.json", r.claim_token) {
                bail!("invalid Bridge request receipt");
            }
            Ok(r)
        });
        match read {
            Ok(r) => index.receipts.push(r),
            Err(_) => index.unreadable += 1,
        }
    }
    index.receipts.sort_by(|a, b| {
        a.created_unix_ms
            .cmp(&b.created_unix_ms)
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    index
}

fn records(reader: &Reader, request: Option<&str>) -> Result<Records> {
    let directory = reader.directory();
    let mut records = Records::new();
    for name in [
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

fn snapshot(records: &Records) -> Result<Snapshot> {
    fn optional<T: for<'de> Deserialize<'de>>(
        records: &Records,
        source: &str,
    ) -> Result<Option<T>> {
        if records[source].is_err() {
            return Ok(None);
        }
        parsed(records, source)
    }
    let index = receipt_index(records);
    let manifest: SessionManifest =
        parsed(records, CoreRecord::Manifest.name())?.context("session has no manifest")?;
    if manifest.schema != SESSION_SCHEMA {
        bail!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        );
    }
    let pending: Option<PendingTurnCompletion> = optional(records, CoreRecord::Completion.name())?;
    if let Some(p) = &pending {
        validate_pending_completion(p)?;
    }
    let pending_event = pending
        .as_ref()
        .map(|p| -> Result<JournaledEventRead> {
            let source = format!("events/{}", p.event_file);
            let bytes = records
                .get(&source)
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.as_ref());
            let committed = bytes.is_some_and(|b| {
                serde_json::to_vec_pretty(&p.event).is_ok_and(|expected| &expected == b)
            });
            Ok(JournaledEventRead {
                state: if committed {
                    JournaledEventState::Committed
                } else {
                    JournaledEventState::Mismatched
                },
                bytes_read: 0,
                committed_text: None,
            })
        })
        .transpose()?;
    let launch: Option<launch::Record> = optional(records, CoreRecord::Launch.name())?;
    if launch
        .as_ref()
        .is_some_and(|l| l.schema != 1 || l.claim_token.is_empty())
    {
        bail!("invalid launch receipt identity");
    }
    let claim = records[CoreRecord::TurnClaim.name()]
        .as_ref()
        .ok()
        .and_then(|v| v.as_ref())
        .map(|b| std::str::from_utf8(b).map(|s| s.trim().to_owned()))
        .transpose()?;
    Ok(Snapshot {
        manifest,
        status: if records[CoreRecord::Status.name()].is_err() {
            SessionStatus {
                state: SessionState::Unknown("unknown".to_owned()),
                generation: 0,
                updated_unix_ms: 0,
                exit_code: None,
                error: None,
            }
        } else {
            parsed(records, CoreRecord::Status.name())?.context("session has no status record")?
        },
        receipts: index.receipts,
        unreadable_requests: index.unreadable,
        request_index_error: records["requests/"].as_ref().err().cloned(),
        paths: records
            .keys()
            .filter(|s| s.starts_with("events/") && s.ends_with(".json"))
            .map(PathBuf::from)
            .collect(),
        claim,
        pending,
        launch,
        pending_event,
        _lock: None,
    })
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

#[allow(clippy::too_many_arguments)]
fn entry(
    id: &str,
    receipt: Option<&requests::Receipt>,
    event: Option<&str>,
    stage: &str,
    time: Option<u128>,
    source: &str,
    state: &str,
    detail: Value,
) -> Value {
    json!({"session": id, "request_id": receipt.map(|r| &r.request_id),
        "event_id": event, "stage": stage, "observed_unix_ms": time.and_then(|t| u64::try_from(t).ok()),
        "source": source, "record_state": state, "detail": detail})
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

pub(in crate::native) fn timeline_value(
    reader: &Reader,
    id: &str,
    request: Option<&str>,
) -> Result<Value> {
    let directory = reader.directory();
    let lock = reader.lock_shared_with_retry(snapshot_retry_window())?;
    let before = records(reader, request)?;
    let mut snapshot = snapshot(&before)?;
    snapshot._lock = lock;
    if directory.file_name().and_then(|n| n.to_str()) != Some(snapshot.manifest.id.as_str()) {
        bail!("session manifest id does not match its directory");
    }
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
    let mut summaries = Vec::new();
    for source in before
        .keys()
        .filter(|s| s.starts_with("requests/") && s.ends_with(".json"))
    {
        if let Err(error) = parsed::<requests::Receipt>(&before, source) {
            session_entries.push(entry(
                id,
                None,
                None,
                "request_receipt",
                None,
                source,
                "unreadable",
                json!(format!("{error:#}")),
            ));
        }
    }

    let mut incomplete = snapshot.unreadable_requests > 0
        || snapshot.request_index_error.is_some()
        || before.values().any(|r| r.is_err());
    for (source, stage) in [
        (CoreRecord::TurnClaim.name(), "turn_claim"),
        (CoreRecord::Completion.name(), "completion_journal"),
        (CoreRecord::Launch.name(), "launch_phase"),
        (CoreRecord::Status.name(), "status"),
    ] {
        if let Err(error) = &before[source] {
            session_entries.push(entry(
                id,
                None,
                None,
                stage,
                None,
                source,
                "unreadable",
                json!(error),
            ));
        }
    }
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
            "request_receipt",
            receipt.created_unix_ms,
            &source,
            "observed",
            json!({"source": receipt.source,
                "context_sources": receipt.context_sources}),
        ));
        let event_source = format!("events/{}", receipt.event_file);
        let mut completion_recorded = false;
        let result = match before.get(&event_source) {
            Some(Err(error)) if snapshot.published(&receipt.event_file) => {
                Err(anyhow::anyhow!("{error}"))
            }
            _ => snapshot.result_with(&Selector::Request(receipt.request_id.clone()), |name| {
                let event = event(&before, &snapshot, name)?;
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
                incomplete = true;
                json!({"request_id": receipt.request_id, "event_id": receipt.event_file,
                    "request_state": "unknown", "delivery": "unknown", "error": format!("{error:#}"),
                    "result_command": format!("{PUBLIC_COMMAND} result {id} --request {} --json", receipt.request_id)})
            }
        };
        let mut summary = summary;
        summary["derived_from"] = json!("result");
        summaries.push(summary);
    }
    before_consistency_check(directory);
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
            _ => event(&before, &snapshot, name),
        };
        let e = match event {
            Ok(Some(event)) => entry(
                id,
                receipt,
                Some(name),
                "completion",
                event.created_unix_ms,
                &source,
                "observed",
                json!(event),
            ),
            Ok(None) => {
                incomplete = true;
                entry(
                    id,
                    receipt,
                    Some(name),
                    "completion",
                    None,
                    &source,
                    if before
                        .get(&source)
                        .is_some_and(|r| matches!(r, Ok(Some(_))))
                    {
                        "unpublished"
                    } else {
                        "missing"
                    },
                    Value::Null,
                )
            }
            Err(error) => {
                incomplete = true;
                entry(
                    id,
                    receipt,
                    Some(name),
                    "completion",
                    None,
                    &source,
                    "unreadable",
                    json!(format!("{error:#}")),
                )
            }
        };
        if receipt.is_some() {
            entries.push(e);
        } else {
            session_entries.push(e);
        }
    }
    let events_present = before["events/"].as_ref().is_ok_and(|v| v.is_some());
    incomplete |= !events_present;
    session_entries.push(entry(
        id,
        None,
        None,
        "event_index",
        None,
        "events/",
        if events_present {
            "observed"
        } else {
            "missing"
        },
        Value::Null,
    ));
    session_entries.push(entry(id, None, None, "request_index", None, "requests/",
        if snapshot.request_index_error.is_some() || snapshot.unreadable_requests > 0 { "unreadable" }
        else if Reader::open_unchecked(directory).record(CoreRecord::Requests).path().to_owned().exists() { "observed" } else { "missing" },
        json!({"unreadable_requests": snapshot.unreadable_requests, "error": snapshot.request_index_error})));
    if snapshot.launch.is_none() && before[CoreRecord::Launch.name()].is_ok() {
        session_entries.push(entry(
            id,
            None,
            None,
            "launch_phase",
            None,
            CoreRecord::Launch.name(),
            "missing",
            Value::Null,
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
            "launch_phase",
            None,
            CoreRecord::Launch.name(),
            "observed",
            json!({"phase": launch.phase, "deadline_unix_ms": launch.deadline_unix_ms}),
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
            "completion_journal",
            None,
            CoreRecord::Completion.name(),
            "observed",
            json!({"published": snapshot.published(&pending.event_file)}),
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
            "status",
            Some(snapshot.status.updated_unix_ms),
            CoreRecord::Status.name(),
            "observed",
            json!(snapshot.status),
        ));
    }
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
                        incomplete = true;
                        session_entries.push(entry(
                            id,
                            None,
                            None,
                            stage,
                            None,
                            source,
                            "unreadable",
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
                        stage,
                        time,
                        source,
                        "observed",
                        json!({"line": index + 1, "text": line}),
                    ));
                }
            }
            Ok(Some(_)) => {
                let parsed = if source == CoreRecord::Closed.name() {
                    parsed::<SessionStatus>(&before, source).map(|v| v.map(|v| json!(v)))
                } else if source == CoreRecord::TerminalClosing.name() {
                    parsed::<terminal::TerminalSession>(&before, source)
                        .map(|v| v.map(|v| json!(v)))
                } else {
                    parsed::<Value>(&before, source)
                };
                match parsed {
                    Ok(Some(detail)) => {
                        let time = detail["updated_unix_ms"].as_u64().map(u128::from);
                        session_entries.push(entry(
                            id, None, None, stage, time, source, "observed", detail,
                        ));
                    }
                    Ok(None) => return Err(SnapshotBusy.into()),
                    Err(error) => {
                        incomplete = true;
                        session_entries.push(entry(
                            id,
                            None,
                            None,
                            stage,
                            None,
                            source,
                            "unreadable",
                            json!(format!("{error:#}")),
                        ));
                    }
                }
            }
            Ok(None) => session_entries.push(entry(
                id,
                None,
                None,
                stage,
                None,
                source,
                "missing",
                Value::Null,
            )),
            Err(error) => {
                incomplete = true;
                session_entries.push(entry(
                    id,
                    None,
                    None,
                    stage,
                    None,
                    source,
                    "unreadable",
                    json!(error),
                ));
            }
        }
    }
    if before != records(reader, request)? {
        return Err(SnapshotBusy.into());
    }
    sort(&mut entries);
    sort(&mut session_entries);
    Ok(
        json!({"schema_version": 1, "ok": true, "session": id, "request_id": request,
        "session_state": snapshot.status.state, "session_error": snapshot.status.error,
        "recovery_required": snapshot.pending.is_some(), "incomplete": incomplete,
        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
        "requests": summaries, "entries": entries, "session_entries": session_entries,
        "doctor_command": format!("{PUBLIC_COMMAND} doctor {id} --json")}),
    )
}

pub(super) fn run(id: &str, json: bool, request: Option<&str>) -> Result<()> {
    let value = timeline_value(
        &Reader::open_unchecked(Reader::session_directory(id)?),
        id,
        request,
    )?;
    if json {
        return print_json(&value);
    }
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
}
