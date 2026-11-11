//! The per-Claim records on their own: their names, the floor each must meet, and the shapes
//! their writers record. A caller's own condition (the pending correlation id, the incoming or
//! bound turn) is not part of a floor, and these tests show it is left out.
use super::*;
use crate::native::provider::{EmittedQuestion, QuestionEmission};
use crate::native::session::requests::Receipt;

const CLAIM: &str = "1-2-3";

fn binding() -> Binding {
    Binding::new(CLAIM, REQUEST, CONVERSATION, TURN)
}

fn receipt() -> Receipt {
    Receipt {
        schema: 1,
        request_id: "request-1-2-3".into(),
        claim_token: CLAIM.into(),
        event_file: "turn-1-2-3.json".into(),
        created_unix_ms: Some(1),
        source: None,
        context_sources: Vec::new(),
    }
}

const CONTINUED_TURN: &str = "00000000-0000-4000-8000-000000000251";
const TWO: Option<&[&str]> = Some(&["Current", "Previous"]);

fn item(id: &str, questions: usize, options: Option<&[&str]>) -> QuestionEmission {
    QuestionEmission {
        id: id.into(),
        questions: (0..questions)
            .map(|_| EmittedQuestion {
                title: "Which baseline?".into(),
                options: options.map(|labels| labels.iter().map(|&label| label.into()).collect()),
            })
            .collect(),
        deflected: false,
    }
}

#[test]
fn each_per_claim_record_has_its_own_name() {
    assert_eq!(binding_name(CLAIM), "claude-mod-bound-1-2-3.json");
    assert_eq!(candidate_name(CLAIM), "claude-mod-candidate-1-2-3.json");
    assert_eq!(
        continuation_name(CLAIM),
        "claude-mod-continuation-1-2-3.json"
    );
    assert_eq!(questions_name(CLAIM), "claude-questions-1-2-3.json");
}

#[test]
fn the_binding_floor_leaves_the_correlation_id_to_the_caller() {
    assert!(binding().valid_for(CLAIM));
    assert!(!binding().valid_for("1-2-4"));
    let changes: [fn(&mut Binding); 6] = [
        |b| b.schema = 2,
        |b| b.claim = "1-2-4".into(),
        |b| b.conversation = "not-a-conversation".into(),
        |b| b.turn_id = "../invalid".into(),
        |b| b.turn_id = String::new(),
        |b| b.turn_id = "x".repeat(129),
    ];
    for change in changes {
        let mut changed = binding();
        change(&mut changed);
        assert!(!changed.valid_for(CLAIM), "{changed:?}");
    }
    let mut changed = binding();
    changed.turn_id = "x".repeat(128);
    assert!(changed.valid_for(CLAIM));
    changed.correlation_id = "../not-an-identity".into();
    assert!(changed.valid_for(CLAIM));
}

#[test]
fn the_candidate_floor_requires_its_binding_a_time_and_a_bounded_preview() {
    let binding = binding();
    let candidate = Candidate::new(&binding, "한".repeat(1024));
    assert_eq!(candidate.schema, 1);
    assert!(candidate.created_unix_ms > 0);
    assert!(candidate.matches(&binding));
    let changes: [fn(&mut Candidate); 4] = [
        |c| c.schema = 2,
        |c| c.binding.turn_id = "other-turn".into(),
        |c| c.created_unix_ms = 0,
        |c| c.answer_preview.push('x'),
    ];
    for change in changes {
        let mut changed = Candidate::new(&binding, "한".repeat(1024));
        change(&mut changed);
        assert!(!changed.matches(&binding));
    }
}

#[test]
fn the_continuation_floor_leaves_the_selected_turn_to_the_caller() {
    let binding = binding();
    for turn in [CONTINUED_TURN, TURN, "../invalid"] {
        let selected = Continuation::new(&binding, turn);
        assert_eq!(selected.turn_id, turn);
        assert!(selected.created_unix_ms > 0);
        assert!(selected.matches(&binding), "{turn}");
    }
    let changes: [fn(&mut Continuation); 3] = [
        |c| c.schema = 2,
        |c| c.binding.correlation_id = "claude-turn-other".into(),
        |c| c.created_unix_ms = 0,
    ];
    for change in changes {
        let mut changed = Continuation::new(&binding, CONTINUED_TURN);
        change(&mut changed);
        assert!(!changed.matches(&binding));
    }
}

