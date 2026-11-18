use super::TerminalKind;
use crate::native::args::one_positional;
use crate::native::session::primitives::require_valid_session_id;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

// The host command names, read from each kind's registration.
pub(in crate::native) const APPLE_TERMINAL_HOST_COMMAND: &str =
    TerminalKind::AppleTerminal.registration().host_command;
pub(in crate::native) const GHOSTTY_HOST_COMMAND: &str =
    TerminalKind::Ghostty.registration().host_command;
pub(in crate::native) const ITERM2_HOST_COMMAND: &str =
    TerminalKind::Iterm2.registration().host_command;
pub(in crate::native) const WARP_HOST_COMMAND: &str =
    TerminalKind::Warp.registration().host_command;
pub(in crate::native) const WEZTERM_HOST_COMMAND: &str =
    TerminalKind::WezTerm.registration().host_command;
pub(in crate::native) const CONSOLE_HOST_COMMAND: &str =
    TerminalKind::WindowsConsole.registration().host_command;
pub(in crate::native) const TMUX_HOST_COMMAND: &str =
    TerminalKind::Tmux.registration().host_command;
pub(in crate::native) const HERDR_HOST_COMMAND: &str =
    TerminalKind::Herdr.registration().host_command;

/// The flag that hands a launch host the command it runs on its bound terminal. Only the host
/// command of a kind that uses the launch host takes it.
pub(in crate::native) const LAUNCH_COMMAND_FLAG: &str = "--launch-command";

// The kind whose registered host command is the name.
fn kind_of(command: &str) -> Option<TerminalKind> {
    TerminalKind::ALL
        .into_iter()
        .find(|kind| kind.registration().host_command == command)
}

#[derive(Debug)]
pub(crate) enum HostCommand {
    Console {
        directory: PathBuf,
    },
    Iterm2 {
        directory: PathBuf,
    },
    AppleTerminal {
        directory: PathBuf,
    },
    WezTerm {
        directory: PathBuf,
    },
    /// The launch host of a kind that uses one, with the command it hands its surface to.
    Launch {
        kind: TerminalKind,
        directory: PathBuf,
        command: String,
    },
    Ghostty,
    Warp {
        directory: PathBuf,
        attempt: String,
    },
    /// The program of a tmux pane: it waits for its binding, then runs the command.
    Tmux {
        directory: PathBuf,
        command: String,
    },
}

pub(in crate::native) fn parse(command: &str, rest: &[String]) -> Result<HostCommand> {
    if let Some(kind) = kind_of(command)
        && kind.uses_launch_host()
        && let [_, flag, launch_command] = rest
        && flag == LAUNCH_COMMAND_FLAG
    {
        return Ok(HostCommand::Launch {
            kind,
            directory: session_directory(command, &rest[..1])?,
            command: launch_command.clone(),
        });
    }
    match kind_of(command) {
        Some(TerminalKind::Ghostty) => parse_ghostty_host(rest),
        Some(TerminalKind::Warp) => parse_warp_host(rest),
        Some(TerminalKind::Tmux) => parse_tmux_host(rest),
        kind => parse_directory_host(command, kind, rest),
    }
}

// The tab's process does not necessarily inherit the state root, so it is given the session
// directory itself.
fn session_directory(command: &str, rest: &[String]) -> Result<PathBuf> {
    let directory = PathBuf::from(one_positional(
        rest,
        &format!("{command} requires one session directory"),
    )?);
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{command} requires a session directory"))?;
    require_valid_session_id(id)?;
    if !directory.is_absolute() {
        bail!("{command} requires an absolute session directory");
    }
    Ok(directory)
}

fn parse_directory_host(
    command: &str,
    kind: Option<TerminalKind>,
    rest: &[String],
) -> Result<HostCommand> {
    let directory = session_directory(command, rest)?;
    match kind {
        Some(TerminalKind::Iterm2) => Ok(HostCommand::Iterm2 { directory }),
        Some(TerminalKind::AppleTerminal) => Ok(HostCommand::AppleTerminal { directory }),
        Some(TerminalKind::WezTerm) => Ok(HostCommand::WezTerm { directory }),
        Some(TerminalKind::WindowsConsole) => Ok(HostCommand::Console { directory }),
        // A Herdr terminal has no legacy host: it is only ever given the launch form.
        Some(
            TerminalKind::Ghostty | TerminalKind::Warp | TerminalKind::Tmux | TerminalKind::Herdr,
        )
        | None => {
            bail!("unknown native host command: {command}")
        }
    }
}

