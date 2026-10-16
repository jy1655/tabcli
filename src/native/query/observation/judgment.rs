//! What an Observation concludes from its Records and, on request, from the OS. Every
//! renderer maps these typed judgments to its own vocabulary and derives none of its own.
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use crate::native::session::OwnerObservation;
use crate::native::session::owner::{ControlAttestation, OwnerAttestation, attest_owner};
use crate::native::terminal::ownership::NativeSessionOwner;
use agent_bridge::process_is_alive;
use anyhow::Result;

pub(in crate::native) const FOREIGN_OWNER: &str = "owner record belongs to another session";

/// The Owner record judged against the session it was read for (CONTEXT.md Attestation).
/// Only a record bound to this session is attested.
#[derive(Debug)]
pub(in crate::native) enum OwnerJudgment {
    /// The record names this session; its incarnation and control identity were attested.
    Bound(OwnerAttestation),
    /// A legacy record without a binding: its PID is observed alive or gone, never attested.
    UnboundLegacy {
        alive: Option<bool>,
    },
    /// The record names another managed session and was not observed at all.
    Foreign,
    Absent,
    Unreadable(String),
}

/// What a Bound Owner's attestation decides. Renderers conclude a death from this,
/// never from the projected bools: an Unknown attestation is never a death (D15).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum BoundOwner {
    /// A live incarnation whose recorded control identity matched.
    #[cfg_attr(not(any(target_os = "macos", windows, test)), allow(dead_code))]
    Verified,
    /// A live incarnation whose control identity changed; not a death.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    ControlChanged,
    /// Alive, with no control identity to compare: a PID-only record, or a recorded
    /// birth without TTY and process groups.
    Uncompared,
    /// The recorded incarnation exited.
    Exited,
    /// The PID now belongs to another incarnation: the recorded one is gone.
    Reused,
    /// The incarnation or its control identity could not be inspected.
    Unknown,
}

/// The OS observations an owner judgment may make. Tests replace them.
pub(in crate::native) struct Observers<'a> {
    pub(in crate::native) attest: &'a dyn Fn(&NativeSessionOwner) -> OwnerAttestation,
    pub(in crate::native) pid_alive: &'a dyn Fn(u32) -> Option<bool>,
}

impl Observers<'static> {
    pub(in crate::native) const SYSTEM: Self = Self {
        attest: &attest_owner,
        pid_alive: &system_pid_alive,
    };
}

fn system_pid_alive(pid: u32) -> Option<bool> {
    Some(process_is_alive(pid))
}

/// Judges the raw Owner read for `session`. Reads no record and takes no lock.
pub(in crate::native) fn owner(
    record: &Result<Option<NativeSessionOwner>>,
    session: &str,
    observers: &Observers<'_>,
) -> OwnerJudgment {
    match record {
        Err(error) => OwnerJudgment::Unreadable(format!("{error:#}")),
        Ok(None) => OwnerJudgment::Absent,
        Ok(Some(owner)) => match owner.managed_session_id.as_deref() {
            Some(bound) if bound == session => OwnerJudgment::Bound((observers.attest)(owner)),
            Some(_) => OwnerJudgment::Foreign,
            None => OwnerJudgment::UnboundLegacy {
                alive: (observers.pid_alive)(owner.pid),
            },
        },
    }
}

impl BoundOwner {
    pub(in crate::native) fn of(attestation: &OwnerAttestation) -> Self {
        match &attestation.process {
            ProcessObservation::Live(Attested::Incarnation) => match &attestation.control {
                #[cfg(any(target_os = "macos", windows, test))]
                ControlAttestation::Verified => Self::Verified,
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Changed(_) => Self::ControlChanged,
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Unknown(_) => Self::Unknown,
                ControlAttestation::NotRecorded => Self::Uncompared,
            },
            ProcessObservation::Live(Attested::PidOnly) => Self::Uncompared,
            ProcessObservation::Gone(Gone::Exited) => Self::Exited,
            ProcessObservation::Gone(Gone::Reused(_)) => Self::Reused,
            ProcessObservation::Unknown(_) => Self::Unknown,
        }
    }
}

impl OwnerJudgment {
    /// The Bound Owner's verdict; None for every record not bound to this session.
    pub(in crate::native) fn bound(&self) -> Option<BoundOwner> {
        match self {
            Self::Bound(attestation) => Some(BoundOwner::of(attestation)),
            _ => None,
        }
    }

    /// The recorded owner is verified gone: a Bound incarnation that exited or whose PID was
    /// reused, or an unbound legacy PID observed absent.
    pub(in crate::native) fn gone(&self) -> bool {
        match self {
            Self::Bound(attestation) => matches!(
                BoundOwner::of(attestation),
                BoundOwner::Exited | BoundOwner::Reused
            ),
            Self::UnboundLegacy { alive } => *alive == Some(false),
            Self::Foreign | Self::Absent | Self::Unreadable(_) => false,
        }
    }

    /// The renderer-side JSON shape of the judgment.
    pub(in crate::native) fn observation(&self) -> OwnerObservation {
        match self {
            Self::Bound(attestation) => OwnerObservation::from(attestation),
            Self::UnboundLegacy { alive } => OwnerObservation {
                process_alive: *alive,
                ..OwnerObservation::default()
            },
            Self::Foreign => OwnerObservation {
                error: Some(FOREIGN_OWNER.to_owned()),
                ..OwnerObservation::default()
            },
            Self::Absent => OwnerObservation::default(),
            Self::Unreadable(reason) => OwnerObservation {
                error: Some(reason.clone()),
                ..OwnerObservation::default()
            },
        }
    }
}

