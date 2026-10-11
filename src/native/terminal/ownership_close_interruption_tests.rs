// These tests interrupt production writes/actions, then re-enter the same dispatch.
// No intent, consumption marker, tombstone, or handle removal is manufactured.
use super::teardown_tests::{fixture, kinds};
use super::*;
use session::close::interruption::{self, Point};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Group {
    Present,
    PermissionDenied,
    Gone,
}

struct Processes {
    owner_live: Cell<bool>,
    group: Cell<Group>,
    after_signal: Group,
    signals: Cell<usize>,
    probes: Cell<usize>,
    attestations: Cell<usize>,
}

impl Processes {
    fn new(after_signal: Group) -> Self {
        Self {
            owner_live: Cell::new(true),
            group: Cell::new(Group::Present),
            after_signal,
            signals: Cell::new(0),
            probes: Cell::new(0),
            attestations: Cell::new(0),
        }
    }

    fn signal(&self, target: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
        assert_eq!(target, -4242, "only the owner's group may be addressed");
        match signal {
            libc::SIGTERM => {
                assert!(self.owner_live.get(), "no signal without a live owner");
                self.signals.set(self.signals.get() + 1);
                self.owner_live.set(false);
                self.group.set(self.after_signal);
                Ok(())
            }
            0 => {
                self.probes.set(self.probes.get() + 1);
                match self.group.get() {
                    Group::Present => Ok(()),
                    Group::PermissionDenied => Err(std::io::Error::from_raw_os_error(libc::EPERM)),
                    Group::Gone => Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
                }
            }
            other => panic!("unexpected signal: {other}"),
        }
    }

    fn attest(
        &self,
        store: &Store,
        live: NativeProcessIdentity,
    ) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
        self.attestations.set(self.attestations.get() + 1);
        if !self.owner_live.get() {
            bail!("owner is dead")
        }
        Ok((store.owner()?, live))
    }

    fn close(&self, store: &Store, live: NativeProcessIdentity) -> Result<CloseOutcome> {
        session::close::compatibility::close_with_owner_observation(
            store,
            Some("pre-close diagnostic".into()),
            |_| Ok(self.owner_live.get()),
            |surface| {
                close_owned_surface_with(
                    || {
                        verify_terminal_close_authority_with_observers(
                            store.directory(),
                            "session-owner123",
                            surface,
                            || Ok(false),
                            |_| Ok(None),
                            || Ok(vec![]),
                            CloseObservers {
                                owner_record: &|_| panic!("not a failed startup"),
                                pid_alive: &|_| self.owner_live.get(),
                                mac_owner_live: &|_| Ok(self.owner_live.get()),
                                resumable: &|| {
                                    session::close::terminal_close_resumable(
                                        store,
                                        surface,
                                        terminal::surface_outlives_owner(surface),
                                    )
                                },
                                ownership_proof: &|| bail!("surface proof failed"),
                                attest_owner: &|| self.attest(store, live).map(|_| ()),
                                group_gone: &|group| {
                                    owner_group_gone_with(group, |target, signal| {
                                        self.signal(target, signal)
                                    })
                                },
                            },
                        )
                    },
                    || {
                        teardown_absent_owner_with(
                            store,
                            "session-owner123",
                            surface,
                            || self.attest(store, live),
                            |group| {
                                terminate_owned_foreground_group_with_io(
                                    group,
                                    Duration::ZERO,
                                    |target, signal| self.signal(target, signal),
                                    |_| {},
                                )
                            },
                        )
                    },
                    |_| panic!("absence/teardown must not call a surface adapter"),
                )
            },
        )
    }
}

fn assert_reason(reason: &str) {
    assert!(reason.contains("existing status diagnostic"), "{reason}");
    assert!(reason.contains("pre-close diagnostic"), "{reason}");
    assert!(reason.contains(OWNER_TEARDOWN_REASON), "{reason}");
}

