use super::support::*;
use super::*;
use crate::native::session::close as session_close;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::{
    CLOSED_STATUS_FILE, SESSION_OWNER_FILE, TERMINAL_CLOSE_INTENT_FILE, TERMINAL_CLOSING_FILE,
    TERMINAL_HANDLE_FILE, TERMINAL_TOMBSTONE_FILE, TURN_CLAIM_FILE, TURN_COMPLETION_FILE,
};
use crate::native::session::turn::{
    recover_pending_completion, require_running_wait_owner, wait_for_status,
};
use crate::native::session::{SessionState, SessionStatus, launch};
use crate::native::terminal::close_authority::{
    TerminalCloseAuthority, test_support::TestObservation,
};
use crate::native::terminal::ownership::NativeSessionOwner;
use crate::native::{session, terminal};
use anyhow::{Result, bail};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
fn assert_closed_without_terminal_records(directory: &Path) {
    for record in [
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
        TERMINAL_CLOSE_INTENT_FILE,
    ] {
        assert!(!directory.join(record).exists(), "{record} was kept");
    }
    assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

// A close interrupted after its intent but before its teardown left the owner alive: the
// intent grants nothing, and the original live-owner rules apply.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_while_the_incarnation_is_live() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", std::process::id());
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    session_close::record_terminal_close_intent(
        &Store::open_unchecked(directory.path()),
        "session-owner123",
        &handle,
        &owner,
    )
    .unwrap();
    assert!(
        session_close::terminal_close_intent_owner(
            &Reader::open_unchecked(directory.path()),
            &handle,
            terminal::surface_outlives_owner(&handle)
        )
        .unwrap()
        .is_some()
    );
    assert!(
        !session::close::terminal_close_resumable_with(
            &Reader::open_unchecked(directory.path()),
            &handle,
            true,
            |_| session::owner::OwnerAttestation {
                process: crate::native::process_identity::ProcessObservation::Live(
                    crate::native::process_identity::Attested::Incarnation
                ),
                control: session::owner::ControlAttestation::Verified,
                pid_alive: Some(true),
            },
        )
        .unwrap()
    );
}

#[cfg(target_os = "macos")]
fn assert_internal_wait_reports_dead_owner(status_wait: bool) {
    let dead_pid = reaped_child_pid();
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Ghostty,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Warp,
    ] {
        for retained in [
            TERMINAL_HANDLE_FILE,
            TERMINAL_CLOSING_FILE,
            TERMINAL_CLOSE_INTENT_FILE,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let owner = write_attested_apple_terminal_state(directory.path(), "working", dead_pid);
            let handle_path = directory.path().join(TERMINAL_HANDLE_FILE);
            let mut handle: terminal::TerminalSession = read_json(&handle_path).unwrap();
            handle.kind = kind;
            write_json_atomic(&handle_path, &handle).unwrap();
            if retained == TERMINAL_CLOSING_FILE {
                fs::rename(&handle_path, directory.path().join(retained)).unwrap();
            } else if retained == TERMINAL_CLOSE_INTENT_FILE {
                session_close::record_terminal_close_intent(
                    &Store::open_unchecked(directory.path()),
                    "session-owner123",
                    &handle,
                    &owner,
                )
                .unwrap();
            }
            let records = ["status.json", SESSION_OWNER_FILE, TURN_CLAIM_FILE, retained];
            let before: Vec<_> = records
                .iter()
                .map(|name| fs::read(directory.path().join(name)).unwrap())
                .collect();
            // An expired deadline makes the priority deterministic, without timing a live app.
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                turn::wait(
                    &Store::open_unchecked(directory.path()),
                    0,
                    None,
                    None,
                    agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO)
                        .unwrap(),
                    Duration::ZERO,
                )
                .unwrap_err()
            };
            for (name, bytes) in records.iter().zip(before) {
                assert_eq!(
                    fs::read(directory.path().join(name)).unwrap(),
                    bytes,
                    "{kind:?}/{retained}/{name}"
                );
            }
            assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            assert!(
                format!("{error:#}").contains("no longer running"),
                "{kind:?}/{retained}: {error:#}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(false);
}

