use super::support::*;
use super::*;
use crate::native::session::primitives::STATE_DIR_ENV;
use crate::native::turn_run::emit_session_result_with;

#[test]
fn envelope_command_output_order() {
    const CHILD: &str = "AGENT_BRIDGE_ENVELOPE_COMMAND_TEST";
    if std::env::var_os(CHILD).is_some() {
        let directory = Reader::session_directory("session-fault").unwrap();
        let reader = Reader::open_unchecked(&directory);
        let snapshot = reader.snapshot().unwrap();
        let receipt = snapshot.receipts[0].clone();
        drop(snapshot);
        for (label, session, extra) in [
            ("QUERY", "session-fault", serde_json::Map::new()),
            ("FALLBACK", "session-missing", serde_json::Map::new()),
            (
                "EXTRA",
                "session-fault",
                serde_json::json!({"ok": true, "error": null, "schema_version": 9})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ] {
            println!("{label}_BEGIN");
            let error = finish_request_with_extra(
                Err(anyhow::anyhow!("inner").context("outer")),
                true,
                session,
                &receipt.request_id,
                extra,
            )
            .unwrap_err();
            assert!(error.to_string().starts_with(&format!(
                "session {session}, request {}",
                receipt.request_id
            )));
            assert!(format!("{error:#}").ends_with("outer: inner"));
            println!("{label}_END");
        }
        println!("PLAIN_FAILURE_BEGIN");
        assert!(
            finish_request_with_extra(
                Err(anyhow::anyhow!("plain")),
                false,
                "session-missing",
                "request-missing",
                serde_json::Map::new()
            )
            .is_err()
        );
        println!("PLAIN_FAILURE_END");
        let extra = serde_json::json!({"ok": false, "schema_version": 9, "error": "override", "z": "line\nnext"}).as_object().unwrap().clone();
        println!("SUCCESS_BEGIN");
        emit_session_result_with(
            true,
            "session-fault",
            &close_test_terminal(),
            FirstPartyCli::Pi,
            &receipt,
            None,
            &extra,
        )
        .unwrap();
        println!("SUCCESS_END");
        let mut receipt = receipt;
        receipt.request_id = "request-fixed".into();
        let event = sample_completion("123-456-0", "body\nnext").event;
        println!("PLAIN_SUCCESS_BEGIN");
        emit_session_result_with(
            false,
            "session-fault",
            &close_test_terminal(),
            FirstPartyCli::Pi,
            &receipt,
            Some(&event),
            &extra,
        )
        .unwrap();
        println!("PLAIN_SUCCESS_END");
        // Force the first inner operation to fail before any address or side effect.
        for (label, command, target, json) in [
            ("PRE_ASK", "ask", "pi", true),
            ("PRE_TELL", "tell", "session-missing", true),
            ("PRE_REOPEN", "reopen", "session-source", true),
            ("PRE_PLAIN", "ask", "pi", false),
            ("PRE_PLAIN_TELL", "tell", "session-missing", false),
            ("PRE_PLAIN_REOPEN", "reopen", "session-source", false),
        ] {
            let mut command = parse_args([command, target, "--prompt", "unused"]).unwrap();
            match &mut command {
                NativeCommand::Ask(request) => {
                    request.timeout = Duration::MAX;
                    request.json = json;
                }
                NativeCommand::Tell(request) => {
                    request.timeout = Duration::MAX;
                    request.json = json;
                }
                NativeCommand::Reopen(request) => {
                    request.timeout = Duration::MAX;
                    request.json = json;
                }
                _ => unreachable!(),
            }
            println!("{label}_BEGIN");
            let error = run(command).unwrap_err();
            assert_eq!(
                error.to_string(),
                checked_deadline_from(Instant::now(), Duration::MAX)
                    .unwrap_err()
                    .to_string()
            );
            println!("{label}_END");
        }
        for (label, before, extra) in [
            ("PRE_ASK", serde_json::json!({}), serde_json::json!({})),
            (
                "PRE_TELL",
                serde_json::json!({"session": "session-missing"}),
                serde_json::json!({}),
            ),
            (
                "PRE_REOPEN",
                serde_json::json!({"source_session": "session-source"}),
                serde_json::json!({"session": null, "request_id": null, "gate": null}),
            ),
        ] {
            for json in [true, false] {
                println!("REPORT_{label}_{json}_BEGIN");
                let report = TurnReport::new(json, before.as_object().unwrap().clone());
                let error = checked_deadline_from(Instant::now(), Duration::MAX).unwrap_err();
                let original = error.to_string();
                let error = report
                    .finish(Err(error), extra.as_object().unwrap().clone())
                    .unwrap_err();
                assert_eq!(error.to_string(), original);
                println!("REPORT_{label}_{json}_END");
            }
        }
        return;
    }
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let before = fs::read(fixture.directory.join("status.json")).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native::tests::output::envelope_command_output_order",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(STATE_DIR_ENV, fixture.directory.parent().unwrap())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    let section = |name: &str| {
        text.split(&format!("{name}_BEGIN\n"))
            .nth(1)
            .unwrap()
            .split(&format!("{name}_END\n"))
            .next()
            .unwrap()
            .to_owned()
    };
    let value = |name: &str| serde_json::from_str::<serde_json::Value>(&section(name)).unwrap();
    let expected_error = checked_deadline_from(Instant::now(), Duration::MAX)
        .unwrap_err()
        .to_string();
    let mut pre_address_mismatches = Vec::new();
    for (label, extra) in [
        ("PRE_ASK", serde_json::json!({})),
        (
            "PRE_TELL",
            serde_json::json!({"session": "session-missing"}),
        ),
        (
            "PRE_REOPEN",
            serde_json::json!({"source_session": "session-source", "session": null, "request_id": null, "gate": null}),
        ),
    ] {
        let expected = envelope::error(
            &anyhow::anyhow!(expected_error.clone()),
            extra.as_object().unwrap().clone(),
        );
        assert_eq!(section(&format!("REPORT_{label}_false")), "");
        assert_eq!(section(&format!("REPORT_{label}_true")), section(label));
        let expected = format!("{}\n", serde_json::to_string_pretty(&expected).unwrap());
        if section(label) != expected {
            pre_address_mismatches.push(format!(
                "{label}: actual {:?}, expected {expected:?}",
                section(label)
            ));
        }
    }
    assert_eq!(section("PRE_PLAIN"), "");
    assert_eq!(section("PRE_PLAIN_TELL"), "");
    assert_eq!(section("PRE_PLAIN_REOPEN"), "");
    assert!(
        pre_address_mismatches.is_empty(),
        "{pre_address_mismatches:#?}"
    );
    let queried = value("QUERY");
    assert_eq!(queried["ok"], false);
    assert_eq!(queried["error"], "outer: inner");
    assert_eq!(queried["result"], "late result");
    assert_eq!(queried["schema_version"], 1);
    let fallback = value("FALLBACK");
    assert_eq!(
        fallback,
        serde_json::json!({"ok": false, "error": "outer: inner", "schema_version": 1,
        "session": "session-missing", "request_id": fixture.request_id, "request_state": "unknown", "result": null})
    );
    let extra = value("EXTRA");
    assert_eq!(extra["ok"], true);
    assert_eq!(extra["error"], serde_json::Value::Null);
    assert_eq!(extra["schema_version"], 9);
    let success = value("SUCCESS");
    assert_eq!(success["ok"], false);
    assert_eq!(success["schema_version"], 9);
    assert_eq!(success["error"], "override");
    assert_eq!(success["request_state"], "accepted");
    assert_eq!(section("PLAIN_FAILURE").as_bytes(), b"");
    assert_eq!(section("PLAIN_SUCCESS").as_bytes(), b"session: session-fault\nrequest: request-fixed\nerror: override\nz: line\\nnext\n\nbody\nnext\n");
    assert_eq!(
        fs::read(fixture.directory.join("status.json")).unwrap(),
        before
    );
}
