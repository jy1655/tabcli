use super::super::PendingAgyTurn;
use super::super::test_support::{marked, planner_line};
use super::*;
use std::fs;

#[test]
fn tool_confirmation_returns_the_tool_name() {
    assert_eq!(
        parse_tool_confirmation("Surfacing tool confirmation: \"RunCommand\" at step 2"),
        Some("RunCommand")
    );
}

#[test]
fn tool_confirmation_rejects_malformed_lines() {
    for line in [
        "Surfacing tool confirmation: \"\" at step 2",
        "Surfacing tool confirmation: \"RunCommand\" at step x",
        "Surfacing tool confirmation: \"RunCommand\" at step 2 trailing",
    ] {
        assert!(parse_tool_confirmation(line).is_none());
    }
}

#[test]
fn only_user_input_carries_requires_exactly_one_marked_input() {
    let root = tempfile::tempdir().unwrap();
    let brain = root.path().join("brain");
    let logs = transcript_logs(&brain, "97ad12fd-9e7a-4556-83a4-8f0147343657");
    fs::create_dir_all(&logs).unwrap();
    let full = logs.join(super::super::FULL_TRANSCRIPT_FILE);
    let pending = PendingAgyTurn::new("82-1-0").unwrap();
    let input = |content: &str| {
        serde_json::json!({"type": "USER_INPUT", "content": content}).to_string() + "\n"
    };
    let marked_input = input(&marked("request", &pending));

    for (transcript, expected) in [
        (planner_line(0, &marked("answer", &pending)) + "\n", false),
        (marked_input.clone(), true),
        (input("another request"), false),
        (marked_input.clone() + &marked_input, false),
    ] {
        fs::write(&full, transcript).unwrap();
        assert!(validated_file_metadata(&full, &brain).unwrap().is_some());
        assert_eq!(
            only_user_input_carries(&full, &brain, &pending.marker).unwrap(),
            expected
        );
    }

    fs::remove_file(&full).unwrap();
    assert!(!only_user_input_carries(&full, &brain, &pending.marker).unwrap());
}
