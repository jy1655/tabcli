use agent_bridge::{
    FirstPartyCli, cli_version_is_supported, confirm_explicit_close, provider_adapter,
    provider_launch_args, provider_model_args, supported_clis, terminal_safe_text,
    validate_terminal_input,
};

#[test]
fn claude_message_guard_control_fails_closed_without_managed_state() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["native-provider-control", "claude", "message-guard"])
        .env_remove("AGENT_BRIDGE_NATIVE_SESSION_DIR")
        .env_remove("AGENT_BRIDGE_NATIVE_STATE_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success());
    let decision: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn supported_cli_versions_use_a_minimum_not_an_exact_pin() {
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.147.0").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.148.3").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.146.9").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.1.234 (Claude Code)").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.2.0 (Claude Code)").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Claude, "2.1.233 (Claude Code)").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Agy, "agy 1.1.12").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Agy, "agy 1.2.0").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Agy, "agy 1.1.11").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Pi, "0.84.1").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Pi, "pi 0.85.0").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Pi, "pi 0.84.0").unwrap());
}

#[test]
fn yolo_is_forwarded_only_when_the_new_session_explicitly_requests_it() {
    assert!(provider_launch_args(FirstPartyCli::Codex, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Claude, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Agy, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Pi, false).is_empty());

    assert_eq!(
        provider_launch_args(FirstPartyCli::Codex, true),
        ["--dangerously-bypass-approvals-and-sandbox"]
    );
    assert_eq!(
        provider_launch_args(FirstPartyCli::Claude, true),
        ["--dangerously-skip-permissions"]
    );
    assert_eq!(
        provider_launch_args(FirstPartyCli::Agy, true),
        ["--dangerously-skip-permissions"]
    );
    assert_eq!(provider_launch_args(FirstPartyCli::Pi, true), ["--approve"]);
}

#[test]
fn close_requires_an_explicit_confirmation_flag() {
    assert!(confirm_explicit_close(false).is_err());
    assert!(confirm_explicit_close(true).is_ok());
}

#[test]
fn the_native_bridge_accepts_each_supported_visible_cli() {
    assert_eq!("codex".parse(), Ok(FirstPartyCli::Codex));
    assert_eq!("claude".parse(), Ok(FirstPartyCli::Claude));
    assert_eq!("agy".parse(), Ok(FirstPartyCli::Agy));
    assert_eq!("pi".parse(), Ok(FirstPartyCli::Pi));
    assert!("unknown".parse::<FirstPartyCli>().is_err());
}

#[test]
fn supported_clis_are_backed_by_one_complete_adapter_registry() {
    assert_eq!(
        supported_clis(),
        &[
            FirstPartyCli::Codex,
            FirstPartyCli::Claude,
            FirstPartyCli::Agy,
            FirstPartyCli::Pi,
        ]
    );

    for &cli in supported_clis() {
        let adapter = provider_adapter(cli);
        assert_eq!(adapter.cli(), cli);
        assert_eq!(adapter.command(), cli.as_str());
        assert_eq!(adapter.minimum_version(), cli.minimum_version());
    }
}

#[test]
fn each_provider_adapter_owns_its_native_effort_policy() {
    assert_eq!(
        provider_adapter(FirstPartyCli::Codex)
            .effort_args("xhigh")
            .unwrap(),
        ["-c", "model_reasoning_effort=\"xhigh\""]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Claude)
            .effort_args("max")
            .unwrap(),
        ["--effort", "max"]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Agy)
            .effort_args("high")
            .unwrap(),
        ["--effort", "high"]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Pi)
            .effort_args("minimal")
            .unwrap(),
        ["--thinking", "minimal"]
    );
}

#[test]
fn codex_normalizes_only_pi_qualified_openai_codex_models() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "openai-codex/gpt-5.6-sol"),
        ["--model", "gpt-5.6-sol"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "gpt-5.6-sol"),
        ["--model", "gpt-5.6-sol"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "openai-codex/"),
        ["--model", "openai-codex/"]
    );

    for provider in [FirstPartyCli::Claude, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(
            provider_model_args(provider, "openai-codex/gpt-5.6-sol"),
            ["--model", "openai-codex/gpt-5.6-sol"]
        );
    }
}

#[test]
fn claude_normalizes_only_the_observed_fable5_model_alias() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "Fable5"),
        ["--model", "Fable"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "Fable"),
        ["--model", "Fable"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "fable5"),
        ["--model", "fable5"]
    );

    for provider in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(
            provider_model_args(provider, "Fable5"),
            ["--model", "Fable5"]
        );
    }
}

#[test]
fn pi_qualifies_only_the_exact_fable_model_alias() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Pi, "Fable"),
        ["--model", "anthropic/claude-fable-5"]
    );
    for model in [
        "fable",
        "Fable5",
        "anthropic/claude-fable-5",
        "provider/model",
    ] {
        assert_eq!(
            provider_model_args(FirstPartyCli::Pi, model),
            ["--model", model]
        );
    }
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

#[test]
fn release_publication_requires_repository_immutable_releases() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let guard = workflow
        .find("- name: Require immutable releases before publication")
        .expect("release workflow has no immutable-release preflight");
    let publish = workflow
        .find("- name: Publish verified release assets")
        .expect("release workflow has no publication step");
    let guard_step = &workflow[guard..publish];

    assert!(
        guard < publish,
        "immutable-release preflight runs after publish"
    );
    assert!(guard_step.contains("repos/${GH_REPO}/immutable-releases"));
    assert!(guard_step.contains(".enabled == true"));
    assert!(guard_step.contains("GH_TOKEN: ${{ secrets.IMMUTABLE_RELEASES_READ_TOKEN }}"));
    assert!(!guard_step.contains("GH_TOKEN: ${{ github.token }}"));
}
