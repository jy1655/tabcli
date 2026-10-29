use super::*;

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
