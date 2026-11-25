//! Shared macOS/Linux surface operations. A platform installs one `Adapters` lookup;
//! Linux adds an adapter by implementing `PosixTerminalAdapter` and returning it from
//! `linux::Installed::adapter`. Kind registration remains in that kind's facts.rs.
//! Creation policy is macOS-local: Linux neither accepts it nor reads its setting.
use super::{
    CloseOutcome, GuardedDialogInput, TerminalKind, TerminalSendFailure, TerminalSendResult,
    TerminalSession,
};
#[cfg(target_os = "macos")]
use crate::native::settings::MacosOpenMode;
use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    time::{Duration, Instant},
};
// Shared with the Windows Herdr surface, which binds the same way.
pub(in crate::native::terminal) use super::open_with;
pub(in crate::native::terminal) type BindSurface<'a> =
    Box<dyn FnOnce(&mut TerminalSession) -> Result<()> + 'a>;

/// Which authority the owned close runs under: the attested owner is live, or only the
/// surface remains (failed start reclaim, resumable intent). An adapter does its pre-close
/// work for a live owner only. `RemoveEnded` reaches only a kind whose core keeps
/// a surface whose program ended: the owner died with it, and the adapter removes that
/// surface only when it reads it ended, never ending a running program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum OwnedSurfaceClose {
    LiveOwner,
    SurfaceOnly,
    RemoveEnded,
}

