use super::*;
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use crate::native::session::close::{prune_blocked_by, repair_owner_is_live};

#[cfg(target_os = "macos")]
fn fixture() -> (
    NativeSessionOwner,
    crate::native::terminal::ownership::NativeProcessIdentity,
) {
    let live = crate::native::terminal::ownership::NativeProcessIdentity {
        pid: 4242,
        parent_pid: 42,
        terminal_tty_device: 9,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 10,
        process_start_microseconds: 20,
    };
    (
        NativeSessionOwner {
            pid: live.pid,
            terminal_tty_device: Some(9),
            process_group: Some(4242),
            terminal_process_group: Some(4242),
            process_start_seconds: Some(10),
            process_start_microseconds: Some(20),
            ..Default::default()
        },
        live,
    )
}

#[cfg(target_os = "macos")]
#[test]
fn r1_c3_control_changes_do_not_make_the_incarnation_dead() {
    let (owner, live) = fixture();
    for field in 0..3 {
        let mut changed = live;
        match field {
            0 => changed.terminal_tty_device += 1,
            1 => changed.process_group += 1,
            _ => changed.terminal_process_group += 1,
        }
        let a = attest_owner_with(
            &owner,
            OwnerObservers {
                pid_alive: &|_| true,
                birth: &|_| Ok(Some((10, 20))),
                control: &|_| Ok(Some(changed)),
            },
        );
        assert!(matches!(
            a.process,
            ProcessObservation::Live(Attested::Incarnation)
        ));
        assert!(matches!(a.control, ControlAttestation::Changed(_)));
        assert!(repair_owner_is_live(&a).unwrap());
        assert_eq!(OwnerObservation::from(&a).identity_matches, Some(false));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn r1_c4_partial_owner_compares_only_recorded_control_fields() {
    let (mut owner, live) = fixture();
    owner.process_group = None;
    owner.terminal_process_group = None;
    let a = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| Ok(Some(live)),
        },
    );
    assert!(repair_owner_is_live(&a).unwrap());
    assert!(matches!(a.control, ControlAttestation::Verified));
    let dead = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| false,
            birth: &|_| Ok(None),
            control: &|_| panic!("dead owner has no control observation"),
        },
    );
    assert!(!repair_owner_is_live(&dead).unwrap());
    let unknown = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| false,
            birth: &|_| anyhow::bail!("denied"),
            control: &|_| panic!("unknown birth"),
        },
    );
    assert!(repair_owner_is_live(&unknown).is_err());
    assert_eq!(OwnerObservation::from(&unknown).process_alive, Some(false));
}

#[test]
fn r1_c2_unknown_blocks_repair_and_prune() {
    let a = OwnerAttestation {
        process: ProcessObservation::Unknown("denied".into()),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(false),
    };
    assert!(repair_owner_is_live(&a).is_err());
    assert!(prune_blocked_by(&a));
}

#[test]
fn r1_c6_reused_projects_mismatch_without_inspection_error() {
    let a = OwnerAttestation {
        process: ProcessObservation::Gone(Gone::Reused("reused".into())),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(true),
    };
    assert_eq!(
        serde_json::to_value(OwnerObservation::from(&a)).unwrap(),
        serde_json::json!({"process_alive":true,"identity_matches":false,"error":null})
    );
}

#[cfg(windows)]
#[test]
fn r1_c1_windows_legacy_dead_does_not_block_prune() {
    let a = attest_owner_with(
        &NativeSessionOwner::default(),
        OwnerObservers {
            pid_alive: &|_| false,
            identity: &|_| panic!("legacy must not inspect identity"),
        },
    );
    assert!(!prune_blocked_by(&a));
}

