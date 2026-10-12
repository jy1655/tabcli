use super::support::*;
use super::*;

#[test]
fn timeline_rejects_a_log_change_during_a_consistent_lifecycle_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (request, _, _) = seed_claimed_session(directory.path());
    fs::write(directory.path().join(launch::LOG), "1 before\n").unwrap();
    let error = query::with_snapshot_hook(
        |directory| fs::write(directory.join(launch::LOG), "2 after\n").unwrap(),
        || {
            query::timeline_value(
                &Reader::open_unchecked(directory.path()),
                "session-query",
                Some(&request),
            )
        },
    )
    .unwrap_err();
    assert!(error.is::<query::SnapshotBusy>(), "{error:#}");
}

#[test]
fn timeline_reports_busy_without_repair_when_status_keeps_changing() {
    let directory = tempfile::tempdir().unwrap();
    let (request, _, _) = seed_claimed_session(directory.path());
    let error = query::with_snapshot_retry_window(Duration::ZERO, || {
        query::with_snapshot_hook(
            |directory| update_status(directory, SessionState::Working, None, None).unwrap(),
            || {
                query::timeline_value(
                    &Reader::open_unchecked(directory.path()),
                    "session-query",
                    Some(&request),
                )
            },
        )
    })
    .unwrap_err();
    assert!(error.is::<query::SnapshotBusy>(), "{error:#}");
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
}

#[test]
fn search_retries_a_busy_snapshot_without_spending_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-retry");
    let message = format!("needle {}", "x".repeat(4200));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    // A later session with an ordinary published hit, reached only with budget to spare.
    let other = root.path().join("session-zzz");
    fs::create_dir_all(other.join("events")).unwrap();
    write_test_manifest(&other);
    update_status(&other, SessionState::Ready, None, None).unwrap();
    let mut plain = read_json::<SessionEvent>(&event_path).unwrap();
    plain.message = "needle in the other session".to_owned();
    write_json_atomic(&other.join("events").join("event-1.json"), &plain).unwrap();

    // The first snapshot of the journaled session finds a state record changed under it
    // and is retried. The snapshot reads no event, so the retry spends nothing: the
    // journaled event is read once, at its scan position, whatever the attempt count.
    // The retry window is widened so a loaded machine cannot turn the retry into a
    // busy verdict; the retry itself, not its timing, is under test.
    let attempts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let search = |max_bytes: u64, disturb: fn(&Path)| {
        let attempts = std::rc::Rc::clone(&attempts);
        attempts.set(0);
        let mut fired = false;
        query::with_snapshot_retry_window(Duration::from_secs(60), || {
            query::with_snapshot_hook(
                move |directory: &Path| {
                    attempts.set(attempts.get() + 1);
                    if !fired {
                        fired = true;
                        disturb(directory);
                    }
                },
                || {
                    let budget = max_bytes.to_string();
                    with_publication_read_log(|| {
                        cli_search(
                            root.path(),
                            &[
                                "search",
                                "needle",
                                "--all-workspaces",
                                "--max-bytes",
                                &budget,
                                "--json",
                            ],
                        )
                    })
                },
            )
        })
    };
    fn bump_status(directory: &Path) {
        update_status(directory, SessionState::Working, None, None).unwrap();
    }
    fn drop_manifest(directory: &Path) {
        fs::remove_file(directory.join("manifest.json")).unwrap();
    }

    // Budget for exactly one read of the journaled event: the retried snapshot costs
    // nothing, the event is read once and is the hit, and the budget is then spent
    // before the later session.
    let (retried, reads) = search(size, bump_status);
    // Two attempts for the journaled session; the later session is never snapshotted.
    assert_eq!(attempts.get(), 2, "{retried}");
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = retried["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{retried}");
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(retried["scanned"]["sessions"], 1, "{retried}");
    assert_eq!(retried["scanned"]["events"], 1, "{retried}");
    assert!(
        retried["incomplete_reasons"].to_string().contains(&format!(
            "scan stopped: byte budget of {size} bytes exhausted"
        )),
        "{retried}"
    );

    // Budget to spare: the same retry, and both sessions are searched in full.
    let (twice, reads) = search(2 * size, bump_status);
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = twice["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{twice}");
    assert_eq!(twice["scanned"]["sessions"], 2, "{twice}");
    assert_eq!(twice["scanned"]["events"], 2, "{twice}");
    assert_eq!(twice["incomplete"], false, "{twice}");

    // A snapshot that fails leaves the budget untouched for the sessions after it.
    let (failed, reads) = search(size, drop_manifest);
    assert!(reads.is_empty(), "{reads:?}");
    let hits = failed["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{failed}");
    assert_eq!(hits[0]["session"], "session-zzz", "{failed}");
    assert_eq!(failed["scanned"]["sessions"], 1, "{failed}");
    assert_eq!(failed["incomplete"], true, "{failed}");
    let reasons = failed["incomplete_reasons"].to_string();
    assert!(reasons.contains("manifest"), "{reasons}");
    assert!(!reasons.contains("byte budget"), "{reasons}");
}

