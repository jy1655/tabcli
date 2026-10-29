use super::*;
use crate::native::args::{
    CommonStartOptions, OptionSet, parse_common_start_option, validate_start_options,
};
use crate::native::reopen::parse_reopen;
use crate::native::session_start::parse_ask;
use crate::native::{NativeCommand, tell, terminal};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

fn parse(command: &str, options: &[&str]) -> Result<NativeCommand> {
    let mut args = vec![
        if command == "ask" {
            "codex"
        } else {
            "session-source"
        }
        .to_owned(),
    ];
    args.extend(options.iter().map(|s| (*s).to_owned()));
    if command == "ask" {
        parse_ask(&args)
    } else {
        parse_reopen(&args)
    }
}

fn error(command: &str, options: &[&str], expected: &str) {
    let actual = parse(command, options).unwrap_err();
    assert_eq!(
        actual.to_string(),
        expected,
        "{command} {options:?}: {actual:#}"
    );
}

#[test]
fn common_error_precedence_matrix() {
    for command in ["ask", "reopen"] {
        for token in ["extra", "--", "--prompt=x", "--Prompt", "--unknown"] {
            error(
                command,
                &[token],
                &format!("unknown {command} option: {token}"),
            );
        }
        for (option, value) in [
            ("--prompt", "x"),
            ("--prompt-file", "x"),
            ("--title", "x"),
            ("--model", "x"),
            ("--effort", "x"),
            ("--terminal", "iterm2"),
            ("--timeout-secs", "1"),
        ] {
            error(command, &[option], &format!("{option} requires a value"));
            error(
                command,
                &[option, value, option],
                &format!("{option} requires a value"),
            );
            error(
                command,
                &[option, value, option, value],
                &format!("{option} may only be specified once"),
            );
        }
        for flag in ["--yolo", "--detach", "--json"] {
            error(
                command,
                &[flag, flag],
                &format!("{flag} may only be specified once"),
            );
            error(
                command,
                &[flag, "false"],
                &format!("unknown {command} option: false"),
            );
        }
        for options in [
            vec!["--prompt", "x", "--prompt-file", "absent"],
            vec!["--prompt-file", "absent", "--prompt", "x"],
        ] {
            error(
                command,
                &options,
                &format!("{command} accepts only one of --prompt or --prompt-file"),
            );
            let mut later_unknown = options;
            later_unknown.push("--bad");
            error(
                command,
                &later_unknown,
                &format!("unknown {command} option: --bad"),
            );
        }
        error(
            command,
            &["--model", "", "--effort", ""],
            &format!("{command} requires --prompt <text> or --prompt-file <path>"),
        );
        for prompt in ["", " \t\r\n"] {
            error(
                command,
                &["--prompt", prompt, "--model", ""],
                "--prompt cannot be empty",
            );
        }
        for (prompt, code) in [("x\r\n", "000D"), ("x\0", "0000"), ("x\x1b", "001B")] {
            error(
                command,
                &["--prompt", prompt, "--model", ""],
                &format!(
                    "--prompt contains terminal control U+{code}; only newline and tab are allowed"
                ),
            );
        }
        error(
            command,
            &["--prompt", "x", "--model", "", "--effort", ""],
            "--model cannot be empty",
        );
        error(
            command,
            &["--prompt", "x", "--effort", " "],
            "--effort cannot be empty",
        );
        error(
            command,
            &["--terminal", "iterm2", "--terminal", "bad"],
            "unsupported terminal \"bad\"; expected ghostty, iterm2, terminal, warp, wezterm, or windows-console",
        );
        for options in [
            vec!["--timeout-secs", "0"],
            vec!["--timeout-secs", "1", "--timeout-secs", "0"],
        ] {
            error(command, &options, "timeout must be greater than zero");
        }
        for value in ["bad", "-1", "18446744073709551616"] {
            let err = parse(command, &["--timeout-secs", value]).unwrap_err();
            assert_eq!(err.to_string(), format!("invalid timeout: {value}"));
            assert!(err.chain().count() > 1);
        }
        let overflow = parse(command, &["--timeout-secs", "18446744073709551615"]).unwrap_err();
        assert_eq!(
            overflow.to_string(),
            "timeout of 18446744073709551615 seconds cannot be represented by this platform's monotonic clock"
        );
    }
}

