use super as terminal;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::CloseOutcome;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::TerminalSession;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::posix_ownership::verified_posix_terminal_owner;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::process::{current_terminal_tty, live_native_process_identity, terminal_tty_device};
#[cfg(windows)]
pub(in crate::native) use super::windows::ownership::verified_windows_native_owner;
#[cfg(all(test, target_os = "macos"))]
use crate::native::process_identity::Gone;
use crate::native::process_identity::{Attested, ProcessObservation};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::session::Reader;
use crate::native::session::close::budget::close_remaining;
use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::session::{self, Store};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
use anyhow::bail;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Instant;
use std::{path::Path, time::Duration};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(in crate::native) struct NativeSessionOwner {
    pub(in crate::native) pid: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "terminal::linux_birth::present"
    )]
    pub(in crate::native) linux_birth: Option<terminal::LinuxBirth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) managed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_tty_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_start_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_start_microseconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_shell: Option<TerminalShellIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_app: Option<MacTerminalAppIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) windows_process_identity: Option<terminal::WindowsProcessIdentity>,
}

/// What an owner record says about the Session it belongs to, for one expected Session.
/// Each reader maps the three answers itself; the record's writer always binds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum OwnerBinding {
    /// The record names the expected Session.
    Bound,
    /// A legacy record that names no Session.
    Unbound,
    /// The record names another Session.
    Foreign,
}