pub(in crate::native::terminal) trait PosixTerminalAdapter {
    fn open_bound_tab(
        &self,
        #[cfg(target_os = "macos")] mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession>;
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult;
    fn verify_surface(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>>;
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool>;
    // A kind that declares a refusal for one of these in its registration is refused
    // by the shared dispatch before its adapter is reached, so the defaults are an
    // invariant failure and never an ordinary answer.
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String> {
        let _ = deadline;
        bail!(
            "{} declares managed screen reads unsupported; the shared dispatch must refuse before its adapter",
            session.kind.display_name()
        )
    }
    fn guarded_dialog_input(
        &self,
        session: &TerminalSession,
        input: &GuardedDialogInput,
        deadline: Instant,
    ) -> Result<bool> {
        let _ = (input, deadline);
        bail!(
            "{} declares managed dialog input unsupported; the shared dispatch must refuse before its adapter",
            session.kind.display_name()
        )
    }
    // Raw adapter close is exercised by tests. Installed POSIX Close uses owned authority.
    #[cfg(test)]
    fn close_session_until(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome>;
    fn close_owned_session(
        &self,
        directory: &Path,
        id: &str,
        session: &TerminalSession,
        close: OwnedSurfaceClose,
        deadline: Instant,
    ) -> Result<CloseOutcome>;
}

/// The adapter of a kind. Every shared operation resolves its adapter here and nowhere
/// else; production passes `&INSTALLED`, a test a `Dispatch` over its own adapters.
pub(in crate::native::terminal) trait Adapters {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter>;
}

pub(in crate::native::terminal) fn open_bound_tab<F, U>(
    adapters: &dyn Adapters,
    kind: TerminalKind,
    command: &str,
    directory: &Path,
    deadline: Instant,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    // Read once for creation. Close uses only the persisted target/ownership record,
    // never a setting that the user may have changed since this launch.
    #[cfg(target_os = "macos")]
    let mode = crate::native::settings::macos_open_mode(
        directory
            .parent()
            .context("session directory has no state root")?,
    )?;
    adapters.adapter(kind)?.open_bound_tab(
        #[cfg(target_os = "macos")]
        mode,
        command,
        directory,
        deadline,
        Box::new(bind),
        Box::new(unbind),
    )
}

pub(in crate::native::terminal) fn send_file(
    adapters: &dyn Adapters,
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    adapters
        .adapter(session.kind)
        .map_err(TerminalSendFailure::not_sent)?
        .send_file(session, prompt_path, deadline)
}

pub(in crate::native::terminal) fn verify_surface(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<Option<String>> {
    adapters
        .adapter(session.kind)?
        .verify_surface(directory, session, timeout)
}

pub(in crate::native::terminal) fn surface_present(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    adapters
        .adapter(session.kind)?
        .surface_present(directory, session, timeout)
}

// The declared refusals answer before any adapter is reached.
pub(in crate::native::terminal) fn read_screen(
    adapters: &dyn Adapters,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<String> {
    if let Some(refusal) = session.kind.registration().screen_read_refusal {
        return Err(anyhow::anyhow!(refusal));
    }
    adapters
        .adapter(session.kind)?
        .read_screen(session, deadline)
}

pub(in crate::native::terminal) fn guarded_dialog_input(
    adapters: &dyn Adapters,
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    if let Some(refusal) = session.kind.registration().guarded_input_refusal {
        return Err(anyhow::anyhow!(refusal));
    }
    adapters
        .adapter(session.kind)?
        .guarded_dialog_input(session, input, deadline)
}

#[cfg(test)]
pub(in crate::native::terminal) fn close_session_until(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    adapters
        .adapter(session.kind)?
        .close_session_until(directory, session, deadline)
}

pub(in crate::native::terminal) fn close_owned_session(
    adapters: &dyn Adapters,
    directory: &Path,
    id: &str,
    session: &TerminalSession,
    close: OwnedSurfaceClose,
    deadline: Instant,
) -> Result<CloseOutcome> {
    // Every other adapter would close its surface under this authority; refuse it first.
    if close == OwnedSurfaceClose::RemoveEnded
        && !super::close_authority::owner_exit::core_retains_finished(session.kind)
    {
        bail!(
            "{} keeps no surface after its program ends; no close was sent and the handle is retained",
            session.kind.display_name()
        );
    }
    adapters
        .adapter(session.kind)?
        .close_owned_session(directory, id, session, close, deadline)
}

pub(in crate::native::terminal) fn timeout_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .context("terminal automation timeout is too large")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_with_preserves_prebind_handles_deadlines_and_failure_order() {
        use std::cell::RefCell;
        for (failure, cleanup_fails, unbind_fails, expected) in [
            ("none", false, false, vec!["create", "bind", "start"]),
            ("create", false, false, vec!["create"]),
            (
                "bind",
                false,
                false,
                vec!["create", "bind", "cleanup", "unbind"],
            ),
            (
                "start",
                false,
                false,
                vec!["create", "bind", "start", "cleanup", "unbind"],
            ),
            ("bind", true, false, vec!["create", "bind", "cleanup"]),
            (
                "start",
                true,
                false,
                vec!["create", "bind", "start", "cleanup"],
            ),
            (
                "bind",
                false,
                true,
                vec!["create", "bind", "cleanup", "unbind"],
            ),
            (
                "start",
                false,
                true,
                vec!["create", "bind", "start", "cleanup", "unbind"],
            ),
        ] {
            let trace = RefCell::new(Vec::new());
            let deadline = Instant::now() + Duration::from_secs(10);
            let startup = deadline - Duration::from_secs(2);
            let result = open_with(
                deadline,
                |received| {
                    trace.borrow_mut().push("create");
                    assert_eq!(received, startup);
                    if failure == "create" {
                        bail!("create failed");
                    }
                    Ok(handle())
                },
                |session, received| {
                    trace.borrow_mut().push("start");
                    assert_eq!(received, startup);
                    assert!(session.managed_session_id.is_none());
                    if failure == "start" {
                        bail!("start failed");
                    }
                    Ok(())
                },
                |session, received| {
                    trace.borrow_mut().push("cleanup");
                    assert_eq!(received, deadline);
                    assert!(session.managed_session_id.is_none());
                    if cleanup_fails {
                        bail!("cleanup failed");
                    }
                    Ok(())
                },
                |session| {
                    trace.borrow_mut().push("bind");
                    session.managed_session_id = Some("bound".into());
                    if failure == "bind" {
                        bail!("bind failed");
                    }
                    Ok(())
                },
                || {
                    trace.borrow_mut().push("unbind");
                    if unbind_fails {
                        bail!("unbind failed");
                    }
                    Ok(())
                },
            );
            assert_eq!(*trace.borrow(), expected);
            if failure == "none" {
                assert_eq!(result.unwrap().managed_session_id.as_deref(), Some("bound"));
            } else {
                let error = result.unwrap_err().to_string();
                let mut expected = format!("{failure} failed");
                if cleanup_fails {
                    expected.push_str("; exact surface cleanup also failed: cleanup failed");
                }
                if unbind_fails {
                    expected.push_str("; exact surface cleanup succeeded but its durable binding could not be removed: unbind failed");
                }
                assert_eq!(error, expected);
            }
        }
        let error = open_with(
            Instant::now(),
            |_| panic!("no creation without reserve"),
            |_, _| panic!(),
            |_, _| panic!(),
            |_| panic!(),
            || panic!(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "terminal startup timeout leaves no room for exact surface cleanup"
        );
    }
    struct Fake;
    impl Adapters for Fake {
        fn adapter(&self, _: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
            Ok(self)
        }
    }
    impl PosixTerminalAdapter for Fake {
        fn open_bound_tab(
            &self,
            #[cfg(target_os = "macos")] _mode: MacosOpenMode,
            _: &str,
            _: &Path,
            deadline: Instant,
            bind: BindSurface<'_>,
            unbind: Box<dyn FnOnce() -> Result<()> + '_>,
        ) -> Result<TerminalSession> {
            open_with(
                deadline,
                |_| Ok(handle()),
                |_, _| Ok(()),
                |_, _| Ok(()),
                bind,
                unbind,
            )
        }
        fn send_file(&self, _: &TerminalSession, _: &Path, _: Instant) -> TerminalSendResult {
            Ok(())
        }
        fn verify_surface(
            &self,
            _: &Path,
            _: &TerminalSession,
            _: Option<Duration>,
        ) -> Result<Option<String>> {
            Ok(Some("/dev/pts/fake".into()))
        }
        fn surface_present(&self, _: &Path, _: &TerminalSession, _: Duration) -> Result<bool> {
            Ok(true)
        }
        fn read_screen(&self, _: &TerminalSession, _: Instant) -> Result<String> {
            Ok("fake screen".into())
        }
        fn guarded_dialog_input(
            &self,
            _: &TerminalSession,
            _: &GuardedDialogInput,
            _: Instant,
        ) -> Result<bool> {
            panic!("declared refusal must precede adapter")
        }
        fn close_session_until(
            &self,
            _: &Path,
            _: &TerminalSession,
            _: Instant,
        ) -> Result<CloseOutcome> {
            Ok(CloseOutcome::Closed)
        }
        fn close_owned_session(
            &self,
            _: &Path,
            _: &str,
            _: &TerminalSession,
            close: OwnedSurfaceClose,
            _: Instant,
        ) -> Result<CloseOutcome> {
            assert_eq!(close, OwnedSurfaceClose::SurfaceOnly);
            Ok(CloseOutcome::Closed)
        }
    }
    #[test]
    fn shared_dispatch_runs_operations_and_refuses_declared_input_before_adapter() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-fake");
        std::fs::create_dir(&dir).unwrap();
        #[cfg(target_os = "linux")]
        std::fs::write(
            root.path().join("settings.json"),
            "{broken macOS preference",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let surface = open_bound_tab(
            &Fake,
            TerminalKind::Iterm2,
            "unused",
            &dir,
            deadline,
            |_| Ok(()),
            || panic!("no unbind on success"),
        )
        .unwrap();
        send_file(&Fake, &surface, Path::new("unused"), deadline).unwrap();
        assert_eq!(
            verify_surface(&Fake, &dir, &surface, None)
                .unwrap()
                .as_deref(),
            Some("/dev/pts/fake")
        );
        assert!(surface_present(&Fake, &dir, &surface, Duration::from_secs(1)).unwrap());
        assert_eq!(
            read_screen(&Fake, &surface, deadline).unwrap(),
            "fake screen"
        );
        assert_eq!(
            close_session_until(&Fake, &dir, &surface, deadline).unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(
            close_owned_session(
                &Fake,
                &dir,
                "session-fake",
                &surface,
                OwnedSurfaceClose::SurfaceOnly,
                deadline
            )
            .unwrap(),
            CloseOutcome::Closed
        );
        let mut refused = surface;
        refused.kind = TerminalKind::WezTerm;
        let input = GuardedDialogInput {
            expected_claim: None,
            expires_unix_ms: None,
            screen: "screen".into(),
            key: super::super::DialogKey::Enter,
        };
        assert!(guarded_dialog_input(&Fake, &refused, &input, deadline).is_err());
    }

    // The fake would close its surface under any authority. A kind whose close evidence keeps
    // no ended surface is refused the remove-only close before its adapter is reached.
    #[test]
    fn remove_ended_is_refused_before_an_adapter_that_keeps_no_ended_surface() {
        let deadline = Instant::now() + Duration::from_secs(5);
        for kind in [
            TerminalKind::Iterm2,
            TerminalKind::AppleTerminal,
            TerminalKind::Ghostty,
            TerminalKind::Warp,
            TerminalKind::WezTerm,
            TerminalKind::WindowsConsole,
            TerminalKind::Tmux,
        ] {
            let mut surface = handle();
            surface.kind = kind;
            let error = close_owned_session(
                &Fake,
                Path::new("/unused"),
                "session-fake",
                &surface,
                OwnedSurfaceClose::RemoveEnded,
                deadline,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("keeps no surface after its program ends; no close was sent"),
                "{kind:?}: {error:#}"
            );
        }
    }

    fn handle() -> TerminalSession {
        serde_json::from_value(serde_json::json!({"session_id":"test"})).unwrap()
    }
}
