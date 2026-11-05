use super::*;

// Shape and protocol copied from HEAD before extraction, not derived from the serializer.
#[test]
fn pending_record_round_trip_pins_json_marker_and_retention() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    assert!(read(&store).unwrap().is_none());
    let turn = ClaudeTurn::new("claude-turn-safe123").unwrap();
    let installed = turn.install(&store).unwrap();
    let expected = serde_json::json!({
        "schema": 1,
        "request_id": "claude-turn-safe123",
        "marker": "<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"
    });
    let text = store
        .private("claude-pending-turn.json")
        .text()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        expected
    );
    assert_eq!(text, serde_json::to_string_pretty(&turn).unwrap());
    let read_back = read(&store).unwrap().unwrap();
    assert_eq!(serde_json::to_value(&read_back).unwrap(), expected);
    assert_eq!(
        read_back.marker(),
        "<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"
    );
    assert_eq!(
        read_back.target_message("prompt"),
        "prompt\n\n[Agent Bridge Claude turn protocol]\nComplete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.\n<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"
    );
    drop(installed);
    assert!(read(&store).unwrap().is_none());
    turn.install(&store).unwrap().retain();
    assert_eq!(read(&store).unwrap().unwrap().id(), turn.id());
}

// The native record is recognized by the header that frames every prompt Bridge sends, not
// by a copy of its text: a prompt the framing function built is accepted, another is not.
#[test]
fn native_pending_record_is_recognized_by_the_shared_delegation_frame() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let turn = ClaudeTurn::new("claude-turn-safe123").unwrap();
    let token = "1700000000000-1";
    let write = |message: &str| {
        store
            .private(PENDING_TURN_FILE)
            .write_json(&serde_json::json!({
                "schema": 2,
                "request_id": turn.id(),
                "marker": turn.marker(),
                "claim_token": token,
                "expected_message": message,
            }))
            .unwrap();
    };
    let framed = turn.target_message(&crate::native::native_delegation_prompt("external", "body"));
    write(&framed);
    assert_eq!(
        read_native(&store, token).unwrap().expected_message(),
        Some(framed.as_str())
    );
    write(&framed.replacen(
        crate::native::NATIVE_DELEGATION_HEADER,
        "[Another header]",
        1,
    ));
    assert!(read_native(&store, token).is_err());
    write(&framed.replacen("\nSource: ", "\nOrigin: ", 1));
    assert!(read_native(&store, token).is_err());
}
