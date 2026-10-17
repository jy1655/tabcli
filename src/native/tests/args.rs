use super::*;
use crate::native::args::parse_timeout;
use crate::native::console_control::{ConsoleControlRequest, parse as parse_console_control};
use crate::native::tell::TellRequest;
use crate::native::{
    AskRequest, COMMANDS, NativeCommand, context, is_command, parse_args, terminal,
};
use std::fs;
use std::path::Path;
use std::time::Duration;

#[test]
fn ask_yolo_is_false_unless_the_child_request_contains_the_flag() {
    let command = parse_args([
        "ask",
        "codex",
        "--workspace",
        "/tmp/project",
        "--prompt",
        "review this",
    ])
    .unwrap();

    assert!(matches!(
        command,
        NativeCommand::Ask(AskRequest {
            provider: FirstPartyCli::Codex,
            workspace,
            yolo: false,
            ..
        }) if workspace.as_path() == Path::new("/tmp/project")
    ));
}

#[test]
fn ask_and_tell_accept_prompt_files_without_putting_prompt_text_in_argv() {
    let directory = tempfile::tempdir().unwrap();
    let prompt_path = directory.path().join("prompt.txt");
    fs::write(&prompt_path, "long prompt\r\nfrom file").unwrap();
    let prompt_path = prompt_path.to_string_lossy().into_owned();

    let ask = parse_args(["ask", "claude", "--prompt-file", &prompt_path]).unwrap();
    assert!(matches!(
        ask,
        NativeCommand::Ask(AskRequest { prompt, .. }) if prompt == "long prompt\nfrom file"
    ));
    let tell = parse_args(["tell", "session-file123", "--prompt-file", &prompt_path]).unwrap();
    assert!(matches!(
        tell,
        NativeCommand::Tell(TellRequest { prompt, .. }) if prompt == "long prompt\nfrom file"
    ));
    assert!(
        parse_args([
            "ask",
            "claude",
            "--prompt",
            "inline",
            "--prompt-file",
            &prompt_path,
        ])
        .is_err()
    );
}

#[test]
fn ask_and_tell_accept_exact_repeatable_context_result_addresses() {
    let ask = parse_args([
        "ask",
        "codex",
        "--prompt",
        "continue",
        "--context-result",
        "session-a/request-1",
        "--context-result",
        "session-b/event-2.json",
    ])
    .unwrap();
    let NativeCommand::Ask(AskRequest {
        context_results, ..
    }) = ask
    else {
        panic!("expected ask");
    };
    assert_eq!(
        context_results
            .iter()
            .map(context::ContextResultRef::address)
            .collect::<Vec<_>>(),
        ["session-a/request-1", "session-b/event-2.json"]
    );
    let tell = parse_args([
        "tell",
        "session-target",
        "--prompt",
        "continue",
        "--context-result",
        "session-a/request-1",
    ])
    .unwrap();
    assert!(matches!(
        tell,
        NativeCommand::Tell(TellRequest { context_results, .. }) if context_results.len() == 1
    ));
    let plain = parse_args(["tell", "session-target", "--prompt", "continue"]).unwrap();
    assert!(matches!(
        plain,
        NativeCommand::Tell(TellRequest { context_results, .. }) if context_results.is_empty()
    ));

    for (arguments, expected) in [
        (
            vec!["tell", "session-t", "--prompt", "x", "--context-result"],
            "requires a value",
        ),
        (
            vec![
                "tell",
                "session-t",
                "--prompt",
                "x",
                "--context-result",
                "session-a/latest",
            ],
            "invalid --context-result",
        ),
        (
            vec![
                "ask",
                "codex",
                "--prompt",
                "x",
                "--context-result",
                "session-a/request-1",
                "--context-result",
                "session-a/request-1",
            ],
            "more than once",
        ),
        (
            vec!["ask", "codex", "--context-result", "session-a/request-1"],
            "requires --prompt",
        ),
    ] {
        let error = format!("{:#}", parse_args(arguments).unwrap_err());
        assert!(error.contains(expected), "{error}");
    }
    let mut nine = vec!["ask", "codex", "--prompt", "x"];
    let addresses = (1..=9)
        .map(|index| format!("session-a/request-{index}"))
        .collect::<Vec<_>>();
    for address in &addresses {
        nine.push("--context-result");
        nine.push(address);
    }
    assert!(format!("{:#}", parse_args(nine).unwrap_err()).contains("at most 8"));
}

