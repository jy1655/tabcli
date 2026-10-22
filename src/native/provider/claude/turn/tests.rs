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
