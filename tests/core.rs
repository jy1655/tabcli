use agent_bridge::{
    AgentId, TabSet, agents, handoff_text_from, session_title, terminal_safe_text,
    validate_terminal_input,
};

#[test]
fn registry_uses_first_party_cli_commands() {
    let definitions = agents();
    assert_eq!(definitions.len(), 3);
    assert_eq!(definitions[0].command, "codex");
    assert_eq!(definitions[1].command, "claude");
    assert_eq!(definitions[2].command, "agy");
}

#[test]
fn handoff_rejects_an_empty_request() {
    assert!(handoff_text_from("Codex 1", "   ", "context").is_err());
}

#[test]
fn handoff_includes_the_request_and_recent_source_context() {
    let text = handoff_text_from(
        "Codex 1 @ D:\\Dev\\alpha",
        "Review the implementation",
        "Tests pass. Remaining risk: timeout handling.",
    )
    .unwrap();

    assert!(text.contains("Source: Codex 1 @ D:\\Dev\\alpha"));
    assert!(text.contains("Request: Review the implementation"));
    assert!(text.contains("Tests pass. Remaining risk: timeout handling."));
}

#[test]
fn duplicate_agents_receive_distinct_tab_titles() {
    assert_eq!(session_title(AgentId::Codex, 1), "Codex 1");
    assert_eq!(session_title(AgentId::Codex, 2), "Codex 2");
    assert_eq!(session_title(AgentId::Claude, 1), "Claude 1");
}

#[test]
fn tabs_can_hold_any_agent_mix_and_activate_the_new_tab() {
    let mut tabs = TabSet::new();
    tabs.push("Codex 1");
    tabs.push("Codex 2");
    tabs.push("Claude 1");

    assert_eq!(tabs.items(), &["Codex 1", "Codex 2", "Claude 1"]);
    assert_eq!(tabs.active_index(), Some(2));
    assert_eq!(tabs.active(), Some(&"Claude 1"));
}

#[test]
fn tab_navigation_wraps_and_close_selects_a_neighbor() {
    let mut tabs = TabSet::new();
    tabs.push("Codex 1");
    tabs.push("Codex 2");
    tabs.push("Claude 1");

    tabs.move_active(1);
    assert_eq!(tabs.active(), Some(&"Codex 1"));
    tabs.move_active(-1);
    assert_eq!(tabs.active(), Some(&"Claude 1"));

    assert_eq!(tabs.remove_active(), Some("Claude 1"));
    assert_eq!(tabs.active(), Some(&"Codex 2"));
    assert_eq!(tabs.remove_active(), Some("Codex 2"));
    assert_eq!(tabs.remove_active(), Some("Codex 1"));
    assert_eq!(tabs.active_index(), None);
}

#[test]
fn removing_a_background_tab_keeps_the_active_selection() {
    let mut tabs = TabSet::new();
    tabs.push("Codex 1");
    tabs.push("Claude 1");
    tabs.push("Codex 2");
    assert_eq!(tabs.active_index(), Some(2));

    assert_eq!(tabs.remove(0), Some("Codex 1"));
    assert_eq!(tabs.active(), Some(&"Codex 2"));

    assert_eq!(tabs.remove(5), None);
    assert_eq!(tabs.remove(1), Some("Codex 2"));
    assert_eq!(tabs.active(), Some(&"Claude 1"));
}

#[test]
fn tabs_can_activate_a_known_index_without_navigation_loops() {
    let mut tabs = TabSet::new();
    tabs.push("Codex 1");
    tabs.push("Claude 1");

    assert!(tabs.set_active(0));
    assert_eq!(tabs.active(), Some(&"Codex 1"));
    assert!(!tabs.set_active(2));
    assert_eq!(tabs.active(), Some(&"Codex 1"));
}

#[test]
fn active_tab_can_be_replaced_in_place() {
    let mut tabs = TabSet::new();
    tabs.push("Codex old");

    assert_eq!(tabs.replace_active("Codex new"), Some("Codex old"));
    assert_eq!(tabs.active(), Some(&"Codex new"));
}

#[test]
fn terminal_input_rejects_submission_and_escape_controls() {
    assert!(validate_terminal_input("line one\nline two\tindented", "prompt").is_ok());
    for control in ['\0', '\r', '\u{1b}', '\u{7f}'] {
        assert!(
            validate_terminal_input(&format!("before{control}after"), "prompt").is_err(),
            "accepted terminal control U+{:04X}",
            u32::from(control)
        );
    }
}

#[test]
fn terminal_output_renders_controls_as_visible_text() {
    let escaped = terminal_safe_text("ok\u{1b}]52;clipboard\u{7}\nnext", true);

    assert_eq!(escaped, "ok\\u{1b}]52;clipboard\\u{7}\nnext");
    assert!(!escaped.contains('\u{1b}'));
    assert!(!escaped.contains('\u{7}'));
}
