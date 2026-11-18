//! Terminal.app window and TTY identities are scoped to one app incarnation.
use super::incarnation::{
    record_terminal_app_for_close, terminal_app_ended, terminal_app_instances,
    terminal_app_process, terminal_surface_absent,
};
use super::verified_close_tty;
use crate::native::session::Store;
use crate::native::session::close::budget::close_remaining;
use crate::native::terminal;
use crate::native::terminal::close_evidence::{CloseEvidence, ResumableAuthority};
use crate::native::terminal::macos::applescript::Automation;
use crate::native::terminal::macos::ownership::{
    ProcessReads, SYSTEM_READS, verified_macos_terminal_owner_with,
};
use crate::native::terminal::macos::process::macos_process_start;
use crate::native::terminal::ownership::{
    self, FreshShell, MacTerminalAppIdentity, NativeSessionOwner, OwnerGroups, OwnerStop,
    OwnerStopIntent, process_group_signal_target,
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
        terminal_app_ended(app, &self.process_birth, &self.terminal_instances)
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
        if terminal_app_ended(app, &self.process_birth, &self.terminal_instances)? {
            return Ok(ResumableAuthority::Absent);
        }
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

    // A Terminal.app window whose program ended is not a separate surface to remove.
    fn removes_ended_surface(&self) -> bool {
        false
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
    terminate_owner_with(
        automation,
        directory,
        expected_session_id,
        session,
        SYSTEM_READS,
        |owner| record_terminal_app_for_close(directory, owner),
        ownership::signal_process_group,
        deadline,
    )
}

// Terminal.app's part of the owner stop: the surface TTY its proof compares, the shell
// observed again, the App incarnation recorded before the intent, and the signals.
#[allow(clippy::too_many_arguments)]
fn terminate_owner_with(
    automation: &dyn Automation,
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    reads: ProcessReads<'_>,
    record_app: impl FnOnce(&mut NativeSessionOwner) -> Result<()>,
    signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
    deadline: std::time::Instant,
) -> Result<()> {
    ownership::stop_attested_owner(
        &Store::open_unchecked(directory),
        expected_session_id,
        session,
        OwnerStopIntent::Close,
        OwnerStop {
            attest: || {
                let surface_tty = verified_close_tty(automation, session, deadline)?;
                verified_macos_terminal_owner_with(
                    directory,
                    expected_session_id,
                    session,
                    Some(&surface_tty),
                    reads,
                )
            },
            shell: FreshShell(reads.identity),
            record_incarnation: record_app,
            stop: |groups: OwnerGroups<u32>| {
                terminate_process_groups(groups.owner, groups.shell, deadline, signal)
            },
        },
        deadline,
    )
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
    mut send: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
) -> Result<()> {
    for (target, signal) in close_signal_plan(managed_process_group, shell_process_group)? {
        close_remaining(deadline)?;
        let Err(error) = send(target, signal) else {
            continue;
        };
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
    use crate::native::session::test_support::TERMINAL_CLOSE_INTENT_FILE;
    use crate::native::terminal::macos::automation_testing::{Method, Replay, Reply};
    use crate::native::terminal::ownership::NativeProcessIdentity;
    use crate::native::tests::write_attested_apple_terminal_state;

    const LIVE: NativeProcessIdentity = NativeProcessIdentity {
        birth: crate::native::terminal::ownership::ProcessBirth::Macos(1_790_000_000, 42),
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
    };
    const SHELL: NativeProcessIdentity = NativeProcessIdentity {
        birth: crate::native::terminal::ownership::ProcessBirth::Macos(1, 0),
        pid: 4000,
        parent_pid: 3000,
        terminal_tty_device: 7,
        process_group: 4000,
        terminal_process_group: 4242,
    };

    // A Terminal.app session whose owner record attests against `LIVE` and `SHELL`.
    fn owner_stop_fixture() -> (tempfile::TempDir, terminal::TerminalSession) {
        let directory = tempfile::tempdir().unwrap();
        write_attested_apple_terminal_state(directory.path(), "ready", 4242);
        let surface = crate::native::session::Reader::open_unchecked(directory.path())
            .terminal()
            .unwrap();
        (directory, surface)
    }

    fn tty_device(path: &Path) -> Result<u64> {
        assert_eq!(path, Path::new("/dev/ttys999"));
        Ok(7)
    }

    fn intent_recorded(directory: &Path) -> bool {
        directory.join(TERMINAL_CLOSE_INTENT_FILE).exists()
    }

    // Terminal.app verifies its tab, attests the owner against the tab's TTY, reads the shell
    // again, records the App incarnation, publishes the intent, then sends SIGTERM to the
    // owner's group and SIGKILL to the shell's group.
    #[test]
    fn terminal_app_owner_stop_order() {
        let (directory, surface) = owner_stop_fixture();
        let events = std::cell::RefCell::new(Vec::<String>::new());
        let automation = Replay::new([Reply::Normal(Ok(surface.id.clone()))]);
        let identity = |pid: u32| {
            events.borrow_mut().push(format!("read {pid}"));
            match pid {
                4242 => Ok(LIVE),
                4000 => Ok(SHELL),
                _ => panic!("unexpected process {pid}"),
            }
        };
        terminate_owner_with(
            &automation,
            directory.path(),
            "session-owner123",
            &surface,
            ProcessReads {
                identity: &identity,
                tty_device: &tty_device,
            },
            |_| {
                events.borrow_mut().push(format!(
                    "incarnation, intent {}",
                    intent_recorded(directory.path())
                ));
                Ok(())
            },
            |target, signal| {
                events.borrow_mut().push(format!(
                    "signal {signal} to {target}, intent {}",
                    intent_recorded(directory.path())
                ));
                Ok(())
            },
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap();
        let calls = automation.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, Method::RunUntil);
        automation.assert_finished();
        assert_eq!(
            *events.borrow(),
            [
                "read 4242".to_owned(),
                "read 4000".to_owned(),
                "read 4000".to_owned(),
                "incarnation, intent false".to_owned(),
                format!("signal {} to -4242, intent true", libc::SIGTERM),
                format!("signal {} to -4000, intent true", libc::SIGKILL),
            ]
        );
    }

    // A shell that ended after the proof refuses before the incarnation record, the intent
    // and the signals.
    #[test]
    fn terminal_app_owner_stop_refuses_a_shell_that_ended_after_the_proof() {
        let (directory, surface) = owner_stop_fixture();
        let automation = Replay::new([Reply::Normal(Ok(surface.id.clone()))]);
        let shell_reads = std::cell::Cell::new(0);
        let identity = |pid: u32| match pid {
            4242 => Ok(LIVE),
            4000 => {
                shell_reads.set(shell_reads.get() + 1);
                if shell_reads.get() == 1 {
                    Ok(SHELL)
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::ESRCH))
                        .context("failed to inspect native-session process 4000")
                }
            }
            _ => panic!("unexpected process {pid}"),
        };
        let error = terminate_owner_with(
            &automation,
            directory.path(),
            "session-owner123",
            &surface,
            ProcessReads {
                identity: &identity,
                tty_device: &tty_device,
            },
            |_| panic!("no incarnation is recorded"),
            |_, _| panic!("no signal is sent"),
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "failed to inspect native-session process 4000"
        );
        assert_eq!(shell_reads.get(), 2);
        assert!(!intent_recorded(directory.path()));
    }

    // The budget is checked before the stop, before the tab verification, before the intent
    // and before each signal. An expiry at each check leaves exactly the effects before it.
    #[test]
    fn terminal_app_owner_stop_deadline_expiry_at_each_check() {
        for unexpired_reads in 0..=5 {
            let (directory, surface) = owner_stop_fixture();
            let start = std::time::Instant::now();
            let deadline = start + std::time::Duration::from_secs(60);
            let clock = crate::native::clock::ExpiresAtRead::new(start, deadline, unexpired_reads);
            let reads = clock.reads();
            let _scope = crate::native::session::close::budget::Scope::with_clock(clock);
            let automation = Replay::new(if unexpired_reads >= 2 {
                vec![Reply::Normal(Ok(surface.id.clone()))]
            } else {
                vec![]
            });
            let identity = |pid: u32| match pid {
                4242 => Ok(LIVE),
                4000 => Ok(SHELL),
                _ => panic!("unexpected process {pid}"),
            };
            let incarnations = std::cell::Cell::new(0);
            let signals = std::cell::RefCell::new(Vec::new());
            let result = terminate_owner_with(
                &automation,
                directory.path(),
                "session-owner123",
                &surface,
                ProcessReads {
                    identity: &identity,
                    tty_device: &tty_device,
                },
                |_| {
                    incarnations.set(incarnations.get() + 1);
                    Ok(())
                },
                |_, signal| {
                    signals.borrow_mut().push(signal);
                    Ok(())
                },
                deadline,
            );
            // (expired, automation calls, incarnation records, intent, signals)
            let expected = match unexpired_reads {
                0 | 1 => (true, 0, 0, false, vec![]),
                2 => (true, 1, 1, false, vec![]),
                3 => (true, 1, 1, true, vec![]),
                4 => (true, 1, 1, true, vec![libc::SIGTERM]),
                _ => (false, 1, 1, true, vec![libc::SIGTERM, libc::SIGKILL]),
            };
            let expired = match &result {
                Ok(()) => false,
                Err(error) => {
                    assert!(
                        error.to_string().contains("close deadline expired"),
                        "{error:#}"
                    );
                    true
                }
            };
            assert_eq!(
                (
                    expired,
                    automation.calls().len(),
                    incarnations.get(),
                    intent_recorded(directory.path()),
                    signals.borrow().clone()
                ),
                expected,
                "{unexpired_reads}"
            );
            automation.assert_finished();
            assert_eq!(
                reads.get(),
                (unexpired_reads + 1).min(5),
                "{unexpired_reads}"
            );
        }
    }

    // A group that is already gone does not stop the plan; any other signal failure stops it
    // with the group named.
    #[test]
    fn terminal_app_signals_continue_past_a_gone_group_and_name_a_failed_one() {
        let sent = std::cell::RefCell::new(Vec::new());
        terminate_process_groups(
            4242,
            4000,
            crate::native::session::close::budget::close_deadline(),
            |target, signal| {
                sent.borrow_mut().push((target, signal));
                Err(std::io::Error::from_raw_os_error(libc::ESRCH))
            },
        )
        .unwrap();
        assert_eq!(
            *sent.borrow(),
            [(-4242, libc::SIGTERM), (-4000, libc::SIGKILL)]
        );
        let error = terminate_process_groups(
            4242,
            4000,
            crate::native::session::close::budget::close_deadline(),
            |_, _| Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "failed to send signal {} to Terminal.app process group 4242",
                libc::SIGTERM
            )
        );
    }

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
