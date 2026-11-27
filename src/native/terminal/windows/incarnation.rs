use super::{TerminalSession, WindowsProcessIdentity};
use anyhow::{Context, Result, ensure};

/// A comparison used by process attestation; record and surface equality stay exact.
#[derive(Debug)]
pub(in crate::native) struct WindowsIncarnation(pub WindowsProcessIdentity);

impl PartialEq for WindowsIncarnation {
    fn eq(&self, other: &Self) -> bool {
        same_incarnation(&self.0, &other.0)
    }
}

fn same_incarnation(recorded: &WindowsProcessIdentity, live: &WindowsProcessIdentity) -> bool {
    recorded.creation_time == live.creation_time
        && recorded
            .executable_path
            .eq_ignore_ascii_case(&live.executable_path)
}

/// Presence of the recorded console root. Other clients can outlive that process.
pub(super) fn console_root_present(
    session: &TerminalSession,
    observe: impl FnOnce(u32) -> Result<Option<WindowsProcessIdentity>>,
) -> Result<bool> {
    let pid: u32 = session.id.parse().context("invalid Windows console pid")?;
    ensure!(pid != 0, "invalid Windows console pid");
    let recorded = session
        .windows_identity()
        .context("Windows console handle is missing its process identity")?;
    ensure!(
        recorded.creation_time != 0 && !recorded.executable_path.is_empty(),
        "Windows console handle has an incomplete process identity"
    );
    let Some(live) = observe(pid)? else {
        return Ok(false);
    };
    if same_incarnation(recorded, &live) {
        return Ok(true);
    }
    ensure!(
        live.creation_time != recorded.creation_time,
        "Windows console process executable identity changed"
    );
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::terminal::{TerminalKind, TerminalSession};

    fn identity() -> WindowsProcessIdentity {
        WindowsProcessIdentity {
            creation_time: 731,
            executable_path: r"C:\Program Files\PowerShell\pwsh.exe".into(),
        }
    }

    fn root() -> TerminalSession {
        TerminalSession::new(TerminalKind::WindowsConsole, std::process::id().to_string())
            .with_windows_identity(Some(identity()))
    }

    #[test]
    fn console_root_presence_accepts_the_same_image_with_different_case() {
        let mut live = identity();
        live.executable_path.make_ascii_uppercase();
        assert!(
            console_root_present(&root(), |pid| {
                assert_eq!(pid, std::process::id());
                Ok(Some(live))
            })
            .unwrap()
        );
    }

    #[test]
    fn console_root_presence_distinguishes_exit_reuse_and_inspection_failure() {
        let handle = root();
        assert!(!console_root_present(&handle, |_| Ok(None)).unwrap());
        let mut reused = identity();
        reused.creation_time += 1;
        assert!(!console_root_present(&handle, |_| Ok(Some(reused))).unwrap());
        let mut conflicting = identity();
        conflicting.executable_path = r"C:\another.exe".into();
        assert_eq!(
            console_root_present(&handle, |_| Ok(Some(conflicting)))
                .unwrap_err()
                .to_string(),
            "Windows console process executable identity changed"
        );
        assert_eq!(
            console_root_present(&handle, |_| anyhow::bail!("open denied"))
                .unwrap_err()
                .to_string(),
            "open denied"
        );
    }

    #[test]
    fn console_root_presence_requires_a_recorded_identity_before_observing() {
        for (id, recorded, reason) in [
            ("invalid", Some(identity()), "invalid Windows console pid"),
            ("0", Some(identity()), "invalid Windows console pid"),
            (
                "123",
                None,
                "Windows console handle is missing its process identity",
            ),
            (
                "123",
                Some(WindowsProcessIdentity {
                    creation_time: 0,
                    ..identity()
                }),
                "Windows console handle has an incomplete process identity",
            ),
            (
                "123",
                Some(WindowsProcessIdentity {
                    executable_path: String::new(),
                    ..identity()
                }),
                "Windows console handle has an incomplete process identity",
            ),
        ] {
            let handle = TerminalSession::new(TerminalKind::WindowsConsole, id.into())
                .with_windows_identity(recorded);
            let error = console_root_present(&handle, |_| panic!("invalid handle reached the OS"))
                .unwrap_err();
            assert_eq!(error.to_string(), reason);
        }
    }

    #[cfg(windows)]
    #[test]
    fn console_root_presence_reaches_the_diagnostic_observation() {
        use crate::native::doctor;
        use crate::native::query::observation::{SessionEvidence, judgment::SurfacePresence};
        use crate::native::session::{Reader, Store};
        use crate::native::terminal::windows::query_process_identity;
        use std::time::{Duration, Instant};

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-probe");
        std::fs::create_dir(&dir).unwrap();
        let pid = std::process::id();
        let mut identity = query_process_identity(pid).unwrap();
        // Toggle the executable's ASCII letters even if the path was already uppercase.
        identity.executable_path = identity
            .executable_path
            .chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        let handle = TerminalSession::new(TerminalKind::WindowsConsole, pid.to_string())
            .with_binding(Some("session-probe".into()));
        let store = Store::open_unchecked(&dir);
        let reader = Reader::open_unchecked(&dir);
        for (recorded, presence, code) in [
            (
                Some(identity),
                SurfacePresence::Present,
                "terminal_surface_present",
            ),
            (
                None,
                SurfacePresence::Unverified(
                    "Windows console handle is missing its process identity".into(),
                ),
                "terminal_surface_unverified",
            ),
        ] {
            store
                .write_terminal(&handle.clone().with_windows_identity(recorded))
                .unwrap();
            let evidence = SessionEvidence::read(&reader, Ok(false));
            let actual =
                evidence.judge_surface_presence(&reader, Instant::now() + Duration::from_secs(60));
            assert_eq!(*actual, presence);
            assert_eq!(doctor::surface_presence_check(actual).reason_code, code);
        }
    }
}