#[test]
fn the_question_record_floor_keeps_its_texts() {
    let receipt = receipt();
    let check = |change: &dyn Fn(&mut QuestionRecord)| {
        let mut record = QuestionRecord::empty(receipt.request_id.clone(), &binding());
        change(&mut record);
        record.check(&receipt).map_err(|error| format!("{error:#}"))
    };
    assert_eq!(check(&|_| {}), Ok(()));
    // The nested binding carries any valid correlation identity, not only the pending one.
    assert_eq!(
        check(&|r| r.binding.correlation_id = "claude-turn-other".into()),
        Ok(())
    );
    assert_eq!(
        check(&|r| {
            r.incomplete = true;
            r.full = true;
        }),
        Ok(())
    );
    assert_eq!(
        check(&|r| r.items = vec![item("step:0/tool:1", 4, Some(&["a", "b", "c", "d"]))]),
        Ok(())
    );
    let changes: [fn(&mut QuestionRecord); 9] = [
        |r| r.schema = 2,
        |r| r.request_id = "request-other".into(),
        |r| r.binding.schema = 2,
        |r| r.binding.claim = "1-2-4".into(),
        |r| r.binding.conversation = "not-a-conversation".into(),
        |r| r.binding.turn_id = "../invalid".into(),
        |r| r.binding.correlation_id = "../invalid".into(),
        |r| r.full = true,
        |r| {
            let label = "x".repeat(65536);
            r.items = vec![item("step:0/tool:0", 1, Some(&[&label, "y"]))];
        },
    ];
    for change in changes {
        assert_eq!(
            check(&change),
            Err("invalid Claude question record".to_owned())
        );
    }
    assert_eq!(
        check(&|r| r.items = vec![item("step:00/tool:0", 1, TWO)]),
        Err("invalid Claude question position".to_owned())
    );
    let prefixes: [fn() -> Vec<QuestionEmission>; 9] = [
        || vec![item("step:1/tool:0", 1, TWO), item("step:0/tool:0", 1, TWO)],
        || vec![item("step:0/tool:0", 1, TWO), item("step:0/tool:0", 1, TWO)],
        || vec![item("step:0/tool:0", 0, TWO)],
        || vec![item("step:0/tool:0", 5, TWO)],
        || vec![item("step:0/tool:0", 1, None)],
        || vec![item("step:0/tool:0", 1, Some(&["only"]))],
        || vec![item("step:0/tool:0", 1, Some(&["a", "b", "c", "d", "e"]))],
        || vec![item("step:0/tool:0", 1, Some(&["a", " "]))],
        || {
            let mut untitled = item("step:0/tool:0", 1, TWO);
            untitled.questions[0].title = " ".into();
            vec![untitled]
        },
    ];
    for items in prefixes {
        assert_eq!(
            check(&|r| r.items = items()),
            Err("invalid Claude retained question prefix".to_owned())
        );
    }
}

#[test]
fn a_position_is_read_only_in_its_canonical_form() {
    assert_eq!(position("step:0/tool:3"), Some((0, 3)));
    assert_eq!(
        position(&format!("step:{MAX_INDEX}/tool:{MAX_INDEX}")),
        Some((MAX_INDEX, MAX_INDEX))
    );
    for id in [
        format!("step:{}/tool:0", MAX_INDEX + 1),
        "step:01/tool:0".into(),
        "step:+1/tool:0".into(),
        "step:1/tool:".into(),
        "step:1".into(),
        "call_a".into(),
    ] {
        assert_eq!(position(&id), None, "{id}");
    }
}