#[cfg(target_os = "macos")]
#[test]
fn internal_status_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(true);
    // Also cover a retained, stale ready status in this internal helper. Current
    // macOS launches pass input as a provider argument and do not call this wait.
    let directory = tempfile::tempdir().unwrap();
    write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let error = wait_for_status(
        directory.path(),
        SessionState::Ready,
        Instant::now(),
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("no longer running"),
        "{error:#}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn internal_wait_does_not_call_live_or_unverified_owner_dead() {
    for mode in ["live", "missing"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), "working", std::process::id());
        if mode == "missing" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        // PID-only compatibility and a missing owner remain non-terminal observations.
        // Incarnation reuse and unknown inspection are covered with injected observations.
        require_running_wait_owner(directory.path()).unwrap();
        for status_wait in [false, true] {
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                turn::wait(
                    &Store::open_unchecked(directory.path()),
                    0,
                    None,
                    None,
                    agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO)
                        .unwrap(),
                    Duration::ZERO,
                )
                .unwrap_err()
            };
            assert!(
                !format!("{error:#}").contains("no longer running"),
                "{mode}: {error:#}"
            );
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_recovers_completion_before_reporting_dead_owner() {
    for published in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write_attested_apple_terminal_state(directory.path(), "working", reaped_child_pid());
        let handle = fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        let token = fs::read_to_string(directory.path().join(TURN_CLAIM_FILE)).unwrap();
        let pending = sample_completion(token.trim(), "completed before owner exit");
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if published {
            recover_pending_completion(directory.path()).unwrap();
        }
        let event = turn::wait(
            &Store::open_unchecked(directory.path()),
            0,
            Some("provider-turn"),
            Some(token.trim()),
            agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO).unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(event.message, "completed before owner exit");
        assert_eq!(
            fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap(),
            handle
        );
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
    }
}

#[cfg(target_os = "macos")]
fn assert_dead_owner_preserves_unverified_surface(kind: terminal::TerminalKind) {
    for state in ["ready", "exited", "failed"] {
        for claimed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_attested_apple_terminal_state(directory.path(), state, reaped_child_pid());
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.kind = kind;
            write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
            if claimed {
                fs::rename(
                    directory.path().join(TERMINAL_HANDLE_FILE),
                    directory.path().join(TERMINAL_CLOSING_FILE),
                )
                .unwrap();
            }
            assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
            let result = {
                let closer =
                    &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                        {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let apple =
                                terminal::macos::apple_terminal::close_evidence::with_observations(
                                    |_| {
                                        let app = owner.terminal_app.as_ref().unwrap();
                                        Ok(Some((app.start_seconds, app.start_microseconds)))
                                    },
                                    || Ok(vec![owner.terminal_app.clone().unwrap()]),
                                );
                            TestObservation::on_records(
                                directory.path(),
                                "session-owner123",
                                session,
                                deadline,
                            )
                            .with_presence(|| Ok(true))
                            .with_evidence(
                                if session.kind == terminal::TerminalKind::AppleTerminal {
                                    &apple
                                } else {
                                    terminal::close_evidence(session.kind)
                                },
                            )
                            .decide(
                                "session-owner123",
                                session,
                                deadline,
                            )
                        }?;
                        panic!("a dead owner without intent must grant no adapter authority")
                    };
                session_close::close(
                    &Store::open_unchecked(directory.path()),
                    None,
                    session_close::budget::close_deadline(),
                    |surface| closer(surface).map(Into::into),
                )
            }
            .map(|_| ());
            assert!(
                result.is_err(),
                "{kind:?}/{state}/{claimed}: no adapter proof, but close reported success"
            );
            assert!(
                directory.path().join(TERMINAL_HANDLE_FILE).exists(),
                "{kind:?}: last surface evidence consumed"
            );
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_ne!(status.state.as_str(), "closed");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::AppleTerminal);
}

#[cfg(target_os = "macos")]
#[test]
fn warp_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Warp);
}

