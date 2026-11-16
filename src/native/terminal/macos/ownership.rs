use super::process::{live_native_process_identity, terminal_tty_device};
use crate::native::session::{self, Reader};
use crate::native::terminal;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, verify_native_terminal_owner_with,
};
use anyhow::{Context, Result};
use std::path::Path;

#[cfg(target_os = "macos")]
pub(in crate::native) fn verified_macos_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let owner = session::owner::observe(&Reader::open_unchecked(directory))
        .decoded()?
        .with_context(|| "Terminal.app ownership requires a live native-session owner")?
        .owner;
    verify_native_terminal_owner_with(
        expected_session_id,
        session,
        surface_tty,
        owner,
        live_native_process_identity,
        terminal_tty_device,
    )
}
