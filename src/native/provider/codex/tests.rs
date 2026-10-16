use super::*;

#[test]
fn console_paste_declares_soft_newlines_and_rejects_only_console_tabs() {
    use crate::native::delivery::TurnKind;
    let facts = CodexInitialPaste.facts().surface.unwrap();
    assert_eq!(facts.submit_count, 2);
    assert_eq!(facts.newline, terminal::ConsoleNewline::ShiftReturn);
    assert!(CodexQueue.facts().surface.is_none());
    let directory = tempfile::tempdir().unwrap();
    for kind in [
        "windows-console",
        "ghostty",
        "iterm2",
        "apple-terminal",
        "wezterm",
        "warp",
    ] {
        let surface = serde_json::from_value(serde_json::json!({
            "terminal": kind, "session_id": "surface"
        }))
        .unwrap();
        let mut turn =
            super::super::transport_test_context(directory.path(), &surface, TurnKind::Initial);
        install_pending_turn(directory.path(), turn.claim_token).unwrap();
        turn.prompt = "first\n\tsecond";
        let result = CodexInitialPaste.prepare(&turn);
        if kind == "windows-console" {
            assert_eq!(
                result.err().unwrap().to_string(),
                "Codex Windows console paste cannot preserve tabs; replace tab characters with spaces"
            );
        } else {
            assert!(result.unwrap().prompt.contains(turn.prompt));
        }
        turn.kind = TurnKind::FollowUp;
        assert!(CodexQueue.prepare(&turn).is_ok());
        turn.kind = TurnKind::Initial;
        turn.prompt = "first\nsecond";
        assert!(
            CodexInitialPaste
                .prepare(&turn)
                .unwrap()
                .prompt
                .contains(turn.prompt)
        );
    }
}

#[test]
fn declared_transports_preserve_framing_and_pending_cleanup() {
    use crate::native::delivery::TurnKind;
    let directory = tempfile::tempdir().unwrap();
    let surface = serde_json::from_value(
        serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
    )
    .unwrap();
    let initial =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::Initial);
    install_pending_turn(directory.path(), initial.claim_token).unwrap();
    let before = std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap();
    let prepared = CodexInitialPaste.prepare(&initial).unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_initial_prompt(directory.path(), initial.prompt).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    CodexInitialPaste.discard_prepared(&initial, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
    let follow =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::FollowUp);
    let prepared = CodexQueue.prepare(&follow).unwrap();
    assert!(prepared.prompt.is_empty());
    assert!(prepared.correlation_id.is_none());
    CodexQueue.discard_prepared(&follow, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
}

#[test]
fn workspace_trust_reads_exact_toml_key_and_no_permission_override() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let homes = consent::fixture_homes(tmp.path());
    let mut key = consent::native_key(&workspace).unwrap();
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    let config = format!(
        "[projects.{}]\ntrust_level = \"trusted\"\n",
        serde_json::to_string(&key).unwrap()
    );
    // Created as Codex creates it in a user profile: nobody else can change it. A
    // plain file in the temporary directory inherits whatever that directory allows.
    super::super::super::write_private(&homes.codex, config.as_bytes()).unwrap();
    assert!(matches!(
        ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
        Trust::Trusted(_)
    ));
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &homes)
            .unwrap(),
        Trust::Absent
    );
    std::fs::write(
        &homes.codex,
        config.replace("trusted", "untrusted").as_bytes(),
    )
    .unwrap();
    assert_eq!(
        ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
        Trust::Declined
    );
    std::fs::write(&homes.codex, b"[bad").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
}

#[test]
fn codex_native_queue_requires_version_0_149_or_newer() {
    for version in ["codex-cli 0.147.0", "codex-cli 0.148.9"] {
        assert!(!codex_version_supports_native_queue(version).unwrap());
    }
    for version in ["codex-cli 0.149.0", "codex-cli 0.153.2"] {
        assert!(codex_version_supports_native_queue(version).unwrap());
    }
    assert!(codex_version_supports_native_queue("codex unknown").is_err());
}

#[test]
fn terminal_follow_up_refuses_before_installing_pending_turn_metadata() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        ADAPTER.follow_up_transport().facts().surface.is_none(),
        "terminal ownership does not identify the active Codex thread"
    );
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
}

#[test]
fn reopen_is_refused_with_the_adapters_own_reason() {
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), CODEX_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Codex"),
        "{error}"
    );
    let directory = tempfile::tempdir().unwrap();
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), CODEX_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Codex"),
        "{error}"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

fn homes_with_codex_config(codex: &str) -> crate::native::consent::Homes {
    crate::native::consent::Homes {
        codex: std::path::PathBuf::from(codex),
        claude: "unused-claude".into(),
        agy: "unused-agy".into(),
        pi: "unused-pi".into(),
    }
}

#[test]
fn the_memory_workspace_is_beside_the_config_that_homes_names() {
    // Only the Codex config path decides; the other providers' homes are never read.
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("custom-codex").join("config.toml");
    assert_eq!(
        memory_workspace(&homes_with_codex_config(config.to_str().unwrap())).unwrap(),
        home.path().join("custom-codex").join("memories")
    );
    // A relative config path stays relative: the caller's working directory resolves it.
    assert_eq!(
        memory_workspace(&homes_with_codex_config("relative/config.toml")).unwrap(),
        std::path::Path::new("relative").join("memories")
    );
    assert_eq!(
        memory_workspace(&homes_with_codex_config("config.toml")).unwrap(),
        std::path::Path::new("memories")
    );
    let error = memory_workspace(&homes_with_codex_config("")).unwrap_err();
    assert_eq!(error.to_string(), "Codex config path has no parent");
    assert_eq!(
        super::super::codex_memory_workspace(&homes_with_codex_config(config.to_str().unwrap()))
            .unwrap(),
        memory_workspace(&homes_with_codex_config(config.to_str().unwrap())).unwrap()
    );
}
