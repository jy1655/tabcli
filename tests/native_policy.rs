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
fn claude_message_receipt_control_records_nothing_without_managed_state() {
    let state = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args([
            "native-provider-control",
            "claude",
            "message-receipt",
            "claude-turn-safe123",
        ])
        .env_remove("AGENT_BRIDGE_NATIVE_SESSION_DIR")
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", state.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    // A PostToolUse hook cannot undo a send; failing leaves the delivery unconfirmed.
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
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

fn release_workflow_job<'a>(workflow: &'a str, job: &str, next_job: &str) -> &'a str {
    let start = workflow
        .find(&format!("\n  {job}:\n"))
        .unwrap_or_else(|| panic!("release workflow has no {job} job"));
    let end = workflow
        .find(&format!("\n  {next_job}:\n"))
        .unwrap_or_else(|| panic!("release workflow has no {next_job} job"));
    &workflow[start..end]
}

#[test]
fn release_validation_checks_the_annotated_tag_on_the_remote() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let validate = release_workflow_job(&workflow, "validate", "test");

    // actions/checkout rewrites the local tag ref to the commit and, without persisted
    // credentials, nothing can be fetched again from a private repository.
    assert!(validate.contains("persist-credentials: false"));
    assert!(validate.contains("git/ref/tags/${RELEASE_TAG}"));
    assert!(validate.contains("git/tags/${ref_sha}"));
    assert!(validate.contains("is not an annotated tag"));
    assert!(!validate.contains("git cat-file"));
    assert!(!validate.contains("git fetch"));
    assert!(
        validate.contains("git merge-base --is-ancestor \"$tag_commit\" refs/remotes/origin/main")
    );
}

#[test]
fn release_stops_before_building_when_immutable_releases_cannot_be_confirmed() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let validate = release_workflow_job(&workflow, "validate", "test");
    let gate = validate
        .find("- name: Require immutable releases before building")
        .expect("release validation has no early immutable-release gate");
    let gate_step = &validate[gate..];

    assert!(gate_step.contains("repos/${GH_REPO}/immutable-releases"));
    assert!(gate_step.contains(".enabled == true"));
    assert!(gate_step.contains("GH_TOKEN: ${{ secrets.IMMUTABLE_RELEASES_READ_TOKEN }}"));
    assert!(!gate_step.contains("GH_TOKEN: ${{ github.token }}"));
    assert!(
        release_workflow_job(&workflow, "build", "publish").contains("needs: [validate, test]"),
        "artifacts must not be built before the gate has passed"
    );
}

#[test]
fn release_rehearsal_never_publishes() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    assert!(workflow.contains("\n  workflow_dispatch:\n"));

    let release_steps = workflow
        .split("\n      - ")
        .filter(|step| step.contains("gh release create") || step.contains("gh release edit"))
        .collect::<Vec<_>>();
    assert_eq!(release_steps.len(), 2);
    for step in release_steps {
        assert!(
            step.contains("\n        if: github.event_name == 'push'\n"),
            "a rehearsal could reach: {step}"
        );
    }

    // Only a rehearsal may continue without the immutable-release token.
    for tolerated in workflow.match_indices("exit 0") {
        let before = &workflow[..tolerated.0];
        let condition = before
            .rfind("if [ \"$GITHUB_EVENT_NAME\" = \"workflow_dispatch\" ]; then")
            .expect("a missing token is tolerated outside a rehearsal");
        assert!(!before[condition..].contains("\n            fi\n"));
    }
}

#[test]
fn release_artifact_job_is_isolated_from_mutable_terminal_app_installs() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let build = workflow
        .find("\n  build:\n")
        .expect("release workflow has no build job");
    let publish = workflow
        .find("\n  publish:\n")
        .expect("release workflow has no publish job");
    let build_job = &workflow[build..publish];

    assert!(
        build_job.contains("needs: [validate, test]"),
        "artifact build must wait for the isolated release test job"
    );
    assert!(
        !build_job.contains("brew install --cask"),
        "mutable terminal app installers must not run in the artifact-producing job"
    );
}