#[cfg(target_os = "macos")]
#[test]
fn wezterm_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::WezTerm);
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Ghostty);
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_exact_absence_can_finish_without_adapter_or_signal() {
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Ghostty,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = kind;
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        let mut observations = 0;
        {
            let closer =
                &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                    let authority = {
                        let deadline = crate::native::session::close::budget::close_deadline();
                        let apple =
                            terminal::macos::apple_terminal::close_evidence::with_observations(
                                |_| {
                                    let app = owner.terminal_app.as_ref().unwrap();
                                    Ok(Some((app.start_seconds, app.start_microseconds)))
                                },
                                || Ok(vec![owner.terminal_app.clone().unwrap()]),
                            );
                        TestObservation::on_records(
                            directory.path(),
                            "session-owner123",
                            session,
                            deadline,
                        )
                        .with_presence(|| {
                            observations += 1;
                            Ok(false)
                        })
                        .with_evidence(if session.kind == terminal::TerminalKind::AppleTerminal {
                            &apple
                        } else {
                            terminal::close_evidence(session.kind)
                        })
                        .decide("session-owner123", session, deadline)
                    }?;
                    assert_eq!(authority, TerminalCloseAuthority::Absent);
                    Ok(terminal::CloseOutcome::Missing)
                };
            session_close::close(
                &Store::open_unchecked(directory.path()),
                None,
                session_close::budget::close_deadline(),
                |surface| closer(surface).map(Into::into),
            )
        }
        .map(|_| ())
        .unwrap();
        assert_eq!(observations, 1);
        assert_closed_without_terminal_records(directory.path());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_absence_errors_and_reused_or_incomplete_owners_retain_handle() {
    for mode in ["read-error", "present", "reused", "incomplete", "foreign"] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        if mode == "reused" {
            owner.pid = std::process::id();
        }
        if mode == "incomplete" {
            owner.process_start_seconds = None;
        }
        if mode == "foreign" {
            owner.managed_session_id = Some("foreign".to_owned());
        }
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let mut observed = 0;
        let result = {
            let closer =
                &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                    {
                        let deadline = crate::native::session::close::budget::close_deadline();
                        let apple =
                            terminal::macos::apple_terminal::close_evidence::with_observations(
                                |_| {
                                    let app = owner.terminal_app.as_ref().unwrap();
                                    Ok(Some((app.start_seconds, app.start_microseconds)))
                                },
                                || Ok(vec![owner.terminal_app.clone().unwrap()]),
                            );
                        TestObservation::on_records(
                            directory.path(),
                            "session-owner123",
                            session,
                            deadline,
                        )
                        .with_presence(|| {
                            observed += 1;
                            match mode {
                                "read-error" => bail!("injected unreadable target"),
                                "present" => Ok(true),
                                _ => panic!("unproven owner must not inspect a surface"),
                            }
                        })
                        .with_evidence(if session.kind == terminal::TerminalKind::AppleTerminal {
                            &apple
                        } else {
                            terminal::close_evidence(session.kind)
                        })
                        .decide("session-owner123", session, deadline)
                    }?;
                    panic!("unproven absence must never close or signal")
                };
            session_close::close(
                &Store::open_unchecked(directory.path()),
                None,
                session_close::budget::close_deadline(),
                |surface| closer(surface).map(Into::into),
            )
        }
        .map(|_| ());
        assert!(result.is_err(), "{mode}");
        assert_eq!(
            observed,
            usize::from(matches!(mode, "read-error" | "present")),
            "{mode}"
        );
        assert!(
            directory.path().join(TERMINAL_HANDLE_FILE).exists(),
            "{mode}"
        );
        assert!(
            !directory.path().join(TERMINAL_TOMBSTONE_FILE).exists(),
            "{mode}"
        );
    }
}

// A live process of this test with a controlling TTY of its own (a private pty): what a
// recorded owner PID is once another process has taken it.
#[cfg(target_os = "macos")]
fn spawn_on_private_pty() -> (std::process::Child, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (command.spawn().unwrap(), master)
}

