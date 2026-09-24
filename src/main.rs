use std::ffi::OsString;

use anyhow::{Result, bail};

mod native;

const LEGACY_REMOVAL_MESSAGE: &str = "the embedded multi-PTY TUI was removed in agent-bridge 0.0.1; use ask, tell, sessions, prune-sessions, or close-session";

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
  agent-bridge ask <codex|claude|agy|pi> [--workspace PATH] (--prompt TEXT | --prompt-file PATH) [--title NAME]
      [--model MODEL] [--effort EFFORT] [--terminal <ghostty|iterm2|terminal|windows-console>]
      [--yolo] [--timeout-secs N] [--detach] [--json] [--context-result <session>/<request-id>]...
  agent-bridge tell <session> (--prompt TEXT | --prompt-file PATH) [--timeout-secs N] [--detach] [--json]
      [--context-result <session>/<request-id>]...
  agent-bridge reopen <closed-session> (--prompt TEXT | --prompt-file PATH) [--title NAME]
      [--model MODEL] [--effort EFFORT] [--terminal <windows-console>] [--yolo] [--timeout-secs N]
      [--detach] [--json]
  agent-bridge sessions [--workspace PATH] [--provider <codex|claude|agy|pi>] [--state STATE]
      [--sort <id|updated>] [--json]
  agent-bridge inspect <session> [--json]
  agent-bridge result <session> [--latest | --list | --event EVENT | --request REQUEST] [--json]
      [--wait --timeout-secs N]
  agent-bridge search <query> [--workspace PATH | --all-workspaces] [--provider <codex|claude|agy|pi>]
      [--limit N] [--json]
  agent-bridge doctor <session> [--probe] [--json]
  agent-bridge doctor --provider <codex|claude|agy|pi> [--probe] [--json]
  agent-bridge prune-sessions --closed-before-days N --explicit [--json]
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
  --model and --effort apply only to the new child session. For Codex, a non-empty
  Pi-qualified openai-codex/<model> value is passed as the native bare <model>.
  For Claude, the exact model value Fable5 is passed to Claude Code as Fable. The
  exact model value Fable is passed to Pi as anthropic/claude-fable-5. All other
  model values are forwarded unchanged.

  --yolo is never inherited. It is forwarded only when the ask or reopen command
  includes it and the provider has a matching option. Codex, Claude, and Agy
  receive their native bypass flags. Pi receives --approve for project-local trust
  while its native tool policy remains in effect.

  reopen continues a closed session's provider conversation in a new session with a
  new id, terminal, and private settings, launched through the provider's official
  resume. This release supports only Claude Code on native Windows: its conversation
  UUID is recorded in every Bridge event and its live-session registry proves
  ownership by pid and process start time. Codex is refused until its thread
  writer-lock and queue gates exist; Agy and Pi are refused because they expose no
  verifiable ownership evidence. Reopen refuses a source that is not closed, has no
  Claude event with a conversation id, has a request whose recorded result is
  missing or unreadable, or is held by a live Claude process. Claude permits
  concurrent resumes and offers no exclusive hold, so the ownership check is
  best-effort detection, not exclusion: it runs again immediately before the
  reopened process is spawned, after launch once the process has registered,
  immediately before the initial prompt is sent, and immediately before every tell
  to the reopened session. Another live holder found at any of those points
  refuses that delivery with gate reopen-conflict; a check that cannot complete
  refuses with gate reopen-verification-failed. After launch either gate fails the
  new session and closes only its surface before any prompt is delivered, and
  releases the source's reopen marker so the source can be reopened again; before a
  tell either gate refuses the delivery and leaves the session ready. A foreign
  claude --resume can still register between two checks and interleave until the
  next one. doctor reports the other live holders of a reopened session's
  conversation. Nothing is copied from the source manifest: only an explicit --model,
  --effort, or --yolo is passed. Without --model, Claude's own resume restores the
  model the conversation was using. Claude also restores the saved permission mode
  except bypass, so bypass is active only with --yolo; Claude documents no restored
  effort. The source is left unchanged except for a reopen marker that admits one
  reopen. inspect and sessions --json report resumed_from for the new session.

  --context-result attaches a previously recorded result, addressed exactly as
  <session>/<request-id> (or <session>/<event-id> for records without a receipt),
  after the prompt as clearly delimited reference material. Up to 8 values are
  accepted; each must be a published successful result at resolution time or the
  command fails before anything is created or sent. The new request receipt
  records the attached sources as context_sources for inspect and result.

  Supported CLI minimums: Codex 0.147.0, Claude 2.1.234, Agy 1.1.12, Pi 0.84.1.
  Session state is stored privately under ~/.agent-bridge/native-sessions.
  Closed records remain until prune-sessions explicitly removes quiescent records
  older than the requested retention window.

Migration:
  The embedded multi-PTY TUI was removed in 0.0.1. Existing legacy files under
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
            Launch::Native(native::NativeCommand::Sessions(native::SessionsRequest {
                json: false,
                ..
            }))
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
            "reopen <closed-session> (--prompt TEXT | --prompt-file PATH)",
            "supports only Claude Code on native Windows",
            "sessions [--workspace PATH]",
            "inspect <session>",
            "result <session>",
            "search <query> [--workspace PATH | --all-workspaces]",
            "doctor <session> [--probe] [--json]",
            "prune-sessions --closed-before-days N --explicit",
            "close-session <session> --explicit",
            "macOS detects Ghostty, iTerm2, or Terminal.app",
            "Terminal.app always\n  uses a dedicated new window",
            "Use --terminal to override",
            "unknown host falls back to Terminal.app",
            "Windows opens a dedicated managed",
            "PowerShell 7 console from either PowerShell or cmd",
            "Pi-qualified openai-codex/<model> value is passed as the native bare <model>",
            "Fable5 is passed to Claude Code as Fable",
            "Fable is passed to Pi as anthropic/claude-fable-5",
            "Pi receives --approve for project-local trust",
            "Attaching to an arbitrary CLI",
            "[--context-result <session>/<request-id>]...",
            "records the attached sources as context_sources",
        ] {
            assert!(help.contains(expected), "help is missing {expected:?}");
        }
        for removed in ["agent-bridge open", "agent-bridge prompt", "--restore"] {
            assert!(!help.contains(removed), "help still advertises {removed:?}");
        }
    }
}
