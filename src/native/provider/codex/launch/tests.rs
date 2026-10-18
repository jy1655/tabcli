use super::*;

#[cfg(windows)]
mod positive_gate {
    use super::*;
    use crate::native::clock::FakeClock;
    use crate::native::session::{SessionState, Store, update_status};
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = super::super::super::test_support::launch_session(root.path(), Some("0.163.0"));
        super::super::super::hooks::prepare(&dir, Path::new(r"C:\tabcli.exe")).unwrap();
        (root, dir)
    }

    #[test]
    fn hook_review_picker_rejects_even_a_stale_empty_composer() {
        for picker in ["Review hooks", "Trust all and continue"] {
            assert!(!composer_is_ready(picker));
            assert!(!composer_is_ready(&format!("{COMPOSER_ROW}\n{picker}")));
            assert!(legacy_composer_is_ready(&format!(
                "{COMPOSER_ROW}\n{picker}"
            )));
        }
    }

    #[test]
    fn injected_gate_sends_zero_input_despite_exact_trust_or_applied_override() {
        for applied in [false, true] {
            let (root, dir) = fixture();
            let config = workspace_trust_override(root.path()).unwrap();
            Store::open_unchecked(root.path())
                .private("config.toml")
                .write_private(config.as_bytes())
                .unwrap();
            if applied {
                Store::open_unchecked(&dir)
                    .private("workspace-consent.json")
                    .write_json(&json!({"state":"verified", "applied":"codex-config-override"}))
                    .unwrap();
            }
            let env = crate::native::consent::Environment {
                home: Some(root.path().into()),
                userprofile: Some(root.path().into()),
                codex_home: Some(root.path().into()),
                ..Default::default()
            };
            use crate::native::provider::NativeProviderAdapter;
            assert!(matches!(
                super::super::super::ADAPTER
                    .workspace_trust(root.path(), &env, None)
                    .unwrap(),
                crate::native::consent::Trust::Trusted(_)
            ));
            let start = Instant::now();
            let mut clock = FakeClock::new(start);
            let mut sends = 0;
            let result = initial_readiness_with(
                &dir,
                start + Duration::from_secs(2),
                &mut clock,
                &mut || Ok(String::new()),
                || panic!("consent cannot bypass injected readiness"),
            );
            if result.is_ok() {
                sends += 1;
            }
            assert_eq!(sends, 0);
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("no initial console input was sent")
            );
            assert_eq!(clock.slept, Duration::from_secs(2));
        }
    }

    #[test]
    fn injected_gate_ended_session_wins_before_and_after_screen_read() {
        for end_during_read in [false, true] {
            let (_root, dir) = fixture();
            if !end_during_read {
                update_status(&dir, SessionState::Closed, None, None).unwrap();
            }
            let start = Instant::now();
            let mut reads = 0;
            let error = initial_readiness_with(
                &dir,
                start + Duration::from_secs(2),
                &mut FakeClock::new(start),
                &mut || {
                    reads += 1;
                    update_status(&dir, SessionState::Closed, None, None).unwrap();
                    Ok(COMPOSER_ROW.into())
                },
                || panic!("legacy"),
            )
            .unwrap_err();
            assert_eq!(reads, usize::from(end_during_read));
            assert!(error.to_string().contains("session is closed"));
        }
    }

    #[test]
    fn injected_gate_screen_failure_and_picker_deadlines_send_nothing() {
        for screen in [
            None,
            Some("Review hooks"),
            Some("Trust all and continue"),
            Some("› Ask Codex to do anything\nReview hooks"),
        ] {
            let (_root, dir) = fixture();
            let start = Instant::now();
            let mut clock = FakeClock::new(start);
            let error = initial_readiness_with(
                &dir,
                start + Duration::from_secs(2),
                &mut clock,
                &mut || {
                    screen
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("read failed"))
                },
                || panic!("legacy"),
            )
            .unwrap_err();
            assert_eq!(clock.slept, Duration::from_secs(2));
            assert_eq!(
                error.to_string().contains("hook-review picker"),
                screen.is_some()
            );
            assert!(
                error
                    .to_string()
                    .contains("no initial console input was sent")
            );
        }
    }

    #[test]
    fn injected_gate_uses_launch_record_after_settings_change_and_accepts_root_trust_composer() {
        let (root, dir) = fixture();
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(&json!({"schema":1,"codex_lifecycle_hooks":"off"}))
            .unwrap();
        let start = Instant::now();
        assert!(
            initial_readiness_with(
                &dir,
                start + Duration::from_secs(1),
                &mut FakeClock::new(start),
                &mut || Ok(String::new()),
                || panic!("settings cannot bypass gate")
            )
            .is_err()
        );
        initial_readiness_with(
            &dir,
            start + Duration::from_secs(1),
            &mut FakeClock::new(start),
            &mut || Ok(COMPOSER_ROW.into()),
            || panic!("exact trust is not required"),
        )
        .unwrap();
    }

    #[test]
    fn legacy_session_keeps_consent_short_circuit_and_injected_read_cannot_extend_deadline() {
        let (_root, dir) = fixture();
        Store::open_unchecked(&dir)
            .private("codex-launch-mode.json")
            .remove()
            .unwrap();
        let start = Instant::now();
        initial_readiness_with(
            &dir,
            start,
            &mut FakeClock::new(start),
            &mut || panic!("legacy short circuit"),
            || Ok(()),
        )
        .unwrap();
        super::super::super::hooks::prepare(&dir, Path::new(r"C:\tabcli.exe")).unwrap();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let deadline = start + Duration::from_secs(1);
        assert!(
            initial_readiness_with(
                &dir,
                deadline,
                &mut clock,
                &mut || {
                    now.set(deadline);
                    Ok(COMPOSER_ROW.into())
                },
                || panic!("legacy")
            )
            .is_err()
        );
    }
}

