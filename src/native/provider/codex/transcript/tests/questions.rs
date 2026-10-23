use super::super::super::questions::tests::with_write_fault;
use super::*;
use crate::native::query;
use crate::native::session::{CoreRecord, requests};

const SESSION: &str = "session-question";

fn question_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    PendingCodexTurn,
    Value,
    Observer,
) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(SESSION);
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    let store = Store::open_unchecked(&directory);
    store
        .record(CoreRecord::Manifest)
        .write_json(&json!({
            "schema":1,"id":SESSION,"provider":"codex","provider_path":"codex",
            "provider_version":"codex-cli 0.163.0","workspace":root.path(),
            "title":"question fixture","model":null,"effort":null,"yolo":true,"created_unix_ms":1
        }))
        .unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&store, &[]).unwrap();
    let pending = install_pending_turn(&directory, claim.token()).unwrap();
    claim.retain();
    let payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":THREAD,
        "turn_id":"turn-1", "transcript_path":directory.join("rollout.jsonl"),
        "prompt":correlated_prompt(&crate::native::native_delegation_prompt("external", "test"), &pending)});
    ADAPTER.handle_hook(&directory, &payload).unwrap();
    append(&directory.join("rollout.jsonl"), &prefix(&payload));
    (root, directory, pending, payload, Observer::default())
}

fn question_item(id: &str, questions: Value) -> Value {
    json!({"type":"item_completed","thread_id":THREAD,"turn_id":"turn-1",
        "item":{"type":"AgentMessage","id":id,"delivery":"async","phase":"final_answer",
            "content":[{"type":"Text","text":"not projected"}],"questions":questions}})
}

fn request(directory: &Path, pending: &PendingCodexTurn) -> requests::Receipt {
    requests::for_claim(&Reader::open_unchecked(directory), &pending.claim_token)
        .unwrap()
        .unwrap()
}

#[test]
fn async_question_is_visible_through_public_queries_before_output_or_terminal() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let questions = json!([
        {"title":"Which report format? 한국어😀", "options":[" Text ","Table"]},
        {"title":"Report heading?", "options":null}
    ]);
    append(
        &directory.join("rollout.jsonl"),
        &line(
            "event_msg",
            question_item("call_example", questions.clone()),
        ),
    );
    observer.poll(&directory, false);
    assert_pending(&directory, &pending);
    let receipt = request(&directory, &pending);
    let result = read_only(root.path(), || {
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap()
    });
    assert_eq!(result["request_state"], "pending");
    assert_eq!(result["result"], Value::Null);
    assert_eq!(
        result["question_emissions"][0]["questions"][0]["title"],
        questions[0]["title"]
    );
    assert_eq!(
        result["question_emissions"],
        json!([{"id":"call_example","questions":questions}])
    );
    let status = read_only(root.path(), || query::status::test_value_in(root.path()));
    assert_eq!(
        status["sessions"][0]["active_request"]["request_id"],
        receipt.request_id
    );
    assert!(
        status["sessions"][0]["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    let inspect = read_only(root.path(), || {
        query::inspect_value(&Reader::open_unchecked(&directory), SESSION).unwrap()
    });
    assert_eq!(inspect["requests"][0]["question_emitted"], true);
    assert_pending(&directory, &pending);
}

fn emit(directory: &Path, item: Value) {
    append(&directory.join("rollout.jsonl"), &line("event_msg", item));
}

fn simple_item(id: &str) -> Value {
    question_item(id, json!([{"title":"Which baseline?","options":null}]))
}

#[test]
fn question_wait_returns_before_completion_without_retiring_the_request() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("call_first"));
    observer.poll(&directory, false);
    let receipt = request(&directory, &pending);
    let address = format!("{SESSION}/{}", receipt.request_id);
    let observed = read_only(root.path(), || {
        query::wait::test_value_in(
            root.path(),
            &[&address, "--questions", "--json"],
            Duration::ZERO,
        )
        .unwrap()
    });
    assert_eq!(observed["ok"], true);
    assert_eq!(observed["ended"], Value::Null);
    assert_eq!(observed["timed_out"], false);
    assert_eq!(observed["remaining"], json!([address]));
    assert_eq!(observed["question"]["request_state"], "pending");
    assert_eq!(observed["question"]["address"], address);
    assert_eq!(
        observed["question"]["question_emissions"][0]["id"],
        "call_first"
    );
    assert_eq!(
        observed["question"]["after_question"],
        format!("{address}/call_first")
    );
    assert_pending(&directory, &pending);
}

fn question_wait(root: &Path, address: &str, after: Option<&str>) -> Result<Value> {
    let mut args = vec![address, "--questions"];
    if let Some(after) = after {
        args.extend(["--after-question", after]);
    }
    read_only(root, || {
        query::wait::test_value_in(root, &args, Duration::ZERO)
    })
}

#[test]
fn question_wait_cursor_skips_seen_items_and_delivers_the_next_retained_prefix() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let address = format!("{SESSION}/{}", request(&directory, &pending).request_id);
    let ordinary_before = read_only(root.path(), || {
        query::wait::test_value_in(root.path(), &[&address], Duration::ZERO).unwrap()
    });
    emit(&directory, simple_item("call/first"));
    observer.poll(&directory, false);
    let first = question_wait(root.path(), &address, None).unwrap();
    let cursor = first["question"]["after_question"].as_str().unwrap();
    assert_eq!(cursor, format!("{address}/call/first"));
    assert_eq!(question_wait(root.path(), &address, None).unwrap(), first);
    assert_eq!(
        question_wait(root.path(), &address, Some(cursor)).unwrap()["timed_out"],
        true
    );
    let ordinary_after = read_only(root.path(), || {
        query::wait::test_value_in(root.path(), &[&address], Duration::ZERO).unwrap()
    });
    assert_eq!(ordinary_after, ordinary_before);
    emit(&directory, simple_item("second"));
    emit(&directory, simple_item("third"));
    observer.poll(&directory, false);
    let next = question_wait(root.path(), &address, Some(cursor)).unwrap();
    let items = next["question"]["question_emissions"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "second");
    assert_eq!(items[1]["id"], "third");
    assert_eq!(
        next["question"]["after_question"],
        format!("{address}/third")
    );
    let unknown = question_wait(root.path(), &address, Some(&format!("{address}/unknown")));
    assert!(format!("{:#}", unknown.unwrap_err()).contains("readable retained evidence"));
    assert_pending(&directory, &pending);
}

