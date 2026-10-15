//! Process evidence only. No records, locks, OS calls, or authority decisions.
use std::fmt;

#[derive(Debug, PartialEq)]
pub(in crate::native) enum ProcessObservation {
    Live(Attested),
    Gone(Gone),
    Unknown(String),
}

#[derive(Debug, PartialEq)]
pub(in crate::native) enum Attested {
    Incarnation,
    /// PID-only compatibility, never an attestation.
    PidOnly,
}

#[derive(Debug, PartialEq)]
pub(in crate::native) enum Gone {
    Exited,
    Reused(String),
}

pub(in crate::native) fn classify<I: PartialEq + fmt::Debug>(
    pid: u32,
    recorded: Option<&I>,
    pid_alive: impl FnOnce() -> bool,
    live: impl FnOnce() -> anyhow::Result<Option<I>>,
) -> ProcessObservation {
    let Some(recorded) = recorded else {
        return if pid_alive() {
            ProcessObservation::Live(Attested::PidOnly)
        } else {
            ProcessObservation::Gone(Gone::Exited)
        };
    };
    match live() {
        Ok(None) => ProcessObservation::Gone(Gone::Exited),
        Ok(Some(live)) if &live == recorded => ProcessObservation::Live(Attested::Incarnation),
        Ok(Some(live)) => ProcessObservation::Gone(Gone::Reused(format!(
            "process {pid} was reused: recorded {recorded:?}, live {live:?}"
        ))),
        Err(error) => ProcessObservation::Unknown(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classification_preserves_evidence_and_calls_only_the_required_observer() {
        for alive in [false, true] {
            let result = classify::<u64>(42, None, || alive, || panic!("legacy identity"));
            assert_eq!(
                result,
                if alive {
                    ProcessObservation::Live(Attested::PidOnly)
                } else {
                    ProcessObservation::Gone(Gone::Exited)
                }
            );
        }
        for (live, expected) in [
            (Ok(None), ProcessObservation::Gone(Gone::Exited)),
            (Ok(Some(1)), ProcessObservation::Live(Attested::Incarnation)),
            (
                Ok(Some(2)),
                ProcessObservation::Gone(Gone::Reused(
                    "process 42 was reused: recorded 1, live 2".into(),
                )),
            ),
            (
                Err(anyhow::anyhow!("denied")),
                ProcessObservation::Unknown("denied".into()),
            ),
            (
                Err(anyhow::anyhow!("denied").context("inspection")),
                ProcessObservation::Unknown("inspection: denied".into()),
            ),
        ] {
            assert_eq!(
                classify(
                    42,
                    Some(&1),
                    || panic!("identity must not use PID fallback"),
                    || live
                ),
                expected
            );
        }
    }
}