#[test]
fn ask_rejects_terminal_control_sequences_from_inline_and_file_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let prompt_path = directory.path().join("prompt.txt");
    fs::write(&prompt_path, "unsafe\rprompt").unwrap();
    let prompt_path = prompt_path.to_string_lossy().into_owned();

    for arguments in [
        vec!["ask", "codex", "--prompt", "unsafe\u{1b}prompt"],
        vec!["ask", "pi", "--prompt-file", prompt_path.as_str()],
    ] {
        let error = parse_args(arguments).unwrap_err();
        assert!(
            format!("{error:#}").contains("terminal control"),
            "unexpected rejection: {error:#}"
        );
    }
}

#[test]
fn ask_model_is_supported_by_every_native_provider() {
    for provider in ["codex", "claude", "agy", "pi"] {
        let command = parse_args([
            "ask",
            provider,
            "--prompt",
            "review this",
            "--model",
            "provider-model",
        ])
        .unwrap_or_else(|error| panic!("{provider} rejected --model: {error:#}"));

        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                model: Some(model),
                ..
            }) if model == "provider-model"
        ));
    }
}

#[test]
fn model_and_effort_are_not_inherited_when_omitted() {
    for provider in ["codex", "claude", "agy", "pi"] {
        let command = parse_args(["ask", provider, "--prompt", "review this"]).unwrap();
        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                model: None,
                effort: None,
                ..
            })
        ));
    }
}

#[test]
fn ask_effort_is_supported_by_every_native_provider() {
    for (provider, requested_effort) in [
        ("codex", "xhigh"),
        ("claude", "max"),
        ("agy", "high"),
        ("pi", "minimal"),
    ] {
        let command = parse_args([
            "ask",
            provider,
            "--prompt",
            "review this",
            "--effort",
            requested_effort,
        ])
        .unwrap();

        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                effort: Some(effort),
                ..
            }) if effort == requested_effort
        ));
    }
}

#[test]
fn ask_terminal_can_be_selected_without_changing_the_auto_default() {
    let automatic = parse_args(["ask", "pi", "--prompt", "review this"]).unwrap();
    assert!(matches!(
        automatic,
        NativeCommand::Ask(AskRequest { terminal: None, .. })
    ));

    for (requested, expected) in [
        ("ghostty", terminal::TerminalKind::Ghostty),
        ("iterm2", terminal::TerminalKind::Iterm2),
        ("terminal", terminal::TerminalKind::AppleTerminal),
    ] {
        let command = parse_args([
            "ask",
            "pi",
            "--prompt",
            "review this",
            "--terminal",
            requested,
        ])
        .unwrap();
        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                terminal: Some(actual),
                ..
            }) if actual == expected
        ));
    }

    assert!(
        parse_args([
            "ask",
            "pi",
            "--prompt",
            "review this",
            "--terminal",
            "vscode",
        ])
        .is_err()
    );
}

#[test]
fn native_ask_and_tell_reject_unrepresentable_timeouts() {
    let too_large = u64::MAX.to_string();
    assert!(
        parse_args([
            "ask",
            "codex",
            "--prompt",
            "review this",
            "--timeout-secs",
            &too_large,
        ])
        .is_err()
    );
    assert!(
        parse_args([
            "tell",
            "session-safe123",
            "--prompt",
            "continue",
            "--timeout-secs",
            &too_large,
        ])
        .is_err()
    );
}

#[test]
fn close_is_rejected_without_the_explicit_flag() {
    assert!(parse_args(["close-session", "session-safe123"]).is_err());
    assert!(parse_args(["close-session", "session-safe123", "--explicit"]).is_ok());
}

