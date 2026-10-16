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

// The kind whose registered host command is the name.
fn kind_of(command: &str) -> Option<TerminalKind> {
    TerminalKind::ALL
        .into_iter()
        .find(|kind| kind.registration().host_command == command)
}

#[derive(Debug)]
pub(crate) enum HostCommand {
    Console { directory: PathBuf },
    Iterm2 { directory: PathBuf },
    AppleTerminal { directory: PathBuf },
    WezTerm { directory: PathBuf },
    Ghostty,
    Warp { directory: PathBuf, attempt: String },
}

pub(in crate::native) fn parse(command: &str, rest: &[String]) -> Result<HostCommand> {
    match kind_of(command) {
        Some(TerminalKind::Ghostty) => parse_ghostty_host(rest),
        Some(TerminalKind::Warp) => parse_warp_host(rest),
        kind => parse_directory_host(command, kind, rest),
    }
}

fn parse_directory_host(
    command: &str,
    kind: Option<TerminalKind>,
    rest: &[String],
) -> Result<HostCommand> {
    // The tab's process does not necessarily inherit the state root, so it is
    // given the session directory itself.
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
    match kind {
        Some(TerminalKind::Iterm2) => Ok(HostCommand::Iterm2 { directory }),
        Some(TerminalKind::AppleTerminal) => Ok(HostCommand::AppleTerminal { directory }),
        Some(TerminalKind::WezTerm) => Ok(HostCommand::WezTerm { directory }),
        Some(TerminalKind::WindowsConsole) => Ok(HostCommand::Console { directory }),
        Some(TerminalKind::Ghostty | TerminalKind::Warp) | None => {
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
                super::macos::ghostty::run_host()
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{parse_args, run};

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
                vec![
                    WARP_HOST_COMMAND,
                    directory,
                    "0123456789abcdef0123456789abcdef",
                ],
                "the native Warp host is only available on macOS",
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
        crate::native::write_private(&directory, b"file").unwrap();
        assert_eq!(
            invoke(&directory).unwrap_err().to_string(),
            format!(
                "refusing non-directory session directory: {}",
                directory.display()
            )
        );
        std::fs::remove_file(&directory).unwrap();
        std::fs::create_dir(&directory).unwrap();
        crate::native::set_private_directory_permissions(&directory).unwrap();
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