// The writers' own records, byte for byte, at the name of the record's own Claim; each reader
// returns what was written.
#[test]
fn the_per_claim_writers_record_their_exact_shapes() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(root.path());
    let text = |name: String| std::fs::read_to_string(root.path().join(name)).unwrap();
    let binding = binding();
    binding.write(&store).unwrap();
    let binding_shape = format!(
        "{{\n  \"schema\": 1,\n  \"claim\": \"{CLAIM}\",\n  \"correlation_id\": \"{REQUEST}\",\n  \"conversation\": \"{CONVERSATION}\",\n  \"turn_id\": \"{TURN}\"\n}}"
    );
    assert_eq!(text(binding_name(CLAIM)), binding_shape);
    let nested = binding_shape.replace("\n", "\n  ");
    Candidate {
        schema: 1,
        binding: binding.clone(),
        answer_preview: "interim".into(),
        created_unix_ms: 7,
    }
    .write(&store)
    .unwrap();
    assert_eq!(
        text(candidate_name(CLAIM)),
        format!(
            "{{\n  \"schema\": 1,\n  \"binding\": {nested},\n  \"answer_preview\": \"interim\",\n  \"created_unix_ms\": 7\n}}"
        )
    );
    Continuation {
        schema: 1,
        binding: binding.clone(),
        turn_id: CONTINUED_TURN.into(),
        created_unix_ms: 8,
    }
    .write(&store)
    .unwrap();
    assert_eq!(
        text(continuation_name(CLAIM)),
        format!(
            "{{\n  \"schema\": 1,\n  \"binding\": {nested},\n  \"turn_id\": \"{CONTINUED_TURN}\",\n  \"created_unix_ms\": 8\n}}"
        )
    );
    let receipt = receipt();
    QuestionRecord::empty(receipt.request_id.clone(), &binding)
        .write(&store)
        .unwrap();
    assert_eq!(
        text(questions_name(CLAIM)),
        format!(
            "{{\n  \"schema\": 1,\n  \"request_id\": \"request-1-2-3\",\n  \"binding\": {nested},\n  \"items\": [],\n  \"incomplete\": false,\n  \"full\": false\n}}"
        )
    );

    assert_eq!(Binding::read(&store, CLAIM).unwrap(), Some(binding.clone()));
    assert_eq!(Binding::require(&store, CLAIM).unwrap(), binding);
    assert_eq!(
        Candidate::read(&store, CLAIM)
            .unwrap()
            .unwrap()
            .answer_preview,
        "interim"
    );
    assert_eq!(
        Continuation::read(&store, CLAIM).unwrap().unwrap().turn_id,
        CONTINUED_TURN
    );
    let questions = QuestionRecord::read(&store, &receipt).unwrap().unwrap();
    assert!(questions.binding == binding && questions.items.is_empty());
    // Another Claim's records are absent: the live reads have none, inspection's read fails.
    assert_eq!(Binding::read(&store, "1-2-4").unwrap(), None);
    assert!(Binding::require(&store, "1-2-4").is_err());
    assert!(Candidate::read(&store, "1-2-4").unwrap().is_none());
    assert!(Continuation::read(&store, "1-2-4").unwrap().is_none());
}

// The decoders' refusals name each record's struct, for a value of the wrong type and for a
// sequence of the wrong length alike.
#[test]
fn the_decoders_name_each_records_struct() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(root.path());
    let write = |name: String, bytes: &[u8]| {
        let record = store.private(&name);
        record.remove().unwrap();
        record.write_private(bytes).unwrap();
    };
    let receipt = receipt();
    for (bytes, form) in [(&br#""x""#[..], "scalar"), (&b"[1]"[..], "sequence")] {
        let expected = |name: &str, fields: usize| {
            if form == "scalar" {
                format!("invalid type: string \"x\", expected struct {name} at line 1 column 3")
            } else {
                format!(
                    "invalid length 1, expected struct {name} with {fields} elements at line 1 column 3"
                )
            }
        };
        for name in [
            binding_name(CLAIM),
            candidate_name(CLAIM),
            continuation_name(CLAIM),
            questions_name(CLAIM),
        ] {
            write(name, bytes);
        }
        let error = |result: Result<()>| format!("{:#}", result.unwrap_err());
        assert!(
            error(Binding::read(&store, CLAIM).map(drop)).ends_with(&expected("Binding", 5)),
            "{form}"
        );
        assert!(
            error(Binding::require(&store, CLAIM).map(drop)).ends_with(&expected("Binding", 5)),
            "{form}"
        );
        assert!(
            error(Candidate::read(&store, CLAIM).map(drop)).ends_with(&expected("Candidate", 4)),
            "{form}"
        );
        assert!(
            error(Continuation::read(&store, CLAIM).map(drop))
                .ends_with(&expected("Continuation", 4)),
            "{form}"
        );
        assert_eq!(
            error(QuestionRecord::read(&store, &receipt).map(drop)),
            expected("Record", 6)
        );
    }
}
