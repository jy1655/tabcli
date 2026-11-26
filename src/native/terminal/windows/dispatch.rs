//! The seven Windows surface operations (open, send, screen, guarded key, verify, close and
//! presence). Screen and guarded key first read the kind's declared refusal; then every
//! operation resolves its adapter through one lookup (`Adapters::adapter`): the managed
//! console or Herdr, and for every other kind the refusal that each OS gives a kind it does
//! not run (`OsTerminals::unavailable`). macOS and Linux do the same through `posix.rs`.
//! Compiled into every test build, so that the lookup and the refusals are tested on every
//! CI target; the installed adapters stay in `windows/mod.rs` (the managed console) and
//! `herdr.rs`.
use super::{
    CloseOutcome, GuardedDialogInput, OsTerminals, TerminalKind, TerminalSendFailure,
    TerminalSendResult, TerminalSession,
};
use anyhow::{Result, bail};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(in crate::native::terminal) type BindSurface<'a> =
    Box<dyn FnOnce(&mut TerminalSession) -> Result<()> + 'a>;
pub(in crate::native::terminal) type UnbindSurface<'a> = Box<dyn FnOnce() -> Result<()> + 'a>;

pub(in crate::native) trait WindowsTerminalAdapter {
    /// `kind` is the requested launch kind, which the lookup gave this adapter. Each adapter
    /// still refuses kinds it does not create.
    fn open_bound_tab(
        &self,
        kind: TerminalKind,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: BindSurface<'_>,
        unbind: UnbindSurface<'_>,
    ) -> Result<TerminalSession>;
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult;
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String>;
    // A kind that declares this refusal in its registration is refused by the shared
    // dispatch before its adapter is reached, so the default is an invariant failure and
    // never an ordinary answer.
    fn guarded_dialog_input(
        &self,
        directory: &Path,
        session: &TerminalSession,
        input: &GuardedDialogInput,
        deadline: Instant,
    ) -> Result<bool> {
        let _ = (directory, input, deadline);
        bail!(
            "{} declares managed dialog input unsupported; the shared dispatch must refuse before its adapter",
            session.kind.display_name()
        )
    }
    /// The surface part of the ownership proof, checked before the owner's.
    fn verify_surface(&self, session: &TerminalSession, timeout: Option<Duration>) -> Result<()>;
    fn close_session_until(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome>;
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool>;
    /// The launcher's probe before startup is confirmed: presence, and for an adapter whose
    /// surface outlives its program (Herdr), whether that program has finished.
    fn launch_surface(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<super::LaunchSurface> {
        self.surface_present(directory, session, timeout)
            .map(super::LaunchSurface::from_presence)
    }
}

/// The adapter of a kind. Every operation resolves its adapter here and nowhere else;
/// production passes the installed `Dispatch`, a test one over its own adapters. A kind that
/// Windows does not run has no adapter.
pub(in crate::native) trait Adapters {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn WindowsTerminalAdapter>;
}

/// The two Windows surfaces: the managed console and a terminal of a Herdr Core.
pub(in crate::native::terminal) struct Dispatch<C, H> {
    console: C,
    herdr: H,
}

impl<C, H> Dispatch<C, H> {
    pub(in crate::native::terminal) const fn new(console: C, herdr: H) -> Self {
        Self { console, herdr }
    }
}

impl<C: WindowsTerminalAdapter, H: WindowsTerminalAdapter> Adapters for Dispatch<C, H> {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn WindowsTerminalAdapter> {
        match kind {
            TerminalKind::WindowsConsole => Ok(&self.console),
            TerminalKind::Herdr => Ok(&self.herdr),
            // A handle of a kind that Windows does not run (a record can name another OS's
            // terminal) gets the refusal every OS gives such a kind, as macOS and Linux do;
            // it never reaches the console helper.
            TerminalKind::Iterm2
            | TerminalKind::AppleTerminal
            | TerminalKind::Ghostty
            | TerminalKind::Warp
            | TerminalKind::WezTerm
            | TerminalKind::Tmux => Err(OsTerminals::WINDOWS.not_here(kind)),
        }
    }
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
    adapters.adapter(kind)?.open_bound_tab(
        kind,
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
    // Nothing is sent to a kind without an adapter.
    adapters
        .adapter(session.kind)
        .map_err(TerminalSendFailure::not_sent)?
        .send_file(session, prompt_path, deadline)
}

// The declared refusals answer before any adapter is looked up.
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
    directory: &Path,
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    if let Some(refusal) = session.kind.registration().guarded_input_refusal {
        return Err(anyhow::anyhow!(refusal));
    }
    adapters
        .adapter(session.kind)?
        .guarded_dialog_input(directory, session, input, deadline)
}

pub(in crate::native::terminal) fn verify_surface(
    adapters: &dyn Adapters,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    adapters
        .adapter(session.kind)?
        .verify_surface(session, timeout)
}

pub(in crate::native) fn close_session_until(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    adapters
        .adapter(session.kind)?
        .close_session_until(directory, session, deadline)
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

pub(in crate::native::terminal) fn launch_surface(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<super::LaunchSurface> {
    adapters
        .adapter(session.kind)?
        .launch_surface(directory, session, timeout)
}

// Loaded through `#[path]` beside `windows/mod.rs`, whose own `tests.rs` is the Windows-only
// one; the explicit path keeps this module's tests apart from it.
#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;