/// The owner proof requires a controlling TTY device, both process groups and a
/// complete platform birth: macOS seconds/microseconds or Linux boot/ticks.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum ControlRecord {
    /// None of them: a legacy record, or one written on another platform.
    NotRecorded,
    /// All five, as the macOS writer records them.
    Complete(RecordedControl),
    /// Some but not all of them.
    Partial,
    /// Complete Linux control; mixed macOS birth fields are Partial.
    Linux {
        terminal_tty_device: u64,
        birth: terminal::LinuxBirth,
        process_group: u32,
        terminal_process_group: u32,
    },
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) struct RecordedControl {
    pub(in crate::native) terminal_tty_device: u64,
    pub(in crate::native) process_start_seconds: u64,
    pub(in crate::native) process_start_microseconds: u64,
    pub(in crate::native) process_group: u32,
    pub(in crate::native) terminal_process_group: u32,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl ControlRecord {
    pub(in crate::native) fn is_complete(self) -> bool {
        matches!(self, Self::Complete(_) | Self::Linux { .. })
    }
}

impl NativeSessionOwner {
    /// Whether this record is bound to `expected_session_id`.
    pub(in crate::native) fn binding(&self, expected_session_id: &str) -> OwnerBinding {
        match self.managed_session_id.as_deref() {
            Some(bound) if bound == expected_session_id => OwnerBinding::Bound,
            Some(_) => OwnerBinding::Foreign,
            None => OwnerBinding::Unbound,
        }
    }

    // Two rules read the macOS control fields, and they differ on purpose (CONTEXT.md
    // Attestation, Close authority):
    // - `control_record` classifies all five. The macOS ownership proof
    //   (`verify_native_terminal_owner_with`), the dead-owner close authority, the close and
    //   teardown intents, Terminal.app's failed-start absence and Warp's close require it to be
    //   complete; `native_owner_identity_matches` also treats a record with none of the five
    //   as a legacy match and a partial one as a mismatch.
    // - `attestation_compares_control` is attestation's: it compares the control identity of a
    //   live incarnation when any of the TTY device and the two process groups is recorded,
    //   and compares only the recorded ones. A partial record can attest verified although
    //   the ownership proof refuses it (`SurfaceProbe::owner_requirement`).

    /// The five control fields, classified for the ownership proof and close authority.
    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(in crate::native) fn control_record(&self) -> ControlRecord {
        if let Some(birth) = self.linux_birth {
            return match (
                self.terminal_tty_device,
                self.process_group,
                self.terminal_process_group,
                self.process_start_seconds,
                self.process_start_microseconds,
            ) {
                (
                    Some(terminal_tty_device),
                    Some(process_group),
                    Some(terminal_process_group),
                    None,
                    None,
                ) => ControlRecord::Linux {
                    terminal_tty_device,
                    birth,
                    process_group,
                    terminal_process_group,
                },
                _ => ControlRecord::Partial,
            };
        }
        match (
            self.terminal_tty_device,
            self.process_start_seconds,
            self.process_start_microseconds,
            self.process_group,
            self.terminal_process_group,
        ) {
            (None, None, None, None, None) => ControlRecord::NotRecorded,
            (
                Some(terminal_tty_device),
                Some(process_start_seconds),
                Some(process_start_microseconds),
                Some(process_group),
                Some(terminal_process_group),
            ) => ControlRecord::Complete(RecordedControl {
                terminal_tty_device,
                process_start_seconds,
                process_start_microseconds,
                process_group,
                terminal_process_group,
            }),
            _ => ControlRecord::Partial,
        }
    }

    /// Attestation's rule: a live incarnation's control identity is compared when any of
    /// the TTY device and the two process groups is recorded; the birth does not count.
    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(in crate::native) fn attestation_compares_control(&self) -> bool {
        self.terminal_tty_device.is_some()
            || self.process_group.is_some()
            || self.terminal_process_group.is_some()
    }
}

// Shell records preserve the old macOS wall-clock fields, including zero
// microseconds. Linux writes only linux_birth. Missing, mixed or partial birth
// payloads refuse decoding instead of becoming a PID-only shell identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "TerminalShellWire")]
pub(in crate::native) struct TerminalShellIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) process_group: u32,
    pub(in crate::native) terminal_tty_device: u64,
    pub(in crate::native) process_start_seconds: u64,
    pub(in crate::native) process_start_microseconds: u64,
    pub(in crate::native) linux_birth: Option<terminal::LinuxBirth>,
}
#[derive(Deserialize)]
struct TerminalShellWire {
    pid: u32,
    process_group: u32,
    terminal_tty_device: u64,
    process_start_seconds: Option<u64>,
    process_start_microseconds: Option<u64>,
    #[serde(default, deserialize_with = "terminal::linux_birth::present")]
    linux_birth: Option<terminal::LinuxBirth>,
}
impl TryFrom<TerminalShellWire> for TerminalShellIdentity {
    type Error = &'static str;
    fn try_from(wire: TerminalShellWire) -> std::result::Result<Self, Self::Error> {
        let (seconds, microseconds) = match (
            wire.linux_birth,
            wire.process_start_seconds,
            wire.process_start_microseconds,
        ) {
            (Some(_), None, None) => (0, 0),
            (None, Some(s), Some(us)) => (s, us),
            _ => return Err("terminal shell has missing, partial or mixed platform birth"),
        };
        Ok(Self {
            pid: wire.pid,
            process_group: wire.process_group,
            terminal_tty_device: wire.terminal_tty_device,
            process_start_seconds: seconds,
            process_start_microseconds: microseconds,
            linux_birth: wire.linux_birth,
        })
    }
}
impl Serialize for TerminalShellIdentity {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut wire = serializer.serialize_struct(
            "TerminalShellIdentity",
            if self.linux_birth.is_some() { 4 } else { 5 },
        )?;
        wire.serialize_field("pid", &self.pid)?;
        wire.serialize_field("process_group", &self.process_group)?;
        wire.serialize_field("terminal_tty_device", &self.terminal_tty_device)?;
        if let Some(birth) = self.linux_birth {
            if self.process_start_seconds != 0 || self.process_start_microseconds != 0 {
                return Err(serde::ser::Error::custom("mixed terminal shell birth"));
            }
            wire.serialize_field("linux_birth", &birth)?;
        } else {
            wire.serialize_field("process_start_seconds", &self.process_start_seconds)?;
            wire.serialize_field(
                "process_start_microseconds",
                &self.process_start_microseconds,
            )?;
        }
        wire.end()
    }
}

// Captured from the verified native owner's ancestor chain before provider start.
// A Terminal window id and tty are identities only inside this app incarnation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct MacTerminalAppIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) start_seconds: u64,
    pub(in crate::native) start_microseconds: u64,
}

/// A native control snapshot carries exactly one platform's birth. Records keep
/// their original macOS fields and optional Linux object; only readers translate them.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::native) enum ProcessBirth {
    Macos(u64, u64),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Linux(terminal::LinuxBirth),
}
// Reuse diagnostics print this: macOS keeps its earlier `(seconds, microseconds)` text.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl std::fmt::Debug for ProcessBirth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Macos(seconds, microseconds) => (seconds, microseconds).fmt(f),
            Self::Linux(birth) => birth.fmt(f),
        }
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl ProcessBirth {
    pub(in crate::native) fn macos(self) -> Option<(u64, u64)> {
        match self {
            Self::Macos(s, us) => Some((s, us)),
            Self::Linux(_) => None,
        }
    }
    pub(in crate::native) fn linux(self) -> Option<terminal::LinuxBirth> {
        match self {
            Self::Linux(birth) => Some(birth),
            Self::Macos(_, _) => None,
        }
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) struct NativeProcessIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) parent_pid: u32,
    pub(in crate::native) terminal_tty_device: u64,
    pub(in crate::native) process_group: u32,
    pub(in crate::native) terminal_process_group: u32,

    pub(in crate::native) birth: ProcessBirth,
}