#[test]
fn prune_sessions_requires_an_explicit_positive_retention_window() {
    assert!(parse_args(["prune-sessions", "--closed-before-days", "30"]).is_err());
    assert!(parse_args(["prune-sessions", "--closed-before-days", "0", "--explicit",]).is_err());
    assert!(parse_args(["prune-sessions", "--closed-before-days", "30", "--explicit",]).is_ok());
}

#[test]
fn search_rejects_empty_queries_conflicting_scopes_and_out_of_range_limits() {
    for args in [
        vec!["search"],
        vec!["search", ""],
        vec!["search", " 	 "],
        vec!["search", "needle", "--workspace", ".", "--all-workspaces"],
        vec!["search", "needle", "--all-workspaces", "--all-workspaces"],
        vec!["search", "needle", "--limit", "0"],
        vec!["search", "needle", "--limit", "201"],
        vec!["search", "needle", "--limit", "many"],
        vec!["search", "needle", "--limit"],
        vec!["search", "needle", "--provider", "gpt"],
        vec!["search", "needle", "--unknown"],
    ] {
        assert!(parse_args(args.clone()).is_err(), "{args:?}");
    }
    let error = parse_args(["search", "needle", "--workspace", ".", "--all-workspaces"])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only one of --workspace or --all-workspaces"),
        "{error}"
    );
    let error = parse_args(["search", "needle", "--limit", "201"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("between 1 and 200"), "{error}");
    assert!(matches!(
        parse_args([
            "search",
            "needle",
            "--all-workspaces",
            "--limit",
            "200",
            "--json"
        ])
        .unwrap(),
        NativeCommand::Search(_)
    ));
    assert!(parse_args(["search", "needle", "--workspace", "."]).is_ok());
}

// The tab's shell reaches the gate only through this command: an unrouted name would
// end every Terminal.app launch at its first line.
#[test]
fn terminal_host_command_takes_one_absolute_session_directory() {
    let root = tempfile::tempdir().unwrap();
    let valid_path = root.path().join("session-abc");
    let invalid_path = root.path().join("not-a-session");
    let valid = valid_path.to_str().unwrap();
    let invalid = invalid_path.to_str().unwrap();
    assert!(is_command("native-terminal-host"));
    assert!(matches!(
        parse_args(["native-terminal-host", valid]).unwrap(),
        NativeCommand::Host(terminal::host::HostCommand::AppleTerminal { directory }) if directory == valid_path
    ));
    for arguments in [
        vec!["native-terminal-host"],
        vec!["native-terminal-host", "state/session-abc"],
        vec!["native-terminal-host", invalid],
        vec!["native-terminal-host", valid, "extra"],
    ] {
        assert!(parse_args(&arguments).is_err(), "{arguments:?}");
    }
}

// The surface's shell reaches the host only through this command. It takes nothing:
// the surface exists before a session is bound to it.
#[test]
fn ghostty_host_command_takes_no_argument() {
    assert!(is_command("native-ghostty-host"));
    assert!(matches!(
        parse_args(["native-ghostty-host"]).unwrap(),
        NativeCommand::Host(terminal::host::HostCommand::Ghostty)
    ));
    assert!(parse_args(["native-ghostty-host", "/state/session-abc"]).is_err());
}

#[test]
fn close_deadline_cli_accepts_only_command_specific_range() {
    for value in ["1", "60", "3600"] {
        assert!(
            parse_args([
                "close-session",
                "session-safe123",
                "--explicit",
                "--timeout-secs",
                value
            ])
            .is_ok()
        );
    }
    for value in ["0", "3601", "-1", "abc", "18446744073709551616"] {
        assert!(
            parse_args([
                "close-session",
                "session-safe123",
                "--explicit",
                "--timeout-secs",
                value
            ])
            .is_err()
        );
    }
    assert!(
        parse_args([
            "close-session",
            "session-safe123",
            "--explicit",
            "--timeout-secs"
        ])
        .is_err()
    );
    assert!(
        parse_args([
            "close-session",
            "session-safe123",
            "--explicit",
            "--timeout-secs",
            "1",
            "--timeout-secs",
            "2"
        ])
        .is_err()
    );
    assert!(parse_args(["close-session", "session-safe123", "--timeout-secs", "1"]).is_err());
    assert!(parse_timeout("3601").is_ok());
}