#[test]
fn queries_reject_an_events_directory_link_before_reading_through_it() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-link");
    let message = format!("needle {}", "x".repeat(64));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    // The committed event now lives outside the state root, behind a link at `events`.
    let target = outside.path().join("events");
    fs::rename(directory.join("events"), &target).unwrap();
    if !link_directory(&target, &directory.join("events")) {
        eprintln!("skipped: this environment cannot create a directory link");
        return;
    }
    let external_event = target.join(&event_id);
    assert_eq!(
        read_json::<SessionEvent>(&external_event).unwrap().message,
        message
    );

    // The general snapshot rejects the link instead of reporting the event behind it.
    let error =
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    let search = cli_search(
        root.path(),
        &["search", "needle", "--all-workspaces", "--json"],
    );
    assert!(search["hits"].as_array().unwrap().is_empty(), "{search}");
    assert_eq!(search["scanned"]["events"], 0, "{search}");
    assert!(
        search["incomplete_reasons"]
            .to_string()
            .contains("events directory is a symlink"),
        "{search}"
    );

    // Nothing behind the link is opened: a record that could not even be read there
    // changes neither verdict.
    fs::remove_file(&external_event).unwrap();
    fs::create_dir(&external_event).unwrap();
    let error =
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    let search = cli_search(
        root.path(),
        &["search", "needle", "--all-workspaces", "--json"],
    );
    let reasons = search["incomplete_reasons"].to_string();
    assert!(
        reasons.contains("events directory is a symlink"),
        "{reasons}"
    );
    assert!(!reasons.contains("non-regular"), "{reasons}");
}

/// A snapshot hook that changes the status record once, so the first snapshot fails its
/// consistency check and is retried exactly once.
fn bump_status_once() -> impl FnMut(&Path) {
    let mut fired = false;
    move |directory: &Path| {
        if !fired {
            fired = true;
            update_status(directory, SessionState::Working, None, None).unwrap();
        }
    }
}

#[test]
fn ordinary_queries_keep_the_per_event_limit_across_a_forced_retry() {
    // A committed 40 MiB event is within the 64 MiB event limit. When the first snapshot
    // fails its consistency check and is retried, the second attempt must compare the
    // unchanged event within the same per-event limit: subtracting the first attempt's
    // read would leave 24 MiB and turn a valid published result into `recovery_required`.
    // A search's snapshot reads nothing, so its retry cannot cost the budget either.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-large");
    let message = format!("needle {}", "x".repeat(40 * 1024 * 1024));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    assert!(
        size > EVENT_READ_LIMIT / 2 && size <= EVENT_READ_LIMIT,
        "{size}"
    );
    // Reading 40 MiB twice outlasts the production retry window in a debug build; the
    // retry itself, not its timing, is under test.
    let retried = |run: &dyn Fn() -> serde_json::Value| {
        query::with_snapshot_retry_window(Duration::from_secs(60), || {
            query::with_snapshot_hook(bump_status_once(), run)
        })
    };

    let result = retried(&|| {
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap()
    });
    assert_eq!(result["request_state"], "completed");
    assert_eq!(result["event_id"], event_id);
    assert_eq!(result["result"].as_str().map(str::len), Some(message.len()));

    let latest = retried(&|| {
        cli_result(
            root.path(),
            &["result", "session-large", "--latest", "--json"],
        )
    });
    assert_eq!(latest["request_state"], "completed");
    assert_eq!(latest["event_id"], event_id);
    assert_eq!(latest["request_id"], request_id);

    // The same forced retry inside a search: the retried snapshot reads nothing, and the
    // record is read once, at its scan position, within the 64 MiB search budget it fits.
    let (search, reads) = with_publication_read_log(|| {
        retried(&|| {
            cli_search(
                root.path(),
                &["search", "needle", "--all-workspaces", "--json"],
            )
        })
    });
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = search["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{}", search["incomplete_reasons"]);
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(search["scanned"]["events"], 1);
    assert_eq!(
        search["incomplete"], false,
        "{}",
        search["incomplete_reasons"]
    );
}

