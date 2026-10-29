use anyhow::Result;
use std::ops::ControlFlow;

use super::CloseOutcome;

type Check<'a> = &'a dyn Fn() -> Result<()>;

#[derive(Clone, Copy)]
pub(super) enum InspectionError {
    Fatal,
    KeepWaiting,
}

pub(super) struct Policy<'a> {
    pub check: Check<'a>,
    pub inspection_error: InspectionError,
}

/// Adapters retain their observations, exact native action and disappearance rules.
/// The transaction owns budget entry, re-probe ordering and inspection-error policy.
/// A waiting error is retained across the pause so deadline diagnostics do not change.
pub(super) fn run<C: ?Sized, S, P>(
    context: &mut C,
    policy: Policy<'_>,
    prepare: impl FnOnce(&mut C, Check<'_>) -> Result<ControlFlow<CloseOutcome, S>>,
    mut probe: impl FnMut(&mut C, &mut S, Check<'_>) -> Result<P>,
    mut advance: impl FnMut(
        &mut C,
        &mut S,
        P,
        Check<'_>,
    ) -> Result<ControlFlow<CloseOutcome, anyhow::Error>>,
    mut pause: impl FnMut(&mut C, &mut S) -> Result<bool>,
) -> Result<CloseOutcome> {
    (policy.check)()?;
    let mut state = match prepare(context, policy.check)? {
        ControlFlow::Break(outcome) => return Ok(outcome),
        ControlFlow::Continue(state) => state,
    };
    loop {
        (policy.check)()?;
        let waiting = match probe(context, &mut state, policy.check) {
            Ok(observation) => match advance(context, &mut state, observation, policy.check)? {
                ControlFlow::Break(outcome) => return Ok(outcome),
                ControlFlow::Continue(waiting) => waiting,
            },
            Err(error) => match policy.inspection_error {
                InspectionError::Fatal => return Err(error),
                InspectionError::KeepWaiting => error,
            },
        };
        if !pause(context, &mut state)? {
            return Err(waiting);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, bail};
    use std::{
        cell::Cell,
        ops::ControlFlow::{Break, Continue},
    };

    #[test]
    fn close_policy_table_preserves_probes_actions_pauses_and_budget() {
        struct Row {
            name: &'static str,
            policy: InspectionError,
            absent_at: usize,
            error_at: Option<usize>,
            exhaust_budget_in_probe: bool,
            budget: usize,
            expected: &'static str,
            probes: usize,
            pauses: usize,
        }
        let rows = [
            Row {
                name: "immediate absence",
                policy: InspectionError::Fatal,
                absent_at: 1,
                error_at: None,
                exhaust_budget_in_probe: false,
                budget: 5,
                expected: "closed",
                probes: 1,
                pauses: 0,
            },
            Row {
                name: "absence after three probes",
                policy: InspectionError::Fatal,
                absent_at: 3,
                error_at: None,
                exhaust_budget_in_probe: false,
                budget: 5,
                expected: "closed",
                probes: 3,
                pauses: 2,
            },
            Row {
                name: "still present at deadline",
                policy: InspectionError::Fatal,
                absent_at: 9,
                error_at: None,
                exhaust_budget_in_probe: false,
                budget: 3,
                expected: "present",
                probes: 3,
                pauses: 3,
            },
            Row {
                name: "Ghostty fatal inspection",
                policy: InspectionError::Fatal,
                absent_at: 3,
                error_at: Some(1),
                exhaust_budget_in_probe: false,
                budget: 5,
                expected: "inspection",
                probes: 1,
                pauses: 0,
            },
            Row {
                name: "WezTerm waits after inspection error",
                policy: InspectionError::KeepWaiting,
                absent_at: 2,
                error_at: Some(1),
                exhaust_budget_in_probe: false,
                budget: 5,
                expected: "closed",
                probes: 2,
                pauses: 1,
            },
            Row {
                name: "WezTerm retains inspection error at deadline",
                policy: InspectionError::KeepWaiting,
                absent_at: 9,
                error_at: Some(1),
                exhaust_budget_in_probe: false,
                budget: 1,
                expected: "inspection",
                probes: 1,
                pauses: 1,
            },
            Row {
                name: "Warp checks time before propagating inspection",
                policy: InspectionError::Fatal,
                absent_at: 3,
                error_at: Some(1),
                exhaust_budget_in_probe: false,
                budget: 5,
                expected: "inspection",
                probes: 1,
                pauses: 0,
            },
            Row {
                name: "probe exhausts budget then reports inspection error",
                policy: InspectionError::Fatal,
                absent_at: 3,
                error_at: Some(1),
                exhaust_budget_in_probe: true,
                budget: 5,
                expected: "inspection",
                probes: 1,
                pauses: 0,
            },
            Row {
                name: "exhausted before ownership proof",
                policy: InspectionError::Fatal,
                absent_at: 1,
                error_at: None,
                exhaust_budget_in_probe: false,
                budget: 0,
                expected: "budget",
                probes: 0,
                pauses: 0,
            },
        ];
        for row in rows {
            let elapsed = Cell::new(0);
            let checks = Cell::new(0);
            let actions = Cell::new(0);
            let probes = Cell::new(0);
            let pauses = Cell::new(0);
            let result = run(
                &mut (),
                Policy {
                    inspection_error: row.policy,
                    check: &|| {
                        checks.set(checks.get() + 1);
                        if elapsed.get() >= row.budget {
                            bail!("budget");
                        }
                        Ok(())
                    },
                },
                |_, _| {
                    actions.set(actions.get() + 1);
                    Ok(Continue(()))
                },
                |_, _, _| {
                    probes.set(probes.get() + 1);
                    if row.exhaust_budget_in_probe {
                        elapsed.set(row.budget);
                    }
                    if row.error_at == Some(probes.get()) {
                        bail!("inspection");
                    }
                    Ok(probes.get() < row.absent_at)
                },
                |_, _, present, _| {
                    Ok(if present {
                        Continue(anyhow!("present"))
                    } else {
                        Break(CloseOutcome::Closed)
                    })
                },
                |_, _| {
                    pauses.set(pauses.get() + 1);
                    elapsed.set(elapsed.get() + 1);
                    Ok(elapsed.get() < row.budget)
                },
            );
            let actual = match result {
                Ok(CloseOutcome::Closed) => "closed".into(),
                Ok(other) => format!("{other:?}"),
                Err(error) => error.to_string(),
            };
            assert_eq!(actual, row.expected, "{}", row.name);
            if row.exhaust_budget_in_probe {
                assert_eq!(elapsed.get(), row.budget, "{}", row.name);
            }
            assert_eq!(probes.get(), row.probes, "{}", row.name);
            assert_eq!(pauses.get(), row.pauses, "{}", row.name);
            assert_eq!(checks.get(), 1 + row.probes, "{}", row.name);
            assert_eq!(actions.get(), usize::from(row.budget > 0), "{}", row.name);
        }
    }
}