fn interrupted_close_replays(point: Point, after_signal: Group) {
    for kind in kinds() {
        let (dir, _, surface, live) = fixture(kind);
        let store = Store::open_unchecked(dir.path());
        let mut status = store.status().unwrap();
        status.error = Some("existing status diagnostic".into());
        store
            .record(CoreRecord::Status)
            .write_json(&status)
            .unwrap();
        let processes = Processes::new(after_signal);

        let interrupted = interruption::during(point, || processes.close(&store, live));
        let panic = interrupted.expect_err("production did not reach the interruption checkpoint");
        assert_eq!(panic.downcast_ref::<Point>(), Some(&point));
        assert!(!store.record(CoreRecord::Terminal).path().exists());

        let handle_retained = matches!(
            point,
            Point::IntentPublished | Point::SignalSent | Point::ConsumptionMarkerWritten
        );
        assert_eq!(
            store.record(CoreRecord::TerminalClosing).path().exists(),
            handle_retained
        );
        let consumed = matches!(
            point,
            Point::ConsumptionMarkerWritten | Point::HandleRemoved | Point::ClosedTombstoneWritten
        );
        assert_eq!(
            store.record(CoreRecord::TerminalClosed).path().exists(),
            consumed
        );
        assert_eq!(
            store.record(CoreRecord::Closed).path().exists(),
            point == Point::ClosedTombstoneWritten
        );
        assert_eq!(
            processes.signals.get(),
            usize::from(point != Point::IntentPublished)
        );

        if point != Point::ClosedTombstoneWritten {
            let intent = session::close::terminal_teardown_intent(&store, &surface)
                .unwrap()
                .unwrap();
            assert_eq!(intent.process_group, 4242);
            assert_eq!(intent.reason, OWNER_TEARDOWN_REASON);
        } else {
            assert!(
                !store
                    .record(CoreRecord::TerminalCloseIntent)
                    .path()
                    .exists()
            );
            let tombstone: SessionStatus = store.record(CoreRecord::Closed).json().unwrap();
            assert_reason(tombstone.error.as_deref().unwrap());
            // The checkpoint is before status.json publication, not after settlement.
            assert_ne!(store.status().unwrap().state, SessionState::Closed);
        }
        if consumed {
            let marker: serde_json::Value =
                store.record(CoreRecord::TerminalClosed).json().unwrap();
            assert_reason(marker["reason"].as_str().unwrap());
        } else {
            assert_eq!(
                store.status().unwrap().error.as_deref(),
                Some("existing status diagnostic")
            );
        }

        let previous_probes = processes.probes.get();
        let previous_attestations = processes.attestations.get();
        if point == Point::SignalSent && after_signal != Group::Gone {
            let reason_before = store
                .record(CoreRecord::TerminalCloseIntent)
                .bytes()
                .unwrap();
            let error = processes.close(&store, live).unwrap_err();
            assert!(error.to_string().contains("not proven gone"), "{error:#}");
            assert_eq!(processes.signals.get(), 1, "dead owner replay signalled");
            assert_eq!(processes.probes.get(), previous_probes + 1);
            assert!(store.record(CoreRecord::Terminal).path().exists());
            assert!(!store.record(CoreRecord::TerminalClosing).path().exists());
            assert_eq!(
                store
                    .record(CoreRecord::TerminalCloseIntent)
                    .bytes()
                    .unwrap(),
                reason_before
            );
            assert!(!store.record(CoreRecord::Closed).path().exists());
            assert_eq!(
                store.status().unwrap().error.as_deref(),
                Some("existing status diagnostic")
            );
            // Only the external observation changes; replay uses untouched production records.
            processes.group.set(Group::Gone);
        }
        let probes_before_success = processes.probes.get();
        assert_eq!(
            processes.close(&store, live).unwrap(),
            CloseOutcome::Missing
        );
        assert_eq!(processes.signals.get(), 1);
        assert_eq!(
            processes.probes.get(),
            probes_before_success + usize::from(!consumed)
        );
        if point == Point::IntentPublished {
            assert!(
                processes.attestations.get() > previous_attestations,
                "retry must attest afresh"
            );
        } else {
            assert_eq!(
                processes.attestations.get(),
                previous_attestations,
                "dead replay must not attest/signal"
            );
        }
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert_reason(store.status().unwrap().error.as_deref().unwrap());
        let tombstone: SessionStatus = store.record(CoreRecord::Closed).json().unwrap();
        assert_reason(tombstone.error.as_deref().unwrap());
        for record in [
            CoreRecord::Terminal,
            CoreRecord::TerminalClosing,
            CoreRecord::TerminalCloseIntent,
        ] {
            assert!(!store.record(record).path().exists());
        }
        let settled = store.record(CoreRecord::Closed).bytes().unwrap();
        processes.close(&store, live).unwrap();
        assert_eq!(store.record(CoreRecord::Closed).bytes().unwrap(), settled);
        assert_eq!(processes.signals.get(), 1);
    }
}

#[test]
fn r3_production_interrupted_after_intent_publication() {
    interrupted_close_replays(Point::IntentPublished, Group::Gone);
}
#[test]
fn r3_production_interrupted_after_signal_sent() {
    for group in [Group::Present, Group::PermissionDenied, Group::Gone] {
        interrupted_close_replays(Point::SignalSent, group);
    }
}
#[test]
fn r3_production_interrupted_after_consumption_marker_written() {
    interrupted_close_replays(Point::ConsumptionMarkerWritten, Group::Gone);
}
#[test]
fn r3_production_interrupted_after_handle_removed() {
    interrupted_close_replays(Point::HandleRemoved, Group::Gone);
}
#[test]
fn r3_production_interrupted_after_closed_tombstone_written() {
    interrupted_close_replays(Point::ClosedTombstoneWritten, Group::Gone);
}