// A launch that failed after its wrapper recorded itself, as `ask`, the wrapper and its
// finalization leave the records: the claim, the launch receipt, the bound handle, the
// complete owner and the failed status. `Pending`: the wrapper failed before the spawn
// (the provider version check). `Spawning`: the spawn is uncertain and keeps the claim.
#[cfg(target_os = "macos")]
fn write_failed_launch_with_owner(
    directory: &Path,
    kind: terminal::TerminalKind,
    phase: launch::Phase,
    owner_pid: u32,
) -> NativeSessionOwner {
    let owner = write_attested_apple_terminal_state(directory, "launching", owner_pid);
    let token = turn::current_claim_token(&Reader::open_unchecked(directory))
        .unwrap()
        .unwrap();
    launch::begin(
        &Store::open_unchecked(directory),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    if phase == launch::Phase::Spawning {
        let mut record = launch::read(&Reader::open_unchecked(directory))
            .unwrap()
            .unwrap();
        record.phase = phase;
        write_json_atomic(&directory.join(launch::FILE), &record).unwrap();
    }
    launch::finalize(
        &Store::open_unchecked(directory),
        &Err(anyhow::anyhow!("provider version check failed")),
    )
    .unwrap();
    let mut handle: terminal::TerminalSession =
        read_json(&directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = kind;
    write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    owner
}

// A failed launch is no close intent. A surface that can outlive its owner follows the
// dead-owner rules whatever the launch phase: the whole owner identity, then the read-only
// proof of absence, and never a close or a signal.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_with_a_recorded_owner_gets_no_close_without_a_prior_intent() {
    let (mut other_process, _pty) = spawn_on_private_pty();
    let mut failures = Vec::new();
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in [
                "absent",
                "present",
                "read-error",
                "incomplete",
                "foreign",
                "reused",
            ] {
                let case = format!("{kind:?}/{phase:?}/{mode}");
                let directory = tempfile::tempdir().unwrap();
                let owner_pid = if mode == "reused" {
                    other_process.id()
                } else {
                    reaped_child_pid()
                };
                let mut owner =
                    write_failed_launch_with_owner(directory.path(), kind, phase, owner_pid);
                if mode == "incomplete" {
                    owner.process_start_seconds = None;
                }
                if mode == "foreign" {
                    owner.managed_session_id = Some("session-foreign".to_owned());
                }
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                let app = owner.terminal_app.clone().unwrap();
                let claim =
                    turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap();
                assert_eq!(claim.is_some(), phase == launch::Phase::Spawning, "{case}");
                assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

                let (mut observations, mut adapter_calls) = (0, 0);
                let result = { let closer = &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
                                |pid| {
                                    assert_eq!(pid, app.pid);
                                    Ok(Some((app.start_seconds, app.start_microseconds)))
                                },
                                || Ok(vec![app.clone()]),
                            );
                            let authority = TestObservation::on_records(
                                directory.path(),
                                "session-owner123",
                                session,
                                deadline,
                            )
                            .with_presence(|| {
                                observations += 1;
                                match mode {
                                    "absent" => Ok(false),
                                    "present" => Ok(true),
                                    _ => bail!("injected unreadable target"),
                                }
                            })
                            .with_evidence(if session.kind == terminal::TerminalKind::AppleTerminal {
                                &apple
                            } else {
                                terminal::close_evidence(session.kind)
                            })
                            .decide("session-owner123", session, deadline)?;
                            if authority == TerminalCloseAuthority::Absent {
                                return Ok(terminal::CloseOutcome::Missing);
                            }
                            // Where `close_session_surface` signals the owner or calls the adapter.
                            adapter_calls += 1;
                            Ok(terminal::CloseOutcome::Closed)
                        }; session_close::close(&Store::open_unchecked(directory.path()), None, session_close::budget::close_deadline(), |surface| closer(surface).map(Into::into)) }
                .map(|_| ());

                if adapter_calls != 0 {
                    failures.push(format!("{case}: {adapter_calls} close/signal step reached"));
                }
                let wanted_observations =
                    usize::from(matches!(mode, "absent" | "present" | "read-error"));
                if observations != wanted_observations {
                    failures.push(format!(
                        "{case}: {observations} surface observations, wanted {wanted_observations}"
                    ));
                }
                let status: SessionStatus =
                    read_json(&directory.path().join("status.json")).unwrap();
                let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
                let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
                if mode == "absent" {
                    if result.is_err()
                        || handle_kept
                        || !tombstone
                        || status.state.as_str() != "closed"
                    {
                        failures.push(format!(
                            "{case}: proven absence did not consume the handle: {result:?}"
                        ));
                    }
                } else if result.is_ok()
                    || !handle_kept
                    || tombstone
                    || status.state.as_str() != "failed"
                    || turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap()
                        != claim
                    || launch::read(&Reader::open_unchecked(directory.path()))
                        .unwrap()
                        .unwrap()
                        .phase
                        != phase
                {
                    failures.push(format!(
                        "{case}: handle, claim or state was not retained (result {result:?}, state {})",
                        status.state
                    ));
                }
            }
        }
    }
    other_process.kill().unwrap();
    other_process.wait().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[cfg(target_os = "macos")]
