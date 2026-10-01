//! A read-only projection of retained evidence, not a history of inferred transitions.
use super::*;
use std::collections::BTreeMap;

type Records = BTreeMap<String, std::result::Result<Option<Vec<u8>>, String>>;

fn records(directory: &Path) -> Result<Records> {
    let mut names = vec![
        "manifest.json".to_owned(),
        "status.json".to_owned(),
        TURN_CLAIM_FILE.to_owned(),
        TURN_COMPLETION_FILE.to_owned(),
        launch::FILE.to_owned(),
        launch::LOG.to_owned(),
        CLOSED_STATUS_FILE.to_owned(),
        TERMINAL_CLOSING_FILE.to_owned(),
        TERMINAL_TOMBSTONE_FILE.to_owned(),
    ];
    // Use the existing index's directory validation before enumerating receipt files.
    let request_index = requests::list(directory);
    if request_index.is_ok() && directory.join("requests").is_dir() {
        for entry in fs::read_dir(directory.join("requests"))? {
            let name = entry?.file_name();
            if let Some(name) = name.to_str()
                && name
                    .strip_suffix(".json")
                    .is_some_and(valid_turn_claim_token)
            {
                names.push(format!("requests/{name}"));
            }
        }
    }
    let events_present = matches!(events_directory_state(directory)?, EventsDirectory::Present);
    for path in if events_present {
        event_paths(directory)?
    } else {
        Vec::new()
    } {
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            names.push(format!("events/{name}"));
        }
    }
    let mut records: Records = names
        .into_iter()
        .map(|name| {
            let value = read_regular_bytes_if_present(&directory.join(&name))
                .map_err(|error| format!("{error:#}"));
            (name, value)
        })
        .collect();
    records.insert("events/".to_owned(), Ok(events_present.then(|| vec![1])));
    let index = match request_index {
        Ok(index) if directory.join("requests").exists() => Ok(Some(serde_json::to_vec(&(
            index.receipts,
            index.unreadable,
        ))?)),
        Ok(_) => Ok(None),
        Err(error) => Err(format!("{error:#}")),
    };
    records.insert("requests/".to_owned(), index);
    Ok(records)
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
        "event_id": event, "stage": stage, "observed_unix_ms": time,
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
    directory: &Path,
    id: &str,
    request: Option<&str>,
) -> Result<Value> {
    let before = records(directory)?;
    let snapshot = observe_snapshot(directory)?;
    if let Some(request) = request {
        // Keep the exact-request lookup's missing/incomplete-index error contract.
        if !snapshot.receipts.iter().any(|r| r.request_id == request) {
            snapshot.result(directory, &Selector::Request(request.to_owned()))?;
        }
    }
    let mut entries = Vec::new();
    let mut session_entries = Vec::new();
    let mut summaries = Vec::new();
    let mut incomplete = snapshot.unreadable_requests > 0 || snapshot.request_index_error.is_some();
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
            Some(receipt.created_unix_ms),
            &source,
            "observed",
            json!({"source": receipt.source,
                "context_sources": receipt.context_sources}),
        ));
        let event_source = format!("events/{}", receipt.event_file);
        let result = match before.get(&event_source) {
            Some(Err(error)) if snapshot.published(&receipt.event_file) => {
                Err(anyhow::anyhow!("{error}"))
            }
            _ => snapshot.result(directory, &Selector::Request(receipt.request_id.clone())),
        };
        let summary = match result {
            Ok(mut result) => {
                result.as_object_mut().unwrap().remove("result");
                result["result_command"] = json!(format!(
                    "agent-bridge result {id} --request {} --json",
                    receipt.request_id
                ));
                result["delivery"] = json!(if matches!(
                    result["request_state"].as_str(),
                    Some("completed" | "failed")
                ) && !result["created_unix_ms"].is_null()
                {
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
                    "result_command": format!("agent-bridge result {id} --request {} --json", receipt.request_id)})
            }
        };
        summaries.push(summary);
    }
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
            Some(Err(error)) if snapshot.published(name) => Err(anyhow::anyhow!("{error}")),
            _ => snapshot.event(directory, name),
        };
        let e = match event {
            Ok(Some(event)) => entry(
                id,
                receipt,
                Some(name),
                "completion",
                Some(event.created_unix_ms),
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
        else if directory.join("requests").exists() { "observed" } else { "missing" },
        json!({"unreadable_requests": snapshot.unreadable_requests, "error": snapshot.request_index_error})));
    if snapshot.launch.is_none() {
        session_entries.push(entry(
            id,
            None,
            None,
            "launch_phase",
            None,
            launch::FILE,
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
            launch::FILE,
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
            TURN_COMPLETION_FILE,
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
    session_entries.push(entry(
        id,
        None,
        None,
        "status",
        Some(snapshot.status.updated_unix_ms),
        "status.json",
        "observed",
        json!(snapshot.status),
    ));
    for (source, stage) in [
        (CLOSED_STATUS_FILE, "closed_status"),
        (TERMINAL_CLOSING_FILE, "terminal_closing"),
        (TERMINAL_TOMBSTONE_FILE, "terminal_closed"),
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
                        .and_then(|(time, _)| time.parse::<u128>().ok());
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
                let parsed = if source == CLOSED_STATUS_FILE {
                    read_regular_status_if_present(&directory.join(source))
                        .map(|v| v.map(|v| json!(v)))
                } else if source == TERMINAL_CLOSING_FILE {
                    optional_json::<terminal::TerminalSession>(&directory.join(source))
                        .map(|v| v.map(|v| json!(v)))
                } else {
                    optional_json::<Value>(&directory.join(source))
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
    if before != records(directory)? {
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
        "doctor_command": format!("agent-bridge doctor {id} --json")}),
    )
}

pub(super) fn run(id: &str, json: bool, request: Option<&str>) -> Result<()> {
    let value = timeline_value(&session_directory(id)?, id, request)?;
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
