//! The Windows surface operations. Each one reads the kind's declared refusals, then
//! resolves its adapter through one lookup (`Adapters::adapter`): the managed console or
//! Herdr. macOS and Linux do the same through `posix.rs`. Compiled into every test build, so
//! that the lookup and the refusals are tested on every CI target; the installed adapters
//! stay in `windows/mod.rs` (the managed console) and `herdr.rs`.
use super::{
    CloseOutcome, GuardedDialogInput, TerminalKind, TerminalSendFailure, TerminalSendResult,
    TerminalSession,
};
use crate::native::failure_kind::Unsupported;
use anyhow::{Result, bail};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(in crate::native::terminal) type BindSurface<'a> =
    Box<dyn FnOnce(&mut TerminalSession) -> Result<()> + 'a>;
pub(in crate::native::terminal) type UnbindSurface<'a> = Box<dyn FnOnce() -> Result<()> + 'a>;

pub(in crate::native::terminal) trait WindowsTerminalAdapter {
    fn open_bound_tab(
        &self,
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
}

/// The adapter of a kind. Every operation resolves its adapter here and nowhere else;
/// production passes the installed `Dispatch`, a test one over its own adapters.
pub(in crate::native::terminal) trait Adapters {
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
            TerminalKind::Iterm2
            | TerminalKind::AppleTerminal
            | TerminalKind::Ghostty
            | TerminalKind::Warp
            | TerminalKind::WezTerm
            | TerminalKind::Tmux => bail!(Unsupported::new(format!(
                "{} is not available on Windows",
                kind.display_name()
            ))),
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

// Loaded through `#[path]` beside `windows/mod.rs`, whose own `tests.rs` is the Windows-only
// one; the explicit path keeps this module's tests apart from it.
#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;
