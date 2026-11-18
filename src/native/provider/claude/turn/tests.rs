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
        read_back.target_message(FrameFormat::Compact, "prompt"),
        "prompt\n\nEnd your final response with this exact line:\n<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"
    );
    drop(installed);
    assert!(read(&store).unwrap().is_none());
    turn.install(&store).unwrap().retain();
    assert_eq!(read(&store).unwrap().unwrap().id(), turn.id());
}

// #396: the frame is the prompt, one instruction line and the marker; no header or label.
#[test]
fn target_message_adds_only_the_marker_instruction() {
    let turn = ClaudeTurn::new("claude-turn-safe123").unwrap();
    assert_eq!(
        turn.target_message(FrameFormat::Compact, "prompt"),
        "prompt\n\nEnd your final response with this exact line:\n<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"
    );
}

// The native record is recognized by the frame this adapter builds (#396), or by the frame
// an earlier binary built for a request still in flight across an upgrade: the shared
// delegation header and Source line ahead of the body, the marker at the end.
#[test]
fn native_pending_record_accepts_the_short_frame_and_the_legacy_frame() {
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
    let marker = turn.marker();
    let legacy = format!(
        "[Agent Bridge native delegation]\nSource: external\n\nbody\n\n[Agent Bridge Claude turn protocol]\nComplete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.\n{marker}"
    );
    // The messenger keeps the legacy start ahead of the current frame for a launch whose
    // completion hook can still be an earlier binary; both readers accept that form.
    let prefixed = format!(
        "[Agent Bridge native delegation]\nSource: external\n\n{}",
        turn.target_message(FrameFormat::Compact, "body")
    );
    for accepted in [
        turn.target_message(FrameFormat::Compact, "body"),
        legacy.clone(),
        prefixed,
    ] {
        write(&accepted);
        assert_eq!(
            read_native(&store, token).unwrap().expected_message(),
            Some(accepted.as_str())
        );
    }
    for refused in [
        format!("body\n\n{marker}"),
        turn.target_message(FrameFormat::Compact, "body").replace(
            marker,
            "<!-- agent-bridge-claude-turn:claude-turn-other -->",
        ),
        legacy.replacen("[Agent Bridge native delegation]", "[Another header]", 1),
        legacy.replacen("\nSource: ", "\nOrigin: ", 1),
    ] {
        write(&refused);
        assert!(read_native(&store, token).is_err(), "{refused:?}");
    }
}
