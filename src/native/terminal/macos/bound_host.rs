use crate::native::private_to_user::owned_and_not_accessible_by_others;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(in crate::native::terminal) struct BoundHostSpec {
    pub(super) admission: Admission,
    pub(in crate::native::terminal) name: &'static str,
    pub(in crate::native::terminal) metadata_context: Option<fn(&Path) -> String>,
}

pub(in crate::native::terminal) fn run_tty_bound_host<I>(
    directory: &Path,
    spec: &BoundHostSpec,
    identity: impl FnOnce() -> Result<I>,
    bind: impl FnOnce(&Path, &str, &I) -> Result<()>,
    record_outcome: impl FnOnce(&Result<I>),
) -> Result<()> {
    run_bound_host(directory, spec, |id| {
        let outcome = (|| {
            let identity = identity()?;
            bind(directory, id, &identity)?;
            // Discard only this host's input after its exact surface has been bound.
            if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot discard {} startup input", spec.name));
            }
            Ok(identity)
        })();
        record_outcome(&outcome);
        outcome.map(|_| ())
    })
}

#[derive(Clone, Copy)]
pub(super) enum Admission {
    MetadataFirst,
    GhosttyFrame,
    WarpPlan,
}

pub(super) fn run_bound_host<T>(
    directory: &Path,
    spec: &BoundHostSpec,
    run: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    run(admit(directory, spec)?)
}

fn admit<'a>(directory: &'a Path, spec: &BoundHostSpec) -> Result<&'a str> {
    if matches!(spec.admission, Admission::WarpPlan) {
        if !directory.is_absolute() {
            bail!("Warp host session directory must be absolute");
        }
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("Warp host session directory has no UTF-8 session id")?;
        if !crate::native::session::primitives::valid_session_id(id) {
            bail!("Warp host session directory has an invalid session id");
        }
        crate::native::session::Reader::open_unchecked(directory)
            .validate_private_session_directory("Warp host session")?;
        return Ok(id);
    }
    let early_id = if matches!(spec.admission, Admission::GhosttyFrame) {
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid Ghostty launch directory")?;
        crate::native::session::primitives::require_valid_session_id(id)?;
        if !directory.is_absolute() {
            bail!("Ghostty launch directory is not private to the current user");
        }
        Some(id)
    } else {
        None
    };
    let metadata = std::fs::symlink_metadata(directory);
    let metadata = match spec.metadata_context {
        Some(context) => metadata.with_context(|| context(directory))?,
        None => metadata?,
    };
    if !directory.is_absolute()
        || !metadata.is_dir()
        || !owned_and_not_accessible_by_others(&metadata, unsafe { libc::geteuid() })
    {
        bail!(
            "{} launch directory is not private to the current user",
            spec.name
        );
    }
    if let Some(id) = early_id {
        return Ok(id);
    }
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("invalid {} launch directory", spec.name))?;
    crate::native::session::primitives::require_valid_session_id(id)?;
    Ok(id)
}

