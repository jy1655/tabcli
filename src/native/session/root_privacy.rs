//! The selected state root. Other private directories retain their own policies.
use anyhow::{Context, Result};
use std::{fmt, path::Path};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Clone, Copy, PartialEq)]
pub(in crate::native) enum Action {
    Inspect,
    Prepare { created: bool },
}

#[derive(Debug)]
pub(in crate::native) struct Mismatch(String);
impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Mismatch {}

#[derive(Debug)]
struct NoTabEntry;
impl fmt::Display for NoTabEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no TAB entry authorizes this repair")
    }
}
impl std::error::Error for NoTabEntry {}

pub(in crate::native) fn check(path: &Path, action: Action) -> Result<bool> {
    platform::check(path, action).with_context(|| format!("state root {}", path.display()))
}

#[derive(Clone, Copy)]
pub(in crate::native) enum FailurePolicy {
    Refuse,
    Warn,
}

pub(in crate::native) fn prepare(path: &Path, created: bool, policy: FailurePolicy) -> Result<()> {
    match check(path, Action::Prepare { created }) {
        Ok(true) => {
            crate::native::session_start::deadline::wrote("state root permissions were set")
        }
        Ok(false) => {}
        Err(error) => match policy {
            FailurePolicy::Warn if !error.is::<NoTabEntry>() => eprintln!("warning: {error:#}"),
            _ => return Err(error),
        },
    }
    Ok(())
}

pub(in crate::native) fn create_leaf(path: &Path, _policy: FailurePolicy) -> std::io::Result<()> {
    let result = platform::create_leaf(path);
    // Standalone writers may still be able to create normally when Windows
    // cannot install the private descriptor at creation. Prepare judges it next.
    #[cfg(windows)]
    if matches!(_policy, FailurePolicy::Warn)
        && result
            .as_ref()
            .is_err_and(|e| e.kind() != std::io::ErrorKind::AlreadyExists)
    {
        return std::fs::create_dir(path);
    }
    result
}

#[cfg(any(target_os = "macos", windows))]
fn tab_entry(name: &str, directory: bool, regular: bool) -> bool {
    use crate::native::session::{names::STATE_ROOT_DURABLE_FILE, primitives::valid_session_id};
    (regular && matches!(name, STATE_ROOT_DURABLE_FILE | "settings.json"))
        || (directory && (name == "workspace-consent" || valid_session_id(name)))
}