#[test]
fn close_deadline_cli_default_and_bounds() {
    for (options, expected) in [
        (vec![], 60),
        (vec!["--timeout-secs", "1"], 1),
        (vec!["--timeout-secs", "3600"], 3600),
    ] {
        let mut args = vec!["close-session", "session-safe123", "--explicit"];
        args.extend(options);
        let NativeCommand::Close(request) = parse_args(args).unwrap() else {
            panic!()
        };
        assert_eq!(request.timeout, Duration::from_secs(expected));
    }
    let NativeCommand::Tell(request) = parse_args([
        "tell",
        "session-safe123",
        "--timeout-secs",
        "3601",
        "--prompt",
        "hello",
    ])
    .unwrap() else {
        panic!()
    };
    assert_eq!(request.timeout, Duration::from_secs(3601));
}

#[test]
fn all_native_host_names_keep_variants_and_argument_validation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session-abc");
    let directory = path.to_str().unwrap();
    for name in [
        "native-console-host",
        "native-iterm2-host",
        "native-terminal-host",
        "native-wezterm-host",
    ] {
        assert!(is_command(name));
        let parsed = parse_args([name, directory]).unwrap();
        let actual = match parsed {
            NativeCommand::Host(terminal::host::HostCommand::Console { directory }) => {
                ("native-console-host", directory)
            }
            NativeCommand::Host(terminal::host::HostCommand::Iterm2 { directory }) => {
                ("native-iterm2-host", directory)
            }
            NativeCommand::Host(terminal::host::HostCommand::AppleTerminal { directory }) => {
                ("native-terminal-host", directory)
            }
            NativeCommand::Host(terminal::host::HostCommand::WezTerm { directory }) => {
                ("native-wezterm-host", directory)
            }
            other => panic!("unexpected host: {other:?}"),
        };
        assert_eq!(actual, (name, path.clone()));
        for args in [
            vec![name],
            vec![name, "relative/session-abc"],
            vec![name, directory, "extra"],
        ] {
            assert!(parse_args(args).is_err());
        }
        assert!(parse_args([name, root.path().join("invalid").to_str().unwrap()]).is_err());
    }
    assert!(is_command("native-ghostty-host"));
    assert!(matches!(
        parse_args(["native-ghostty-host"]).unwrap(),
        NativeCommand::Host(terminal::host::HostCommand::Ghostty)
    ));
    assert!(parse_args(["native-ghostty-host", directory]).is_err());
    let token = "0123456789abcdef0123456789abcdef";
    assert!(is_command("native-warp-host"));
    assert!(
        matches!(parse_args(["native-warp-host", directory, token]).unwrap(), NativeCommand::Host(terminal::host::HostCommand::Warp { directory: d, attempt }) if d == path && attempt == token)
    );
    for args in [
        vec!["native-warp-host"],
        vec!["native-warp-host", directory],
        vec!["native-warp-host", directory, "bad"],
        vec!["native-warp-host", "relative/session-abc", token],
        vec!["native-warp-host", directory, token, "extra"],
    ] {
        assert!(parse_args(args).is_err());
    }
    assert!(!is_command("native-unknown-host"));
    assert!(
        parse_args(["native-unknown-host", directory])
            .unwrap_err()
            .to_string()
            .contains("unknown native command")
    );
}