// What this probe requires of the owner record is stated over its attestation by
// `SurfaceProbe::owner_requirement` below; the parity tests in `ownership/tests.rs` hold
// the two together.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(in crate::native::terminal) fn verify_native_terminal_owner_with(
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
    owner: NativeSessionOwner,
    mut process: impl FnMut(u32) -> Result<NativeProcessIdentity>,
    tty_device: impl FnOnce(&Path) -> Result<u64>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let live = process(owner.pid)?;
    session.verify_managed_session(expected_session_id)?;
    if owner.binding(expected_session_id) != OwnerBinding::Bound {
        bail!("native-session owner is not bound to this managed session")
    }
    if !owner.control_record().is_complete() || !native_owner_identity_matches(&owner, &live) {
        bail!("native-session owner birth, TTY, or foreground identity changed")
    }
    let owner_tty = owner
        .terminal_tty
        .as_deref()
        .context("native-session owner is missing its controlling TTY")?;
    if surface_tty.is_some_and(|tty| tty != owner_tty) {
        bail!("terminal surface is attached to a different native-session TTY")
    }
    if tty_device(Path::new(owner_tty))? != live.terminal_tty_device {
        bail!("native-session owner TTY path no longer identifies its controlling TTY")
    }
    verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = process(live.parent_pid)?;
    verified_terminal_shell_process_group(session.kind.display_name(), &owner, &live, &live_shell)?;
    Ok((owner, live))
}

/// The surface probe `tell` runs to prove that the recorded owner controls its surface.
/// The host's probe is chosen here, once, as [`SurfaceProbe::HOST`]; every probe stays a
/// value so that each platform's requirement is tested on every host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum SurfaceProbe {
    /// macOS and Linux (`verify_native_terminal_owner_with`): the recorded birth, controlling TTY
    /// device and both process groups must be present and match the live owner.
    PosixControl,
    /// Native Windows (`verified_windows_native_owner`): the recorded process identity must
    /// be present and match the live owner.
    WindowsProcess,
    /// No terminal is supported on this platform, so no probe runs.
    Unprobed,
}

/// What a probe's owner requirement says of a recorded owner, judged from its attestation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum OwnerRequirement {
    /// The live incarnation matched every identity this probe compares (none where no
    /// probe runs).
    Met,
    /// The owner is live, but an identity this probe compares changed or was never
    /// recorded. A changed control identity is not a death.
    ControlUnverified,
    /// The recorded incarnation is gone: the probe refuses it, and that is a death, not a
    /// changed identity.
    Gone,
    /// Cannot be told: the incarnation or its control identity could not be inspected.
    Uninspected,
}

impl SurfaceProbe {
    pub(in crate::native) const HOST: Self = if cfg!(any(target_os = "macos", target_os = "linux"))
    {
        Self::PosixControl
    } else if cfg!(windows) {
        Self::WindowsProcess
    } else {
        Self::Unprobed
    };

    /// What this probe requires of an owner record already bound to the session, judged
    /// from the record's attestation (CONTEXT.md Attestation) instead of running the probe;
    /// the binding and the surface are the caller's to judge. An attestation does not say
    /// which control fields were recorded: a record with only some of them attests verified
    /// when those match, and is met here although the macOS probe requires all of them.
    pub(in crate::native) fn owner_requirement(
        self,
        attestation: &OwnerAttestation,
    ) -> OwnerRequirement {
        match &attestation.process {
            ProcessObservation::Live(Attested::Incarnation) => match &attestation.control {
                #[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
                ControlAttestation::Verified => OwnerRequirement::Met,
                // POSIX records a control identity that can change; the Windows probe
                // compares the process identity the incarnation already matched.
                #[cfg(any(target_os = "macos", target_os = "linux", test))]
                ControlAttestation::Changed(_) => match self {
                    Self::PosixControl => OwnerRequirement::ControlUnverified,
                    Self::WindowsProcess | Self::Unprobed => OwnerRequirement::Met,
                },
                #[cfg(any(target_os = "macos", target_os = "linux", test))]
                ControlAttestation::Unknown(_) => OwnerRequirement::Uninspected,
                ControlAttestation::NotRecorded => self.uncompared(),
            },
            // A Windows record without its process identity is attested PID-only.
            ProcessObservation::Live(Attested::PidOnly) => self.uncompared(),
            ProcessObservation::Gone(_) => OwnerRequirement::Gone,
            ProcessObservation::Unknown(_) => OwnerRequirement::Uninspected,
        }
    }

