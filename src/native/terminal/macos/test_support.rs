use super::{MacTerminalAdapter, OwnedSurfaceClose};
use crate::native::session::primitives::set_private_directory_permissions;
use crate::native::session::{SessionState, Store, launch, turn, update_status};
use crate::native::settings::MacosOpenMode;
use crate::native::terminal::{CloseOutcome, TerminalSendResult, TerminalSession};
use anyhow::Result;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// The adapter of a kind a dispatch test does not exercise: reaching it is the
/// test's failure, named by the kind.
pub(in crate::native::terminal) struct Unused(pub(in crate::native::terminal) &'static str);

impl Unused {
    fn reached(&self, operation: &str) -> ! {
        panic!("{} adapter is not under test: {operation}", self.0)
    }
}

impl MacTerminalAdapter for Unused {
    fn open_bound_tab(
        &self,
        _: MacosOpenMode,
        _: &str,
        _: &Path,
        _: Instant,
        _: super::BindSurface<'_>,
        _: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        self.reached("open_bound_tab")
    }
    fn send_file(&self, _: &TerminalSession, _: &Path, _: Instant) -> TerminalSendResult {
        self.reached("send_file")
    }
    fn verify_surface(
        &self,
        _: &Path,
        _: &TerminalSession,
        _: Option<Duration>,
    ) -> Result<Option<String>> {
        self.reached("verify_surface")
    }
    fn surface_present(&self, _: &Path, _: &TerminalSession, _: Duration) -> Result<bool> {
        self.reached("surface_present")
    }
    fn close_session_until(
        &self,
        _: &Path,
        _: &TerminalSession,
        _: Instant,
    ) -> Result<CloseOutcome> {
        self.reached("close_session_until")
    }
    fn close_owned_session(
        &self,
        _: &Path,
        _: &str,
        _: &TerminalSession,
        _: OwnedSurfaceClose,
        _: Instant,
    ) -> Result<CloseOutcome> {
        self.reached("close_owned_session")
    }
}

pub(super) fn launch_fixture(prefix: &str, timeout: Duration, parent: &Path) -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .unwrap();
    // On macOS this is exactly set_permissions(Permissions::from_mode(0o700)).
    set_private_directory_permissions(directory.path()).unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let store = Store::open_unchecked(directory.path());
    let claim = turn::claim(&store, &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launch::begin(&store, &token, Instant::now() + timeout).unwrap();
    directory
}

// Let zsh decode the actual builder's quoting, without running the host or wrapper.
pub(super) fn host_argv(invocation: &str) -> Vec<String> {
    let mut args = shell_argv(invocation);
    assert_eq!(
        args.remove(0),
        std::env::current_exe().unwrap().to_str().unwrap()
    );
    args
}

pub(super) fn shell_argv(invocation: &str) -> Vec<String> {
    let output = std::process::Command::new("/bin/zsh")
        .args([
            "-f",
            "-c",
            &format!("set -- {invocation}; printf '%s\\0' \"$@\""),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .split_terminator('\0')
        .map(str::to_owned)
        .collect()
}

pub(super) fn host_log_messages(directory: &Path, prefix: &str) -> Vec<String> {
    let text = std::fs::read_to_string(directory.join(launch::LOG)).unwrap_or_default();
    text.split_inclusive('\n')
        .filter_map(|line| {
            let (timestamp, message) = line.split_once(' ')?;
            if !message.starts_with(prefix) {
                return None;
            }
            timestamp.parse::<u128>().unwrap();
            Some(message.to_owned())
        })
        .collect()
}
