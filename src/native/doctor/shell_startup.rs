//! #376: the launch host's recorded stage and an opt-in run of the shell environment
//! preparation. Neither check writes anything, and neither shows startup-file output or an
//! environment value.
use super::super::diagnostics::{Availability, Check};
use crate::native::diagnostics::vocabulary::{CheckId, Reason};
use crate::native::session::launch::host::{
    Failure, FailureReason, HostObservation, HostRecord, Stage,
};
use crate::native::terminal::TerminalKind;
use agent_bridge::PUBLIC_COMMAND;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Facts other records hold about the same launch. Absence establishes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LaterEvidence {
    /// The wrapper recorded an owner: the launch shell ran.
    pub(super) owner_recorded: bool,
}

pub(super) fn launch_stage_check(
    observation: &HostObservation,
    known_terminal: Option<TerminalKind>,
    later: LaterEvidence,
    session: &str,
) -> Check {
    use Availability::*;
    let probe = format!(
        "Run `{PUBLIC_COMMAND} doctor {session} --probe` to run the same preparation once now."
    );
    match observation {
        HostObservation::Recorded(host) => {
            let evidence =
                json!({"record": record_evidence(host), "owner_recorded": later.owner_recorded});
            match &host.failure {
                Some(failure) => Check::new(
                    CheckId::LAUNCH_STAGE,
                    Unavailable,
                    Reason::LAUNCH_HOST_FAILED,
                    format!(
                        "The {} launch host failed after stage {}: {}.",
                        terminal_name(host),
                        stage_name(host.stage),
                        failure_text(failure, host)
                    ),
                    failure_action(failure.reason, &probe),
                )
                .evidence(evidence),
                None if host.stage == Stage::LaunchShellStarting || later.owner_recorded => {
                    Check::new(
                        CheckId::LAUNCH_STAGE,
                        Available,
                        Reason::LAUNCH_STAGE_OBSERVED,
                        format!(
                            "The {} launch host recorded stage {}; the shell environment was prepared without a terminal.",
                            terminal_name(host),
                            stage_name(host.stage)
                        ),
                        "No host action is needed; inspect the provider and request records for later stages.",
                    )
                    .evidence(evidence)
                }
                None => Check::new(
                    CheckId::LAUNCH_STAGE,
                    Unknown,
                    Reason::LAUNCH_STAGE_OBSERVED,
                    format!(
                        "The last recorded stage of the {} launch host is {}. A retained earlier stage does not prove that the step is still running.",
                        terminal_name(host),
                        stage_name(host.stage)
                    ),
                    format!("Inspect the launch log with `{PUBLIC_COMMAND} inspect {session} --timeline`. {probe}"),
                )
                .evidence(evidence),
            }
        }
        HostObservation::NotRecorded => match known_terminal {
            Some(kind) if !host_terminal(kind) => not_applicable(kind),
            _ if !cfg!(target_os = "macos") => Check::new(
                CheckId::LAUNCH_STAGE,
                Unavailable,
                Reason::LAUNCH_STAGE_NOT_APPLICABLE,
                "Launch host records exist only for macOS iTerm2 and WezTerm launches.",
                "No action is needed.",
            ),
            _ => Check::new(
                CheckId::LAUNCH_STAGE,
                Unknown,
                Reason::LAUNCH_HOST_NOT_RECORDED,
                "No launch host record is retained. A session from an earlier release, another terminal, or a launch that did not reach surface creation has none; absence is not evidence that the host never started.",
                format!(
                    "Inspect the launch log with `{PUBLIC_COMMAND} inspect {session} --timeline`."
                ),
            ),
        },
        HostObservation::Unreadable(reason) => Check::new(
            CheckId::LAUNCH_STAGE,
            Unknown,
            Reason::LAUNCH_HOST_RECORD_UNREADABLE,
            format!("The launch host record could not be read: {reason}"),
            "Preserve the session records; doctor does not repair them.",
        ),
        HostObservation::SchemaUnsupported(schema) => Check::new(
            CheckId::LAUNCH_STAGE,
            Unknown,
            Reason::LAUNCH_HOST_SCHEMA_UNSUPPORTED,
            format!("The launch host record has schema {schema}, which this build does not read."),
            "Use the Bridge build that launched the session to read it.",
        ),
        HostObservation::IdentityMismatch => Check::new(
            CheckId::LAUNCH_STAGE,
            Unknown,
            Reason::LAUNCH_HOST_IDENTITY_MISMATCH,
            "The launch host record does not name this session's launch receipt, so it says nothing about this launch.",
            "Preserve the session records and inspect the launch receipt.",
        ),
    }
}