#[test]
fn workspace_trust_override_uses_a_toml_value_for_paths_with_dots_and_quotes() {
    for workspace in [
        Path::new("/work/space.dir/a\"b"),
        Path::new("C:\\Work\\한글.dir"),
    ] {
        let argument = workspace_trust_override(workspace).unwrap();
        let (key, value) = argument.split_once('=').unwrap();
        // The installed Codex splits this side with str::split('.'), without
        // TOML quoted-key parsing. It must therefore contain only 'projects'.
        assert_eq!(key, "projects");
        let parsed: toml::Value = format!("value={value}").parse().unwrap();
        let mut workspace_key = crate::native::consent::native_key(workspace).unwrap();
        if cfg!(windows) {
            workspace_key = workspace_key.to_lowercase();
        }
        assert_eq!(
            parsed["value"][&workspace_key]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(parsed["value"].as_table().unwrap().len(), 1);
    }
}

#[test]
fn submission_observed_rejects_pasted_content_placeholder() {
    let pasted = "› [Pasted Content 21657 chars]";
    assert!(!submission_observed(pasted));
    assert!(!submission_observed(&format!(
        "{COMPOSER_ROW}\n  {pasted}  "
    )));
    assert!(!submission_observed(&format!("{pasted}\n{COMPOSER_ROW}")));
}

#[test]
fn submission_observed_accepts_empty_composer_while_turn_runs() {
    let screen = "  Working (esc to interrupt)\n› Ask Codex to do anything  \n  ? for shortcuts";
    assert!(submission_observed(screen));
}

#[test]
fn submission_observed_rejects_empty_screen() {
    for screen in ["", "\n  \n", "› Ask Codex to do anything else"] {
        assert!(!submission_observed(screen));
    }
}

#[test]
fn submission_check_accepts_observed_submission() {
    let start = Instant::now();
    let mut screens = ["› [Pasted Content 21657 chars]", COMPOSER_ROW].into_iter();
    wait_for_submission_with(
        || Ok(screens.next().unwrap().to_owned()),
        start + Duration::from_secs(10),
        Duration::ZERO,
        || start,
    )
    .unwrap();
    assert_eq!(screens.next(), None);
}

#[test]
fn submission_check_timeout_is_uncertain() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut clock = [start, start, deadline].into_iter();
    let mut reads = 0;
    let failure = wait_for_submission_with(
        || {
            reads += 1;
            Ok("› [Pasted Content 21657 chars]".to_owned())
        },
        deadline,
        Duration::ZERO,
        || clock.next().unwrap(),
    )
    .unwrap_err();
    assert_eq!(reads, 1);
    assert!(failure.delivery_may_have_occurred());
    assert_eq!(
        failure.error().to_string(),
        "Codex composer still holds the pasted prompt; the submission was not observed. Press Enter in the surface or close the session; Agent Bridge does not resend"
    );
}