#[cfg(test)]
pub(in crate::native) mod tests {
    use super::*;
    use ControlAttestation::*;
    use serde_json::{Value, json};

    pub(in crate::native) fn attestation(
        process: ProcessObservation,
        control: ControlAttestation,
        pid_alive: Option<bool>,
    ) -> OwnerAttestation {
        OwnerAttestation {
            process,
            control,
            pid_alive,
        }
    }

    fn live(control: ControlAttestation) -> OwnerAttestation {
        attestation(
            ProcessObservation::Live(Attested::Incarnation),
            control,
            Some(true),
        )
    }

    fn projection(judgment: &OwnerJudgment) -> Value {
        let o = judgment.observation();
        json!([o.process_alive, o.identity_matches, o.error])
    }

    fn record(binding: Option<&str>) -> Result<Option<NativeSessionOwner>> {
        Ok(Some(NativeSessionOwner {
            pid: 4242,
            managed_session_id: binding.map(str::to_owned),
            ..Default::default()
        }))
    }

    #[test]
    fn bound_rows_keep_the_attestation_and_decide_death_from_it_alone() {
        type Row = (fn() -> OwnerAttestation, BoundOwner, Value, bool);
        let rows: [Row; 9] = [
            (
                || live(Verified),
                BoundOwner::Verified,
                json!([true, true, null]),
                false,
            ),
            (
                || live(Changed("owner TTY changed".into())),
                BoundOwner::ControlChanged,
                json!([true, false, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Live(Attested::Incarnation),
                        Unknown("no TTY".into()),
                        Some(false),
                    )
                },
                BoundOwner::Unknown,
                json!([false, null, "no TTY"]),
                false,
            ),
            (
                || live(NotRecorded),
                BoundOwner::Uncompared,
                json!([true, null, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Live(Attested::PidOnly),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Uncompared,
                json!([true, null, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Gone(Gone::Exited),
                        NotRecorded,
                        Some(false),
                    )
                },
                BoundOwner::Exited,
                json!([false, null, null]),
                true,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Gone(Gone::Reused("other birth".into())),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Reused,
                json!([true, false, null]),
                true,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Unknown("denied".into()),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Unknown,
                json!([true, null, "denied"]),
                false,
            ),
            // A false PID hint beside an Unknown attestation is not a death (D15).
            (
                || {
                    attestation(
                        ProcessObservation::Unknown("denied".into()),
                        NotRecorded,
                        Some(false),
                    )
                },
                BoundOwner::Unknown,
                json!([false, null, "denied"]),
                false,
            ),
        ];
        for (index, (make, verdict, expected, gone)) in rows.into_iter().enumerate() {
            let judgment = owner(
                &record(Some("session-test")),
                "session-test",
                &Observers {
                    attest: &|_| make(),
                    pid_alive: &|_| panic!("a bound record is attested, not PID-observed"),
                },
            );
            assert!(matches!(judgment, OwnerJudgment::Bound(_)), "{index}");
            assert_eq!(judgment.bound(), Some(verdict), "{index}");
            assert_eq!(projection(&judgment), expected, "{index}");
            assert_eq!(judgment.gone(), gone, "{index}");
        }
    }

    #[test]
    fn unbound_foreign_absent_and_unreadable_records_are_never_attested() {
        let no_attestation = |_: &NativeSessionOwner| -> OwnerAttestation {
            panic!("only a bound record is attested")
        };
        for (alive, expected, gone) in [
            (Some(true), json!([true, null, null]), false),
            (Some(false), json!([false, null, null]), true),
            (None, json!([null, null, null]), false),
        ] {
            let judgment = owner(
                &record(None),
                "session-test",
                &Observers {
                    attest: &no_attestation,
                    pid_alive: &|pid| {
                        assert_eq!(pid, 4242);
                        alive
                    },
                },
            );
            assert!(matches!(judgment, OwnerJudgment::UnboundLegacy { alive: a } if a == alive));
            assert_eq!(judgment.bound(), None);
            assert_eq!(projection(&judgment), expected);
            assert_eq!(judgment.gone(), gone);
        }
        let unobserved = Observers {
            attest: &no_attestation,
            pid_alive: &|_| panic!("a foreign, absent or unreadable record is not observed"),
        };
        let unreadable = Err(anyhow::anyhow!("invalid JSON").context("owner record"));
        for (read, expected) in [
            (
                record(Some("session-other")),
                json!([null, null, FOREIGN_OWNER]),
            ),
            (Ok(None), json!([null, null, null])),
            (
                unreadable,
                json!([null, null, "owner record: invalid JSON"]),
            ),
        ] {
            let judgment = owner(&read, "session-test", &unobserved);
            match &judgment {
                OwnerJudgment::Foreign => assert!(read.as_ref().is_ok_and(Option::is_some)),
                OwnerJudgment::Absent => assert!(read.as_ref().is_ok_and(Option::is_none)),
                OwnerJudgment::Unreadable(reason) => {
                    assert_eq!(reason, "owner record: invalid JSON")
                }
                other => panic!("unexpected judgment {other:?}"),
            }
            assert_eq!(judgment.bound(), None);
            assert_eq!(projection(&judgment), expected);
            assert!(!judgment.gone());
        }
    }
}