#[test]
fn registry_routes_every_command_to_its_variant_and_fields() {
    // Debug exposes private module request fields without widening their visibility for tests.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-route");
    let directory_text = directory.to_str().unwrap();
    let workspace = std::env::current_dir().unwrap();
    let ask = format!(
        "AskRequest {{ provider: Codex, workspace: {workspace:?}, prompt: \"hello\", title: None, model: None, effort: None, terminal: None, yolo: false, timeout: 900s, detach: false, json: false, context_results: [] }}"
    );
    let self_test_ask = format!(
        "AskRequest {{ provider: Pi, workspace: {workspace:?}, prompt: \"self-test\", title: None, model: None, effort: None, terminal: None, yolo: false, timeout: 120s, detach: false, json: true, context_results: [] }}"
    );
    let token = "0123456789abcdef0123456789abcdef";
    // Independent name literals pin the portable declarations as well as hidden routing.
    for (expected, actual) in [
        ("native-console-host", terminal::host::CONSOLE_HOST_COMMAND),
        ("native-iterm2-host", terminal::host::ITERM2_HOST_COMMAND),
        (
            "native-terminal-host",
            terminal::host::APPLE_TERMINAL_HOST_COMMAND,
        ),
        ("native-wezterm-host", terminal::host::WEZTERM_HOST_COMMAND),
        ("native-ghostty-host", terminal::host::GHOSTTY_HOST_COMMAND),
        ("native-warp-host", terminal::host::WARP_HOST_COMMAND),
    ] {
        assert_eq!(actual, expected);
        let row = COMMANDS.iter().find(|row| row.name == actual).unwrap();
        assert!(row.hidden, "{expected}");
    }
    // The host entries keep their registry order: the registrations supply the
    // names, the table supplies the order.
    let hosts: Vec<_> = COMMANDS
        .iter()
        .map(|row| row.name)
        .filter(|name| name.starts_with("native-") && name.ends_with("-host"))
        .collect();
    assert_eq!(
        hosts,
        [
            "native-console-host",
            "native-iterm2-host",
            "native-terminal-host",
            "native-wezterm-host",
            "native-ghostty-host",
            "native-warp-host",
        ]
    );
    assert_eq!(
        COMMANDS.len() - hosts.len(),
        COMMANDS
            .iter()
            .position(|row| row.name == hosts[0])
            .unwrap(),
        "the host entries end the registry"
    );
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec!["ask", "codex", "--prompt", "hello"], format!("Ask({ask})")),
        (vec!["self-test", "pi", "--isolated", "--json"], format!("SelfTest(Request {{ ask: {self_test_ask}, isolated: true }})")),
        (vec!["tell", "session-route", "--prompt", "hello"], "Tell(TellRequest { id: \"session-route\", prompt: \"hello\", timeout: 900s, detach: false, json: false, context_results: [] })".into()),
        (vec!["hold", "session-route", "--release", "--json"], "Hold(Request { id: \"session-route\", release: true, json: true })".into()),
        (vec!["cancel", "session-route", "--json"], "Cancel(Request { id: \"session-route\", json: true })".into()),
        (vec!["consent", "arbitrary", "--bad"], "Consent([\"arbitrary\", \"--bad\"])".into()),
        (vec!["settings", "arbitrary", "--bad"], "Settings([\"arbitrary\", \"--bad\"])".into()),
        (vec!["reopen", "session-route", "--prompt", "hello"], "Reopen(ReopenRequest { id: \"session-route\", prompt: \"hello\", title: None, model: None, effort: None, terminal: None, yolo: false, timeout: 900s, detach: false, json: false })".into()),
        (vec!["inspect", "session-route", "--timeline", "--request", "request-1", "--json"], "Inspect { id: \"session-route\", json: true, timeline: true, request: Some(\"request-1\") }".into()),
        (vec!["result", "session-route", "--request", "request-1", "--wait", "--json"], "Result(ResultRequest { id: \"session-route\", selector: Request(\"request-1\"), wait: true, timeout: 900s, json: true })".into()),
        (vec!["wait", "session-route/request-1", "--timeout-secs", "7", "--json"], "Wait(WaitRequest { addresses: [Address { session: \"session-route\", request: \"request-1\" }], timeout: 7s, json: true })".into()),
        (vec!["search", "needle", "--all-workspaces", "--provider", "claude", "--json"], "Search(SearchRequest { query: \"needle\", scope: AllWorkspaces, provider: Some(Claude), limit: 20, byte_budget: 67108864, json: true })".into()),
        (vec!["doctor", "--provider", "agy", "--probe", "--json"], "Doctor(DoctorRequest { session: None, provider: Some(Agy), probe: true, json: true })".into()),
        (vec!["sessions", "--provider", "pi", "--state", "ready", "--sort", "updated", "--json"], "Sessions(SessionsRequest { json: true, workspace: None, provider: Some(Pi), state: Some(\"ready\"), sort_updated: true })".into()),
        (vec!["status", "--all-workspaces", "--provider", "codex", "--all", "--json"], "Status(StatusRequest { scope: AllWorkspaces, provider: Some(Codex), include_closed: true, json: true })".into()),
        (vec!["prune-sessions", "--closed-before-days", "30", "--explicit", "--json"], "Prune(PruneRequest { closed_before_days: 30, explicit: true, json: true })".into()),
        (vec!["close-session", "session-route", "--explicit", "--timeout-secs", "7", "--json"], "Close(CloseRequest { id: \"session-route\", explicit: true, json: true, with_started_from: false, timeout: 7s })".into()),
        (vec!["native-session", "session-route"], "RunSession { id: \"session-route\" }".into()),
        (vec!["native-hook", "codex", "{}"], "Hook { provider: Codex, payload: Some(\"{}\") }".into()),
        (vec!["native-provider-control", "claude", "operation", "payload"], "ProviderControl { provider: Claude, arguments: [\"operation\", \"payload\"] }".into()),
        (vec!["native-console-control", "send", "session-route", "pending-prompt-route.txt", "7"], "ConsoleControl { request: Send { input_name: \"pending-prompt-route.txt\", timeout: 7ms }, id: \"session-route\" }".into()),
        (vec!["native-console-host", directory_text], format!("Host(Console {{ directory: {directory:?} }})")),
        (vec!["native-iterm2-host", directory_text], format!("Host(Iterm2 {{ directory: {directory:?} }})")),
        (vec!["native-terminal-host", directory_text], format!("Host(AppleTerminal {{ directory: {directory:?} }})")),
        (vec!["native-wezterm-host", directory_text], format!("Host(WezTerm {{ directory: {directory:?} }})")),
        (vec!["native-ghostty-host"], "Host(Ghostty)".into()),
        (vec!["native-warp-host", directory_text, token], format!("Host(Warp {{ directory: {directory:?}, attempt: {token:?} }})")),
    ];
    let covered = cases
        .iter()
        .map(|(args, _)| args[0])
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(covered.len(), cases.len());
    assert_eq!(covered, COMMANDS.iter().map(|entry| entry.name).collect());
    for (args, expected) in cases {
        assert_eq!(
            format!("{:?}", parse_args(&args).unwrap()),
            expected,
            "{args:?}"
        );
    }
}

