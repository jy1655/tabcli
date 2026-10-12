use super::support::*;
use super::*;

#[test]
fn observed_elapsed_queries_preserve_existing_records_and_timestamp_boundaries() {
    use serde_json::{Value, json};
    for (label, receipt_time, result_time, has_receipt, has_event, failed, expected, reason) in [
        (
            "completed",
            Some(100),
            Some(145),
            true,
            true,
            false,
            Some(45),
            None,
        ),
        (
            "failed",
            Some(100),
            Some(145),
            true,
            true,
            true,
            Some(45),
            None,
        ),
        (
            "pending",
            Some(100),
            None,
            true,
            false,
            false,
            None,
            Some("no_published_result"),
        ),
        (
            "legacy",
            Some(100),
            Some(145),
            false,
            true,
            false,
            None,
            Some("missing_receipt"),
        ),
        (
            "missing receipt time",
            None,
            Some(145),
            true,
            true,
            false,
            None,
            Some("missing_receipt_time"),
        ),
        (
            "missing result time",
            Some(100),
            None,
            true,
            true,
            false,
            None,
            Some("missing_result_time"),
        ),
        (
            "inverted",
            Some(145),
            Some(100),
            true,
            true,
            false,
            None,
            Some("inverted_time"),
        ),
        ("zero", Some(0), Some(0), true, true, false, Some(0), None),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-elapsed");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let event_path = directory.join("events/event-1.json");
        let receipt_path = directory.join("requests/1-2-3.json");
        if has_receipt {
            let mut receipt = json!({"schema":1, "request_id":"request-elapsed",
                "claim_token":"1-2-3", "event_file":"event-1.json"});
            if let Some(time) = receipt_time {
                receipt["created_unix_ms"] = json!(time);
            }
            fs::create_dir(directory.join("requests")).unwrap();
            write_json_atomic(&receipt_path, &receipt).unwrap();
        }
        if has_event {
            let mut event = json!({"provider":"codex", "message":"original result",
                "provider_session_id":"thread", "turn_id":"turn"});
            if failed {
                event["error"] = json!("provider failed");
            }
            if let Some(time) = result_time {
                event["created_unix_ms"] = json!(time);
            }
            write_json_atomic(&event_path, &event).unwrap();
        } else {
            write_private(&directory.join(TURN_CLAIM_FILE), b"1-2-3").unwrap();
        }
        let paths = [
            directory.join("manifest.json"),
            directory.join("status.json"),
            receipt_path,
            event_path,
            directory.join(TURN_CLAIM_FILE),
        ];
        let before = paths.each_ref().map(|path| fs::read(path).ok());
        let selector = if has_receipt { "--request" } else { "--event" };
        let address = if has_receipt {
            "request-elapsed"
        } else {
            "event-1.json"
        };
        let result = cli_result(
            root.path(),
            &["result", "session-elapsed", selector, address, "--json"],
        );
        assert_eq!(
            result["bridge_observed_elapsed_ms"],
            json!(expected),
            "{label}"
        );
        assert_eq!(
            result["bridge_observed_elapsed_reason"],
            json!(reason),
            "{label}"
        );
        assert_eq!(
            result["request_state"],
            if !has_event {
                "pending"
            } else if failed {
                "failed"
            } else {
                "completed"
            },
            "{label}"
        );
        assert_eq!(
            result["result"],
            if has_event {
                json!("original result")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            result,
            cli_result(
                root.path(),
                &["result", "session-elapsed", selector, address, "--json"]
            ),
            "repeat: {label}"
        );
        let list = cli_result(
            root.path(),
            &["result", "session-elapsed", "--list", "--json"],
        );
        for entry in list["events"]
            .as_array()
            .unwrap()
            .iter()
            .chain(list["requests"].as_array().unwrap())
        {
            assert_eq!(
                entry["bridge_observed_elapsed_ms"],
                json!(expected),
                "list: {label}"
            );
            assert_eq!(
                entry["bridge_observed_elapsed_reason"],
                json!(reason),
                "list: {label}"
            );
        }
        let inspect =
            query::inspect_value(&Reader::open_unchecked(&directory), "session-elapsed").unwrap();
        if has_event {
            assert_eq!(
                inspect["latest_result"]["bridge_observed_elapsed_ms"],
                json!(expected),
                "inspect latest: {label}"
            );
            assert_eq!(
                inspect["latest_result"]["bridge_observed_elapsed_reason"],
                json!(reason),
                "inspect latest: {label}"
            );
        }
        if has_receipt {
            assert_eq!(
                inspect["requests"][0]["bridge_observed_elapsed_ms"],
                json!(expected),
                "inspect request: {label}"
            );
            assert_eq!(
                inspect["requests"][0]["bridge_observed_elapsed_reason"],
                json!(reason),
                "inspect request: {label}"
            );
        }
        assert_eq!(
            before,
            paths.each_ref().map(|path| fs::read(path).ok()),
            "query wrote records: {label}"
        );
    }
}

#[test]
fn observed_elapsed_requires_a_published_result_and_keeps_inspect_available() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-elapsed-boundary");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let receipt = claim.receipt().clone();
    let mut event = sample_completion(claim.token(), "not published").event;
    event.created_unix_ms = receipt.created_unix_ms;
    write_json_atomic(&directory.join("events").join(&receipt.event_file), &event).unwrap();
    let pending =
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(pending["request_state"], "pending");
    assert_eq!(
        pending["bridge_observed_elapsed_ms"],
        serde_json::Value::Null
    );
    assert_eq!(
        pending["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
    fs::remove_file(directory.join("events").join(&receipt.event_file)).unwrap();
    drop(claim);
    let unresolved =
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(unresolved["request_state"], "unresolved");
    assert_eq!(
        unresolved["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
    // A damaged older result does not prevent inspection of session records or its latest result.
    fs::write(
        directory.join("events").join(&receipt.event_file),
        b"not JSON",
    )
    .unwrap();
    write_json_atomic(
        &directory.join("events/event-99999999999999999999.json"),
        &event,
    )
    .unwrap();
    let inspected = query::inspect_value(
        &Reader::open_unchecked(&directory),
        "session-elapsed-boundary",
    )
    .unwrap();
    assert_eq!(
        inspected["requests"][0]["bridge_observed_elapsed_ms"],
        serde_json::Value::Null
    );
    assert_eq!(
        inspected["requests"][0]["bridge_observed_elapsed_reason"],
        "unreadable_result"
    );
}
