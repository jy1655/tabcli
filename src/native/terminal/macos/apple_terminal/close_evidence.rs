//! Terminal.app window and TTY identities are scoped to one app incarnation.
use super::{
    record_legacy_terminal_app, require_unique_terminal_app, terminal_app_alive_with,
    terminal_app_instances, terminal_app_process, terminal_surface_absent, verified_close_tty,
};
use crate::native::session::close::budget::close_remaining;
use crate::native::session::{self, Store};
use crate::native::terminal;
use crate::native::terminal::close_evidence::{CloseEvidence, ResumableAuthority};
use crate::native::terminal::macos::applescript::Automation;
use crate::native::terminal::macos::ownership::verified_macos_terminal_owner;
use crate::native::terminal::macos::process::{live_native_process_identity, macos_process_start};
use crate::native::terminal::ownership::{
    MacTerminalAppIdentity, NativeSessionOwner, process_group_signal_target,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
use anyhow::{Context, Result, bail};
use std::path::Path;

type ProcessBirth = fn(u32) -> Result<Option<(u64, u64)>>;
type TerminalInstances = fn() -> Result<Vec<MacTerminalAppIdentity>>;

pub(in crate::native) struct Evidence<B, I> {
    process_birth: B,
    terminal_instances: I,
}

pub(in crate::native) static EVIDENCE: Evidence<ProcessBirth, TerminalInstances> = Evidence {
    process_birth: macos_process_start,
    terminal_instances: terminal_app_instances,
};

#[cfg(test)]
pub(in crate::native) fn with_observations<B, I>(
    process_birth: B,
    terminal_instances: I,
) -> Evidence<B, I>
where
    B: Fn(u32) -> Result<Option<(u64, u64)>>,
    I: Fn() -> Result<Vec<MacTerminalAppIdentity>>,
{
    Evidence {
        process_birth,
        terminal_instances,
    }
}

impl<B, I> CloseEvidence for Evidence<B, I>
where
    B: Fn(u32) -> Result<Option<(u64, u64)>>,
    I: Fn() -> Result<Vec<MacTerminalAppIdentity>>,
{
    fn surface_absent(
        &self,
        owner: Option<&NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<bool> {
        terminal_surface_absent(
            owner.and_then(|owner| owner.terminal_app.as_ref()),
            &self.process_birth,
            &self.terminal_instances,
            surface_present,
        )
    }

    fn incarnation_ended(&self, owner: &NativeSessionOwner) -> Result<bool> {
        let Some(app) = &owner.terminal_app else {
            return Ok(false);
        };
        let alive = terminal_app_alive_with(app, &self.process_birth)?;
        if alive {
            require_unique_terminal_app(app, &(self.terminal_instances)()?)?;
        }
        Ok(!alive)
    }

    fn resumable_authority(
        &self,
        read_owner: &dyn Fn() -> Result<NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<ResumableAuthority> {
        let owner = read_owner()?;
        let Some(app) = &owner.terminal_app else {
            // An intent of 0.0.10 or earlier names no app incarnation, and
            // its owner is gone: nothing can tie the window to an app now.
            if self.surface_absent(None, surface_present)? {
                return Ok(ResumableAuthority::Absent);
            }
            bail!(
                "Terminal.app process incarnation was not recorded and its window is not proven gone; no close was sent and the exact surface handle is retained"
            );
        };
        if !terminal_app_alive_with(app, &self.process_birth)? {
            return Ok(ResumableAuthority::Absent);
        }
        require_unique_terminal_app(app, &(self.terminal_instances)()?)?;
        Ok(ResumableAuthority::SurfaceOnly)
    }

    fn startup_retained_reason(&self) -> &'static str {
        "Terminal.app startup has no native owner/app incarnation and its window is not proven gone; no close was sent and the surface handle is retained"
    }

    fn derive_app_incarnation(
        &self,
        owner: &NativeSessionOwner,
    ) -> Result<Option<MacTerminalAppIdentity>> {
        terminal_app_process(owner).map(Some)
    }
}

// `automation` is the closing adapter's: the surface verification here is the second
// of the close and runs through the same dependency as the first.
pub(in crate::native::terminal) fn terminate_owner(
    automation: &dyn Automation,
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    deadline: std::time::Instant,
) -> Result<()> {
    close_remaining(deadline)?;
    let surface_tty = verified_close_tty(automation, session, deadline)?;
    let (mut owner, live) =
        verified_macos_terminal_owner(directory, expected_session_id, session, Some(&surface_tty))?;
    let process_group = verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = live_native_process_identity(live.parent_pid)?;
    let shell_process_group = verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    record_legacy_terminal_app(directory, &mut owner, terminal_app_process)?;
    require_unique_terminal_app(
        owner
            .terminal_app
            .as_ref()
            .context("Terminal.app identity was not recorded")?,
        &terminal_app_instances()?,
    )?;
    close_remaining(deadline)?;
    session::close::record_terminal_close_intent(
        &Store::open_unchecked(directory),
        expected_session_id,
        session,
        &owner,
    )?;
    terminate_process_groups(process_group, shell_process_group, deadline)
}

fn close_signal_plan(
    managed_process_group: u32,
    shell_process_group: u32,
) -> Result<[(libc::pid_t, libc::c_int); 2]> {
    if managed_process_group == shell_process_group {
        bail!("Terminal.app managed and shell process groups must be distinct")
    }
    Ok([
        (
            process_group_signal_target(managed_process_group)?,
            libc::SIGTERM,
        ),
        (
            process_group_signal_target(shell_process_group)?,
            libc::SIGKILL,
        ),
    ])
}

fn terminate_process_groups(
    managed_process_group: u32,
    shell_process_group: u32,
    deadline: std::time::Instant,
) -> Result<()> {
    for (target, signal) in close_signal_plan(managed_process_group, shell_process_group)? {
        close_remaining(deadline)?;
        let result = unsafe { libc::kill(target, signal) };
        if result == 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            continue;
        }
        return Err(error).with_context(|| {
            format!(
                "failed to send signal {signal} to Terminal.app process group {}",
                target.checked_neg().unwrap_or_default()
            )
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_app_close_signal_targets_and_plan() {
        assert_eq!(process_group_signal_target(4242).unwrap(), -4242);
        assert!(process_group_signal_target(0).is_err());
        assert!(process_group_signal_target(i32::MAX as u32 + 1).is_err());
        assert_eq!(
            close_signal_plan(4242, 4000).unwrap(),
            [(-4242, libc::SIGTERM), (-4000, libc::SIGKILL)]
        );
    }

    use std::cell::RefCell;

    #[test]
    fn resumable_reads_owner_once_before_incarnation_observations() {
        let trace = RefCell::new(Vec::new());
        let app = MacTerminalAppIdentity {
            pid: 456,
            start_seconds: 100,
            start_microseconds: 42,
        };
        let evidence = with_observations(
            |pid| {
                trace.borrow_mut().push("birth");
                assert_eq!(pid, app.pid);
                Ok(Some((app.start_seconds, app.start_microseconds)))
            },
            || {
                trace.borrow_mut().push("instances");
                Ok(vec![app.clone()])
            },
        );
        assert!(matches!(
            evidence
                .resumable_authority(
                    &|| {
                        trace.borrow_mut().push("owner");
                        Ok(NativeSessionOwner {
                            terminal_app: Some(app.clone()),
                            ..NativeSessionOwner::default()
                        })
                    },
                    Box::new(|| panic!("recorded live incarnation needs no presence query")),
                )
                .unwrap(),
            ResumableAuthority::SurfaceOnly
        ));
        assert_eq!(*trace.borrow(), ["owner", "birth", "instances"]);
    }

    #[test]
    fn resumable_owner_read_error_precedes_all_observations() {
        let evidence = with_observations(
            |_| panic!("owner read failed before birth observation"),
            || panic!("owner read failed before instance observation"),
        );
        let error = evidence
            .resumable_authority(
                &|| Err(anyhow::anyhow!("owner read cause")).context("owner read context"),
                Box::new(|| panic!("owner read failed before presence observation")),
            )
            .err()
            .expect("owner read must fail");
        assert_eq!(
            error.chain().map(ToString::to_string).collect::<Vec<_>>(),
            ["owner read context", "owner read cause"]
        );
    }
}
