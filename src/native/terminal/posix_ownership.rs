use super::process::{live_native_process_identity, terminal_tty_device};
use crate::native::session::{self, Reader};
use crate::native::terminal;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, verify_native_terminal_owner_with,
};
use anyhow::{Context, Result};
use std::path::Path;

/// What the POSIX ownership proof reads from the system: the identity of a process and the
/// device of a TTY path. Production reads the system; a test substitutes both, so that no
/// real process is observed.
#[derive(Clone, Copy)]
pub(in crate::native::terminal) struct ProcessReads<'a> {
    pub(in crate::native::terminal) identity: &'a dyn Fn(u32) -> Result<NativeProcessIdentity>,
    pub(in crate::native::terminal) tty_device: &'a dyn Fn(&Path) -> Result<u64>,
}

pub(in crate::native::terminal) const SYSTEM_READS: ProcessReads<'static> = ProcessReads {
    identity: &live_native_process_identity,
    tty_device: &terminal_tty_device,
};

pub(in crate::native) fn verified_posix_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    verified_posix_terminal_owner_with(
        directory,
        expected_session_id,
        session,
        surface_tty,
        SYSTEM_READS,
    )
}

pub(in crate::native::terminal) fn verified_posix_terminal_owner_with(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
    reads: ProcessReads<'_>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let owner = session::owner::observe(&Reader::open_unchecked(directory))
        .decoded()?
        .with_context(|| {
            format!(
                "{} ownership requires a live native-session owner",
                session.kind.display_name()
            )
        })?
        .owner;
    verify_native_terminal_owner_with(
        expected_session_id,
        session,
        surface_tty,
        owner,
        reads.identity,
        reads.tty_device,
    )
}

// Source compatibility for the macOS application adapters; Linux uses the POSIX names.
#[cfg(target_os = "macos")]
pub(in crate::native::terminal) use verified_posix_terminal_owner_with as verified_macos_terminal_owner_with;