#[test]
fn console_control_parser_preserves_positional_action_contract() {
    let timeout = Duration::from_millis(7);
    for (tail, expected) in [
        (
            vec!["screen", "session-route"],
            ConsoleControlRequest::Screen,
        ),
        (vec!["close", "session-route"], ConsoleControlRequest::Close),
        (
            vec!["window", "session-route", "7"],
            ConsoleControlRequest::Window { timeout },
        ),
        (
            vec!["send", "session-route", "pending-prompt-route.txt", "7"],
            ConsoleControlRequest::Send {
                input_name: "pending-prompt-route.txt".into(),
                timeout,
            },
        ),
        (
            vec!["dialog", "session-route", "pending-prompt-route.txt", "7"],
            ConsoleControlRequest::Dialog {
                input_name: "pending-prompt-route.txt".into(),
                timeout,
            },
        ),
    ] {
        let NativeCommand::ConsoleControl { request, id } =
            parse_console_control(&tail.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(request, expected);
        assert_eq!(id, "session-route");
    }
    for tail in [
        vec!["screen", "session-route", "7"],
        vec!["close", "session-route", "7"],
        vec!["window", "session-route"],
        vec!["window", "session-route", "7", "extra"],
        vec!["send", "session-route"],
        vec!["send", "session-route", "pending-prompt-route.txt"],
        vec![
            "send",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "extra",
        ],
        vec!["dialog", "session-route"],
        vec!["dialog", "session-route", "pending-prompt-route.txt"],
        vec![
            "dialog",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "extra",
        ],
    ] {
        let error =
            parse_console_control(&tail.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid native console control arguments",
            "{tail:?}"
        );
    }
    for action in ["send", "dialog"] {
        for input in [
            "outside.txt".to_owned(),
            "../pending-prompt-route.txt".into(),
            "pending-prompt-route.json".into(),
            format!("pending-prompt-{}.txt", "x".repeat(110)),
            format!("pending-prompt-{}.txt", "가".repeat(37)),
        ] {
            let error = parse_args([
                "native-console-control",
                action,
                "session-route",
                &input,
                "invalid",
            ])
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid native console control arguments"
            );
        }
        let longest = format!("pending-prompt-{}.txt", "x".repeat(109));
        assert!(
            parse_args([
                "native-console-control",
                action,
                "session-route",
                &longest,
                "18446744073709551615"
            ])
            .is_ok()
        );
    }
    for action in ["send", "dialog", "window"] {
        for (timeout, expected) in [
            ("0", "native console timeout must be positive"),
            (
                "bad",
                "invalid native console timeout: invalid digit found in string",
            ),
            (
                "18446744073709551616",
                "invalid native console timeout: number too large to fit in target type",
            ),
        ] {
            let mut args = vec!["native-console-control", action, "session-route"];
            if action != "window" {
                args.push("pending-prompt-route.txt");
            }
            args.push(timeout);
            assert_eq!(format!("{:#}", parse_args(args).unwrap_err()), expected);
        }
    }
}