#[test]
fn positional_and_command_specific_matrix() {
    assert_eq!(
        parse_ask(&[]).unwrap_err().to_string(),
        "ask requires codex, claude, agy, or pi"
    );
    assert_eq!(
        parse_reopen(&[]).unwrap_err().to_string(),
        "reopen requires one closed session id"
    );
    for value in ["CODEX", "other", "--help", "-h"] {
        assert_eq!(
            parse_ask(&[value.into()]).unwrap_err().to_string(),
            format!("unsupported CLI {value:?}; expected codex, claude, agy, or pi")
        );
    }
    for value in [
        "bad".to_owned(),
        "session-한".into(),
        "session-x/".into(),
        format!("session-{}", "a".repeat(89)),
    ] {
        assert_eq!(
            parse_reopen(std::slice::from_ref(&value))
                .unwrap_err()
                .to_string(),
            format!("invalid Agent Bridge session id: {value:?}")
        );
    }
    for value in [
        "session-".to_owned(),
        "session-a_A9".into(),
        format!("session-{}", "a".repeat(88)),
    ] {
        assert!(parse_reopen(&[value, "--prompt".into(), "x".into()]).is_ok());
    }
    for provider in ["codex", "claude", "agy", "pi"] {
        assert!(parse_ask(&[provider.into(), "--prompt".into(), "x".into()]).is_ok());
    }
    for option in ["--workspace", "--context-result"] {
        error(
            "reopen",
            &[option],
            &format!("unknown reopen option: {option}"),
        );
    }
    error("ask", &["--workspace"], "--workspace requires a value");
    error(
        "ask",
        &["--workspace", "x", "--workspace", "y"],
        "--workspace may only be specified once",
    );
    error(
        "ask",
        &["--context-result"],
        "--context-result requires a value",
    );
    let refs: Vec<String> = (0..9).map(|i| format!("session-s/request-{i}")).collect();
    let mut opts = vec!["--prompt", "x"];
    for address in &refs[..8] {
        opts.extend(["--context-result", address]);
    }
    let NativeCommand::Ask(request) = parse("ask", &opts).unwrap() else {
        panic!()
    };
    assert_eq!(
        request
            .context_results
            .iter()
            .map(|r| r.address())
            .collect::<Vec<_>>(),
        refs[..8]
    );
    for (value, expected) in [
        (refs[8].as_str(), "--context-result may be given at most 8 times".to_owned()),
        (refs[0].as_str(), format!("--context-result {} was given more than once", refs[0])),
        ("bad", "invalid --context-result value \"bad\"; expected <session-id>/<request-id> or <session-id>/<event-id>".into()),
    ] {
        let mut options = opts.clone(); options.extend(["--context-result", value]); error("ask", &options, &expected);
    }
    assert!(
        parse(
            "ask",
            &[
                "--prompt",
                "x",
                "--context-result",
                "session-s/event-1-1.json"
            ]
        )
        .is_ok()
    );
}

// Compare common request fields without changing the public request types.
type CommonFields = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<terminal::TerminalKind>,
    bool,
    Duration,
    bool,
    bool,
);

fn common(request: NativeCommand) -> CommonFields {
    match request {
        NativeCommand::Ask(r) => {
            assert_eq!(r.provider, FirstPartyCli::Codex);
            (
                r.prompt, r.title, r.model, r.effort, r.terminal, r.yolo, r.timeout, r.detach,
                r.json,
            )
        }
        NativeCommand::Reopen(r) => {
            assert_eq!(r.id, "session-source");
            (
                r.prompt, r.title, r.model, r.effort, r.terminal, r.yolo, r.timeout, r.detach,
                r.json,
            )
        }
        _ => panic!(),
    }
}