pub(super) fn binding_names_host(
    surface: &super::TerminalSession,
    kind: super::TerminalKind,
    names_host: impl FnOnce(&super::TerminalSession) -> bool,
    refusal: &'static str,
) -> Result<()> {
    if surface.kind != kind || !names_host(surface) {
        bail!("{refusal}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{apple_terminal, iterm2, test_support, wezterm};
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };

    type HostRunner = fn(&Path) -> Result<()>;

    fn hosts() -> [(&'static str, HostRunner); 3] {
        [
            ("Terminal.app", apple_terminal::run_host),
            ("iTerm2", iterm2::run_host),
            ("WezTerm", wezterm::run_host),
        ]
    }

    #[test]
    fn preflight_preserves_first_error_and_never_writes_a_log() {
        let root = tempfile::tempdir().unwrap();
        let relative = tempfile::Builder::new()
            .prefix("session-relative-")
            .tempdir_in(".")
            .unwrap();
        let relative_path = Path::new(".").join(relative.path().file_name().unwrap());
        assert!(!relative_path.is_absolute());
        let file = root.path().join("session-file");
        fs::write(&file, b"file").unwrap();
        let private = root.path().join("session-private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("session-link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        let shared = root.path().join("invalid-shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o750)).unwrap();
        let invalid = root.path().join("invalid-private");
        fs::create_dir(&invalid).unwrap();
        fs::set_permissions(&invalid, fs::Permissions::from_mode(0o700)).unwrap();
        // Existing bytes also must survive preflight, including invalid-id failure.
        for path in [relative_path.as_path(), &private, &shared, &invalid] {
            fs::write(
                path.join(crate::native::session::launch::LOG),
                b"existing log\n",
            )
            .unwrap();
        }
        for (name, run) in hosts() {
            for missing in [
                root.path().join("invalid-missing"),
                relative_path.join("missing"),
            ] {
                let cause = fs::symlink_metadata(&missing).unwrap_err().to_string();
                let expected = match name {
                    "Terminal.app" => format!("failed to inspect {}: {cause}", missing.display()),
                    "WezTerm" => {
                        format!("no WezTerm launch directory {}: {cause}", missing.display())
                    }
                    _ => cause,
                };
                assert_eq!(format!("{:#}", run(&missing).unwrap_err()), expected);
                assert!(!missing.exists());
            }
            for path in [relative_path.as_path(), &file, &link, &shared] {
                assert_eq!(
                    format!("{:#}", run(path).unwrap_err()),
                    format!("{name} launch directory is not private to the current user")
                );
            }
            assert_eq!(
                format!("{:#}", run(&invalid).unwrap_err()),
                "invalid Agent Bridge session id: \"invalid-private\""
            );
            for path in [relative_path.as_path(), &private, &shared, &invalid] {
                assert_eq!(
                    fs::read(path.join(crate::native::session::launch::LOG)).unwrap(),
                    b"existing log\n"
                );
            }
        }
    }

    #[test]
    fn frame_and_plan_host_refusal_table_preserves_order_and_warp_privacy() {
        use std::cell::Cell;
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("session-private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let shared = root.path().join("session-shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o750)).unwrap();
        let invalid = root.path().join("invalid-shared");
        fs::create_dir(&invalid).unwrap();
        fs::set_permissions(&invalid, fs::Permissions::from_mode(0o750)).unwrap();
        let file = root.path().join("session-file");
        fs::write(&file, "file").unwrap();
        let link = root.path().join("session-link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        let missing = root.path().join("session-missing");
        let relative = Path::new("session-relative");
        for (name, admission) in [
            ("Ghostty", Admission::GhosttyFrame),
            ("Warp", Admission::WarpPlan),
        ] {
            let spec = BoundHostSpec {
                name,
                admission,
                metadata_context: None,
            };
            for path in [
                &private, &shared, &invalid, &file, &link, &missing, relative,
            ] {
                let called = Cell::new(false);
                let result = run_bound_host(path, &spec, |_| {
                    called.set(true);
                    Ok(())
                });
                let expected = if path == relative {
                    Some(
                        if name == "Warp" {
                            "Warp host session directory must be absolute"
                        } else {
                            "Ghostty launch directory is not private to the current user"
                        }
                        .to_string(),
                    )
                } else if path == invalid {
                    Some(if name == "Warp" {
                        "Warp host session directory has an invalid session id".to_string()
                    } else {
                        "invalid Agent Bridge session id: \"invalid-shared\"".to_string()
                    })
                } else if name == "Warp" {
                    // The plan host must use the very same privacy reader, including
                    // symlink, non-directory, ownership/mode and metadata diagnostics.
                    crate::native::session::Reader::open_unchecked(path)
                        .validate_private_session_directory("Warp host session")
                        .err()
                        .map(|error| format!("{error:#}"))
                } else if path == missing {
                    Some(fs::symlink_metadata(path).unwrap_err().to_string())
                } else if path != private {
                    Some("Ghostty launch directory is not private to the current user".to_string())
                } else {
                    None
                };
                assert_eq!(
                    result.err().map(|error| format!("{error:#}")),
                    expected,
                    "{name} {}",
                    path.display()
                );
                assert_eq!(called.get(), expected.is_none());
            }
        }
    }

    #[test]
    fn identity_binding_and_flush_failures_keep_exact_log_boundaries() {
        for mode in [
            "terminal-identity",
            "iterm-missing",
            "iterm-empty",
            "wezterm-pane",
            "wezterm-socket",
            "iterm-bind",
            "wezterm-bind",
            "iterm-flush",
            "wezterm-flush",
        ] {
            let directory = tempfile::Builder::new()
                .prefix("session-host-")
                .tempdir()
                .unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native::terminal::macos::bound_host::tests::failure_probe",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("AB_BOUND_HOST_PROBE", mode)
                .env("AB_BOUND_HOST_DIRECTORY", directory.path())
                .env_remove("ITERM_SESSION_ID")
                .env_remove("WEZTERM_PANE")
                .env_remove("WEZTERM_UNIX_SOCKET")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn failure_probe() {
        let Ok(mode) = std::env::var("AB_BOUND_HOST_PROBE") else {
            return;
        };
        let directory =
            std::path::PathBuf::from(std::env::var_os("AB_BOUND_HOST_DIRECTORY").unwrap());
        let is_iterm = mode.starts_with("iterm");
        let is_terminal = mode.starts_with("terminal");
        crate::native::session::primitives::set_private_directory_permissions(&directory).unwrap();
        let name = if is_terminal {
            "Terminal.app"
        } else if is_iterm {
            "iTerm2"
        } else {
            "WezTerm"
        };
        // This test only mutates environment inside its dedicated child process.
        unsafe {
            if mode == "iterm-empty" {
                std::env::set_var("ITERM_SESSION_ID", "a:");
            }
            if mode.ends_with("bind") || mode.ends_with("flush") {
                std::env::set_var("ITERM_SESSION_ID", "a:host-id");
                std::env::set_var("WEZTERM_PANE", "host-id");
                std::env::set_var("WEZTERM_UNIX_SOCKET", "/socket");
            }
            if mode == "wezterm-socket" {
                std::env::set_var("WEZTERM_PANE", "");
            }
        }
        let expected = match mode.as_str() {
            "terminal-identity" => format!(
                "{:#}",
                super::super::process::current_terminal_tty().unwrap_err()
            ),
            "iterm-missing" => {
                "iTerm2 did not supply its session identity: environment variable not found".into()
            }
            "iterm-empty" => "empty iTerm2 session identity".into(),
            "wezterm-pane" => {
                "WezTerm did not name the pane of this host: environment variable not found".into()
            }
            "wezterm-socket" => "WezTerm did not name the GUI of this host".into(),
            _ if mode.ends_with("bind") => format!("missing {name} launch receipt"),
            _ => {
                use crate::native::session::{SessionState, Store, turn};
                let store = Store::open_unchecked(&directory);
                crate::native::session::update_status(
                    &directory,
                    SessionState::Launching,
                    None,
                    None,
                )
                .unwrap();
                let claim = turn::claim(&store, &[]).unwrap();
                crate::native::session::launch::begin(
                    &store,
                    claim.token(),
                    std::time::Instant::now() + std::time::Duration::from_secs(10),
                )
                .unwrap();
                claim.retain();
                let mut surface = serde_json::json!({
                    "terminal": if is_iterm { "iterm2" } else { "wezterm" },
                    "session_id": "host-id",
                    "managed_session_id": directory.file_name().unwrap().to_str().unwrap(),
                });
                if !is_iterm {
                    surface["wezterm_mux"] = serde_json::json!({"socket": "/socket", "pid": 1, "start_seconds": 1, "start_microseconds": 0, "owns_gui": false});
                }
                crate::native::session::primitives::write_json_atomic(
                    &directory.join(crate::native::session::test_support::TERMINAL_HANDLE_FILE),
                    &surface,
                )
                .unwrap();
                assert_ne!(
                    unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) },
                    0
                );
                format!(
                    "cannot discard {name} startup input: {}",
                    std::io::Error::last_os_error()
                )
            }
        };
        let run = hosts()
            .into_iter()
            .find(|(host, _)| *host == name)
            .unwrap()
            .1;
        let before = fs::read(directory.join(crate::native::session::launch::LOG)).ok();
        let error = format!("{:#}", run(&directory).unwrap_err());
        assert_eq!(error, expected, "{mode}");
        if is_iterm {
            assert_eq!(
                fs::read(directory.join(crate::native::session::launch::LOG)).ok(),
                before
            );
        } else {
            let prefix = if is_terminal {
                "terminal_host_"
            } else {
                "wezterm_host_"
            };
            assert_eq!(
                test_support::host_log_messages(&directory, prefix),
                [format!("{prefix}refused: {expected}\n")]
            );
        }
    }
}
