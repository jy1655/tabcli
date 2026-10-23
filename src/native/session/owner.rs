//! Observation of a session's recorded owner process. Read-only: no record changes.
use super::{CoreRecord, Reader, RecordReader};
use crate::native::process_identity::{Attested, Gone, ProcessObservation, classify};
#[cfg(target_os = "macos")]
use crate::native::terminal::ownership::NativeProcessIdentity;
use crate::native::terminal::ownership::NativeSessionOwner;
use agent_bridge::process_is_alive;
use anyhow::Result;
use serde::Serialize;

#[derive(Clone, Default, Serialize)]
pub(in crate::native) struct OwnerObservation {
    pub(in crate::native) process_alive: Option<bool>,
    pub(in crate::native) identity_matches: Option<bool>,
    pub(in crate::native) error: Option<String>,
}

// Reopen fixtures inspect this diagnostic projection. Launch repair uses the
// typed attestation below rather than these lossy diagnostic booleans.
#[cfg(test)]
pub(in crate::native) fn observe_owner(reader: &Reader) -> OwnerObservation {
    match attest_recorded_owner(reader) {
        Ok(Some(owner)) => OwnerObservation::from(&owner),
        Ok(None) => OwnerObservation::default(),
        Err(error) => OwnerObservation {
            error: Some(format!("{error:#}")),
            ..OwnerObservation::default()
        },
    }
}

/// Preserve process evidence for lifecycle callers; the diagnostic bools are lossy.
pub(super) fn attest_recorded_owner(reader: &Reader) -> Result<Option<OwnerAttestation>> {
    Ok(RecordReader::at(reader.record(CoreRecord::Owner).path())
        .optional_json::<NativeSessionOwner>()?
        .map(|owner| attest_owner(&owner)))
}

#[cfg(any(target_os = "macos", windows))]
pub(in crate::native) fn observe_owner_record(owner: &NativeSessionOwner) -> OwnerObservation {
    OwnerObservation::from(&attest_owner(owner))
}

#[derive(Debug)]
pub(in crate::native) struct OwnerAttestation {
    pub process: ProcessObservation,
    pub control: ControlAttestation,
    pub pid_alive: Option<bool>,
}

#[derive(Debug)]
pub(in crate::native) enum ControlAttestation {
    NotRecorded,
    #[cfg(any(target_os = "macos", windows, test))]
    Verified,
    #[cfg(any(target_os = "macos", test))]
    Changed(String),
    #[cfg(any(target_os = "macos", test))]
    Unknown(String),
}

pub(in crate::native) struct OwnerObservers<'a> {
    pub pid_alive: &'a dyn Fn(u32) -> bool,
    #[cfg(target_os = "macos")]
    pub birth: &'a dyn Fn(u32) -> Result<Option<(u64, u64)>>,
    #[cfg(target_os = "macos")]
    pub control: &'a dyn Fn(u32) -> Result<Option<NativeProcessIdentity>>,
    #[cfg(windows)]
    pub identity:
        &'a dyn Fn(u32) -> Result<Option<crate::native::terminal::WindowsProcessIdentity>>,
}

/// Reads only the supplied owner and OS evidence. Never acquires a session lock or reads records.
pub(in crate::native) fn attest_owner(owner: &NativeSessionOwner) -> OwnerAttestation {
    attest_owner_with(
        owner,
        OwnerObservers {
            pid_alive: &process_is_alive,
            #[cfg(target_os = "macos")]
            birth: &crate::native::terminal::macos::process::macos_process_start,
            #[cfg(target_os = "macos")]
            control:
                &crate::native::terminal::macos::process::live_native_process_identity_if_present,
            #[cfg(windows)]
            identity: &crate::native::terminal::observe_windows_process_identity,
        },
    )
}