fn not_applicable(kind: TerminalKind) -> Check {
    Check::new(
        CheckId::LAUNCH_STAGE,
        Availability::Unavailable,
        Reason::LAUNCH_STAGE_NOT_APPLICABLE,
        "Launch host records exist only for macOS iTerm2 and WezTerm launches.",
        "No action is needed.",
    )
    .evidence(json!({"terminal": kind}))
}

pub(super) fn host_terminal(kind: TerminalKind) -> bool {
    matches!(kind, TerminalKind::Iterm2 | TerminalKind::WezTerm)
}

fn terminal_name(host: &HostRecord) -> &'static str {
    match host.terminal {
        crate::native::session::launch::host::Terminal::Iterm2 => "iTerm2",
        crate::native::session::launch::host::Terminal::Wezterm => "WezTerm",
    }
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::AwaitingHost => "awaiting_host",
        Stage::HostStarted => "host_started",
        Stage::EnvironmentPreparing => "environment_preparing",
        Stage::EnvironmentReady => "environment_ready",
        Stage::LaunchShellStarting => "launch_shell_starting",
    }
}

fn reason_name(reason: FailureReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn failure_text(failure: &Failure, host: &HostRecord) -> String {
    let mut text = reason_name(failure.reason);
    if let Some(code) = failure.exit_code {
        text.push_str(&format!(", exit status {code}"));
    }
    if let Some(signal) = failure.signal {
        text.push_str(&format!(", signal {signal}"));
    }
    if failure.reason == FailureReason::CaptureTimeout {
        text.push_str(&format!(
            ", the startup files did not finish within {} ms",
            host.capture_budget_ms
        ));
    }
    text
}

fn failure_action(reason: FailureReason, probe: &str) -> String {
    use FailureReason::*;
    match reason {
        CaptureTimeout | CaptureExitNonzero | CaptureSignaled | CaptureNoOutput
        | CaptureOversized | CaptureInvalid => format!(
            "Your zsh startup files (`/bin/zsh -l -i`) did not produce an environment when run without a terminal. Check them for commands that wait, fail or print to the environment without a terminal. {probe}"
        ),
        LaunchBudgetExhausted | BindingTimeout => format!(
            "The launch deadline left no time for this step. Check the terminal application and system load, then start a new session. {probe}"
        ),
        IdentityUnavailable | BindingMismatch => {
            "The terminal did not identify the surface Bridge created. Close this session and start a new one; do not reuse its surface.".to_owned()
        }
        CaptureSpawnFailed | CleanupFailed | InputFlushFailed | LaunchShellExecFailed => {
            format!("Bridge could not complete this host step. Close this session and start a new one. {probe}")
        }
    }
}

fn record_evidence(host: &HostRecord) -> Value {
    json!({
        "terminal": host.terminal,
        "stage": host.stage,
        "failure": host.failure,
        "updated_unix_ms": host.updated_unix_ms,
        "elapsed_ms": host.elapsed_ms,
        "capture_budget_ms": host.capture_budget_ms,
        "capture_elapsed_ms": host.capture_elapsed_ms,
        "variables": host.variables,
        "background_children_stopped": host.background_children_stopped,
    })
}

/// Where the probe would apply, decided before it runs.
#[derive(Clone, Copy, Debug)]
pub(super) enum ProbeContext {
    NotRequested,
    Terminal(TerminalKind),
    Unknown,
}

/// What a probe that ran returned, reduced to counters and numeric codes.
#[derive(Debug)]
pub(super) enum ProbeOutcome {
    Ready {
        elapsed: Duration,
        variables: usize,
        background_children_stopped: u32,
    },
    Failed {
        reason: FailureReason,
        exit_code: Option<i32>,
        signal: Option<i32>,
        elapsed: Duration,
    },
}

/// The launch's own capture limit; a shorter budget that ran out is inconclusive.
const LAUNCH_CAPTURE_LIMIT: Duration = Duration::from_secs(10);

pub(super) fn shell_environment_check(
    context: ProbeContext,
    run: impl FnOnce() -> (ProbeOutcome, Duration),
) -> Check {
    use Availability::*;
    const SOURCE: &str = "current_caller";
    let kind = match context {
        ProbeContext::NotRequested => {
            return Check::new(
                CheckId::SHELL_ENVIRONMENT,
                Unknown,
                Reason::SHELL_ENVIRONMENT_PROBE_NOT_REQUESTED,
                "The shell environment preparation was not run.",
                "Add --probe to run it once; it runs your zsh startup files.",
            );
        }
        ProbeContext::Unknown => {
            return Check::new(
                CheckId::SHELL_ENVIRONMENT,
                Unknown,
                Reason::SHELL_ENVIRONMENT_PROBE_CONTEXT_UNAVAILABLE,
                "The terminal of this launch could not be determined from its records, so the preparation was not run.",
                "Run doctor --provider PROVIDER --probe from the terminal you launch with.",
            );
        }
        ProbeContext::Terminal(kind) if !cfg!(target_os = "macos") || !host_terminal(kind) => {
            return Check::new(
                CheckId::SHELL_ENVIRONMENT,
                Unavailable,
                Reason::SHELL_ENVIRONMENT_PROBE_NOT_APPLICABLE,
                "Only macOS iTerm2 and WezTerm launches prepare the shell environment this way.",
                "No action is needed.",
            )
            .evidence(json!({"terminal": kind}));
        }
        ProbeContext::Terminal(kind) => kind,
    };
    let (outcome, budget) = run();
    let budget_ms = budget.as_millis();
    match outcome {
        ProbeOutcome::Ready {
            elapsed,
            variables,
            background_children_stopped,
        } => Check::new(
            CheckId::SHELL_ENVIRONMENT,
            Available,
            Reason::SHELL_ENVIRONMENT_READY,
            format!("Your zsh startup files prepared {variables} environment variables without a terminal in {} ms.", elapsed.as_millis()),
            "This ran in the caller's current environment, not the terminal's; a launch can still differ.",
        )
        .evidence(json!({"source": SOURCE, "terminal": kind, "elapsed_ms": elapsed.as_millis(),
            "budget_ms": budget_ms, "variables": variables,
            "background_children_stopped": background_children_stopped})),
        ProbeOutcome::Failed {
            reason,
            exit_code,
            signal,
            elapsed,
        } if reason == FailureReason::LaunchBudgetExhausted
            || (reason == FailureReason::CaptureTimeout && budget < LAUNCH_CAPTURE_LIMIT) =>
        {
            Check::new(
                CheckId::SHELL_ENVIRONMENT,
                Unknown,
                Reason::SHELL_ENVIRONMENT_PROBE_BUDGET_EXHAUSTED,
                format!("The probe budget ({budget_ms} ms) ran out before the startup files finished; the result is inconclusive."),
                "Run doctor again with fewer other probes, or start a launch and read its launch_stage.",
            )
            .evidence(json!({"source": SOURCE, "terminal": kind, "elapsed_ms": elapsed.as_millis(),
                "budget_ms": budget_ms, "reason": reason, "exit_code": exit_code, "signal": signal}))
        }
        ProbeOutcome::Failed {
            reason,
            exit_code,
            signal,
            elapsed,
        } => Check::new(
            CheckId::SHELL_ENVIRONMENT,
            Unavailable,
            Reason::SHELL_ENVIRONMENT_FAILED,
            format!("Your zsh startup files did not prepare an environment without a terminal: {}.", reason_name(reason)),
            "Check `/bin/zsh -l -i` startup files for commands that wait, fail or need a terminal. Their output is not shown here.",
        )
        .evidence(json!({"source": SOURCE, "terminal": kind, "elapsed_ms": elapsed.as_millis(),
            "budget_ms": budget_ms, "reason": reason, "exit_code": exit_code, "signal": signal})),
    }
}

/// Runs the launch's capture once in the caller's environment. The capture reserves time
/// for a launch handoff that a probe does not make, so the probe hands it that reserve back;
/// the capture still ends, cleanup included, by `deadline`.
#[cfg(target_os = "macos")]
pub(super) fn run_probe(deadline: Instant) -> (ProbeOutcome, Duration) {
    use crate::native::terminal::macos::shell_environment::{CLEANUP, HANDOFF_RESERVE, capture};
    let budget = deadline
        .saturating_duration_since(Instant::now())
        .saturating_sub(CLEANUP)
        .min(LAUNCH_CAPTURE_LIMIT);
    let outcome = match capture(std::env::vars_os(), deadline + HANDOFF_RESERVE) {
        Ok(captured) => ProbeOutcome::Ready {
            elapsed: captured.elapsed,
            variables: captured.variables.len(),
            background_children_stopped: captured.background_children_stopped,
        },
        Err(failure) => ProbeOutcome::Failed {
            reason: failure.reason,
            exit_code: failure.exit_code,
            signal: failure.signal,
            elapsed: failure.elapsed,
        },
    };
    (outcome, budget)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn run_probe(_deadline: Instant) -> (ProbeOutcome, Duration) {
    unreachable!("the shell environment probe is gated on macOS before it runs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::launch::host::{HostIdentity, Terminal};

    fn host(stage: Stage, failure: Option<Failure>) -> HostRecord {
        HostRecord {
            schema: 1,
            managed_session_id: "session-abc".into(),
            claim_token: "token".into(),
            terminal: Terminal::Iterm2,
            stage,
            host: (stage != Stage::AwaitingHost).then_some(HostIdentity {
                pid: 10,
                start_seconds: 1,
                start_microseconds: 0,
            }),
            updated_unix_ms: 5,
            elapsed_ms: 7,
            capture_budget_ms: 10_000,
            capture_elapsed_ms: Some(10_001),
            variables: None,
            background_children_stopped: None,
            failure,
        }
    }

    fn check(observation: HostObservation, kind: Option<TerminalKind>, owner: bool) -> Value {
        serde_json::to_value(launch_stage_check(
            &observation,
            kind,
            LaterEvidence {
                owner_recorded: owner,
            },
            "session-abc",
        ))
        .unwrap()
    }

    #[test]
    fn launch_stage_names_the_recorded_stage_failure_and_absence() {
        let failed = check(
            HostObservation::Recorded(host(
                Stage::EnvironmentPreparing,
                Some(Failure {
                    reason: FailureReason::CaptureTimeout,
                    exit_code: None,
                    signal: None,
                }),
            )),
            None,
            false,
        );
        assert_eq!(failed["availability"], "unavailable");
        assert_eq!(failed["reason_code"], "launch_host_failed");
        let detail = failed["detail"].as_str().unwrap();
        assert!(detail.contains("environment_preparing"), "{detail}");
        assert!(detail.contains("capture_timeout"), "{detail}");
        assert!(detail.contains("10000 ms"), "{detail}");
        assert_eq!(
            failed["evidence"]["record"]["failure"]["reason"],
            "capture_timeout"
        );

        let exited = check(
            HostObservation::Recorded(host(
                Stage::EnvironmentPreparing,
                Some(Failure {
                    reason: FailureReason::CaptureExitNonzero,
                    exit_code: Some(3),
                    signal: None,
                }),
            )),
            None,
            false,
        );
        assert!(exited["detail"].as_str().unwrap().contains("exit status 3"));

        let done = check(
            HostObservation::Recorded(host(Stage::LaunchShellStarting, None)),
            None,
            false,
        );
        assert_eq!(done["availability"], "available");
        assert_eq!(done["reason_code"], "launch_stage_observed");

        // A retained earlier stage is reported as such, not as a step still running.
        let earlier = check(
            HostObservation::Recorded(host(Stage::EnvironmentPreparing, None)),
            None,
            false,
        );
        assert_eq!(earlier["availability"], "unknown");
        assert_eq!(earlier["reason_code"], "launch_stage_observed");
        // The wrapper's owner record shows the launch went further.
        let later = check(
            HostObservation::Recorded(host(Stage::EnvironmentReady, None)),
            None,
            true,
        );
        assert_eq!(later["availability"], "available");

        let absent = check(HostObservation::NotRecorded, None, false);
        assert_eq!(
            absent["availability"],
            if cfg!(target_os = "macos") {
                "unknown"
            } else {
                "unavailable"
            }
        );
        assert_eq!(
            absent["reason_code"],
            if cfg!(target_os = "macos") {
                "launch_host_not_recorded"
            } else {
                "launch_stage_not_applicable"
            }
        );
        let other = check(
            HostObservation::NotRecorded,
            Some(TerminalKind::Ghostty),
            false,
        );
        assert_eq!(other["reason_code"], "launch_stage_not_applicable");
        for (observation, reason) in [
            (
                HostObservation::Unreadable("bad".into()),
                "launch_host_record_unreadable",
            ),
            (
                HostObservation::SchemaUnsupported(2),
                "launch_host_schema_unsupported",
            ),
            (
                HostObservation::IdentityMismatch,
                "launch_host_identity_mismatch",
            ),
        ] {
            let row = check(observation, None, false);
            assert_eq!(row["availability"], "unknown");
            assert_eq!(row["reason_code"], reason);
        }
    }

    fn probe(context: ProbeContext, outcome: Option<(ProbeOutcome, Duration)>) -> Value {
        let ran = std::cell::Cell::new(false);
        let check = shell_environment_check(context, || {
            ran.set(true);
            outcome.expect("the probe must not run")
        });
        let mut value = serde_json::to_value(check).unwrap();
        value["ran"] = json!(ran.get());
        value
    }

    #[test]
    fn shell_environment_runs_only_when_requested_and_applicable() {
        let row = probe(ProbeContext::NotRequested, None);
        assert_eq!(row["reason_code"], "shell_environment_probe_not_requested");
        assert_eq!(row["ran"], false);
        let row = probe(ProbeContext::Unknown, None);
        assert_eq!(
            row["reason_code"],
            "shell_environment_probe_context_unavailable"
        );
        assert_eq!(row["ran"], false);
        let row = probe(ProbeContext::Terminal(TerminalKind::Ghostty), None);
        assert_eq!(row["reason_code"], "shell_environment_probe_not_applicable");
        assert_eq!(row["ran"], false);
        if !cfg!(target_os = "macos") {
            let row = probe(ProbeContext::Terminal(TerminalKind::Iterm2), None);
            assert_eq!(row["reason_code"], "shell_environment_probe_not_applicable");
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shell_environment_reports_outcomes_without_output() {
        let ready = probe(
            ProbeContext::Terminal(TerminalKind::WezTerm),
            Some((
                ProbeOutcome::Ready {
                    elapsed: Duration::from_millis(1350),
                    variables: 70,
                    background_children_stopped: 1,
                },
                Duration::from_millis(4000),
            )),
        );
        assert_eq!(ready["availability"], "available");
        assert_eq!(ready["reason_code"], "shell_environment_ready");
        assert_eq!(ready["evidence"]["source"], "current_caller");
        assert_eq!(ready["evidence"]["variables"], 70);
        assert_eq!(ready["evidence"]["background_children_stopped"], 1);
        assert_eq!(ready["evidence"]["budget_ms"], 4000);

        let failed = |reason, budget| {
            probe(
                ProbeContext::Terminal(TerminalKind::Iterm2),
                Some((
                    ProbeOutcome::Failed {
                        reason,
                        exit_code: Some(3),
                        signal: None,
                        elapsed: Duration::from_millis(20),
                    },
                    budget,
                )),
            )
        };
        let row = failed(FailureReason::CaptureExitNonzero, Duration::from_secs(4));
        assert_eq!(row["availability"], "unavailable");
        assert_eq!(row["reason_code"], "shell_environment_failed");
        assert_eq!(row["evidence"]["exit_code"], 3);
        // A timeout within a budget shorter than the launch's own limit proves nothing.
        let row = failed(FailureReason::CaptureTimeout, Duration::from_secs(4));
        assert_eq!(row["availability"], "unknown");
        assert_eq!(
            row["reason_code"],
            "shell_environment_probe_budget_exhausted"
        );
        let row = failed(FailureReason::LaunchBudgetExhausted, Duration::ZERO);
        assert_eq!(
            row["reason_code"],
            "shell_environment_probe_budget_exhausted"
        );
        let row = failed(FailureReason::CaptureTimeout, LAUNCH_CAPTURE_LIMIT);
        assert_eq!(row["reason_code"], "shell_environment_failed");
    }
}