#[cfg(windows)]
#[test]
fn r1_c5_windows_attestation_ignores_path_case_but_record_equality_does_not() {
    let root = tempfile::tempdir().unwrap();
    let recorded = crate::native::terminal::WindowsProcessIdentity {
        creation_time: 123,
        executable_path: root.path().join("Provider.EXE").display().to_string(),
    };
    let live = crate::native::terminal::WindowsProcessIdentity {
        executable_path: recorded.executable_path.to_ascii_lowercase(),
        ..recorded.clone()
    };
    assert_ne!(recorded, live);
    let owner = NativeSessionOwner {
        pid: 4242,
        windows_process_identity: Some(recorded),
        ..Default::default()
    };
    let a = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            identity: &|_| Ok(Some(live.clone())),
        },
    );
    assert!(prune_blocked_by(&a));
    assert!(matches!(
        a.process,
        ProcessObservation::Live(Attested::Incarnation)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn r1_c7_wait_reused_birth_ends_but_live_and_unknown_continue() {
    let root = tempfile::tempdir().unwrap();
    let (owner, _) = fixture();
    crate::native::session::Store::open_unchecked(root.path())
        .write_owner(&owner)
        .unwrap();
    for (birth, ends) in [
        (Ok(Some((11, 20))), true),
        (Ok(Some((10, 20))), false),
        (Ok(None), true),
        (Err(anyhow::anyhow!("denied")), false),
    ] {
        let a = attest_owner_with(
            &owner,
            OwnerObservers {
                pid_alive: &|_| true,
                birth: &|_| {
                    birth
                        .as_ref()
                        .map(|v| *v)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                },
                control: &|_| anyhow::bail!("no TTY"),
            },
        );
        let result = crate::native::session::turn::running_wait_owner_with(root.path(), |_| a);
        assert_eq!(result.is_err(), ends);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn r1_c8_close_resume_requires_exact_intent_and_gone_incarnation() {
    use crate::native::session::{Store, close};
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(root.path());
    let (mut owner, _) = fixture();
    owner.managed_session_id = Some("session-test".into());
    store.write_owner(&owner).unwrap();
    let handle: crate::native::terminal::TerminalSession =
        serde_json::from_value(serde_json::json!({
            "terminal":"iterm2", "session_id":"surface", "managed_session_id":"session-test"
        }))
        .unwrap();
    let reused = |owner: &NativeSessionOwner| {
        attest_owner_with(
            owner,
            OwnerObservers {
                pid_alive: &|_| true,
                birth: &|_| Ok(Some((11, 20))),
                control: &|_| panic!("reuse needs no TTY"),
            },
        )
    };
    assert!(!close::terminal_close_resumable_with(&store, &handle, true, reused).unwrap());
    close::record_terminal_close_intent(&store, "session-test", &handle, &owner).unwrap();
    assert!(close::terminal_close_resumable_with(&store, &handle, true, reused).unwrap());
    for (process, resumable) in [
        (ProcessObservation::Live(Attested::Incarnation), false),
        (ProcessObservation::Live(Attested::PidOnly), false),
        (ProcessObservation::Gone(Gone::Exited), true),
        (ProcessObservation::Unknown("denied".into()), false),
    ] {
        assert_eq!(
            close::terminal_close_resumable_with(&store, &handle, true, |_| OwnerAttestation {
                process,
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(true),
            })
            .unwrap(),
            resumable
        );
    }

    assert!(!close::terminal_close_resumable_with(&store, &handle, false, reused).unwrap());
    let mut other = handle.clone();
    other.id = "other".into();
    assert!(!close::terminal_close_resumable_with(&store, &other, true, reused).unwrap());
    let mut foreign = owner.clone();
    foreign.pid += 1;
    store.write_owner(&foreign).unwrap();
    assert!(!close::terminal_close_resumable_with(&store, &handle, true, reused).unwrap());
    store.write_owner(&owner).unwrap();
    close::record_terminal_teardown_intent(&store, "session-test", &handle, &owner, 4242, "test")
        .unwrap();
    assert!(!close::terminal_close_resumable_with(&store, &handle, true, reused).unwrap());
}

#[test]
fn projection_rows_keep_pid_evidence_and_control_separate() {
    use ControlAttestation::*;
    let cases = [
        (
            ProcessObservation::Live(Attested::Incarnation),
            Verified,
            Some(true),
            serde_json::json!([true, true, null]),
        ),
        (
            ProcessObservation::Live(Attested::Incarnation),
            Changed("TTY".into()),
            Some(true),
            serde_json::json!([true, false, null]),
        ),
        (
            ProcessObservation::Live(Attested::Incarnation),
            Unknown("no TTY".into()),
            Some(false),
            serde_json::json!([false, null, "no TTY"]),
        ),
        (
            ProcessObservation::Live(Attested::Incarnation),
            NotRecorded,
            Some(true),
            serde_json::json!([true, null, null]),
        ),
        (
            ProcessObservation::Live(Attested::PidOnly),
            NotRecorded,
            Some(true),
            serde_json::json!([true, null, null]),
        ),
        (
            ProcessObservation::Gone(Gone::Exited),
            NotRecorded,
            Some(false),
            serde_json::json!([false, null, null]),
        ),
        (
            ProcessObservation::Gone(Gone::Reused("birth".into())),
            NotRecorded,
            Some(true),
            serde_json::json!([true, false, null]),
        ),
        (
            ProcessObservation::Unknown("denied".into()),
            NotRecorded,
            None,
            serde_json::json!([null, null, "denied"]),
        ),
    ];
    for (process, control, pid_alive, expected) in cases {
        let a = OwnerAttestation {
            process,
            control,
            pid_alive,
        };
        let o = OwnerObservation::from(&a);
        assert_eq!(
            serde_json::json!([o.process_alive, o.identity_matches, o.error]),
            expected
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn legacy_and_birth_only_observations_skip_control_and_read_pid_once() {
    use std::cell::Cell;
    for recorded in [false, true] {
        for alive in [false, true] {
            let (mut owner, _) = fixture();
            owner.terminal_tty_device = None;
            owner.process_group = None;
            owner.terminal_process_group = None;
            if !recorded {
                owner.process_start_seconds = None;
                owner.process_start_microseconds = None;
            }
            let count = Cell::new(0);
            let a = attest_owner_with(
                &owner,
                OwnerObservers {
                    pid_alive: &|_| {
                        count.set(count.get() + 1);
                        alive
                    },
                    birth: &|_| {
                        assert!(recorded);
                        Ok(alive.then_some((10, 20)))
                    },
                    control: &|_| panic!("no recorded control"),
                },
            );
            assert_eq!(count.get(), 1);
            assert_eq!(repair_owner_is_live(&a).unwrap(), alive);
        }
    }
}

pub(in crate::native) fn reused_observation() -> OwnerObservation {
    OwnerObservation::from(&OwnerAttestation {
        process: ProcessObservation::Gone(Gone::Reused("different birth".into())),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(true),
    })
}

#[cfg(windows)]
#[test]
fn windows_identity_matrix_preserves_absence_error_and_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let identity = crate::native::terminal::WindowsProcessIdentity {
        creation_time: 1,
        executable_path: dir.path().join("provider.exe").display().to_string(),
    };
    let owner = NativeSessionOwner {
        pid: 4242,
        windows_process_identity: Some(identity.clone()),
        ..Default::default()
    };
    let changed = crate::native::terminal::WindowsProcessIdentity {
        creation_time: 2,
        ..identity.clone()
    };
    for (value, expected) in [
        (Ok(Some(identity)), "live"),
        (Ok(Some(changed)), "reuse"),
        (Ok(None), "exited"),
        (Err(anyhow::anyhow!("denied")), "unknown"),
    ] {
        let a = attest_owner_with(
            &owner,
            OwnerObservers {
                pid_alive: &|_| false,
                identity: &|_| value.as_ref().cloned().map_err(|e| anyhow::anyhow!("{e}")),
            },
        );
        match expected {
            "live" => assert!(matches!(
                a.process,
                ProcessObservation::Live(Attested::Incarnation)
            )),
            "reuse" => assert!(matches!(
                a.process,
                ProcessObservation::Gone(Gone::Reused(_))
            )),
            "exited" => assert!(matches!(a.process, ProcessObservation::Gone(Gone::Exited))),
            _ => {
                assert!(repair_owner_is_live(&a).is_err());
                assert_eq!(OwnerObservation::from(&a).process_alive, Some(false));
            }
        }
    }
    let legacy = NativeSessionOwner::default();
    assert!(matches!(
        attest_owner_with(
            &legacy,
            OwnerObservers {
                pid_alive: &|_| true,
                identity: &|_| panic!("legacy")
            }
        )
        .process,
        ProcessObservation::Live(Attested::PidOnly)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn full_incarnation_control_match_and_failed_control_preserve_process_evidence() {
    let (owner, live) = fixture();
    for (control, expected) in [
        (Ok(Some(live)), "verified"),
        (Ok(None), "exited"),
        (Err(anyhow::anyhow!("no TTY")), "unknown"),
    ] {
        let a = attest_owner_with(
            &owner,
            OwnerObservers {
                pid_alive: &|_| true,
                birth: &|_| Ok(Some((10, 20))),
                control: &|_| {
                    control
                        .as_ref()
                        .copied()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                },
            },
        );
        assert!(repair_owner_is_live(&a).unwrap());
        let observation = OwnerObservation::from(&a);
        if expected == "verified" {
            assert_eq!(observation.identity_matches, Some(true));
        } else {
            assert!(observation.error.is_some());
            assert_eq!(observation.identity_matches, None);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn independent_review_control_snapshot_must_belong_to_attested_incarnation() {
    let (owner, mut next_incarnation) = fixture();
    next_incarnation.process_start_seconds += 1;
    // PID and recorded control fields are unchanged, but the second OS snapshot
    // now positively identifies a different incarnation.
    assert!(
        !crate::native::terminal::ownership::native_owner_identity_matches(
            &owner,
            &next_incarnation
        )
    );
    let observed = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| Ok(Some(next_incarnation)),
        },
    );
    assert!(matches!(
        observed.process,
        ProcessObservation::Gone(Gone::Reused(_))
    ));
    assert_ne!(
        OwnerObservation::from(&observed).identity_matches,
        Some(true),
        "a later control snapshot of a reused PID must not verify the recorded owner"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn independent_review_control_birth_reuse_must_end_running_wait() {
    let root = tempfile::tempdir().unwrap();
    let (owner, mut next_incarnation) = fixture();
    next_incarnation.process_start_seconds += 1;
    next_incarnation.process_group += 1;
    crate::native::session::Store::open_unchecked(root.path())
        .write_owner(&owner)
        .unwrap();
    let observed = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| Ok(Some(next_incarnation)),
        },
    );
    assert!(
        crate::native::session::turn::running_wait_owner_with(root.path(), |_| observed).is_err(),
        "the later observed birth proves reuse even when the control fields also change"
    );
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn correction_a1_recorded_identity_absence_allows_prune_and_repair() {
    let root = tempfile::tempdir().unwrap();
    let owner = NativeSessionOwner {
        pid: 4242,
        process_start_seconds: Some(10),
        process_start_microseconds: Some(20),
        windows_process_identity: Some(crate::native::terminal::WindowsProcessIdentity {
            creation_time: 123,
            executable_path: root.path().join("provider.exe").display().to_string(),
        }),
        ..Default::default()
    };
    let identity = |_| Ok(None);
    let a = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            #[cfg(target_os = "macos")]
            birth: &identity,
            #[cfg(target_os = "macos")]
            control: &|_| panic!("absent incarnation has no control snapshot"),
            #[cfg(windows)]
            identity: &identity,
        },
    );
    assert!(!prune_blocked_by(&a));
    assert!(!repair_owner_is_live(&a).unwrap());
    assert_eq!(OwnerObservation::from(&a).process_alive, Some(false));
}

#[cfg(target_os = "macos")]
#[test]
fn correction_a2_wrong_control_pid_is_unknown_not_verified() {
    let (owner, mut live) = fixture();
    live.pid += 1;
    let a = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| Ok(Some(live)),
        },
    );
    assert!(matches!(a.process, ProcessObservation::Unknown(_)));
    assert_ne!(OwnerObservation::from(&a).identity_matches, Some(true));
}

#[cfg(target_os = "macos")]
#[test]
fn correction_a4_control_error_keeps_wrapper_and_underlying_context() {
    let (owner, _) = fixture();
    let a = attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| Err(anyhow::anyhow!("access denied").context("control lookup")),
        },
    );
    let expected = "failed to verify the birth identity of live native-session process 4242: control lookup: access denied";
    assert!(matches!(&a.control, ControlAttestation::Unknown(reason) if reason == expected));
    assert_eq!(OwnerObservation::from(&a).error.as_deref(), Some(expected));
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn intermediate_observation(control: &str) -> OwnerObservation {
    let (mut owner, mut live) = fixture();
    owner.process_group = None;
    owner.terminal_process_group = None;
    if control == "changed" {
        live.terminal_tty_device += 1;
    }
    OwnerObservation::from(&attest_owner_with(
        &owner,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some((10, 20))),
            control: &|_| {
                if control == "unknown" {
                    anyhow::bail!("control unavailable")
                } else {
                    Ok(Some(live))
                }
            },
        },
    ))
}