#[test]
fn question_wait_keeps_incompleteness_and_never_wakes_on_missing_or_unreadable_evidence() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let address = format!("{SESSION}/{}", request(&directory, &pending).request_id);
    assert_eq!(
        question_wait(root.path(), &address, None).unwrap()["timed_out"],
        true
    );
    emit(&directory, simple_item("first"));
    emit(&directory, question_item("invalid", json!([{"title":""}])));
    observer.poll(&directory, false);
    let value = question_wait(root.path(), &address, None).unwrap();
    assert_eq!(value["question"]["question_observation"], "incomplete");
    let cursor = value["question"]["after_question"].as_str().unwrap();
    std::fs::write(question_path(&directory, &pending), b"invalid").unwrap();
    assert_eq!(
        question_wait(root.path(), &address, None).unwrap()["timed_out"],
        true
    );
    assert!(question_wait(root.path(), &address, Some(cursor)).is_err());
    std::fs::remove_file(question_path(&directory, &pending)).unwrap();
    assert_eq!(
        question_wait(root.path(), &address, None).unwrap()["timed_out"],
        true
    );
    assert!(question_wait(root.path(), &address, Some(cursor)).is_err());
    assert_pending(&directory, &pending);
}

#[test]
fn question_wait_completion_wins_for_the_same_request_and_retains_unseen_questions() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let address = format!("{SESSION}/{}", request(&directory, &pending).request_id);
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let first = question_wait(root.path(), &address, None).unwrap();
    let cursor = first["question"]["after_question"].as_str().unwrap();
    emit(&directory, simple_item("last"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, true);
    let completed = question_wait(root.path(), &address, Some(cursor)).unwrap();
    assert_eq!(completed["ok"], true);
    assert!(completed.get("question").is_none());
    assert_eq!(completed["remaining"], json!([]));
    assert_eq!(completed["ended"]["request_state"], "completed");
    assert_eq!(completed["ended"]["result"], "ordinary answer");
    assert_eq!(
        completed["ended"]["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(completed["ended"]["question_emissions"][0]["id"], "last");
    assert_eq!(
        completed["ended"]["after_question"],
        format!("{address}/last")
    );
    update_status(&directory, SessionState::Closed, None, None).unwrap();
    let closed = question_wait(root.path(), &address, None).unwrap();
    assert!(closed.get("question").is_none());
    assert_eq!(
        closed["ended"]["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn question_wait_does_not_turn_an_ended_unresolved_request_into_attention() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let address = format!("{SESSION}/{}", request(&directory, &pending).request_id);
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    update_status(&directory, SessionState::Closed, None, None).unwrap();
    let value = question_wait(root.path(), &address, None).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value.get("question").is_none());
    assert_eq!(value["ended"]["request_state"], "unresolved");
    assert_eq!(value["ended"]["question_emissions"][0]["id"], "first");
}

#[test]
fn question_wait_orders_sessions_by_input_and_validates_every_readable_address() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let first_address = format!("{SESSION}/{}", request(&directory, &pending).request_id);
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let (_other_root, other_directory, other_pending, _, mut other_observer) = question_fixture();
    emit(&other_directory, simple_item("first"));
    append(
        &other_directory.join("rollout.jsonl"),
        &complete(json!("second answer")),
    );
    other_observer.poll(&other_directory, true);
    drop(other_observer);
    let other_receipt = request(&other_directory, &other_pending);
    let other_address = format!("session-other/{}", other_receipt.request_id);
    let target = root.path().join("session-other");
    std::fs::rename(&other_directory, &target).unwrap();
    let manifest = target.join("manifest.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["id"] = json!("session-other");
    crate::native::session::primitives::write_json_atomic(&manifest, &value).unwrap();
    let wait = |addresses: &[&str]| {
        read_only(root.path(), || {
            query::wait::test_value_in(root.path(), addresses, Duration::ZERO)
        })
    };
    let first = wait(&[&first_address, &other_address, "--questions"]).unwrap();
    assert_eq!(first["question"]["address"], first_address);
    assert_eq!(first["remaining"], json!([first_address, other_address]));
    let second = wait(&[&other_address, &first_address, "--questions"]).unwrap();
    assert_eq!(second["ended"]["address"], other_address);
    assert_eq!(second["remaining"], json!([first_address]));
    let cursor = format!("{other_address}/first");
    let scoped = wait(&[
        &first_address,
        &other_address,
        "--questions",
        "--after-question",
        &cursor,
    ])
    .unwrap();
    assert_eq!(scoped["question"]["address"], first_address);
    assert_eq!(scoped["question"]["question_emissions"][0]["id"], "first");
    std::fs::remove_file(
        target
            .join("requests")
            .join(format!("{}.json", other_receipt.claim_token)),
    )
    .unwrap();
    let error = wait(&[&first_address, &other_address, "--questions"]).unwrap_err();
    assert!(format!("{error:#}").contains("no such Bridge request"));
    assert_pending(&directory, &pending);
}

fn question_path(directory: &Path, pending: &PendingCodexTurn) -> std::path::PathBuf {
    directory.join(format!("codex-questions-{}.json", pending.claim_token))
}

fn tree(directory: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_symlink() {
            entries.push((
                entry.path(),
                Some(
                    std::fs::read_link(entry.path())
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                ),
            ));
        } else if entry.file_type().unwrap().is_dir() {
            entries.push((entry.path(), None));
            entries.extend(tree(&entry.path()));
        } else {
            entries.push((entry.path(), Some(std::fs::read(entry.path()).unwrap())));
        }
    }
    entries.sort();
    entries
}

fn read_only<T>(root: &Path, read: impl FnOnce() -> T) -> T {
    let before = tree(root);
    let value = read();
    assert_eq!(
        tree(root),
        before,
        "queries must leave every record unchanged"
    );
    value
}

fn result(directory: &Path, pending: &PendingCodexTurn) -> Value {
    read_only(directory, || {
        query::request_result(
            &Reader::open_unchecked(directory),
            &request(directory, pending).request_id,
        )
        .unwrap()
    })
}

fn cli_result(root: &Path, args: &[&str]) -> Value {
    let crate::native::NativeCommand::Result(request) =
        query::parse_result(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>()).unwrap()
    else {
        unreachable!()
    };
    read_only(root, || query::result_value_in(root, &request).unwrap())
}

fn status(root: &Path) -> Value {
    read_only(root, || query::status::test_value_in(root))["sessions"][0].clone()
}

fn inspect(directory: &Path) -> Value {
    read_only(directory, || {
        query::inspect_value(&Reader::open_unchecked(directory), SESSION).unwrap()
    })
}

#[test]
fn emissions_survive_completion_rollout_removal_next_request_and_close() {
    let (root, directory, pending, mut payload, mut observer) = question_fixture();
    emit(&directory, simple_item("call_first"));
    emit(&directory, simple_item("call_second"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    emit(&directory, simple_item("call_after_terminal"));
    observer.poll(&directory, true);
    let completed = result(&directory, &pending);
    assert_eq!(completed["request_state"], "completed");
    assert_eq!(completed["result"], "ordinary answer");
    assert_eq!(completed["question_emissions"].as_array().unwrap().len(), 2);
    assert_eq!(completed["provider_session_id"], THREAD);
    assert_eq!(completed["turn_id"], "turn-1");
    assert_eq!(completed["usage"], Value::Null);
    assert_eq!(completed["usage_reason"], "not_reported");
    let event = events(&directory).pop().unwrap();
    assert_eq!(event["provenance"], COMPLETE_SOURCE);
    assert_eq!(event["message"], "ordinary answer");
    assert!(event.get("usage").is_none());
    assert!(event.get("question_emissions").is_none());
    assert!(
        turn::current_claim_token(&Reader::open_unchecked(&directory))
            .unwrap()
            .is_none()
    );
    let old_receipt = request(&directory, &pending);
    assert_eq!(
        cli_result(root.path(), &[SESSION, "--latest", "--json"]),
        completed
    );
    assert_eq!(
        cli_result(
            root.path(),
            &[SESSION, "--event", &old_receipt.event_file, "--json"]
        ),
        completed
    );
    assert!(
        status(root.path())["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    std::fs::remove_file(directory.join("rollout.jsonl")).unwrap();
    std::fs::remove_file(directory.join(format!("codex-turn-{}.json", pending.claim_token)))
        .unwrap();
    assert_eq!(result(&directory, &pending), completed);
    let next = next_claim(&directory, &mut payload);
    ADAPTER.handle_hook(&directory, &payload).unwrap();
    append(&directory.join("rollout.jsonl"), &prefix(&payload));
    observer.poll(&directory, false);
    assert!(
        result(&directory, &next)
            .get("question_emissions")
            .is_none()
    );
    let focused = status(root.path());
    assert_eq!(
        focused["active_request"]["request_id"],
        request(&directory, &next).request_id
    );
    assert!(
        !focused["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    assert_eq!(
        result(&directory, &pending)["question_emissions"],
        completed["question_emissions"]
    );
    append(
        &directory.join("rollout.jsonl"),
        &terminal_for(&payload, json!("known fact received")),
    );
    observer.poll(&directory, true);
    assert!(
        !status(root.path())["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    update_status(&directory, SessionState::Closed, None, None).unwrap();
    assert_eq!(
        result(&directory, &pending)["question_emissions"],
        completed["question_emissions"]
    );
    let historical = inspect(&directory);
    assert!(
        historical["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["request_id"] == old_receipt.request_id && r["question_emitted"] == true)
    );
    assert!(
        !status(root.path())["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
}

#[test]
fn question_after_an_oversized_tool_output_is_recorded() {
    let (_root, directory, pending, _, mut observer) = question_fixture();
    append(
        &directory.join("rollout.jsonl"),
        &line(
            "response_item",
            json!({"type":"custom_tool_call_output","call_id":"large","output":"x".repeat(LINE_BYTES + 1)}),
        ),
    );
    emit(&directory, simple_item("after_overflow"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, true);
    let observed = result(&directory, &pending);
    assert_eq!(observed["request_state"], "completed");
    assert_eq!(
        observed["question_emissions"],
        json!([{"id":"after_overflow","questions":[{"title":"Which baseline?","options":null}]}])
    );
}
#[test]
fn acceptance_is_required_without_buffering_and_optional_attribution_cannot_poison_replay() {
    for accepted_later in [false, true] {
        let (root, directory, pending, payload, mut observer) = question_fixture();
        std::fs::write(
            directory.join("rollout.jsonl"),
            line("session_meta", json!({"id":THREAD})),
        )
        .unwrap();
        emit(&directory, simple_item("too_early"));
        observer.poll(&directory, false);
        assert!(!question_path(&directory, &pending).exists());
        assert!(
            result(&directory, &pending)
                .get("question_observation")
                .is_none()
        );
        if accepted_later {
            append(&directory.join("rollout.jsonl"), &prefix(&payload));
            emit(&directory, simple_item("accepted"));
        }
        append(
            &directory.join("rollout.jsonl"),
            &complete(json!("ordinary answer")),
        );
        observer.poll(&directory, true);
        let observed = result(&directory, &pending);
        if accepted_later {
            assert_eq!(observed["request_state"], "completed");
            assert_eq!(observed["question_observation"], "incomplete");
            assert_eq!(observed["question_emissions"].as_array().unwrap().len(), 1);
            assert_eq!(observed["question_emissions"][0]["id"], "accepted");
            assert_eq!(status(root.path())["question_observation"], "incomplete");
        } else {
            assert_eq!(observed["request_state"], "failed");
            assert_eq!(
                observed["error"],
                "Codex did not accept the delegated prompt"
            );
            assert!(!question_path(&directory, &pending).exists());
        }
    }
    let (_root, directory, pending, _, mut observer) = question_fixture();
    for turn in [Value::Null, json!(42), json!("old-turn")] {
        let mut item = simple_item("ignored");
        item["turn_id"] = turn;
        item["thread_id"] = json!({"malformed":true});
        item["item"]["questions"] = json!([{"title":0}]);
        emit(&directory, item);
    }
    let mut item = simple_item("without_optional_thread");
    item.as_object_mut().unwrap().remove("thread_id");
    emit(&directory, item);
    observer.poll(&directory, false);
    let observed = result(&directory, &pending);
    assert_eq!(
        observed["question_emissions"][0]["id"],
        "without_optional_thread"
    );
    assert!(observed.get("question_observation").is_none());
}

#[test]
fn duplicate_ids_are_noops_and_conflicts_preserve_the_first_valid_item() {
    let (_root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let before = std::fs::read(question_path(&directory, &pending)).unwrap();
    emit(&directory, simple_item("first"));
    let (_, syncs) =
        crate::native::session::primitives::with_sync_log(|| observer.poll(&directory, false));
    assert!(syncs.is_empty());
    assert_eq!(
        std::fs::read(question_path(&directory, &pending)).unwrap(),
        before
    );
    emit(
        &directory,
        question_item("first", json!([{"title":"Conflicting question"}])),
    );
    emit(&directory, simple_item("second"));
    observer.poll(&directory, false);
    let observed = result(&directory, &pending);
    assert_eq!(
        observed["question_emissions"][0]["questions"][0]["title"],
        "Which baseline?"
    );
    assert_eq!(observed["question_emissions"][1]["id"], "second");
    assert_eq!(observed["question_observation"], "incomplete");
    let stored: Value =
        serde_json::from_slice(&std::fs::read(question_path(&directory, &pending)).unwrap())
            .unwrap();
    assert_eq!(
        stored
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["claim_token", "incomplete", "items", "request_id", "schema"]
    );
    assert_eq!(stored["items"][0].as_object().unwrap().len(), 2);
}

#[test]
fn invalid_optional_items_do_not_change_ordinary_completion() {
    for variant in [
        "id",
        "questions",
        "empty_questions",
        "title",
        "blank_title",
        "options",
        "empty_options",
        "blank_option",
        "option_type",
        "thread",
        "thread_type",
    ] {
        let (root, directory, pending, _, mut observer) = question_fixture();
        let mut item = simple_item("bad");
        match variant {
            "id" => item["item"]["id"] = json!(""),
            "questions" => item["item"]["questions"] = json!({}),
            "empty_questions" => item["item"]["questions"] = json!([]),
            "title" => item["item"]["questions"][0]["title"] = json!(42),
            "blank_title" => item["item"]["questions"][0]["title"] = json!(" \n\t"),
            "options" => item["item"]["questions"][0]["options"] = json!({}),
            "empty_options" => item["item"]["questions"][0]["options"] = json!([]),
            "blank_option" => item["item"]["questions"][0]["options"] = json!(["valid", " \n"]),
            "option_type" => item["item"]["questions"][0]["options"] = json!([0]),
            "thread" => item["thread_id"] = json!("another-thread"),
            "thread_type" => item["thread_id"] = json!(3),
            _ => unreachable!(),
        }
        emit(&directory, item);
        emit(&directory, simple_item("good"));
        append(
            &directory.join("rollout.jsonl"),
            &complete(json!("ordinary answer")),
        );
        observer.poll(&directory, true);
        let observed = result(&directory, &pending);
        assert_eq!(observed["request_state"], "completed", "{variant}");
        assert_eq!(observed["result"], "ordinary answer");
        assert_eq!(observed["question_observation"], "incomplete");
        assert_eq!(observed["question_emissions"].as_array().unwrap().len(), 1);
        assert_eq!(observed["session_error"], Value::Null);
        assert_eq!(
            inspect(&directory)["requests"][0]["question_observation"],
            "incomplete"
        );
        assert_eq!(status(root.path())["question_observation"], "incomplete");
    }
}

#[test]
fn budget_charges_the_full_pretty_record_and_keeps_only_a_whole_prefix() {
    for unit in ["x", "😀\n\""] {
        for extra in [0, 1] {
            let (_root, directory, pending, _, mut observer) = question_fixture();
            let receipt = request(&directory, &pending);
            let mut expected = json!({"schema":1,"claim_token":pending.claim_token,"request_id":receipt.request_id,
            "items":[{"id":"near_limit","questions":[{"title":"","options":null}]}],"incomplete":false});
            let overhead = serde_json::to_vec_pretty(&expected).unwrap().len();
            let capacity = 64 * 1024 - overhead + extra;
            let unit_bytes = serde_json::to_vec(unit).unwrap().len() - 2;
            let title = format!(
                "{}{}",
                unit.repeat(capacity / unit_bytes),
                "x".repeat(capacity % unit_bytes)
            );
            expected["items"][0]["questions"][0]["title"] = json!(title);
            emit(
                &directory,
                question_item("near_limit", expected["items"][0]["questions"].clone()),
            );
            observer.poll(&directory, false);
            let path = question_path(&directory, &pending);
            if extra == 0 {
                assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 * 1024);
                assert_eq!(
                    result(&directory, &pending)["question_emissions"][0]["questions"][0]["title"],
                    title
                );
            } else {
                assert!(
                    result(&directory, &pending)
                        .get("question_emissions")
                        .is_none()
                );
                assert_eq!(
                    result(&directory, &pending)["question_observation"],
                    "incomplete"
                );
            }
            emit(&directory, simple_item("later"));
            observer.poll(&directory, false);
            let observed = result(&directory, &pending);
            assert_eq!(observed["question_observation"], "incomplete");
            assert_eq!(
                observed["question_emissions"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0),
                usize::from(extra == 0)
            );
            assert!(std::fs::metadata(&path).unwrap().len() <= 64 * 1024);
        }
    }
}

#[test]
fn flush_rechecks_claim_state_pending_and_binding_under_the_existing_lock() {
    for change in ["claim", "closed", "pending", "binding"] {
        let (_root, directory, pending, _, mut observer) = question_fixture();
        emit(&directory, simple_item("old"));
        let mutation = directory.clone();
        during_read(
            move |point| {
                if !matches!(point, ReadCheckpoint::BeforeQuestionLock) {
                    return;
                }
                let store = Store::open_unchecked(&mutation);
                let _lock = if change == "claim" {
                    None
                } else {
                    Some(store.lock().unwrap())
                };
                match change {
                    "claim" => {
                        store.record(CoreRecord::TurnClaim).remove_raw().unwrap();
                        let claim = turn::claim(&store, &[]).unwrap();
                        install_pending_turn(&mutation, claim.token()).unwrap();
                        claim.retain();
                    }
                    "closed" => update_status(&mutation, SessionState::Closed, None, None).unwrap(),
                    "pending" => {
                        install_pending_turn(&mutation, "1-2-3").unwrap();
                    }
                    "binding" => {
                        let token = turn::current_claim_token(&store).unwrap().unwrap();
                        let name = format!("codex-turn-{token}.json");
                        let mut binding: Value =
                            store.private(&name).optional_json().unwrap().unwrap();
                        binding["turn_id"] = json!("changed");
                        store.private(&name).write_json(&binding).unwrap();
                    }
                    _ => unreachable!(),
                }
            },
            || observer.poll(&directory, false),
        );
        assert!(!question_path(&directory, &pending).exists(), "{change}");
        assert!(events(&directory).is_empty());
        if change == "claim" {
            let reader = Reader::open_unchecked(&directory);
            let token = turn::current_claim_token(&reader).unwrap().unwrap();
            assert_ne!(token, pending.claim_token);
            assert!(requests::for_claim(&reader, &token).unwrap().is_some());
        }
    }
}

#[test]
fn replay_preserves_stored_suffix_and_monotone_incomplete_even_after_rename_error() {
    for fault in [false, true] {
        let (_root, directory, pending, _, mut observer) = question_fixture();
        emit(&directory, simple_item("first"));
        emit(
            &directory,
            question_item("first", json!([{"title":"conflict"}])),
        );
        emit(&directory, simple_item("second"));
        if fault {
            with_write_fault(3, || observer.poll(&directory, false));
        } else {
            observer.poll(&directory, false);
        }
        let before = std::fs::read(question_path(&directory, &pending)).unwrap();
        assert_eq!(
            result(&directory, &pending)["question_observation"],
            "incomplete"
        );
        let mut replay = Observer::default();
        let path = directory.join("rollout.jsonl");
        // A chunk boundary makes replay memory a strict prefix of the stored collection.
        let all: Vec<u8> = std::fs::read(&path)
            .unwrap()
            .split_inclusive(|b| *b == b'\n')
            .filter(|line| !std::str::from_utf8(line).unwrap().contains("conflict"))
            .flatten()
            .copied()
            .collect();
        let first_end = all
            .split_inclusive(|b| *b == b'\n')
            .take(3)
            .map(<[u8]>::len)
            .sum::<usize>();
        let padding = line(
            "unknown",
            json!({"padding":"x".repeat(CHUNK_BYTES - first_end)}),
        );
        let mut replay_bytes = all[..first_end].to_vec();
        replay_bytes.extend(padding);
        replay_bytes.extend_from_slice(&all[first_end..]);
        std::fs::write(&path, &replay_bytes).unwrap();
        replay.poll(&directory, false);
        assert_eq!(
            std::fs::read(question_path(&directory, &pending)).unwrap(),
            before
        );
        replay.poll(&directory, false);
        emit(&directory, simple_item("third"));
        replay.poll(&directory, false);
        let observed = result(&directory, &pending);
        assert_eq!(observed["question_emissions"].as_array().unwrap().len(), 3);
        assert_eq!(observed["question_observation"], "incomplete");
        assert_eq!(observed["question_emissions"][0]["id"], "first");
        assert_eq!(observed["question_emissions"][1]["id"], "second");
    }
}

#[test]
fn auxiliary_write_faults_retry_on_later_polls_and_never_block_terminal_settlement() {
    for budget in [0, 2, 3] {
        for terminal in [false, true] {
            let (_root, directory, pending, _, mut observer) = question_fixture();
            observer.poll(&directory, false);
            emit(&directory, simple_item("first"));
            if terminal {
                append(
                    &directory.join("rollout.jsonl"),
                    &complete(json!("ordinary answer")),
                );
            }
            with_write_fault(budget, || observer.poll(&directory, false));
            let path = question_path(&directory, &pending);
            assert_eq!(path.exists(), budget == 3);
            if terminal {
                assert_eq!(result(&directory, &pending)["result"], "ordinary answer");
                let before = tree(&directory);
                observer.poll(&directory, false);
                assert_eq!(tree(&directory), before, "no backfill after release");
            } else {
                assert_pending(&directory, &pending);
                observer.poll(&directory, false);
                assert_eq!(
                    result(&directory, &pending)["question_emissions"][0]["id"],
                    "first"
                );
                append(
                    &directory.join("rollout.jsonl"),
                    &complete(json!("ordinary answer")),
                );
                observer.poll(&directory, false);
                assert_eq!(result(&directory, &pending)["result"], "ordinary answer");
            }
        }
    }
}

#[test]
fn unreadable_records_are_optional_and_queries_never_repair_them() {
    for variant in [
        "json",
        "utf8",
        "request",
        "claim",
        "schema",
        "item",
        "duplicate",
        "oversized",
        "directory",
    ] {
        let (root, directory, pending, _, mut observer) = question_fixture();
        emit(&directory, simple_item("first"));
        observer.poll(&directory, false);
        let path = question_path(&directory, &pending);
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        match variant {
            "request" => record["request_id"] = json!("request-unknown"),
            "claim" => record["claim_token"] = json!("1-2-3"),
            "schema" => record["schema"] = json!(2),
            "item" => record["items"][0]["questions"][0]["options"] = json!([]),
            "duplicate" => {
                let duplicate = record["items"][0].clone();
                record["items"].as_array_mut().unwrap().push(duplicate);
            }
            _ => (),
        }
        let bytes = match variant {
            "json" => b"{".to_vec(),
            "utf8" => vec![0xff],
            "oversized" => vec![b' '; 64 * 1024 + 1],
            _ => serde_json::to_vec_pretty(&record).unwrap(),
        };
        if variant == "directory" {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, bytes).unwrap();
        }
        let original = tree(&directory)
            .into_iter()
            .find(|(p, _)| p == &path)
            .unwrap();
        emit(&directory, simple_item("second"));
        append(
            &directory.join("rollout.jsonl"),
            &complete(json!("ordinary answer")),
        );
        observer.poll(&directory, true);
        assert_eq!(
            tree(&directory)
                .into_iter()
                .find(|(p, _)| p == &path)
                .unwrap(),
            original
        );
        let observed = result(&directory, &pending);
        assert_eq!(observed["request_state"], "completed", "{variant}");
        assert_eq!(observed["result"], "ordinary answer");
        assert_eq!(observed["error"], Value::Null);
        assert_eq!(observed["session_error"], Value::Null);
        assert!(observed.get("question_emissions").is_none());
        assert_eq!(observed["question_observation"], "unreadable");
        assert_eq!(
            inspect(&directory)["requests"][0]["question_observation"],
            "unreadable"
        );
        let status = status(root.path());
        assert_eq!(status["question_observation"], "unreadable");
        assert!(
            status["attention"]
                .as_array()
                .unwrap()
                .contains(&json!("records_partially_unreadable"))
        );
        assert!(
            !status["attention"]
                .as_array()
                .unwrap()
                .contains(&json!("question_emitted"))
        );
    }
}

#[test]
fn divergent_stored_prefix_is_never_overwritten_or_added_to() {
    let (_root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let path = question_path(&directory, &pending);
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["items"][0]["questions"][0]["title"] = json!("different recorded question");
    Store::open_unchecked(&directory)
        .private(path.file_name().unwrap().to_str().unwrap())
        .write_json(&record)
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    emit(&directory, simple_item("second"));
    observer.poll(&directory, false);
    emit(&directory, simple_item("third"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, false);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(result(&directory, &pending)["result"], "ordinary answer");
}

#[test]
fn optional_question_flush_cannot_suspend_an_already_verified_terminal() {
    for change in ["truncate", "replace"] {
        for drain in [false, true] {
            let (root, directory, pending, _, mut observer) = question_fixture();
            emit(&directory, simple_item("terminal_question"));
            let path = directory.join("rollout.jsonl");
            append(&path, &complete(json!("verified answer")));
            assert_pending(&directory, &pending);
            assert!(!question_path(&directory, &pending).exists());
            let bytes = std::fs::read(&path).unwrap();
            let mutated = std::rc::Rc::new(std::cell::Cell::new(false));
            let checkpoint_mutated = mutated.clone();
            during_read(
                move |point| {
                    if !matches!(point, ReadCheckpoint::AfterTerminalVerify) {
                        return;
                    }
                    assert!(!checkpoint_mutated.replace(true));
                    if change == "truncate" {
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .unwrap()
                            .set_len(0)
                            .unwrap();
                    } else {
                        std::fs::rename(&path, path.with_extension("old")).unwrap();
                        std::fs::write(&path, &bytes).unwrap();
                    }
                },
                || observer.poll(&directory, drain),
            );
            assert!(
                mutated.get(),
                "successful terminal checkpoint must be reached"
            );
            let completed = result(&directory, &pending);
            assert_eq!(
                completed["request_state"], "completed",
                "{change}, drain={drain}"
            );
            assert_eq!(completed["result"], "verified answer");
            assert_eq!(completed["error"], Value::Null);
            assert_eq!(
                completed["question_emissions"][0]["id"],
                "terminal_question"
            );
            assert_eq!(events(&directory)[0]["provenance"], COMPLETE_SOURCE);
            assert_eq!(diagnostic(&directory, &pending)["state"], "settled");
            assert!(observer.cursor.is_none());
            assert!(
                turn::current_claim_token(&Reader::open_unchecked(&directory))
                    .unwrap()
                    .is_none()
            );
            assert!(
                status(root.path())["attention"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("question_emitted"))
            );
            assert_eq!(inspect(&directory)["requests"][0]["question_emitted"], true);
        }
    }
}

#[test]
fn strict_transcript_checks_before_optional_publication_preserve_prior_evidence() {
    for change in ["truncate", "replace", "json"] {
        let (_root, directory, pending, _, mut observer) = question_fixture();
        emit(&directory, simple_item("first"));
        observer.poll(&directory, false);
        let saved = std::fs::read(question_path(&directory, &pending)).unwrap();
        emit(&directory, simple_item("second"));
        let path = directory.join("rollout.jsonl");
        if change == "json" {
            append(&path, b"broken json\n");
            observer.poll(&directory, false);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            let mutation = path.clone();
            during_read(
                move |point| {
                    if matches!(point, ReadCheckpoint::AfterPostReadVerify) {
                        if change == "truncate" {
                            std::fs::OpenOptions::new()
                                .write(true)
                                .open(&mutation)
                                .unwrap()
                                .set_len(0)
                                .unwrap();
                        } else {
                            std::fs::rename(&mutation, mutation.with_extension("old")).unwrap();
                            std::fs::write(&mutation, &bytes).unwrap();
                        }
                    }
                },
                || observer.poll(&directory, false),
            );
        }
        assert_pending(&directory, &pending);
        assert_eq!(diagnostic(&directory, &pending)["state"], "suspended");
        assert_eq!(
            std::fs::read(question_path(&directory, &pending)).unwrap(),
            saved
        );
        assert_eq!(
            result(&directory, &pending)["question_emissions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn unselected_signals_leave_exact_no_question_outputs_unchanged() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    observer.poll(&directory, false);
    let before_result = result(&directory, &pending);
    let before_status = status(root.path());
    let before_inspect = inspect(&directory);
    let before_list = cli_result(root.path(), &[SESSION, "--list"]);
    let reader = Reader::open_unchecked(&directory);
    let before_timeline = read_only(root.path(), || {
        query::timeline_value(&reader, SESSION, None).unwrap()
    });
    for variant in [
        "normal",
        "no_questions",
        "delivery",
        "item_kind",
        "event_kind",
        "raw_call",
        "output",
    ] {
        let mut item = simple_item("ignored");
        let kind = match variant {
            "raw_call" => {
                item = json!({"type":"function_call","name":"request_user_input_async","arguments":"{\"questions\":[{\"title\":\"Which baseline?\"}]}"});
                "response_item"
            }
            "output" => {
                item = json!({"type":"function_call_output","output":"{\"accepted\":true}"});
                "response_item"
            }
            _ => "event_msg",
        };
        match variant {
            "normal" => item["item"]["delivery"] = Value::Null,
            "no_questions" => item["item"]["questions"] = Value::Null,
            "delivery" => item["item"]["delivery"] = json!("sync"),
            "item_kind" => item["item"]["type"] = json!("OtherItem"),
            "event_kind" => item["type"] = json!("item_started"),
            _ => (),
        }
        append(&directory.join("rollout.jsonl"), &line(kind, item));
    }
    observer.poll(&directory, false);
    assert!(!question_path(&directory, &pending).exists());
    assert_eq!(
        serde_json::to_vec(&result(&directory, &pending)).unwrap(),
        serde_json::to_vec(&before_result).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&status(root.path())).unwrap(),
        serde_json::to_vec(&before_status).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&inspect(&directory)).unwrap(),
        serde_json::to_vec(&before_inspect).unwrap()
    );
    assert_eq!(cli_result(root.path(), &[SESSION, "--list"]), before_list);
    assert_eq!(
        read_only(root.path(), || query::timeline_value(
            &reader, SESSION, None
        )
        .unwrap()),
        before_timeline
    );
    assert_eq!(
        query::observe_snapshot(&reader)
            .unwrap()
            .result(
                &reader,
                &query::Selector::Request(request(&directory, &pending).request_id)
            )
            .unwrap(),
        before_result
    );
}

#[test]
fn excluded_queries_keep_exact_bytes_and_completion_journal_and_usage_stay_compatible() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    observer.poll(&directory, false);
    let reader = Reader::open_unchecked(&directory);
    let receipt = request(&directory, &pending);
    let before_list = cli_result(root.path(), &[SESSION, "--list"]);
    let before_timeline = read_only(root.path(), || {
        query::timeline_value(&reader, SESSION, None).unwrap()
    });
    let pending_wait = cli_result(
        root.path(),
        &[
            SESSION,
            "--request",
            &receipt.request_id,
            "--wait",
            "--timeout-secs",
            "1",
        ],
    );
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    assert_eq!(cli_result(root.path(), &[SESSION, "--list"]), before_list);
    assert_eq!(
        read_only(root.path(), || query::timeline_value(
            &reader, SESSION, None
        )
        .unwrap()),
        before_timeline
    );
    assert_eq!(
        cli_result(
            root.path(),
            &[
                SESSION,
                "--request",
                &receipt.request_id,
                "--wait",
                "--timeout-secs",
                "1"
            ]
        ),
        pending_wait
    );
    let event: crate::native::session::schema::SessionEvent = serde_json::from_value(json!({
        "provider":"codex","message":"ordinary answer","error":null,"provider_session_id":THREAD,"turn_id":"turn-1",
        "created_unix_ms":2,"provenance":COMPLETE_SOURCE,
        "usage":{"source":"pi_extension_agent_end","scope":"request","coverage":"complete","input_tokens":5,"output_tokens":7,"total_tokens":12,"assistant_messages":1,"runs":1,"cache_read_tokens":0,"cache_write_tokens":0}
    })).unwrap();
    let mut journal =
        turn::PendingTurnCompletion::new(&pending.claim_token, event.clone(), None).unwrap();
    journal.event_file = receipt.event_file.clone();
    Store::open_unchecked(&directory)
        .write_completion(&journal)
        .unwrap();
    let journal_bytes = std::fs::read(reader.record(CoreRecord::Completion).path()).unwrap();
    let decoded = crate::native::session::record::journal(&journal_bytes).unwrap();
    assert_eq!(decoded.event, event);
    assert!(
        !String::from_utf8(journal_bytes)
            .unwrap()
            .contains("question")
    );
    turn::recover_pending_completion(&directory).unwrap();
    let baseline_wait = cli_result(
        root.path(),
        &[SESSION, "--request", &receipt.request_id, "--wait"],
    );
    let baseline_timeline = read_only(root.path(), || {
        query::timeline_value(&reader, SESSION, None).unwrap()
    });
    let baseline_list = cli_result(root.path(), &[SESSION, "--list"]);
    let selected = query::Selector::Request(receipt.request_id.clone());
    let attachment = query::observe_snapshot(&reader)
        .unwrap()
        .attachable_result(&reader, &selected, SESSION)
        .ok()
        .unwrap();
    assert_eq!(attachment.1, "ordinary answer");
    assert!(baseline_wait.get("question_emissions").is_none());
    assert_eq!(result(&directory, &pending)["usage"]["input_tokens"], 5);
    std::fs::write(question_path(&directory, &pending), b"invalid").unwrap();
    assert_eq!(
        cli_result(
            root.path(),
            &[SESSION, "--request", &receipt.request_id, "--wait"]
        ),
        baseline_wait
    );
    assert_eq!(cli_result(root.path(), &[SESSION, "--list"]), baseline_list);
    assert_eq!(
        read_only(root.path(), || query::timeline_value(
            &reader, SESSION, None
        )
        .unwrap()),
        baseline_timeline
    );
    assert_eq!(
        query::observe_snapshot(&reader)
            .unwrap()
            .attachable_result(&reader, &selected, SESSION)
            .ok()
            .unwrap(),
        attachment
    );
    let observed = result(&directory, &pending);
    assert_eq!(observed["usage"], baseline_wait["usage"]);
    assert_eq!(observed["request_state"], baseline_wait["request_state"]);
}

#[cfg(windows)]
#[test]
fn windows_question_record_sharing_failure_retries_without_changing_completion() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let path = question_path(&directory, &pending);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    emit(&directory, simple_item("second"));
    observer.poll(&directory, false);
    assert_eq!(
        result(&directory, &pending)["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(held);
    observer.poll(&directory, false);
    assert_eq!(
        result(&directory, &pending)["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    emit(&directory, simple_item("third_unpublished"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    emit(&directory, simple_item("after_terminal"));
    observer.poll(&directory, false);
    drop(held);
    assert_eq!(result(&directory, &pending)["result"], "ordinary answer");
    assert_eq!(
        result(&directory, &pending)["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn legacy_without_a_lock_and_other_providers_contribute_no_new_fields() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let store = Store::open_unchecked(&directory);
    let mut manifest: Value = store.record(CoreRecord::Manifest).json().unwrap();
    for provider in ["pi", "claude", "agy"] {
        manifest["provider"] = json!(provider);
        store
            .record(CoreRecord::Manifest)
            .write_json(&manifest)
            .unwrap();
        assert!(
            result(&directory, &pending)
                .get("question_emissions")
                .is_none()
        );
        assert!(
            inspect(&directory)["requests"][0]
                .get("question_emitted")
                .is_none()
        );
        assert!(
            !status(root.path())["attention"]
                .as_array()
                .unwrap()
                .contains(&json!("question_emitted"))
        );
    }
    manifest["provider"] = json!("codex");
    store
        .record(CoreRecord::Manifest)
        .write_json(&manifest)
        .unwrap();
    std::fs::remove_file(directory.join("turn.claim.lock")).unwrap();
    assert!(
        result(&directory, &pending)
            .get("question_emissions")
            .is_none()
    );
    assert!(
        inspect(&directory)["requests"][0]
            .get("question_emitted")
            .is_none()
    );
    assert!(
        !status(root.path())["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
}

#[test]
fn closed_focus_and_claim_without_receipt_do_not_flag_an_older_question() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, true);
    update_status(&directory, SessionState::Closed, None, None).unwrap();
    assert!(
        !status(root.path())["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    assert_eq!(inspect(&directory)["requests"][0]["question_emitted"], true);
    assert_eq!(
        result(&directory, &pending)["question_emissions"][0]["id"],
        "first"
    );
    let (root, directory, _, mut payload, mut observer) = question_fixture();
    emit(&directory, simple_item("old"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, true);
    let next = next_claim(&directory, &mut payload);
    Store::open_unchecked(&directory)
        .record(CoreRecord::Requests)
        .child(&format!("{}.json", next.claim_token))
        .remove_raw()
        .unwrap();
    let focused = status(root.path());
    assert_eq!(focused["active_request"], Value::Null);
    assert!(
        focused["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("claim_without_receipt"))
    );
    assert!(
        !focused["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
}

#[test]
fn linked_question_record_is_refused_without_reading_or_replacing_its_target() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    emit(&directory, simple_item("first"));
    observer.poll(&directory, false);
    let path = question_path(&directory, &pending);
    let target = root.path().join("question-target.json");
    std::fs::rename(&path, &target).unwrap();
    let before = std::fs::read(&target).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &path).unwrap();
    #[cfg(windows)]
    match std::os::windows::fs::symlink_file(&target, &path) {
        Ok(()) => eprintln!("linked-record coverage: actual file symlink"),
        Err(error) => {
            assert_eq!(error.raw_os_error(), Some(1314));
            eprintln!(
                "linked-record coverage: directory junction only; file symlink denied (1314)"
            );
            let target_directory = root.path().join("linked-target");
            std::fs::create_dir(&target_directory).unwrap();
            let output = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&path)
                .arg(&target_directory)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
    }
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    emit(&directory, simple_item("second"));
    append(
        &directory.join("rollout.jsonl"),
        &complete(json!("ordinary answer")),
    );
    observer.poll(&directory, false);
    assert_eq!(
        result(&directory, &pending)["question_observation"],
        "unreadable"
    );
    assert_eq!(result(&directory, &pending)["result"], "ordinary answer");
    assert_eq!(
        inspect(&directory)["requests"][0]["question_observation"],
        "unreadable"
    );
    assert_eq!(status(root.path())["question_observation"], "unreadable");
    assert_eq!(std::fs::read(&target).unwrap(), before);
}