    /// A live owner with no identity to compare: a PID-only record, or a recorded birth
    /// without TTY and process groups.
    fn uncompared(self) -> OwnerRequirement {
        match self {
            Self::PosixControl | Self::WindowsProcess => OwnerRequirement::ControlUnverified,
            Self::Unprobed => OwnerRequirement::Met,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    let live = live_native_process_identity(pid)?;
    let terminal_tty = current_terminal_tty()?;
    let terminal_tty_device = terminal_tty_device(Path::new(&terminal_tty))?;
    if live.terminal_tty_device != terminal_tty_device {
        bail!("native-session process is not attached to its reported terminal TTY")
    }
    let live_shell = live_native_process_identity(live.parent_pid)?;
    let terminal_shell = TerminalShellIdentity {
        pid: live_shell.pid,
        process_group: live_shell.process_group,
        terminal_tty_device: live_shell.terminal_tty_device,
        process_start_seconds: live_shell.birth.macos().map_or(0, |b| b.0),
        process_start_microseconds: live_shell.birth.macos().map_or(0, |b| b.1),
        linux_birth: live_shell.birth.linux(),
    };
    let owner = NativeSessionOwner {
        pid,
        linux_birth: live.birth.linux(),
        managed_session_id: Some(session_id.to_owned()),
        terminal_tty: Some(terminal_tty),
        terminal_tty_device: Some(terminal_tty_device),
        process_start_seconds: live
            .birth
            .linux()
            .is_none()
            .then_some(live.birth.macos().map_or(0, |b| b.0)),
        process_start_microseconds: live
            .birth
            .linux()
            .is_none()
            .then_some(live.birth.macos().map_or(0, |b| b.1)),
        process_group: Some(live.process_group),
        terminal_process_group: Some(live.terminal_process_group),
        terminal_shell: Some(terminal_shell),
        terminal_app: None,
        windows_process_identity: None,
    };
    verified_terminal_owner_process_group(&owner, &live)?;
    // The surface record, and with it the kind, is read only after the owner is built
    // (`current_surface_owner`), so this check names no terminal application.
    verified_terminal_shell_process_group("terminal", &owner, &live, &live_shell)?;
    Ok(owner)
}

#[cfg(windows)]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    Ok(NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        windows_process_identity: Some(terminal::windows_process_identity(pid)?),
        ..NativeSessionOwner::default()
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    Ok(NativeSessionOwner {
        pid: std::process::id(),
        managed_session_id: Some(session_id.to_owned()),
        ..NativeSessionOwner::default()
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(in crate::native) fn native_owner_identity_matches(
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
) -> bool {
    if owner.pid != live.pid {
        return false;
    }
    match owner.control_record() {
        ControlRecord::NotRecorded => true,
        ControlRecord::Complete(recorded) => {
            recorded.terminal_tty_device == live.terminal_tty_device
                && live.birth
                    == ProcessBirth::Macos(
                        recorded.process_start_seconds,
                        recorded.process_start_microseconds,
                    )
                && recorded.process_group == live.process_group
                && recorded.terminal_process_group == live.terminal_process_group
        }
        ControlRecord::Linux {
            terminal_tty_device,
            birth,
            process_group,
            terminal_process_group,
        } => {
            live.birth.linux() == Some(birth)
                && terminal_tty_device == live.terminal_tty_device
                && process_group == live.process_group
                && terminal_process_group == live.terminal_process_group
        }
        ControlRecord::Partial => false,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(in crate::native) fn verified_terminal_owner_process_group(
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live.pid {
        bail!("native-session process group no longer belongs to the recorded owner")
    }
    match (owner.process_group, owner.terminal_process_group) {
        (Some(process_group), Some(terminal_process_group)) => {
            if process_group != live.process_group
                || terminal_process_group != live.terminal_process_group
            {
                bail!("native-session process group identity changed")
            }
        }
        (None, None) => {}
        _ => bail!("native-session owner has an incomplete process group identity"),
    }
    let process_group = live.process_group;
    let terminal_process_group = live.terminal_process_group;
    if process_group != owner.pid || terminal_process_group != process_group {
        bail!("native-session owner does not lead the terminal foreground process group")
    }
    Ok(process_group)
}

/// Checks that `live_shell`, the owner's parent, holds the owner's foreground job on the
/// owner's TTY and is the shell the owner recorded. `terminal` names the terminal
/// application in the refusal, or is the word "terminal" where the kind is not known.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(in crate::native) fn verified_terminal_shell_process_group(
    terminal: &str,
    owner: &NativeSessionOwner,
    live_owner: &NativeProcessIdentity,
    live_shell: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live_owner.pid || live_owner.parent_pid != live_shell.pid {
        bail!("{terminal} shell no longer owns the native-session process")
    }
    if live_shell.terminal_tty_device != live_owner.terminal_tty_device {
        bail!("{terminal} shell is attached to a different TTY")
    }
    if live_shell.process_group != live_shell.pid
        || live_shell.terminal_process_group != live_owner.process_group
        || live_shell.process_group == live_owner.process_group
    {
        bail!("{terminal} shell does not own the expected foreground job")
    }
    if let Some(recorded) = &owner.terminal_shell
        && (recorded.pid != live_shell.pid
            || recorded.process_group != live_shell.process_group
            || recorded.terminal_tty_device != live_shell.terminal_tty_device
            || recorded.linux_birth != live_shell.birth.linux()
            || recorded.process_start_seconds != live_shell.birth.macos().map_or(0, |b| b.0)
            || recorded.process_start_microseconds != live_shell.birth.macos().map_or(0, |b| b.1))
    {
        bail!("{terminal} shell identity changed")
    }
    Ok(live_shell.process_group)
}

#[cfg(windows)]
pub(in crate::native) fn verify_terminal_surface_ownership(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    verify_terminal_surface_ownership_with_timeout(directory, expected_session_id, session, None)
}

pub(in crate::native) fn verify_terminal_surface_ownership_with_timeout(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    verified_terminal_surface_owner(directory, expected_session_id, session, timeout).map(|_| ())
}

/// The same check, returning the owner record whose live incarnation it verified; `None`
/// on a platform without a managed terminal, where no owner is verified.
pub(in crate::native) fn verified_terminal_surface_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    timeout: Option<Duration>,
) -> Result<Option<NativeSessionOwner>> {
    session.verify_managed_session(expected_session_id)?;
    #[cfg(windows)]
    let owner = {
        let _ = timeout;
        Some(verified_windows_native_owner(
            directory,
            expected_session_id,
        )?)
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let owner = {
        let surface_tty = terminal::verify_posix_surface(directory, session, timeout)?;
        let (owner, _) = verified_posix_terminal_owner(
            directory,
            expected_session_id,
            session,
            surface_tty.as_deref(),
        )?;
        Some(owner)
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let owner = {
        let _ = (directory, timeout);
        None
    };
    Ok(owner)
}

pub(in crate::native) fn close_dead_owner_surface_until(
    directory: &Path,
    session: &terminal::TerminalSession,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    close_dead_owner_surface_with(
        session,
        |surface| terminal::close_session_until(directory, surface, deadline),
        deadline,
    )
}

pub(in crate::native) fn close_dead_owner_surface_with(
    session: &terminal::TerminalSession,
    mut closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    #[cfg(windows)]
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("dead Windows native owner has a non-Windows terminal handle")
    }
    close_remaining(deadline)?;
    let outcome = closer(session)?;
    close_remaining(deadline)?;
    Ok(outcome)
}

// The close decision and its dispatch live in `close_authority`; the production entry
// keeps its path here for `native.rs`.
pub(in crate::native) use super::close_authority::close_owned_surface;

#[cfg(any(target_os = "macos", target_os = "linux"))]
const OWNER_TEARDOWN_REASON: &str =
    "the surface was proven absent while its owner was live; the owner was ended";

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn owner_group_gone(group: u32) -> Result<bool> {
    owner_group_gone_with(group, signal_process_group)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn owner_group_gone_with(
    group: u32,
    mut signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
) -> Result<bool> {
    let target = process_group_signal_target(group)?;
    match signal(target, 0) {
        Ok(()) => Ok(false),
        Err(error) => match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            Some(libc::EPERM) => Ok(false),
            _ => Err(error).context("could not observe the teardown owner's process group"),
        },
    }
}

// One step stops the attested owner before its surface is ended, for the teardown of an
// owner whose surface is proven absent and for every kind's close with a live owner:
// - the close budget, then the ownership proof (`attest`: the owner record and its live
//   incarnation, compared with the surface TTY where the kind supplies one);
// - the shell check after the proof: Terminal.app and Warp observe the shell again from the
//   system, so a shell that ended, changed incarnation or cannot be inspected since the
//   proof refuses here; the proof's own observation is not reused;
// - what the kind records about the owner before the intent names it (Terminal.app: a
//   missing App incarnation);
// - the close budget, the owner's group, and the intent;
// - the teardown alone attests the owner again after its intent and refuses a changed one;
//   a close with a live owner does not;
// - the caller's stop, which checks the budget itself where it does.
// The caller ends the surface afterwards.

/// How a stop of the attested owner checks the terminal shell after the ownership proof.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) trait ShellCheck {
    /// What the stop learns of the shell.
    type Group;
    fn check(
        self,
        surface: &TerminalSession,
        owner: &NativeSessionOwner,
        live: &NativeProcessIdentity,
    ) -> Result<Self::Group>;
}

/// The ownership proof's own shell check is the last one; the stop learns nothing of the
/// shell.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) struct ProvenShell;

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ShellCheck for ProvenShell {
    type Group = ();
    fn check(
        self,
        _: &TerminalSession,
        _: &NativeSessionOwner,
        _: &NativeProcessIdentity,
    ) -> Result<()> {
        Ok(())
    }
}

/// The shell is observed again by the owner's parent PID and checked as the proof checks it;
/// the stop learns the shell's process group from this observation.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[cfg_attr(target_os = "linux", allow(dead_code))] // No Phase 1 adapter selects a fresh-shell stop.
pub(in crate::native::terminal) struct FreshShell<F>(pub(in crate::native::terminal) F);

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl<F: FnOnce(u32) -> Result<NativeProcessIdentity>> ShellCheck for FreshShell<F> {
    type Group = u32;
    fn check(
        self,
        surface: &TerminalSession,
        owner: &NativeSessionOwner,
        live: &NativeProcessIdentity,
    ) -> Result<u32> {
        let shell = (self.0)(live.parent_pid)?;
        verified_terminal_shell_process_group(surface.kind.display_name(), owner, live, &shell)
    }
}

/// The process groups a stop acts on: the owner's foreground group, and what the shell
/// check learned of the shell.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) struct OwnerGroups<S> {
    pub(in crate::native::terminal) owner: u32,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(in crate::native::terminal) shell: S,
}

/// The intent a stop publishes before it acts.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native::terminal) enum OwnerStopIntent {
    /// A close with a live attested owner: the close intent names the owner.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Close,
    /// The end of a live attested owner whose surface is proven absent: the intent names the
    /// owner's group and the teardown reason, and the owner is attested again after it.
    Teardown,
}

/// What a caller supplies to [`stop_attested_owner`].
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) struct OwnerStop<A, S, H, P> {
    /// The ownership proof. The teardown calls it a second time after its intent.
    pub(in crate::native::terminal) attest: A,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(in crate::native::terminal) shell: S,
    /// Completes the owner record before the intent names it.
    pub(in crate::native::terminal) record_incarnation: H,
    /// What is stopped once the intent is published, and how.
    pub(in crate::native::terminal) stop: P,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn stop_attested_owner<A, S, H, P>(
    store: &Store,
    id: &str,
    surface: &TerminalSession,
    intent: OwnerStopIntent,
    owner_stop: OwnerStop<A, S, H, P>,
    deadline: Instant,
) -> Result<()>
where
    A: FnMut() -> Result<(NativeSessionOwner, NativeProcessIdentity)>,
    S: ShellCheck,
    H: FnOnce(&mut NativeSessionOwner) -> Result<()>,
    P: FnOnce(OwnerGroups<S::Group>) -> Result<()>,
{
    let OwnerStop {
        mut attest,
        shell,
        record_incarnation,
        stop,
    } = owner_stop;
    close_remaining(deadline)?;
    let (mut owner, live) = attest()?;
    let shell = shell.check(surface, &owner, &live)?;
    record_incarnation(&mut owner)?;
    close_remaining(deadline)?;
    let group = verified_terminal_owner_process_group(&owner, &live)?;
    match intent {
        OwnerStopIntent::Close => {
            session::close::record_terminal_close_intent(store, id, surface, &owner)?;
        }
        OwnerStopIntent::Teardown => {
            session::close::record_terminal_teardown_intent(
                store,
                id,
                surface,
                &owner,
                group,
                OWNER_TEARDOWN_REASON,
            )?;
            #[cfg(test)]
            session::close::interruption::at(session::close::interruption::Point::IntentPublished);
            // The second full attestation reads the owner, tty and parent shell again.
            // A replacement record, even if it attests successfully, is not this intent.
            close_remaining(deadline)?;
            let (again, current) = attest()?;
            close_remaining(deadline)?;
            if serde_json::to_value(&owner)? != serde_json::to_value(&again)?
                || !native_owner_identity_matches(&owner, &current)
                || verified_terminal_owner_process_group(&again, &current)? != group
            {
                bail!("the teardown owner changed after intent publication; no signal was sent");
            }
        }
    }
    stop(OwnerGroups {
        owner: group,
        shell,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn teardown_absent_owner_with(
    store: &Store,
    id: &str,
    surface: &TerminalSession,
    attest: impl FnMut() -> Result<(NativeSessionOwner, NativeProcessIdentity)>,
    stop: impl FnOnce(u32) -> Result<()>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    stop_attested_owner(
        store,
        id,
        surface,
        OwnerStopIntent::Teardown,
        OwnerStop {
            attest,
            shell: ProvenShell,
            record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
            stop: |groups: OwnerGroups<()>| {
                close_remaining(deadline)?;
                stop(groups.owner)
            },
        },
        deadline,
    )?;
    close_remaining(deadline)?;
    Ok(terminal::CloseResult {
        outcome: CloseOutcome::Missing,
        reason: Some(OWNER_TEARDOWN_REASON.to_owned()),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn process_group_signal_target(
    process_group: u32,
) -> Result<libc::pid_t> {
    let process_group =
        libc::pid_t::try_from(process_group).context("terminal process group is out of range")?;
    if process_group <= 0 {
        bail!("terminal process group must be positive")
    }
    process_group
        .checked_neg()
        .context("terminal process group cannot be represented as a signal target")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn terminate_owned_foreground_group_until(
    group: u32,
    deadline: Instant,
) -> Result<()> {
    close_remaining(deadline)?;
    terminate_owned_foreground_group_until_with_io(
        group,
        deadline.min(Instant::now() + Duration::from_secs(3)),
        signal_process_group,
        |_| {},
    )
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(in crate::native) fn terminate_owned_foreground_group(group: u32) -> Result<()> {
    terminate_owned_foreground_group_with(group, Duration::from_secs(3), |_| {})
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn terminate_owned_foreground_group_with(
    group: u32,
    timeout: Duration,
    observe: impl FnMut(Option<i32>),
) -> Result<()> {
    terminate_owned_foreground_group_with_io(group, timeout, signal_process_group, observe)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
// POSIX owner-group stop retains the numeric PGID check/use race on both platforms.
// pidfds are exclusively for an individually attested host, not a group-exit proof.
pub(in crate::native::terminal) fn signal_process_group(
    target: libc::pid_t,
    signal: libc::c_int,
) -> std::io::Result<()> {
    if unsafe { libc::kill(target, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn terminate_owned_foreground_group_with_io(
    group: u32,
    timeout: Duration,
    signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
    observe: impl FnMut(Option<i32>),
) -> Result<()> {
    terminate_owned_foreground_group_until_with_io(group, Instant::now() + timeout, signal, observe)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn terminate_owned_foreground_group_until_with_io(
    group: u32,
    deadline: Instant,
    mut signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
    mut observe: impl FnMut(Option<i32>),
) -> Result<()> {
    close_remaining(deadline)?;
    let target = process_group_signal_target(group)?;
    if let Err(error) = signal(target, libc::SIGTERM) {
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error).context("could not send SIGTERM to the attested process group");
    }
    #[cfg(test)]
    session::close::interruption::at(session::close::interruption::Point::SignalSent);
    loop {
        if let Err(error) = signal(target, 0) {
            observe(error.raw_os_error());
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // EPERM does not establish absence, including for a zombie-only group
            // (measured on macOS 26.6.2, 2026-10-07, issue #85); only ESRCH does.
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(error)
                    .context("could not observe the attested process group after SIGTERM");
            }
        } else {
            observe(None);
        }
        if crate::native::session::close::budget::close_now() >= deadline {
            bail!("the attested process group was not confirmed gone before the deadline");
        }
        std::thread::sleep(Duration::from_millis(20).min(
            deadline.saturating_duration_since(crate::native::session::close::budget::close_now()),
        ));
    }
}

pub(in crate::native) fn current_surface_owner(
    directory: &Path,
    id: &str,
) -> Result<NativeSessionOwner> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = directory;
    let owner = current_native_session_owner(id)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let owner = {
        let mut owner = owner;
        let surface: terminal::TerminalSession = Reader::open_unchecked(directory).terminal()?;
        surface.verify_managed_session(id)?;
        owner.terminal_app =
            terminal::close_evidence(surface.kind).derive_app_incarnation(&owner)?;
        owner
    };
    Ok(owner)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod process_close_tests {
    use super::*;
    use std::{os::unix::process::CommandExt, process::Command};

    #[test]
    fn r3_replay_group_probe_does_not_accept_zombie_only_group() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        assert!(!owner_group_gone(group).unwrap());
        assert_eq!(unsafe { libc::kill(-(group as i32), libc::SIGTERM) }, 0);
        child.wait_for_zombie();
        assert!(!owner_group_gone(group).unwrap());
        drop(child);
        assert!(owner_group_gone(group).unwrap());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    struct ForegroundGroupChild(std::process::Child);

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl ForegroundGroupChild {
        fn spawn() -> Self {
            Self(
                Command::new("/bin/sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            )
        }

        fn wait_for_zombie(&self) {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let probe = match unsafe { libc::kill(-(self.0.id() as i32), 0) } {
                    0 => None,
                    _ => std::io::Error::last_os_error().raw_os_error(),
                };
                if zombie_probe(self.0.id(), probe) {
                    return;
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // What a group probe reports while the group holds only an unreaped zombie. Neither
    // answer establishes absence; only ESRCH does.
    // - macOS: EPERM (measured on macOS 26.6.2, 2026-10-07, issue #85).
    // - Linux: success, so the zombie is read from procfs (measured on Linux 7.0,
    //   2026-11-17).
    #[cfg(target_os = "macos")]
    fn zombie_probe(_pid: u32, probe: Option<i32>) -> bool {
        probe == Some(libc::EPERM)
    }
    #[cfg(target_os = "linux")]
    fn zombie_probe(pid: u32, probe: Option<i32>) -> bool {
        probe.is_none()
            && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_whitespace().next())
                    == Some("Z")
            })
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl Drop for ForegroundGroupChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn owned_foreground_group_termination_waits_for_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        let (reap, ready) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let received = ready.recv_timeout(Duration::from_secs(5));
            drop(child);
            received
        });
        let mut probes = Vec::new();
        let mut zombie_probes = 0;
        let result =
            terminate_owned_foreground_group_with(group, Duration::from_secs(3), |errno| {
                let zombie = zombie_probe(group, errno);
                probes.push((errno, zombie));
                if zombie {
                    zombie_probes += 1;
                    if zombie_probes == 2 {
                        reap.send(()).unwrap();
                    }
                }
            });
        drop(reap);
        let reaped = waiter.join();
        assert!(result.is_ok(), "{result:?}");
        reaped.unwrap().unwrap();
        let gone = probes
            .iter()
            .position(|(errno, _)| *errno == Some(libc::ESRCH))
            .unwrap();
        assert!(
            probes[..gone].iter().filter(|(_, zombie)| *zombie).count() >= 2,
            "{probes:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn owned_foreground_group_termination_times_out_before_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        let timeout = Duration::from_secs(1);
        let mut zombie_probes = 0;
        let started = Instant::now();
        let result = terminate_owned_foreground_group_with(group, timeout, |errno| {
            if zombie_probe(group, errno) {
                zombie_probes += 1;
            }
        });
        let elapsed = started.elapsed();
        assert_eq!(
            result.as_ref().unwrap_err().to_string(),
            "the attested process group was not confirmed gone before the deadline",
            "{result:?}"
        );
        assert!(zombie_probes > 0, "no zombie-only probe was observed");
        assert!(elapsed >= timeout);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn owned_foreground_group_termination_refuses_failed_signal_to_zombie() {
        let child = ForegroundGroupChild::spawn();
        assert_eq!(
            unsafe { libc::kill(-(child.0.id() as i32), libc::SIGTERM) },
            0
        );
        child.wait_for_zombie();
        let error = terminate_owned_foreground_group(child.0.id()).unwrap_err();
        // macOS refuses the signal itself; Linux delivers it to the zombie and then never
        // observes the group gone. Both refuse.
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                error.to_string(),
                "could not send SIGTERM to the attested process group"
            );
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EPERM)
            );
        }
        #[cfg(target_os = "linux")]
        assert_eq!(
            error.to_string(),
            "the attested process group was not confirmed gone before the deadline"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn owned_foreground_group_termination_waits_for_real_private_process_exit() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id();
        let waiter = std::thread::spawn(move || child.wait().unwrap());
        let result = terminate_owned_foreground_group(group);
        let status = waiter.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "macos"))]
#[path = "ownership_teardown_tests.rs"]
mod teardown_tests;

#[cfg(all(test, target_os = "macos"))]
#[path = "ownership_close_interruption_tests.rs"]
mod close_interruption_tests;

#[cfg(all(test, target_os = "macos"))]
pub(in crate::native::terminal) fn test_owner_attestation(live: bool) -> OwnerAttestation {
    OwnerAttestation {
        process: if live {
            ProcessObservation::Live(crate::native::process_identity::Attested::Incarnation)
        } else {
            ProcessObservation::Gone(Gone::Exited)
        },
        control: crate::native::session::owner::ControlAttestation::NotRecorded,
        pid_alive: Some(live),
    }
}