#[test]
fn host_parser_errors_preserve_validation_precedence() {
    let error = |args: &[&str]| format!("{:#}", parse_args(args).unwrap_err());
    for (args, expected) in [
        (
            vec!["native-console-control", "bad"],
            "native-console-control requires an action and managed session id",
        ),
        (
            vec![
                "native-console-control",
                "bad",
                "bad-id",
                "bad-input",
                "bad-timeout",
            ],
            "invalid Agent Bridge session id: \"bad-id\"",
        ),
        (
            vec![
                "native-console-control",
                "bad",
                "session-route",
                "bad-input",
                "bad-timeout",
            ],
            "unsupported native console action: bad",
        ),
        (
            vec![
                "native-console-control",
                "send",
                "session-route",
                "bad-input",
                "bad-timeout",
            ],
            "invalid native console control arguments",
        ),
        (
            vec![
                "native-console-control",
                "send",
                "session-route",
                "pending-prompt-route.txt",
                "bad-timeout",
            ],
            "invalid native console timeout: invalid digit found in string",
        ),
        (
            vec![
                "native-console-control",
                "send",
                "session-route",
                "pending-prompt-route.txt",
                "0",
            ],
            "native console timeout must be positive",
        ),
    ] {
        assert_eq!(error(&args), expected, "{args:?}");
    }
    for name in [
        "native-console-host",
        "native-iterm2-host",
        "native-terminal-host",
        "native-wezterm-host",
    ] {
        assert_eq!(
            error(&[name, "bad-id", "extra"]),
            format!("{name} requires one session directory")
        );
        assert_eq!(
            error(&[name, ""]),
            format!("{name} requires a session directory")
        );
        assert_eq!(
            error(&[name, "bad-id"]),
            "invalid Agent Bridge session id: \"bad-id\""
        );
        assert_eq!(
            error(&[name, "session-route"]),
            format!("{name} requires an absolute session directory")
        );
    }
    assert_eq!(
        error(&["native-warp-host", "bad-id"]),
        "native-warp-host requires a session directory and attempt token"
    );
    assert_eq!(
        error(&["native-warp-host", "", "bad"]),
        "native-warp-host requires a session directory"
    );
    assert_eq!(
        error(&["native-warp-host", "bad-id", "bad"]),
        "invalid Agent Bridge session id: \"bad-id\""
    );
    assert_eq!(
        error(&["native-warp-host", "session-route", "bad"]),
        "native-warp-host requires an absolute session directory"
    );
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-route");
    assert_eq!(
        error(&["native-warp-host", directory.to_str().unwrap(), "bad"]),
        "native-warp-host requires a valid attempt token"
    );
}
