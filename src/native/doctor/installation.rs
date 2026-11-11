//! The provider-installation rows of a doctor report: the executable, its version and the
//! working directory. Each row is computed from what `assemble_checks` observed, in the order
//! it observed it; nothing here reads the file system or runs a probe.
use crate::native::diagnostics::vocabulary::{CheckId, Reason};
use crate::native::diagnostics::{Availability, Check};
use crate::native::provider_installation::{Executable, VersionReport, VersionSupport};
use agent_bridge::FirstPartyCli;
use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

/// The executable row, and the executable a version probe may run.
pub(super) fn executable_check(observed: Result<Executable>) -> (Check, Option<PathBuf>) {
    use Availability::*;
    let unavailable = |detail: String| {
        Check::new(
            CheckId::PROVIDER_EXECUTABLE,
            Unavailable,
            Reason::EXECUTABLE_UNAVAILABLE,
            detail,
            "Check the provider installation and PATH; existing sessions retain their recorded executable path.",
        )
    };
    match observed {
        Ok(Executable::Usable(path)) => (
            Check::new(
                CheckId::PROVIDER_EXECUTABLE,
                Available,
                Reason::EXECUTABLE_FOUND,
                "Provider executable found; presence alone does not establish runtime capability.",
                "Use --probe to read its current version.",
            )
            .evidence(json!({"path": path})),
            Some(path),
        ),
        Ok(Executable::Relative(path)) => (
            Check::new(
                CheckId::PROVIDER_EXECUTABLE,
                Unknown,
                Reason::EXECUTABLE_PATH_UNVERIFIED,
                "The recorded executable path is relative; doctor does not substitute a PATH lookup for it.",
                "Inspect the original launch configuration.",
            )
            .evidence(json!({"path": path})),
            None,
        ),
        Ok(Executable::Unusable(path)) => (
            unavailable(format!(
                "Provider executable is missing, not executable, or not absolute: {}",
                path.display()
            )),
            None,
        ),
        Err(error) => (unavailable(format!("{error:#}")), None),
    }
}

/// The opt-in version probe as doctor ran it, or why it did not run.
pub(super) enum VersionProbe {
    NotRequested,
    NoExecutable,
    /// The probe produced no output: it could not start, overran its budget or its output cap.
    Failed(anyhow::Error),
    Completed(VersionReport),
}

/// The version row, and the version text it reports as current.
pub(super) fn version_check(
    provider: FirstPartyCli,
    probe: VersionProbe,
) -> (Check, Option<String>) {
    use Availability::*;
    let failed = |detail: String| {
        Check::new(
            CheckId::PROVIDER_VERSION,
            Unknown,
            Reason::VERSION_PROBE_FAILED,
            detail,
            "Inspect the provider installation; a failed probe does not prove availability.",
        )
    };
    match probe {
        VersionProbe::Completed(VersionReport::Read { text, support }) => {
            let (availability, reason) = match support {
                VersionSupport::Supported => (Available, Reason::VERSION_SUPPORTED),
                VersionSupport::TooOld => (Unavailable, Reason::VERSION_UNSUPPORTED),
                VersionSupport::Unrecognized(_) => (Unknown, Reason::VERSION_UNRECOGNIZED),
            };
            (
                Check::new(
                    CheckId::PROVIDER_VERSION,
                    availability,
                    reason,
                    format!(
                        "Current CLI version; required minimum {}",
                        provider.minimum_version()
                    ),
                    "CLI version is only one gate; inspect the provider-specific checks.",
                )
                .evidence(json!({"current_version": text})),
                Some(text),
            )
        }
        VersionProbe::Completed(VersionReport::Exited(status)) => {
            (failed(format!("Version probe exited with {status}")), None)
        }
        VersionProbe::Failed(error) => (failed(format!("{error:#}")), None),
        VersionProbe::NoExecutable => (
            Check::new(
                CheckId::PROVIDER_VERSION,
                Unknown,
                Reason::EXECUTABLE_UNAVAILABLE,
                "No unambiguous executable is available for a version probe.",
                "Inspect the provider_executable check.",
            ),
            None,
        ),
        VersionProbe::NotRequested => (
            Check::new(
                CheckId::PROVIDER_VERSION,
                Unknown,
                Reason::PROBE_NOT_REQUESTED,
                "Current CLI version was not queried. A launch-recorded version is historical evidence only.",
                "Add --probe for bounded local version checks; no model or message is sent.",
            )
            .evidence(json!({"minimum_version": provider.minimum_version().to_string()})),
            None,
        ),
    }
}

/// The workspace row, from whether the path was observed to be a directory.
pub(super) fn workspace_check(workspace: &Path, is_directory: std::io::Result<bool>) -> Check {
    use Availability::*;
    let (availability, reason) = match is_directory {
        Ok(true) => (Available, Reason::WORKSPACE_PRESENT),
        Ok(false) => (Unavailable, Reason::WORKSPACE_NOT_DIRECTORY),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (Unavailable, Reason::WORKSPACE_MISSING)
        }
        Err(_) => (Unknown, Reason::WORKSPACE_UNREADABLE),
    };
    Check::new(
        CheckId::WORKSPACE,
        availability,
        reason,
        "Working directory observation; CLI version checks do not depend on this directory.",
        "Restore or inspect the recorded directory before workspace-relative operations.",
    )
    .evidence(json!({"path": workspace}))
}

#[cfg(test)]
mod tests;
