//! The seven Windows surface operations (open, send, screen, guarded key, verify, close, in
//! its ordinary form or by creation record once the owner is gone, and presence). Screen and
//! guarded key first read the kind's declared refusal; then every
//! operation resolves its adapter through one lookup (`Adapters::adapter`): Herdr, or the
//! managed console for every other kind. macOS and Linux do the same through `posix.rs`.
//! Compiled into every test build, so that the lookup and the refusals are tested on every
//! CI target; the installed adapters stay in `windows/mod.rs` (the managed console) and
//! `herdr.rs`.
use super::{CloseOutcome, GuardedDialogInput, TerminalKind, TerminalSendResult, TerminalSession};
use anyhow::{Result, bail};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(in crate::native::terminal) type BindSurface<'a> =
    Box<dyn FnOnce(&mut TerminalSession) -> Result<()> + 'a>;
pub(in crate::native::terminal) type UnbindSurface<'a> = Box<dyn FnOnce() -> Result<()> + 'a>;

pub(in crate::native) trait WindowsTerminalAdapter {
    /// `kind` is the requested launch kind. Each adapter refuses kinds it does not create.
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
    /// Closes the surface with no live owner of this session left, on the creation record in
    /// the handle and the decision's `evidence`. Only a kind whose core keeps its terminals has
    /// such a record, and the dispatch refuses every other kind before its adapter, so the
    /// default is an invariant failure and never an ordinary answer.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    fn close_created_surface(
        &self,
        directory: &Path,
        session: &TerminalSession,
        evidence: super::close_authority::CreationEvidence,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        let _ = (directory, evidence, deadline);
        bail!(
            "{} keeps no creation record; the shared dispatch must refuse before its adapter",
            session.kind.display_name()
        )
    }
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool>;
}

/// The adapter of a kind. Every operation resolves its adapter here and nowhere else;
/// production passes the installed `Dispatch`, a test one over its own adapters.
pub(in crate::native) trait Adapters {
    fn adapter(&self, kind: TerminalKind) -> &dyn WindowsTerminalAdapter;
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
    fn adapter(&self, kind: TerminalKind) -> &dyn WindowsTerminalAdapter {
        match kind {
            TerminalKind::Herdr => &self.herdr,
            // A handle of a kind that Windows does not run (a record can name another
            // platform) stays with the managed console, whose open refuses the kind and
            // whose helper refuses the handle. Refusing it here instead would change what
            // those handles report; that is a separate decision.
            TerminalKind::WindowsConsole
            | TerminalKind::Iterm2
            | TerminalKind::AppleTerminal
            | TerminalKind::Ghostty
            | TerminalKind::Warp
            | TerminalKind::WezTerm
            | TerminalKind::Tmux => &self.console,
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
    adapters.adapter(kind).open_bound_tab(
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
    adapters
        .adapter(session.kind)
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
        .adapter(session.kind)
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
        .adapter(session.kind)
        .guarded_dialog_input(directory, session, input, deadline)
}

pub(in crate::native::terminal) fn verify_surface(
    adapters: &dyn Adapters,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    adapters
        .adapter(session.kind)
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
        .adapter(session.kind)
        .close_session_until(directory, session, deadline)
}

/// The close of a gone owner's surface, on its creation record. As on macOS and Linux, a kind
/// whose core keeps no terminals is refused before its adapter.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
pub(in crate::native) fn close_created_surface(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    evidence: super::close_authority::CreationEvidence,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    if !super::close_authority::owner_exit::core_retains_finished(session.kind) {
        bail!(
            "{} keeps no creation record that proves its surface without the owner; no close was sent and the handle is retained",
            session.kind.display_name()
        );
    }
    adapters
        .adapter(session.kind)
        .close_created_surface(directory, session, evidence, deadline)
}

pub(in crate::native::terminal) fn surface_present(
    adapters: &dyn Adapters,
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    adapters
        .adapter(session.kind)
        .surface_present(directory, session, timeout)
}

// Loaded through `#[path]` beside `windows/mod.rs`, whose own `tests.rs` is the Windows-only
// one; the explicit path keeps this module's tests apart from it.
#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;