#[test]
fn success_defaults_values_and_terminal_alias_matrix() {
    for command in ["ask", "reopen"] {
        let r = common(parse(command, &["--prompt", "--json"]).unwrap());
        assert_eq!(
            r,
            (
                "--json".into(),
                None,
                None,
                None,
                None,
                false,
                Duration::from_secs(900),
                false,
                false
            )
        );
        let r = common(
            parse(
                command,
                &[
                    "--prompt",
                    " x\n\t ",
                    "--title",
                    "\x1b",
                    "--model",
                    " x\x1b ",
                    "--effort",
                    " arbitrary ",
                    "--yolo",
                    "--detach",
                    "--json",
                    "--timeout-secs",
                    "2",
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            r,
            (
                " x\n\t ".into(),
                Some("\x1b".into()),
                Some(" x\x1b ".into()),
                Some(" arbitrary ".into()),
                None,
                true,
                Duration::from_secs(2),
                true,
                true
            )
        );
        assert_eq!(
            common(parse(command, &["--prompt", "x", "--title", ""]).unwrap()).1,
            Some(String::new())
        );
        for (values, kind) in [
            (&["ghostty", "GHOSTTY"][..], terminal::TerminalKind::Ghostty),
            (&["warp", "warpterminal"], terminal::TerminalKind::Warp),
            (&["wezterm"], terminal::TerminalKind::WezTerm),
            (
                &["windows-console", "windows", "console"],
                terminal::TerminalKind::WindowsConsole,
            ),
            (
                &["iterm", "iterm.app", "iterm2", "ITERM2"],
                terminal::TerminalKind::Iterm2,
            ),
            (
                &[
                    "apple-terminal",
                    "apple_terminal",
                    "default",
                    "terminal",
                    "terminal.app",
                ],
                terminal::TerminalKind::AppleTerminal,
            ),
        ] {
            for value in values {
                assert_eq!(
                    common(parse(command, &["--prompt", "x", "--terminal", value]).unwrap()).4,
                    Some(kind)
                );
            }
        }
    }
    let NativeCommand::Ask(default) = parse("ask", &["--prompt", "x"]).unwrap() else {
        panic!()
    };
    assert_eq!(default.workspace, std::env::current_dir().unwrap());
    assert!(default.context_results.is_empty());
    let NativeCommand::Ask(explicit) =
        parse("ask", &["--prompt", "x", "--workspace", "relative"]).unwrap()
    else {
        panic!()
    };
    assert_eq!(explicit.workspace, PathBuf::from("relative"));
    let options = vec!["--workspace".into()];
    let mut index = 0;
    assert!(
        !parse_common_start_option(
            &options,
            &mut index,
            &mut CommonStartOptions::default(),
            OptionSet::Start
        )
        .unwrap()
    );
    assert_eq!(index, 0);
}

// tell parses through the common option helper, with the follow-up set: the options that
// shape a session are not consumed and reach tell's own rejection, as before.
#[test]
fn tell_rejects_session_shaping_options_as_unknown() {
    let tell_error = |options: &[&str]| {
        let mut args = vec!["session-target".to_owned()];
        args.extend(options.iter().map(|option| (*option).to_owned()));
        tell::parse(&args).unwrap_err().to_string()
    };
    for (option, value) in [
        ("--title", "x"),
        ("--model", "x"),
        ("--effort", "x"),
        ("--terminal", "iterm2"),
        ("--yolo", "x"),
    ] {
        assert_eq!(
            tell_error(&["--prompt", "x", option, value]),
            format!("unknown tell option: {option}")
        );
        assert_eq!(
            tell_error(&[option, value, "--prompt", "x"]),
            format!("unknown tell option: {option}")
        );
        // The follow-up set leaves the token where it stands, before any value is read.
        let options = vec![option.to_owned()];
        let mut index = 0;
        assert!(
            !parse_common_start_option(
                &options,
                &mut index,
                &mut CommonStartOptions::default(),
                OptionSet::FollowUp
            )
            .unwrap()
        );
        assert_eq!(index, 0);
    }
    // The options a follow-up does take keep their precedence and messages.
    for (option, value) in [
        ("--prompt", "x"),
        ("--prompt-file", "x"),
        ("--timeout-secs", "1"),
    ] {
        assert_eq!(tell_error(&[option]), format!("{option} requires a value"));
        assert_eq!(
            tell_error(&[option, value, option, value]),
            format!("{option} may only be specified once")
        );
    }
    for flag in ["--detach", "--json"] {
        assert_eq!(
            tell_error(&[flag, flag]),
            format!("{flag} may only be specified once")
        );
    }
    assert_eq!(
        tell_error(&[]),
        "tell requires --prompt <text> or --prompt-file <path>"
    );
    assert_eq!(tell_error(&["--prompt", " \n"]), "--prompt cannot be empty");
    assert_eq!(
        tell_error(&["--prompt", "x\x1b"]),
        "--prompt contains terminal control U+001B; only newline and tab are allowed"
    );
    let NativeCommand::Tell(request) = tell::parse(&[
        "session-target".into(),
        "--prompt".into(),
        "x".into(),
        "--timeout-secs".into(),
        "2".into(),
        "--detach".into(),
        "--json".into(),
    ])
    .unwrap() else {
        panic!()
    };
    assert_eq!(request.prompt, "x");
    assert_eq!(request.timeout, Duration::from_secs(2));
    assert!(request.json);
}

// The one validation every start command applies, in its order: the prompt is read, then
// checked for emptiness and terminal control, then `--model` and `--effort` for blankness.
#[test]
fn shared_validation_produces_the_four_messages_in_order() {
    let options =
        |prompt: Option<&str>, model: Option<&str>, effort: Option<&str>| CommonStartOptions {
            prompt: prompt.map(str::to_owned),
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
            ..CommonStartOptions::default()
        };
    let message = |parsed: CommonStartOptions| {
        validate_start_options(parsed, "ask")
            .err()
            .map(|error| error.to_string())
    };
    assert_eq!(
        message(options(None, Some(""), Some(""))),
        Some("ask requires --prompt <text> or --prompt-file <path>".into())
    );
    assert_eq!(
        message(options(Some(" \t\r\n"), Some(""), Some(""))),
        Some("--prompt cannot be empty".into())
    );
    assert_eq!(
        message(options(Some("x\0"), Some(""), Some(""))),
        Some("--prompt contains terminal control U+0000; only newline and tab are allowed".into())
    );
    assert_eq!(
        message(options(Some("x"), Some(" "), Some(""))),
        Some("--model cannot be empty".into())
    );
    assert_eq!(
        message(options(Some("x"), Some("m"), Some(" "))),
        Some("--effort cannot be empty".into())
    );
    assert_eq!(
        validate_start_options(options(None, None, None), "reopen")
            .err()
            .unwrap()
            .to_string(),
        "reopen requires --prompt <text> or --prompt-file <path>"
    );
    let validated = validate_start_options(
        CommonStartOptions {
            title: Some("t".into()),
            terminal: Some(terminal::TerminalKind::Iterm2),
            yolo: true,
            timeout: Some(Duration::from_secs(3)),
            detach: true,
            json: true,
            ..options(Some(" x "), Some("m"), Some("e"))
        },
        "ask",
    )
    .unwrap();
    assert_eq!(validated.prompt, " x ");
    assert_eq!(validated.title.as_deref(), Some("t"));
    assert_eq!(validated.model.as_deref(), Some("m"));
    assert_eq!(validated.effort.as_deref(), Some("e"));
    assert_eq!(validated.terminal, Some(terminal::TerminalKind::Iterm2));
    assert!(validated.yolo && validated.detach && validated.json);
    assert_eq!(validated.timeout, Some(Duration::from_secs(3)));
}

#[test]
fn prompt_file_matrix() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("prompt");
    let path = file.to_str().unwrap();
    for command in ["ask", "reopen"] {
        for (content, expected) in [
            (b" x\r\n\t \n".as_slice(), Ok(" x\n\t \n")),
            (
                b"x\r",
                Err("--prompt contains terminal control U+000D; only newline and tab are allowed"),
            ),
            (b" \n", Err("--prompt cannot be empty")),
            (b"", Err("--prompt cannot be empty")),
        ] {
            fs::write(&file, content).unwrap();
            match expected {
                Ok(value) => assert_eq!(
                    common(parse(command, &["--prompt-file", path]).unwrap()).0,
                    value
                ),
                Err(value) => error(command, &["--prompt-file", path], value),
            }
        }
        fs::write(&file, [0xff]).unwrap();
        for path in [
            path.to_owned(),
            root.path().to_str().unwrap().to_owned(),
            root.path().join("missing").to_str().unwrap().to_owned(),
        ] {
            let err = parse(command, &["--prompt-file", &path]).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to read prompt file {path}")
            );
            assert!(err.chain().count() > 1);
        }
    }
}