fn parse_ghostty_host(rest: &[String]) -> Result<HostCommand> {
    // The surface is created before its session is bound to it, so this host
    // is given nothing: its launch reaches it through its own terminal.
    if !rest.is_empty() {
        bail!("{GHOSTTY_HOST_COMMAND} takes no argument");
    }
    Ok(HostCommand::Ghostty)
}

// The pane host is only ever given the launch form: a tmux pane has no legacy host.
fn parse_tmux_host(rest: &[String]) -> Result<HostCommand> {
    let [_, flag, command] = rest else {
        bail!(
            "{TMUX_HOST_COMMAND} requires a session directory and {LAUNCH_COMMAND_FLAG} <command>"
        );
    };
    if flag != LAUNCH_COMMAND_FLAG {
        bail!(
            "{TMUX_HOST_COMMAND} requires a session directory and {LAUNCH_COMMAND_FLAG} <command>"
        );
    }
    Ok(HostCommand::Tmux {
        directory: session_directory(TMUX_HOST_COMMAND, &rest[..1])?,
        command: command.clone(),
    })
}

fn parse_warp_host(rest: &[String]) -> Result<HostCommand> {
    let [directory, attempt] = rest else {
        bail!("{WARP_HOST_COMMAND} requires a session directory and attempt token");
    };
    let directory = PathBuf::from(directory);
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{WARP_HOST_COMMAND} requires a session directory"))?;
    require_valid_session_id(id)?;
    if !directory.is_absolute() {
        bail!("{WARP_HOST_COMMAND} requires an absolute session directory");
    }
    if attempt.len() != 32 || !attempt.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{WARP_HOST_COMMAND} requires a valid attempt token");
    }
    Ok(HostCommand::Warp {
        directory,
        attempt: attempt.clone(),
    })
}

pub(in crate::native) fn run(command: HostCommand) -> Result<()> {
    match command {
        HostCommand::Launch {
            kind,
            directory,
            command,
        } => run_launch_host(directory, command, kind),
        HostCommand::Console { directory } => {
            #[cfg(target_os = "windows")]
            {
                let metadata = std::fs::symlink_metadata(&directory).with_context(|| {
                    format!("no such session directory: {}", directory.display())
                })?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!(
                        "refusing non-directory session directory: {}",
                        directory.display()
                    );
                }
                super::windows::run_console_host(&directory)
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = directory;
                bail!("the native Windows console host is only available on Windows")
            }
        }
        HostCommand::Iterm2 { directory } => {
            #[cfg(target_os = "macos")]
            {
                super::macos::iterm2::run_host(&directory)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = directory;
                bail!("the native iTerm2 host is only available on macOS")
            }
        }
        HostCommand::AppleTerminal { directory } => {
            #[cfg(target_os = "macos")]
            {
                super::macos::apple_terminal::run_host(&directory)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = directory;
                bail!("the native Terminal.app host is only available on macOS")
            }
        }
        HostCommand::WezTerm { directory } => {
            #[cfg(target_os = "macos")]
            {
                super::macos::wezterm::run_host(&directory)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = directory;
                bail!("the native WezTerm host is only available on macOS")
            }
        }
        HostCommand::Ghostty => {
            #[cfg(target_os = "macos")]
            {
                super::macos::ghostty::host::run_host()
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("the native Ghostty host is only available on macOS")
            }
        }
        HostCommand::Warp { directory, attempt } => {
            #[cfg(target_os = "macos")]
            {
                super::macos::warp::run_host(&directory, &attempt)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (directory, attempt);
                bail!("the native Warp host is only available on macOS")
            }
        }
        HostCommand::Tmux { directory, command } => {
            #[cfg(target_os = "linux")]
            {
                super::linux::tmux::run_host(&directory, &command)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (directory, command);
                bail!("the native tmux host is only available on Linux")
            }
        }
    }
}

