use std::ffi::OsString;

use anyhow::{Result, bail};

mod native;

const LEGACY_REMOVAL_MESSAGE: &str = "the embedded multi-PTY TUI was removed in agent-bridge 0.2.0; use ask, tell, sessions, or close-session";

#[derive(Debug)]
enum Launch {
    Help,
    Version,
    Native(native::NativeCommand),
}

fn help_text() -> String {
    format!(
        "agent-bridge {} — visible native terminal bridge for coding agent CLIs

Usage:
  agent-bridge ask <codex|claude|agy|pi> [--workspace PATH] --prompt TEXT [--title NAME]
      [--model MODEL] [--effort EFFORT] [--terminal <ghostty|iterm2|terminal|windows-console>]
      [--yolo] [--timeout-secs N] [--detach] [--json]
  agent-bridge tell <session> --prompt TEXT [--timeout-secs N] [--detach] [--json]
  agent-bridge sessions [--json]
  agent-bridge close-session <session> --explicit [--json]
  agent-bridge --help | --version

Runtime:
  macOS detects Ghostty, iTerm2, or Terminal.app from the invoking environment
  and opens a real surface for Codex, Claude, Agy, or Pi. Terminal.app always
  uses a dedicated new window. Use --terminal to override detection. An
  unknown host falls back to Terminal.app. Windows opens a dedicated managed
  PowerShell 7 console from either PowerShell or cmd. Linux is not yet supported.
  Agent Bridge controls only sessions that it launched.
  Attaching to an arbitrary CLI is not supported.

Session policy:
  --model and --effort apply only to the new child session. For Claude, the exact
  model value Fable5 is passed to Claude Code as Fable. The exact model value
  Fable is passed to Pi as anthropic/claude-fable-5. All other model values are
  forwarded unchanged.

  --yolo is never inherited. It is forwarded only when the ask command includes
  it and the provider has a matching option. Codex, Claude, and Agy receive their
  native bypass flags. Pi receives --approve for project-local trust while its
  native tool policy remains in effect.

  Supported CLI minimums: Codex 0.147.0, Claude 2.1.229, Agy 1.1.12, Pi 0.84.1.
  Session state is stored privately under ~/.agent-bridge/native-sessions.

Migration:
  The embedded multi-PTY TUI was removed in 0.2.0. Existing legacy files under
  ~/.agent-bridge are not read or deleted automatically.",
        env!("CARGO_PKG_VERSION")
    )
}

fn parse_args_from(args: impl IntoIterator<Item = OsString>) -> Result<Launch> {
    let args = args.into_iter().collect::<Vec<_>>();
    let Some(first) = args.first() else {
        bail!(LEGACY_REMOVAL_MESSAGE);
    };

    if first == "--help" || first == "-h" {
        if args.len() != 1 {
            bail!("--help does not accept arguments");
        }
        return Ok(Launch::Help);
    }
    if first == "--version" || first == "-V" {
        if args.len() != 1 {
            bail!("--version does not accept arguments");
        }
        return Ok(Launch::Version);
    }

    let command = first.to_string_lossy().into_owned();
    if native::is_command(&command) {
        let rest = args[1..]
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if rest
            .first()
            .is_some_and(|value| value == "--help" || value == "-h")
        {
            return Ok(Launch::Help);
        }
        return Ok(Launch::Native(native::parse_args(
            std::iter::once(command).chain(rest),
        )?));
    }

    if matches!(
        command.as_str(),
        "open"
            | "prompt"
            | "status"
            | "read"
            | "wait"
            | "list"
            | "close"
            | "hook"
            | "--restore"
            | "--yolo"
            | "-yolo"
    ) || !command.starts_with('-')
    {
        bail!(LEGACY_REMOVAL_MESSAGE);
    }

    bail!("unknown command or option: {command}")
}

fn main() -> Result<()> {
    match parse_args_from(std::env::args_os().skip(1))? {
        Launch::Help => {
            println!("{}", help_text());
            Ok(())
        }
        Launch::Version => {
            println!("agent-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Launch::Native(command) => native::run(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_routes_the_native_visible_session_surface() {
        assert!(matches!(
            parse_args_from([
                OsString::from("ask"),
                OsString::from("codex"),
                OsString::from("--workspace"),
                OsString::from("."),
                OsString::from("--prompt"),
                OsString::from("review this"),
                OsString::from("--yolo"),
            ])
            .unwrap(),
            Launch::Native(native::NativeCommand::Ask(native::AskRequest {
                yolo: true,
                ..
            }))
        ));
        assert!(matches!(
            parse_args_from([OsString::from("sessions")]).unwrap(),
            Launch::Native(native::NativeCommand::Sessions { json: false })
        ));
    }

    #[test]
    fn help_and_version_are_the_only_top_level_options() {
        assert!(matches!(
            parse_args_from([OsString::from("--help")]).unwrap(),
            Launch::Help
        ));
        assert!(matches!(
            parse_args_from([OsString::from("--version")]).unwrap(),
            Launch::Version
        ));
        assert!(matches!(
            parse_args_from([OsString::from("ask"), OsString::from("--help")]).unwrap(),
            Launch::Help
        ));
        assert!(parse_args_from([OsString::from("--unknown")]).is_err());
    }

    #[test]
    fn removed_tui_entrypoints_return_a_migration_error() {
        let cases = [
            Vec::new(),
            vec![OsString::from(".")],
            vec![OsString::from("--restore")],
            vec![OsString::from("--yolo")],
            vec![OsString::from("open"), OsString::from("claude")],
            vec![OsString::from("prompt"), OsString::from("Claude 1")],
            vec![OsString::from("hook"), OsString::from("finished")],
        ];

        for args in cases {
            let error = parse_args_from(args).unwrap_err().to_string();
            assert!(error.contains("multi-PTY TUI was removed"), "{error}");
        }
    }

    #[test]
    fn help_describes_only_the_current_runtime_contract() {
        let help = help_text();
        for expected in [
            "ask <codex|claude|agy|pi>",
            "tell <session>",
            "sessions [--json]",
            "close-session <session> --explicit",
            "macOS detects Ghostty, iTerm2, or Terminal.app",
            "Terminal.app always\n  uses a dedicated new window",
            "Use --terminal to override",
            "unknown host falls back to Terminal.app",
            "Windows opens a dedicated managed",
            "PowerShell 7 console from either PowerShell or cmd",
            "Fable5 is passed to Claude Code as Fable",
            "Fable is passed to Pi as anthropic/claude-fable-5",
            "Pi receives --approve for project-local trust",
            "Attaching to an arbitrary CLI",
        ] {
            assert!(help.contains(expected), "help is missing {expected:?}");
        }
        for removed in ["agent-bridge open", "agent-bridge prompt", "--restore"] {
            assert!(!help.contains(removed), "help still advertises {removed:?}");
        }
    }
}