#[test]
fn submission_check_screen_read_failure_is_uncertain() {
    let start = Instant::now();
    let failure = wait_for_submission_with(
        || anyhow::bail!("screen unavailable"),
        start + Duration::from_secs(10),
        Duration::ZERO,
        || start,
    )
    .unwrap_err();
    assert!(failure.delivery_may_have_occurred());
    assert!(
        failure
            .error()
            .to_string()
            .contains("submission was not observed")
    );
}

// The screens an initial paste can meet, as Codex CLI 0.159.3 drew them on
// 2026-10-02 (session-kkpVsB: the dialog; session-nXZtvt: the composer), in the
// order of a start in which the dialog opens and the user answers it.
#[test]
fn only_the_empty_composer_is_evidence_for_an_initial_paste() {
    let dialog = "  Folder access\n  C:\\work\\project\n  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings\n  can run code automatically, even without a model request. Continue only if you trust these files. Your trust\n  decision will be saved.\n› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit";
    let composer = "  >_ OpenAI Codex (v0.159.3)\n     D:\\Dev\\project\n  How deep does this codebase go?\n› Ask Codex to do anything                                   \n  GPT-6.1-Sol low · D:\\Dev\\project\n  ? for shortcuts";
    let start = [
        "",
        "\n\n\n",
        "  >_ OpenAI Codex (v0.159.3)\n",
        dialog,
        composer,
    ];
    assert_eq!(
        start.map(composer_is_ready),
        [false, false, false, false, true],
        "the gate opens with the composer and not before"
    );

    // Another row that begins with the glyph is not the composer: a selected
    // option, a draft, another placeholder.
    for screen in [
        "› 1. Trust and continue",
        "› Ask Codex to do anything else",
        "› Ask a follow-up question",
        "Ask Codex to do anything",
    ] {
        assert!(!composer_is_ready(screen), "{screen}");
    }
    // A screen that shows both is not one this knows.
    assert!(!composer_is_ready(&format!("{dialog}\n{composer}")));
    assert!(!composer_is_ready(&format!("{composer}\n{dialog}")));
}

#[test]
fn windows_codex_inherits_the_exact_workspace_without_a_c_override() {
    let pending = PendingCodexTurn::new("1-2-3").unwrap();
    let workspace = Path::new(r"\\?\C:\repo\trailing.");
    let windows = codex_launch_arguments(
        Path::new(r"C:\agent-bridge.exe"),
        workspace,
        "prompt",
        &pending,
        true,
    )
    .unwrap();
    assert!(!windows.iter().any(|argument| argument == "-C"));
    assert!(!windows.iter().any(|argument| argument == workspace));

    let non_windows = codex_launch_arguments(
        Path::new("/opt/agent-bridge"),
        workspace,
        "prompt",
        &pending,
        false,
    )
    .unwrap();
    assert_eq!(non_windows[2], OsString::from("-C"));
    assert_eq!(non_windows[3], workspace.as_os_str());
    assert_eq!(
        non_windows.last(),
        Some(&OsString::from(correlated_prompt("prompt", &pending)))
    );
}