#[test]
fn search_stops_at_the_event_budget_before_reading_a_journaled_event() {
    // 5,000 ordinary events use up the event budget exactly; the committed journaled
    // event that sorts after them is one more event than the budget allows, so the scan
    // must stop before it without ever opening it, exactly as it does once the journal
    // is gone.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-many");
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, "needle at the end", "needle at the end");
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let filler = serde_json::to_vec_pretty(&oversized_sample_event("filler")).unwrap();
    for index in 0..5_000 {
        let name = format!("event-0-{index:05}.json");
        assert!(
            name.as_str() < event_id.as_str(),
            "{name} sorts after {event_id}"
        );
        fs::write(directory.join("events").join(name), &filler).unwrap();
    }
    let search = || {
        cli_search(
            root.path(),
            &["search", "needle", "--all-workspaces", "--json"],
        )
    };

    let (journaled, reads) = with_publication_read_log(search);
    assert!(
        reads.is_empty(),
        "the journaled event was opened: {reads:?}"
    );
    assert!(
        journaled["hits"].as_array().unwrap().is_empty(),
        "{journaled}"
    );
    assert_eq!(journaled["scanned"]["events"], 5_000, "{journaled}");
    assert_eq!(journaled["incomplete"], true, "{journaled}");
    assert!(
        journaled["incomplete_reasons"]
            .to_string()
            .contains("scan stopped: event budget of 5000 reads exhausted"),
        "{journaled}"
    );

    // `sessions` recovers the completion and removes the journal; the same scan must
    // report the same events, the same cut-off, and the same reason.
    cli_sessions(root.path());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let published = search();
    assert_eq!(published["hits"], journaled["hits"]);
    assert_eq!(published["scanned"], journaled["scanned"]);
    assert_eq!(published["incomplete"], journaled["incomplete"]);
    assert_eq!(
        published["incomplete_reasons"],
        journaled["incomplete_reasons"]
    );
}

