use agent_bridge::{
    FirstPartyCli, cli_version_is_supported, confirm_explicit_close, provider_launch_args,
};

#[test]
fn supported_cli_versions_use_a_minimum_not_an_exact_pin() {
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.147.0").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.148.3").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.146.9").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.1.229 (Claude Code)").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.2.0 (Claude Code)").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Claude, "2.1.228 (Claude Code)").unwrap());
}

#[test]
fn yolo_is_forwarded_only_when_the_new_session_explicitly_requests_it() {
    assert!(provider_launch_args(FirstPartyCli::Codex, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Claude, false).is_empty());

    assert_eq!(
        provider_launch_args(FirstPartyCli::Codex, true),
        ["--dangerously-bypass-approvals-and-sandbox"]
    );
    assert_eq!(
        provider_launch_args(FirstPartyCli::Claude, true),
        ["--dangerously-skip-permissions"]
    );
}

#[test]
fn close_requires_an_explicit_confirmation_flag() {
    assert!(confirm_explicit_close(false).is_err());
    assert!(confirm_explicit_close(true).is_ok());
}

#[test]
fn the_native_bridge_accepts_only_first_party_codex_and_claude() {
    assert_eq!("codex".parse(), Ok(FirstPartyCli::Codex));
    assert_eq!("claude".parse(), Ok(FirstPartyCli::Claude));
    assert!("agy".parse::<FirstPartyCli>().is_err());
}
