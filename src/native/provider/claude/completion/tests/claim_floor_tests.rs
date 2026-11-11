//! Characterization of the floor each per-Claim record must meet, through the module's entry
//! points: the binding read by completion and by inspection, the selected continuation read
//! by selection and by inspection, and the candidate read by all three. Each reader keeps its
//! own policy and refusal text.
use super::*;

const CONTINUED: &str = "00000000-0000-4000-8000-000000000251";
const NO_BINDING: &str = "Claude module event has no matching turn.start binding for the pending request; unrelated turns and input without its own exact start cannot settle it";
const CANDIDATE_MISMATCH: &str =
    "Claude candidate does not match the current request and original binding";

/// The fixture after an unmarked bound answer: its binding and candidate are recorded.
fn interim() -> Fixture {
    let f = Fixture::new();
    f.start();
    f.complete("interim");
    f
}

/// The latest rejected module signal's reason.
fn rejection(f: &Fixture) -> String {
    f.store().private(DIAGNOSTIC_FILE).json::<Value>().unwrap()["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn observe(f: &Fixture) -> Option<Value> {
    let store = f.store();
    let _lock = store.lock().unwrap();
    observe_candidate(&store, &f.token)
}

fn unreadable(detail: &str) -> Option<Value> {
    Some(json!({"state": "unreadable", "detail": detail}))
}

fn rewrite(f: &Fixture, name: &str, pointer: &str, value: &Value) {
    let record = f.store().private(name);
    let mut record_value: Value = record.json().unwrap();
    *record_value.pointer_mut(pointer).unwrap() = value.clone();
    record.write_json(&record_value).unwrap();
}

fn replace(f: &Fixture, name: &str, bytes: &[u8]) {
    let record = f.store().private(name);
    record.remove().unwrap();
    record.write_private(bytes).unwrap();
}

fn question() -> Value {
    json!({"turnId":TURN,"step":0,"items":[{"index":0,"questions":[
        {"question":"Which baseline?","header":"Baseline","multiSelect":false,
         "options":[{"label":"Current","description":"Current baseline"},
                    {"label":"Previous","description":"Previous baseline"}]}
    ]}],"incomplete":false})
}

fn unmarked_bound_answer(f: &Fixture) -> Result<()> {
    receive(
        f.root.path(),
        &f.wire(
            "turn.complete",
            json!({"turnId":TURN,"reason":"answer","isAborted":false,"answer":"second interim"}),
        ),
    )
}

// Inspection requires the binding to name the pending request; completion treats a binding
// below that floor as no binding, so a question is ignored, a completion retained, and an
// exact start cannot replace it.
#[test]
fn both_binding_readers_refuse_a_binding_below_the_floor_with_their_texts() {
    for (field, value) in [
        ("/schema", json!(2)),
        ("/claim", json!("999-999-999")),
        ("/correlation_id", json!("claude-turn-other")),
        ("/conversation", json!("not-a-conversation")),
        ("/turn_id", json!("../invalid")),
    ] {
        let f = interim();
        rewrite(&f, &binding_name(&f.token), field, &value);
        let before = f.store().private(&binding_name(&f.token)).text().unwrap();
        assert_eq!(observe(&f), unreadable(CANDIDATE_MISMATCH), "{field}");
        let rejected = rejection(&f);
        f.send("question.emissions", question());
        assert_eq!(rejection(&f), rejected, "{field}");
        assert!(
            !f.root.path().join(questions_name(&f.token)).exists(),
            "{field}"
        );
        f.send("turn.complete", f.final_data(TURN));
        assert_eq!(rejection(&f), NO_BINDING, "{field}");
        f.start();
        assert_eq!(
            rejection(&f),
            "conflicting Claude turn identity; the original binding was retained",
            "{field}"
        );
        f.retained();
        assert_eq!(
            f.store().private(&binding_name(&f.token)).text().unwrap(),
            before,
            "{field}"
        );
    }
}

// Completion reads the binding like its other records: whole, lossy, absence as no binding,
// a refusal before any event is dispatched. Inspection requires it and reads strict UTF-8.
#[test]
fn the_binding_readers_keep_their_policies() {
    // Undecodable: both fail with the decoder's text, which names the record's struct.
    let f = interim();
    let path = f.root.path().join(binding_name(&f.token));
    replace(&f, &binding_name(&f.token), br#""broken""#);
    let decode = format!(
        "invalid JSON in {}: invalid type: string \"broken\", expected struct Binding at line 1 column 8",
        path.display()
    );
    assert_eq!(observe(&f), unreadable(&decode));
    let invalid = format!("invalid Claude turn binding: {decode}");
    f.send("question.emissions", question());
    assert_eq!(rejection(&f), invalid);
    f.send("turn.complete", f.final_data(TURN));
    assert_eq!(rejection(&f), invalid);
    f.retained();

    // Absent: inspection's required read fails; completion has no binding.
    let f = interim();
    let path = f.root.path().join(binding_name(&f.token));
    f.store().private(&binding_name(&f.token)).remove().unwrap();
    let missing = format!(
        "failed to read {}: {}",
        path.display(),
        std::fs::read_to_string(&path).unwrap_err()
    );
    assert_eq!(observe(&f), unreadable(&missing));
    f.send("turn.complete", f.final_data(TURN));
    assert_eq!(rejection(&f), NO_BINDING);
    f.retained();

    // Not a regular file: inspection's read fails as the OS reports it; completion refuses it.
    let f = interim();
    let path = f.root.path().join(binding_name(&f.token));
    f.store().private(&binding_name(&f.token)).remove().unwrap();
    std::fs::create_dir(&path).unwrap();
    let unreadable_directory = format!(
        "failed to read {}: {}",
        path.display(),
        std::fs::read_to_string(&path).unwrap_err()
    );
    assert_eq!(observe(&f), unreadable(&unreadable_directory));
    f.send("turn.complete", f.final_data(TURN));
    assert_eq!(
        rejection(&f),
        format!(
            "invalid Claude turn binding: refusing non-regular session file: {}",
            path.display()
        )
    );
    f.retained();

    // Invalid UTF-8 outside the binding's fields: inspection refuses it, completion accepts it.
    let f = interim();
    let path = f.root.path().join(binding_name(&f.token));
    let text = f
        .store()
        .private(&binding_name(&f.token))
        .text()
        .unwrap()
        .unwrap();
    let mut lossy = text
        .trim_end()
        .strip_suffix('}')
        .unwrap()
        .as_bytes()
        .to_vec();
    lossy.extend_from_slice(b",\"pad\":\"\xff\"}");
    replace(&f, &binding_name(&f.token), &lossy);
    let strict = format!(
        "failed to read {}: {}",
        path.display(),
        std::fs::read_to_string(&path).unwrap_err()
    );
    assert_eq!(observe(&f), unreadable(&strict));
    f.complete_marked("lossy binding final");
    let events = f.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "lossy binding final");
    assert_eq!(
        events[0].provenance.as_deref(),
        Some("claude_mod_turn_complete")
    );
}

// Selection requires the incoming turn; inspection requires a valid identity other than the
// bound turn. Both require the schema, the original binding and a creation time.
#[test]
fn selection_and_inspection_each_keep_their_own_continuation_predicate() {
    const CONFLICT: &str =
        "Claude continuation conflicts with the already selected completion identity";
    const INVALID: &str = "Claude selected continuation identity is invalid";
    for (case, pointer, value, inspected, publishes) in [
        (
            "selected",
            "/turn_id",
            json!(CONTINUED),
            Some(CONTINUED),
            true,
        ),
        ("schema", "/schema", json!(2), None, false),
        ("claim", "/binding/claim", json!("999-999-999"), None, false),
        (
            "binding turn",
            "/binding/turn_id",
            json!("00000000-0000-4000-8000-000000000252"),
            None,
            false,
        ),
        ("created", "/created_unix_ms", json!(0), None, false),
        ("invalid turn", "/turn_id", json!("../invalid"), None, false),
        ("bound turn", "/turn_id", json!(TURN), None, false),
        (
            "other turn",
            "/turn_id",
            json!("other-turn"),
            Some("other-turn"),
            false,
        ),
    ] {
        let f = interim();
        let binding: Binding = f.store().private(&binding_name(&f.token)).json().unwrap();
        let selected = select_continuation(&f.store(), &binding, CONTINUED)
            .unwrap()
            .unwrap();
        let mut selected = serde_json::to_value(selected).unwrap();
        *selected.pointer_mut(pointer).unwrap() = value;
        let record = f.store().private(&continuation_name(&f.token));
        record.write_json(&selected).unwrap();
        let before = record.text().unwrap();
        let observed = observe(&f).unwrap();
        match inspected {
            Some(turn) => {
                assert_eq!(observed["state"], "unmarked_bound_turn", "{case}");
                assert_eq!(observed["selected_completion_turn_id"], turn, "{case}");
            }
            None => assert_eq!(Some(observed), unreadable(INVALID), "{case}"),
        }
        f.send("turn.complete", f.final_data(CONTINUED));
        if publishes {
            let events = f.events();
            assert_eq!(events.len(), 1, "{case}");
            assert_eq!(
                events[0].provenance.as_deref(),
                Some("claude_mod_marked_continuation")
            );
        } else {
            f.retained();
            assert_eq!(rejection(&f), CONFLICT, "{case}");
        }
        assert_eq!(record.text().unwrap(), before, "{case}");
    }

    // An undecodable selection is the decoder's text for both; the candidate is read first.
    let f = interim();
    let path = f.root.path().join(continuation_name(&f.token));
    f.store()
        .private(&continuation_name(&f.token))
        .write_private(br#""broken""#)
        .unwrap();
    let decode = format!(
        "invalid JSON in {}: invalid type: string \"broken\", expected struct Continuation at line 1 column 8",
        path.display()
    );
    assert_eq!(observe(&f), unreadable(&decode));
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(rejection(&f), decode);
    f.store()
        .private(&candidate_name(&f.token))
        .remove()
        .unwrap();
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(
        rejection(&f),
        "Claude continuation has no unmarked bound-turn candidate"
    );
    assert_eq!(observe(&f), None);
    f.retained();
}

// Remembering, selecting and inspecting all require the candidate to match the binding:
// its schema, the binding itself, a creation time and a preview of at most 1024 characters.
#[test]
fn every_candidate_reader_refuses_a_candidate_that_does_not_match_its_binding() {
    const REMEMBER: &str = "Claude completion candidate does not match the immutable binding";
    const SELECT: &str = "Claude continuation candidate does not match the immutable binding";
    let damaged = |pointer: &str, value: &Value| {
        let f = interim();
        rewrite(&f, &candidate_name(&f.token), pointer, value);
        let before = f.store().private(&candidate_name(&f.token)).text().unwrap();
        (f, before)
    };
    for (pointer, value) in [
        ("/schema", json!(2)),
        ("/binding/claim", json!("999-999-999")),
        ("/binding/correlation_id", json!("claude-turn-other")),
        (
            "/binding/conversation",
            json!("00000000-0000-0000-0000-000000000000"),
        ),
        ("/binding/turn_id", json!("wrong-turn")),
        ("/created_unix_ms", json!(0)),
        ("/answer_preview", json!("x".repeat(1025))),
    ] {
        let (f, _) = damaged(pointer, &value);
        assert_eq!(observe(&f), unreadable(CANDIDATE_MISMATCH), "{pointer}");

        let (f, before) = damaged(pointer, &value);
        let error = unmarked_bound_answer(&f).unwrap_err();
        assert_eq!(format!("{error:#}"), REMEMBER, "{pointer}");
        assert_eq!(
            f.store().private(&candidate_name(&f.token)).text().unwrap(),
            before,
            "{pointer}"
        );
        f.retained();

        let (f, before) = damaged(pointer, &value);
        f.send("turn.complete", f.final_data(CONTINUED));
        assert_eq!(rejection(&f), SELECT, "{pointer}");
        assert!(
            !f.root.path().join(continuation_name(&f.token)).exists(),
            "{pointer}"
        );
        assert_eq!(
            f.store().private(&candidate_name(&f.token)).text().unwrap(),
            before,
            "{pointer}"
        );
        f.retained();
    }

    // A preview of exactly 1024 characters matches, however many bytes it takes.
    let preview = json!("한".repeat(1024));
    let (f, _) = damaged("/answer_preview", &preview);
    assert_eq!(observe(&f).unwrap()["answer_preview"], preview);
    let (f, before) = damaged("/answer_preview", &preview);
    unmarked_bound_answer(&f).unwrap();
    assert_eq!(
        f.store().private(&candidate_name(&f.token)).text().unwrap(),
        before
    );
    let (f, _) = damaged("/answer_preview", &preview);
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(f.events()[0].message, "final result");

    // Undecodable: the decoder's text for every reader, naming the record's struct.
    let undecodable = || {
        let f = interim();
        replace(&f, &candidate_name(&f.token), br#""broken""#);
        let decode = format!(
            "invalid JSON in {}: invalid type: string \"broken\", expected struct Candidate at line 1 column 8",
            f.root.path().join(candidate_name(&f.token)).display()
        );
        (f, decode)
    };
    let (f, decode) = undecodable();
    assert_eq!(observe(&f), unreadable(&decode));
    let (f, decode) = undecodable();
    assert_eq!(
        format!("{:#}", unmarked_bound_answer(&f).unwrap_err()),
        decode
    );
    let (f, decode) = undecodable();
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(rejection(&f), decode);

    // Absent: inspection has nothing to report, even before reading a damaged binding;
    // remembering records the new answer; selection has no candidate.
    let f = interim();
    f.store()
        .private(&candidate_name(&f.token))
        .remove()
        .unwrap();
    replace(&f, &binding_name(&f.token), b"{");
    assert_eq!(observe(&f), None);
    let f = interim();
    f.store()
        .private(&candidate_name(&f.token))
        .remove()
        .unwrap();
    unmarked_bound_answer(&f).unwrap();
    assert_eq!(
        f.store()
            .private(&candidate_name(&f.token))
            .json::<Value>()
            .unwrap()["answer_preview"],
        "second interim"
    );
    let f = interim();
    f.store()
        .private(&candidate_name(&f.token))
        .remove()
        .unwrap();
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(
        rejection(&f),
        "Claude continuation has no unmarked bound-turn candidate"
    );
    f.retained();
}