#[test]
fn search_reads_a_journaled_event_only_at_its_scan_position() {
    // An older published event and a later committed journaled event, with a budget that
    // fits either record but not both. The scan spends its budget in filename order: the
    // older result is read and found, and the journaled event is the record that does
    // not fit. Reading the journaled event ahead of the scan reported no hit while the
    // journal existed and the older hit as soon as `sessions` removed the journal.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-order");
    let later = format!("needle {}", "x".repeat(4000));
    let (request_id, later_path) = seed_event_written_completion_with(&directory, &later, &later);
    let later_id = later_path.file_name().unwrap().to_str().unwrap().to_owned();
    let older_id = "event-0-older.json";
    assert!(
        older_id < later_id.as_str(),
        "{older_id} sorts after {later_id}"
    );
    let mut older = read_json::<SessionEvent>(&later_path).unwrap();
    older.message = format!("needle {}", "y".repeat(3000));
    let older_path = directory.join("events").join(older_id);
    write_json_atomic(&older_path, &older).unwrap();
    let older_size = fs::metadata(&older_path).unwrap().len();
    let later_size = fs::metadata(&later_path).unwrap().len();
    let budget = (older_size + later_size - 1).to_string();
    let search = || {
        cli_search(
            root.path(),
            &[
                "search",
                "needle",
                "--all-workspaces",
                "--max-bytes",
                &budget,
                "--json",
            ],
        )
    };

    let (journaled, reads) = with_publication_read_log(search);
    let hits = journaled["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{journaled}");
    assert_eq!(hits[0]["event_id"], older_id, "{journaled}");
    assert_eq!(hits[0]["request_id"], serde_json::Value::Null);
    assert_eq!(journaled["scanned"]["events"], 1, "{journaled}");
    assert_eq!(journaled["incomplete"], true, "{journaled}");
    let reasons = journaled["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "byte budget of {budget} bytes exhausted; session-order/{later_id} is {later_size} bytes with {} bytes remaining",
            later_size - 1
        )),
        "{reasons}"
    );
    // The journaled event was never opened: at its position its size alone exceeds what
    // the budget has left, so the scan stopped without reading it.
    assert!(reads.is_empty(), "{reads:?}");

    // With budget for both records, both are hits, and the journaled event is opened
    // exactly once, for the publication comparison that is also its search read.
    let both = (older_size + later_size).to_string();
    let search_both = || {
        cli_search(
            root.path(),
            &[
                "search",
                "needle",
                "--all-workspaces",
                "--max-bytes",
                &both,
                "--json",
            ],
        )
    };
    let (complete, reads) = with_publication_read_log(search_both);
    assert_eq!(reads, std::slice::from_ref(&later_path));
    let hits = complete["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{complete}");
    assert_eq!(complete["incomplete"], false, "{complete}");
    assert_eq!(complete["scanned"]["events"], 2, "{complete}");
    assert!(
        hits.iter()
            .any(|hit| hit["event_id"] == later_id && hit["request_id"] == request_id),
        "{complete}"
    );

    // `sessions` recovers the completion and removes the journal; the committed event is
    // now an ordinary published record and both scans must report the same results.
    cli_sessions(root.path());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let (published, reads) = with_publication_read_log(search);
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(published["hits"], journaled["hits"]);
    assert_eq!(published["scanned"], journaled["scanned"]);
    assert_eq!(published["incomplete"], journaled["incomplete"]);
    assert_eq!(
        published["incomplete_reasons"],
        journaled["incomplete_reasons"]
    );
    let (published, reads) = with_publication_read_log(search_both);
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(published["hits"], complete["hits"]);
    assert_eq!(published["scanned"], complete["scanned"]);
    assert_eq!(published["incomplete"], complete["incomplete"]);
}

#[test]
fn timeline_aba_replacement_keeps_summary_and_entry_together() {
    let directory = tempfile::tempdir().unwrap();
    let (request, claimed, path) = seed_claimed_session_with_claim(directory.path());
    drop(claimed);
    let a = oversized_sample_event("A");
    write_json_atomic(&path, &a).unwrap();
    let original = fs::read(&path).unwrap();
    let mut calls = 0;
    let value = query::with_snapshot_hook(
        move |_| {
            calls += 1;
            if calls == 1 {
                let mut b = oversized_sample_event("B");
                b.error = Some("B failure".to_owned());
                write_json_atomic(&path, &b).unwrap();
            } else {
                fs::write(&path, &original).unwrap();
            }
        },
        || {
            query::timeline_value(
                &Reader::open_unchecked(directory.path()),
                "session-query",
                Some(&request),
            )
        },
    )
    .unwrap();
    let completion = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stage"] == "completion")
        .unwrap();
    assert_eq!(value["requests"][0]["error"], completion["detail"]["error"]);
}

