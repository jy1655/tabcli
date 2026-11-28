//! The selected state root. Other private directories retain their own policies.
use anyhow::{Context, Result};
use std::path::Path;

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

mod diagnosis;
pub(super) use diagnosis::Observation;
pub(in crate::native) use diagnosis::{Diagnosis, RootStatus};
use diagnosis::{Entries, Flags};

impl Action {
    fn entries(self, list: impl FnOnce() -> Result<bool>) -> Entries {
        match self {
            Self::Prepare { created: true } => Entries::Known,
            _ => match list() {
                Ok(true) => Entries::Known,
                Ok(false) => Entries::Empty {
                    required_for_repair: false,
                },
                Err(error) => Entries::Undetermined(Some(error)),
            },
        }
    }

    fn unopened(self) -> Entries {
        match self {
            Self::Prepare { created: true } => Entries::Known,
            _ => Entries::Undetermined(None),
        }
    }
}

pub(super) fn path_error(
    path: &Path,
    operation: Observation,
    error: std::io::Error,
) -> anyhow::Error {
    operation.render(
        path,
        &Entries::Undetermined(None),
        Flags::None,
        Some(error.into()),
    )
}

pub(in crate::native) fn inspect_selected() -> Result<bool> {
    let root = crate::native::session::Reader::state_root().map_err(|error| {
        Observation::Resolve.render(
            Path::new(""),
            &Entries::Undetermined(None),
            Flags::None,
            Some(error),
        )
    })?;
    check(&root, Action::Inspect)
}

pub(in crate::native) fn check(path: &Path, action: Action) -> Result<bool> {
    platform::check(path, action)
        .map_err(|error| {
            if error.is::<Diagnosis>() {
                error
            } else {
                Observation::Inspect.render(path, &action.unopened(), Flags::None, Some(error))
            }
        })
        .with_context(|| format!("state root {}", path.display()))
}

#[derive(Clone, Copy)]
pub(in crate::native) enum FailurePolicy {
    Refuse,
    Warn,
}

pub(in crate::native) fn prepare(path: &Path, created: bool, policy: FailurePolicy) -> Result<()> {
    apply_policy(
        check(path, Action::Prepare { created }),
        policy,
        |warning| eprintln!("{warning}"),
    )
}

fn apply_policy(
    result: Result<bool>,
    policy: FailurePolicy,
    warn: impl FnOnce(String),
) -> Result<()> {
    match result {
        Ok(true) => {
            crate::native::session_start::deadline::wrote("state root permissions were set")
        }
        Ok(false) => {}
        Err(error) => match policy {
            FailurePolicy::Warn
                if !error
                    .downcast_ref::<Diagnosis>()
                    .is_some_and(|d| d.refuse_writers) =>
            {
                warn(format!("warning: {error:#}"))
            }
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

fn tab_entry(name: &str, directory: bool, regular: bool) -> bool {
    use crate::native::session::{names::STATE_ROOT_DURABLE_FILE, primitives::valid_session_id};
    (regular && matches!(name, STATE_ROOT_DURABLE_FILE | "settings.json"))
        || (directory && (name == "workspace-consent" || valid_session_id(name)))
}