#[cfg(unix)]
#[test]
fn cwd_and_literal_prompt_file_subprocess() {
    const CHILD: &str = "TABCLI_START_PARSE_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        if mode == "deleted" {
            let cwd = std::env::current_dir().unwrap();
            fs::remove_dir(&cwd).unwrap();
            let expected = std::env::current_dir().unwrap_err().to_string();
            for options in [
                vec!["--workspace", "/"],
                vec![],
                vec!["--workspace", "/", "--prompt-file", "missing"],
            ] {
                assert_eq!(parse("ask", &options).unwrap_err().to_string(), expected);
            }
            error(
                "reopen",
                &[],
                "reopen requires --prompt <text> or --prompt-file <path>",
            );
            assert!(parse("reopen", &["--prompt", "x"]).is_ok());
        } else {
            fs::write("-", "literal\r\n").unwrap();
            for command in ["ask", "reopen"] {
                assert_eq!(
                    common(parse(command, &["--prompt-file", "-"]).unwrap()).0,
                    "literal\n"
                );
            }
            let NativeCommand::Ask(r) =
                parse("ask", &["--workspace", "/", "--prompt-file", "-"]).unwrap()
            else {
                panic!()
            };
            assert_eq!(r.prompt, "literal\n");
        }
        return;
    }
    for mode in ["deleted", "literal"] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        fs::create_dir(&cwd).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native::tests::start_parse::cwd_and_literal_prompt_file_subprocess",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(cwd)
            .env(CHILD, mode)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}