#[test]
fn published_count_is_lock_free_and_keeps_rows_when_evidence_changes() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-count");
    seed_event_written_completion_with(&directory, "result", "result");
    let reader = Reader::open_unchecked(&directory);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join(TURN_CLAIM_LOCK_FILE))
        .unwrap();
    lock.lock().unwrap();
    assert_eq!(reader.published_result_count().unwrap(), 1);
    // Broken status is irrelevant to both counting and ownership discovery.
    fs::write(directory.join(STATUS_FILE), "broken").unwrap();
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!()
    };
    let rows = sessions_in_read_only(root.path(), &request).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "unknown");
    assert_eq!(rows[0]["results"], 1);
    let rows = session::snapshot::with_snapshot_hook(
        |directory| {
            fs::write(directory.join(TURN_CLAIM_FILE), "changed-claim").unwrap();
        },
        || sessions_in_read_only(root.path(), &request).unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["results"], 0);
    assert_eq!(rows[0]["id"], "session-count");
}

#[test]
fn published_count_uses_claim_receipts_without_decoding_ordinary_events() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-count");
    let (_, event_path) = seed_event_written_completion_with(&directory, "result", "result");
    fs::remove_file(directory.join(TURN_COMPLETION_FILE)).unwrap();
    let reader = Reader::open_unchecked(&directory);
    assert_eq!(reader.published_result_count().unwrap(), 0);
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    fs::write(event_path, "not json").unwrap();
    assert_eq!(reader.published_result_count().unwrap(), 1);
    fs::write(directory.join("events/event-1-legacy.json"), "legacy").unwrap();
    fs::write(
        directory.join("events/unpublished-event-2.json"),
        "set aside",
    )
    .unwrap();
    assert_eq!(reader.published_result_count().unwrap(), 2);
    let error = session::snapshot::with_snapshot_hook(
        |directory| {
            fs::write(directory.join(TURN_COMPLETION_FILE), "changed").unwrap();
        },
        || reader.published_result_count(),
    )
    .unwrap_err();
    assert!(error.is::<session::SnapshotBusy>());
}

#[test]
fn enumeration_faults_keep_search_and_listing_error_policies() {
    use session::enumerate::{TestEntry, with_entries};
    let root = tempfile::tempdir().unwrap();
    let NativeCommand::Search(search) =
        parse_args(["search", "needle", "--all-workspaces", "--json"]).unwrap()
    else {
        panic!()
    };
    let NativeCommand::Sessions(listing) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!()
    };
    let kind = |id: &str| {
        vec![Ok(TestEntry {
            id: id.into(),
            directory: root.path().join(id),
            directory_kind: Err(std::io::Error::other("kind fault")),
        })]
    };
    let value = with_entries(kind("session-z"), || {
        query::search_value_in(root.path(), &search)
    })
    .unwrap();
    assert_eq!(
        value["incomplete_reasons"],
        serde_json::json!([{"session": null, "reason": "session entry session-z: failed to read its file type: kind fault"}])
    );
    let value = with_entries(kind("invalid"), || {
        query::search_value_in(root.path(), &search)
    })
    .unwrap();
    assert_eq!(value["incomplete"], false);
    for id in ["invalid", "session-z"] {
        let error =
            with_entries(kind(id), || sessions_in_read_only(root.path(), &listing)).unwrap_err();
        assert_eq!(error.to_string(), "kind fault");
        let error = with_entries(kind(id), || prune_closed_sessions(root.path(), 0)).unwrap_err();
        assert_eq!(error.to_string(), "kind fault");
    }
    let error = with_entries(vec![Err(std::io::Error::other("entry fault"))], || {
        query::search_value_in(root.path(), &search)
    })
    .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "failed to read the native state root: entry fault"
    );
}

#[test]
fn published_count_rejects_a_receipt_changed_during_the_read() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-count");
    seed_event_written_completion_with(&directory, "result", "result");
    let reader = Reader::open_unchecked(&directory);
    let receipt = requests::list(&reader).unwrap().receipts.remove(0);
    let error = session::snapshot::with_snapshot_hook(
        move |directory| {
            fs::remove_file(
                directory
                    .join("requests")
                    .join(format!("{}.json", receipt.claim_token)),
            )
            .unwrap();
        },
        || reader.published_result_count(),
    )
    .unwrap_err();
    assert!(error.is::<session::SnapshotBusy>());
}