pub(in crate::native) fn attest_owner_with(
    owner: &NativeSessionOwner,
    observers: OwnerObservers<'_>,
) -> OwnerAttestation {
    // One actual PID observation, including for projection of failed identity inspection.
    let pid_alive = (observers.pid_alive)(owner.pid);
    #[cfg(target_os = "macos")]
    let recorded_birth = owner
        .process_start_seconds
        .zip(owner.process_start_microseconds);
    #[cfg(target_os = "macos")]
    let process = classify(
        owner.pid,
        recorded_birth.as_ref(),
        || pid_alive,
        || (observers.birth)(owner.pid),
    );
    #[cfg(windows)]
    let process = {
        use crate::native::terminal::WindowsIncarnation;
        classify(
            owner.pid,
            owner
                .windows_process_identity
                .as_ref()
                .map(|i| WindowsIncarnation(i.clone()))
                .as_ref(),
            || pid_alive,
            || (observers.identity)(owner.pid).map(|i| i.map(WindowsIncarnation)),
        )
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let process = classify::<()>(owner.pid, None, || pid_alive, || unreachable!());
    #[cfg(target_os = "macos")]
    let control = if matches!(process, ProcessObservation::Live(Attested::Incarnation))
        && (owner.terminal_tty_device.is_some()
            || owner.process_group.is_some()
            || owner.terminal_process_group.is_some())
    {
        match (observers.control)(owner.pid) {
            Ok(Some(live)) => {
                // This later snapshot must describe the incarnation whose birth we
                // just attested. Do not discard positive evidence of intervening reuse.
                if live.pid != owner.pid {
                    return OwnerAttestation {
                        process: ProcessObservation::Unknown(format!(
                            "control snapshot returned process {} instead of {}",
                            live.pid, owner.pid
                        )),
                        control: ControlAttestation::NotRecorded,
                        pid_alive: Some(pid_alive),
                    };
                }
                let live_birth = (live.process_start_seconds, live.process_start_microseconds);
                if Some(live_birth) != recorded_birth {
                    return OwnerAttestation {
                        process: classify(
                            owner.pid,
                            recorded_birth.as_ref(),
                            || pid_alive,
                            || Ok(Some(live_birth)),
                        ),
                        control: ControlAttestation::NotRecorded,
                        pid_alive: Some(pid_alive),
                    };
                }
                let changed = [
                    (
                        owner
                            .terminal_tty_device
                            .is_some_and(|v| v != live.terminal_tty_device),
                        "TTY",
                    ),
                    (
                        owner.process_group.is_some_and(|v| v != live.process_group),
                        "process group",
                    ),
                    (
                        owner
                            .terminal_process_group
                            .is_some_and(|v| v != live.terminal_process_group),
                        "terminal process group",
                    ),
                ]
                .into_iter()
                .filter_map(|(changed, name)| changed.then_some(name))
                .collect::<Vec<_>>();
                if changed.is_empty() {
                    ControlAttestation::Verified
                } else {
                    ControlAttestation::Changed(format!("owner {} changed", changed.join(", ")))
                }
            }
            Ok(None) => ControlAttestation::Unknown("exited during attestation".into()),
            Err(e) => ControlAttestation::Unknown(format!(
                "failed to verify the birth identity of live native-session process {}: {e:#}",
                owner.pid
            )),
        }
    } else {
        ControlAttestation::NotRecorded
    };
    #[cfg(windows)]
    let control = if matches!(process, ProcessObservation::Live(Attested::Incarnation)) {
        ControlAttestation::Verified
    } else {
        ControlAttestation::NotRecorded
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let control = ControlAttestation::NotRecorded;
    OwnerAttestation {
        process,
        control,
        pid_alive: Some(pid_alive),
    }
}

impl From<&OwnerAttestation> for OwnerObservation {
    fn from(a: &OwnerAttestation) -> Self {
        let (process_alive, identity_matches, error) = match &a.process {
            ProcessObservation::Live(Attested::Incarnation) => match &a.control {
                #[cfg(any(target_os = "macos", windows, test))]
                ControlAttestation::Verified => (Some(true), Some(true), None),
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Changed(reason) => {
                    // The reason belongs to control evidence, not an inspection error.
                    let _ = reason;
                    (Some(true), Some(false), None)
                }
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Unknown(reason) => (a.pid_alive, None, Some(reason.clone())),
                ControlAttestation::NotRecorded => (Some(true), None, None),
            },
            ProcessObservation::Live(Attested::PidOnly) => (Some(true), None, None),
            ProcessObservation::Gone(Gone::Exited) => (Some(false), None, None),
            ProcessObservation::Gone(Gone::Reused(_)) => (Some(true), Some(false), None),
            ProcessObservation::Unknown(reason) => (a.pid_alive, None, Some(reason.clone())),
        };
        Self {
            process_alive,
            identity_matches,
            error,
        }
    }
}

#[cfg(test)]
pub(in crate::native) mod tests;
