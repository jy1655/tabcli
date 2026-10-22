use super::*;

fn auth_output(text: &str, exit: i32) -> Result<std::process::Output> {
    #[cfg(unix)]
    let status = {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(exit << 8)
    };
    #[cfg(windows)]
    let status = {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(exit as u32)
    };
    Ok(std::process::Output {
        status,
        stdout: text.as_bytes().to_vec(),
        stderr: b"sensitive stderr must not be reported".to_vec(),
    })
}

#[test]
fn credential_status_shapes_are_bounded_to_known_fields_and_exit_codes() {
    // Pi 1.0.0, 2026-10-07: ready OAuth=0, missing credentials=1,
    // unknown provider=1. invalid_state=2 is from Pi's auth-check source.
    for (body, exit, expected) in [
        (
            r#"{"status":"ready","provider":"openai","authType":"oauth"}"#,
            0,
            "ready",
        ),
        (
            r#"{"status":"ready","provider":"openai","authType":"api_key","token":"secret"}"#,
            0,
            "ready",
        ),
        (
            r#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
            1,
            "not_ready",
        ),
        (
            r#"{"status":"not_ready","provider":"openai","reason":"provider_not_found"}"#,
            1,
            "unknown",
        ),
        (
            r#"{"status":"invalid","provider":"openai","reason":"invalid_state"}"#,
            2,
            "unknown",
        ),
        (
            r#"{"status":"ready","provider":"other","authType":"oauth"}"#,
            0,
            "unknown",
        ),
        (
            r#"{"status":"ready","provider":"openai","authType":"oauth"}"#,
            1,
            "unknown",
        ),
        (
            r#"{"status":"ready","provider":"openai","authType":"future_scheme"}"#,
            0,
            "ready",
        ),
        ("secret malformed JSON", 0, "unknown"),
        ("{}", 0, "unknown"),
    ] {
        let check = serde_json::to_value(parse_credentials(
            "openai",
            auth_output(body, exit),
            (Some("openai/test-model"), Some(Path::new("pi"))),
        ))
        .unwrap();
        assert_eq!(check["evidence"]["status"], expected, "{body}");
        assert_eq!(check["evidence"]["exit_code"], exit);
        assert_eq!(check["evidence"]["model"], "openai/test-model");
        assert_eq!(check["evidence"]["executable"], "pi");
        assert!(!check.to_string().contains("secret"));
        assert!(!check.to_string().contains("sensitive"));
    }
    for (error, detail) in [
        ("secret command missing", "could not run"),
        ("diagnostic probe timed out", "timed out"),
        ("diagnostic probe deadline exhausted", "timed out"),
    ] {
        let check = serde_json::to_value(parse_credentials(
            "openai",
            Err(anyhow::anyhow!(error)),
            (Some("openai/test-model"), Some(Path::new("pi"))),
        ))
        .unwrap();
        assert_eq!(check["evidence"]["status"], "unknown");
        assert!(!check.to_string().contains("secret"));
        assert!(check["detail"].as_str().unwrap().contains(detail));
    }
}

#[test]
fn ready_credentials_preserve_auth_type_as_evidence_only() {
    use crate::native::diagnostics::Availability;
    for auth_type in [Some("future_scheme"), None] {
        let output = serde_json::json!({
            "status": "ready", "provider": "openai", "authType": auth_type
        });
        let check = parse_credentials("openai", auth_output(&output.to_string(), 0), (None, None));
        assert_eq!(check.availability, Availability::Available);
        assert_eq!(check.reason_code, "pi_provider_credentials_ready");
        let check = serde_json::to_value(check).unwrap();
        assert_eq!(check["evidence"]["authType"], serde_json::json!(auth_type));
        assert_eq!(check["evidence"]["status"], "ready");
    }
}

#[test]
fn model_provider_requires_a_qualified_id_without_guessing() {
    for (model, expected) in [
        ("openai-codex/gpt-5.6-sol", Some("openai-codex")),
        ("OpenAI/gpt-4o-mini", Some("openai")),
        ("openrouter/openai/gpt-4o", Some("openrouter")),
        ("openai/custom:high", Some("openai")),
        ("gpt-4o", None),
        ("Fable", None),
        ("", None),
        ("/model", None),
        ("openai/", None),
        ("openai /model", None),
        ("openai/a b", None),
    ] {
        assert_eq!(model_provider(model).as_deref(), expected);
    }
}