#[test]
fn ownerless_warp_and_wezterm_failures_only_consume_proven_absence() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in ["absent", "present", "unreadable", "foreign"] {
                let directory = tempfile::tempdir().unwrap();
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
                let claim =
                    turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap();
                let (mut observations, mut mutations) = (0, 0);
                let result = { let closer = &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let id = if mode == "foreign" {
                                "session-foreign"
                            } else {
                                "session-owner123"
                            };
                            let authority = TestObservation::on_records(
                                directory.path(),
                                id,
                                session,
                                deadline,
                            )
                            .with_presence(|| {
                                observations += 1;
                                match mode {
                                    "absent" => Ok(false),
                                    "present" => Ok(true),
                                    _ => bail!("injected unreadable surface"),
                                }
                            })
                            .with_evidence(terminal::close_evidence(session.kind))
                            .decide(id, session, deadline)?;
                            if authority == TerminalCloseAuthority::Absent {
                                return Ok(terminal::CloseOutcome::Missing);
                            }
                            mutations += 1;
                            Ok(terminal::CloseOutcome::Closed)
                        }; session_close::close(&Store::open_unchecked(directory.path()), None, session_close::budget::close_deadline(), |surface| closer(surface).map(Into::into)) }
                .map(|_| ());
                assert_eq!(
                    mutations, 0,
                    "{kind:?}/{phase:?}/{mode}: mutation without owner or intent"
                );
                assert_eq!(observations, usize::from(mode != "foreign"));
                if mode == "absent" {
                    result.unwrap();
                    assert_closed_without_terminal_records(directory.path());
                } else {
                    assert!(result.is_err(), "{kind:?}/{phase:?}/{mode}");
                    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
                    assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
                    assert_eq!(
                        turn::current_claim_token(&Reader::open_unchecked(directory.path()))
                            .unwrap(),
                        claim
                    );
                    assert_eq!(
                        launch::read(&Reader::open_unchecked(directory.path()))
                            .unwrap()
                            .unwrap()
                            .phase,
                        phase
                    );
                }
            }
        }
    }
}

// The close that recorded its intent while the owner was live still finishes after a
// failed launch, and a surface with an app-unique native id keeps its startup recovery.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_keeps_the_prior_intent_retry_and_the_stable_id_recovery() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Iterm2,
        terminal::TerminalKind::Ghostty,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
            let handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            if terminal::surface_outlives_owner(&handle) {
                session_close::record_terminal_close_intent(
                    &Store::open_unchecked(directory.path()),
                    "session-owner123",
                    &handle,
                    &owner,
                )
                .unwrap();
            }
            let app = owner.terminal_app.clone().unwrap();
            let mut adapter_calls = 0;
            {
                let closer =
                    &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                        let authority = {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let apple =
                                terminal::macos::apple_terminal::close_evidence::with_observations(
                                    |_| Ok(Some((app.start_seconds, app.start_microseconds))),
                                    || Ok(vec![app.clone()]),
                                );
                            TestObservation::on_records(
                                directory.path(),
                                "session-owner123",
                                session,
                                deadline,
                            )
                            .with_presence(|| {
                                panic!("{kind:?}/{phase:?}: no surface observation is needed")
                            })
                            .with_evidence(
                                if session.kind == terminal::TerminalKind::AppleTerminal {
                                    &apple
                                } else {
                                    terminal::close_evidence(session.kind)
                                },
                            )
                            .decide(
                                "session-owner123",
                                session,
                                deadline,
                            )
                        }?;
                        assert_eq!(
                            authority,
                            TerminalCloseAuthority::SurfaceOnly,
                            "{kind:?}/{phase:?}"
                        );
                        adapter_calls += 1;
                        Ok(terminal::CloseOutcome::Closed)
                    };
                session_close::close(
                    &Store::open_unchecked(directory.path()),
                    None,
                    session_close::budget::close_deadline(),
                    |surface| closer(surface).map(Into::into),
                )
            }
            .map(|_| ())
            .unwrap();
            assert_eq!(adapter_calls, 1, "{kind:?}/{phase:?}");
            assert_closed_without_terminal_records(directory.path());
        }
    }
}