fn run_launch_host(directory: PathBuf, command: String, kind: TerminalKind) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        super::macos::launch_host::run(&directory, &command, kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (directory, command, kind);
        bail!("native launch hosts are only available on macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{parse_args, run};

    #[test]
    fn command_hosts_accept_exact_new_and_legacy_forms() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-args");
        let directory = directory.to_str().unwrap();
        for command in [ITERM2_HOST_COMMAND, WEZTERM_HOST_COMMAND] {
            let legacy = parse(command, &[directory.into()]).unwrap();
            assert!(matches!(
                legacy,
                HostCommand::Iterm2 { .. } | HostCommand::WezTerm { .. }
            ));
            let parsed = parse(
                command,
                &[
                    directory.into(),
                    "--launch-command".into(),
                    ". 'quoted $$ script'".into(),
                ],
            )
            .unwrap();
            match parsed {
                HostCommand::Launch { command, .. } => {
                    assert_eq!(command, ". 'quoted $$ script'")
                }
                _ => panic!("lost launch command"),
            }
            for rest in [
                vec![directory.into(), "--launch-command".into()],
                vec![directory.into(), "--other".into(), "script".into()],
                vec![
                    directory.into(),
                    "--launch-command".into(),
                    "script".into(),
                    "extra".into(),
                ],
            ] {
                assert!(parse(command, &rest).is_err());
            }
        }
        for command in [APPLE_TERMINAL_HOST_COMMAND, CONSOLE_HOST_COMMAND] {
            assert!(
                parse(
                    command,
                    &[directory.into(), "--launch-command".into(), "script".into()]
                )
                .is_err()
            );
        }
    }

    // Every kind's host command, given the launch form and its malformed shapes: only a
    // launch host command takes it, and every refusal keeps its command's own text.
    #[test]
    fn launch_form_is_taken_only_by_launch_host_commands() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-args");
        let directory = directory.to_str().unwrap();
        let invalid = root.path().join("invalid id");
        let invalid = invalid.to_str().unwrap();
        let mut actual = Vec::new();
        for kind in TerminalKind::ALL {
            let command = kind.registration().host_command;
            for rest in [
                vec![directory, "--launch-command", "script"],
                vec![directory, "--launch-command"],
                vec![directory, "--other", "script"],
                vec![directory, "--launch-command", "script", "extra"],
                vec!["session-relative", "--launch-command", "script"],
                vec![invalid, "--launch-command", "script"],
                vec![directory],
            ] {
                let shape = rest.len();
                let rest: Vec<String> = rest.into_iter().map(Into::into).collect();
                let outcome = match parse(command, &rest) {
                    Ok(HostCommand::Launch {
                        kind,
                        directory,
                        command,
                    }) => format!("launch {} {} {command}", kind.as_str(), directory.display()),
                    Ok(HostCommand::Iterm2 { directory }) => {
                        format!("host iterm2 {}", directory.display())
                    }
                    Ok(HostCommand::AppleTerminal { directory }) => {
                        format!("host apple-terminal {}", directory.display())
                    }
                    Ok(HostCommand::WezTerm { directory }) => {
                        format!("host wezterm {}", directory.display())
                    }
                    Ok(HostCommand::Console { directory }) => {
                        format!("host windows-console {}", directory.display())
                    }
                    Ok(HostCommand::Ghostty) => "host ghostty".into(),
                    Ok(HostCommand::Warp { directory, attempt }) => {
                        format!("host warp {} {attempt}", directory.display())
                    }
                    Ok(HostCommand::Tmux { directory, command }) => {
                        format!("host tmux {} {command}", directory.display())
                    }
                    Err(error) => format!("refused: {error:#}"),
                };
                actual.push(
                    format!("{command} [{shape}] {outcome}")
                        .replace(directory, "<dir>")
                        .replace(invalid, "<invalid>"),
                );
            }
        }
        let refusals = |command: &str, text: &str| {
            [3, 2, 3, 4, 3, 3].map(|shape| format!("{command} [{shape}] refused: {text}"))
        };
        let launch_host = |command: &str, kind: &str| {
            [
                format!("{command} [3] launch {kind} <dir> script"),
                format!("{command} [2] refused: {command} requires one session directory"),
                format!("{command} [3] refused: {command} requires one session directory"),
                format!("{command} [4] refused: {command} requires one session directory"),
                format!("{command} [3] refused: {command} requires an absolute session directory"),
                format!("{command} [3] refused: invalid Agent Bridge session id: \"invalid id\""),
                format!("{command} [1] host {kind} <dir>"),
            ]
        };
        let directory_host = |command: &str, kind: &str| {
            let mut lines = refusals(
                command,
                &format!("{command} requires one session directory"),
            )
            .to_vec();
            lines.push(format!("{command} [1] host {kind} <dir>"));
            lines
        };
        let mut ghostty = refusals(
            GHOSTTY_HOST_COMMAND,
            "native-ghostty-host takes no argument",
        )
        .to_vec();
        ghostty.push(format!(
            "{GHOSTTY_HOST_COMMAND} [1] refused: native-ghostty-host takes no argument"
        ));
        let pair = "native-warp-host requires a session directory and attempt token";
        let mut warp = refusals(WARP_HOST_COMMAND, pair).to_vec();
        warp[1] = format!(
            "{WARP_HOST_COMMAND} [2] refused: native-warp-host requires a valid attempt token"
        );
        warp.push(format!("{WARP_HOST_COMMAND} [1] refused: {pair}"));
        let form = "native-tmux-host requires a session directory and --launch-command <command>";
        let tmux = vec![
            format!("{TMUX_HOST_COMMAND} [3] host tmux <dir> script"),
            format!("{TMUX_HOST_COMMAND} [2] refused: {form}"),
            format!("{TMUX_HOST_COMMAND} [3] refused: {form}"),
            format!("{TMUX_HOST_COMMAND} [4] refused: {form}"),
            format!(
                "{TMUX_HOST_COMMAND} [3] refused: {TMUX_HOST_COMMAND} requires an absolute session directory"
            ),
            format!(
                "{TMUX_HOST_COMMAND} [3] refused: invalid Agent Bridge session id: \"invalid id\""
            ),
            format!("{TMUX_HOST_COMMAND} [1] refused: {form}"),
        ];
        let mut herdr = launch_host(HERDR_HOST_COMMAND, "herdr").to_vec();
        herdr[6] = format!(
            "{HERDR_HOST_COMMAND} [1] refused: unknown native host command: {HERDR_HOST_COMMAND}"
        );
        let expected: Vec<String> = [
            launch_host(ITERM2_HOST_COMMAND, "iterm2").to_vec(),
            directory_host(APPLE_TERMINAL_HOST_COMMAND, "apple-terminal"),
            ghostty,
            warp,
            launch_host(WEZTERM_HOST_COMMAND, "wezterm").to_vec(),
            directory_host(CONSOLE_HOST_COMMAND, "windows-console"),
            tmux,
            herdr,
        ]
        .concat();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unsupported_hosts_refuse_before_reading_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("session-missing");
        let directory = missing.to_str().unwrap();
        let cases: Vec<(Vec<&str>, &str)> = vec![
            #[cfg(not(target_os = "windows"))]
            (
                vec![CONSOLE_HOST_COMMAND, directory],
                "the native Windows console host is only available on Windows",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![ITERM2_HOST_COMMAND, directory],
                "the native iTerm2 host is only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![APPLE_TERMINAL_HOST_COMMAND, directory],
                "the native Terminal.app host is only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![WEZTERM_HOST_COMMAND, directory],
                "the native WezTerm host is only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![GHOSTTY_HOST_COMMAND],
                "the native Ghostty host is only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![ITERM2_HOST_COMMAND, directory, "--launch-command", "script"],
                "native launch hosts are only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![
                    WEZTERM_HOST_COMMAND,
                    directory,
                    "--launch-command",
                    "script",
                ],
                "native launch hosts are only available on macOS",
            ),
            #[cfg(not(target_os = "macos"))]
            (
                vec![
                    WARP_HOST_COMMAND,
                    directory,
                    "0123456789abcdef0123456789abcdef",
                ],
                "the native Warp host is only available on macOS",
            ),
            #[cfg(not(target_os = "linux"))]
            (
                vec![TMUX_HOST_COMMAND, directory, "--launch-command", "script"],
                "the native tmux host is only available on Linux",
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(
                format!("{:#}", run(parse_args(args).unwrap()).unwrap_err()),
                expected
            );
        }
        assert!(!missing.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn console_dispatch_keeps_outer_errors_and_no_request_success() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-dispatch");
        let invoke = |path: &std::path::Path| {
            run(parse_args([CONSOLE_HOST_COMMAND, path.to_str().unwrap()]).unwrap())
        };
        let cause = std::fs::symlink_metadata(&directory).unwrap_err();
        assert_eq!(
            format!("{:#}", invoke(&directory).unwrap_err()),
            format!(
                "no such session directory: {}: {cause}",
                directory.display()
            )
        );
        crate::native::session::primitives::write_private(&directory, b"file").unwrap();
        assert_eq!(
            invoke(&directory).unwrap_err().to_string(),
            format!(
                "refusing non-directory session directory: {}",
                directory.display()
            )
        );
        std::fs::remove_file(&directory).unwrap();
        std::fs::create_dir(&directory).unwrap();
        crate::native::session::primitives::set_private_directory_permissions(&directory).unwrap();
        invoke(&directory).unwrap();
        let link = root.path().join("session-link");
        match std::os::windows::fs::symlink_dir(&directory, &link) {
            Ok(()) => assert_eq!(
                invoke(&link).unwrap_err().to_string(),
                format!(
                    "refusing non-directory session directory: {}",
                    link.display()
                )
            ),
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("symlink fixture unavailable: {error}")
            }
            Err(error) => panic!("cannot create symlink fixture: {error}"),
        }
    }
}