#[test]
fn credential_doctor_requires_session_model_probe_and_pi_resolution() {
    use crate::native::diagnostics;
    use crate::native::session::schema::SessionManifest;
    let mut manifest = SessionManifest {
        schema: 1,
        id: "test".into(),
        provider: "pi".into(),
        provider_path: "pi".into(),
        provider_version: "1.0.0".into(),
        workspace: ".".into(),
        title: "test".into(),
        model: Some("openai/gpt-4o-mini".into()),
        effort: None,
        yolo: false,
        created_unix_ms: 1,
    };
    let base = diagnostics::Context {
        directory: None,
        manifest: None,
        executable: Some(Path::new("pi")),
        current_version: None,
        workspace: Path::new("."),
        probe: true,
        deadline: Instant::now() + Duration::from_secs(5),
        environment: &crate::native::consent::Environment::default(),
    };
    let check = credential_diagnosis(base, |_, _, _, _| panic!("provider-only must not run Pi"));
    let check = serde_json::to_value(check).unwrap();
    assert_eq!(check["id"], "pi_provider_credentials");
    assert_eq!(check["evidence"]["status"], "unknown");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("No model is known")
    );
    assert!(check["evidence"]["model"].is_null());
    assert_eq!(check["evidence"]["executable"], "pi");
    assert!(check["evidence"]["exit_code"].is_null());
    for model in [None, Some("unqualified".into())] {
        manifest.model = model;
        let check = credential_diagnosis(
            diagnostics::Context {
                manifest: Some(&manifest),
                ..base
            },
            |_, _, _, _| panic!("no derivable provider"),
        );
        let check = serde_json::to_value(check).unwrap();
        assert_eq!(
            check["evidence"],
            serde_json::json!({
                "status":"unknown", "provider":null, "authType":null, "exit_code":null,
                "model":manifest.model, "executable":"pi"
            })
        );
    }
    manifest.model = Some("openai/gpt-4o-mini".into());
    for (probe, executable) in [(false, base.executable), (true, None)] {
        let check = credential_diagnosis(
            diagnostics::Context {
                manifest: Some(&manifest),
                probe,
                executable,
                ..base
            },
            |_, _, _, _| panic!("must not run"),
        );
        assert_eq!(check.availability, diagnostics::Availability::Unknown);
        let check = serde_json::to_value(check).unwrap();
        assert_eq!(
            check["evidence"],
            serde_json::json!({
                "status":"unknown", "provider":"openai", "authType":null, "exit_code":null,
                "model":manifest.model, "executable":executable
            })
        );
    }
    for (resolved, credentials_missing) in [("openai", false), ("other", false), ("openai", true)] {
        let mut calls = Vec::new();
        let mut deadlines = Vec::new();
        let check = credential_diagnosis(
            diagnostics::Context {
                manifest: Some(&manifest),
                ..base
            },
            |executable, args, _, deadline| {
                assert_eq!(Some(executable), base.executable);
                calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                deadlines.push(deadline);
                if credentials_missing && calls.len() == 2 {
                    auth_output(
                        r#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
                        1,
                    )
                } else {
                    auth_output(
                        &format!(
                            r#"{{"status":"ready","provider":"{resolved}","authType":"oauth"}}"#
                        ),
                        0,
                    )
                }
            },
        );
        let expected = if credentials_missing {
            diagnostics::Availability::Unavailable
        } else if resolved == "openai" {
            diagnostics::Availability::Available
        } else {
            diagnostics::Availability::Unknown
        };
        assert_eq!(check.availability, expected);
        assert_eq!(
            calls[0],
            [
                "auth",
                "check",
                "--model",
                "openai/gpt-4o-mini",
                "--no-refresh",
                "--json"
            ]
        );
        if resolved == "openai" {
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[1],
                [
                    "auth",
                    "check",
                    "--provider",
                    "openai",
                    "--no-refresh",
                    "--json"
                ]
            );
            assert_eq!(deadlines[0], deadlines[1]);
        } else {
            assert_eq!(calls.len(), 1);
        }
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["model"], "openai/gpt-4o-mini");
        assert_eq!(evidence["executable"], "pi");
        assert_eq!(evidence["provider"], "openai");
        assert_eq!(evidence.as_object().unwrap().len(), 6);
        assert_eq!(
            evidence["authType"],
            if resolved == "openai" && !credentials_missing {
                serde_json::json!("oauth")
            } else {
                serde_json::Value::Null
            }
        );
        assert!(!evidence.to_string().contains("sensitive"));
        if credentials_missing {
            assert_eq!(check.id, "pi_provider_credentials");
            assert_eq!(check.reason_code, "pi_provider_credentials_not_ready");
            let check = serde_json::to_value(check).unwrap();
            assert_eq!(
                check["detail"],
                "Pi provider credentials not_ready for openai: Pi has no credentials configured for this provider."
            );
            assert_eq!(check["evidence"]["status"], "not_ready");
            assert_eq!(check["evidence"]["exit_code"], 1);
        }
    }
}
