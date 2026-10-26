//! Delivery owns the ordering and settlement of a claimed Turn. Adapters own Transport.
use super::failure_kind::{DeliveryUncertain, TimedOut};
use super::session::{Reader, RecordStore, SessionState, turn};
use super::terminal;
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnKind {
    Initial,
    FollowUp,
}
pub(super) enum Turn<'a> {
    Initial {
        gate: &'a mut dyn FnMut(Gate) -> Result<()>,
    },
    FollowUp {
        prompt: &'a str,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gate {
    AfterLaunch,
    BeforeDelivery,
}
pub(super) struct DeliveryContext<'a> {
    pub directory: &'a Path,
    pub session_id: &'a str,
    pub surface: &'a terminal::TerminalSession,
    pub provider_path: &'a Path,
    pub deadline: Instant,
    pub requested: Duration,
}
#[derive(Debug)]
pub(super) struct Delivered {
    pub correlation_id: Option<String>,
}
pub(super) trait Transport {
    fn facts(&self) -> TransportFacts;
    fn prepare(&self, turn: &TurnContext<'_>) -> Result<Prepared>;
    fn send(
        &self,
        turn: &TurnContext<'_>,
        prepared: &Prepared,
        prompt_file: Option<&Path>,
    ) -> Outcome;
    fn discard_prepared(&self, turn: &TurnContext<'_>, prepared: Option<&Prepared>);
}
pub(super) struct TransportFacts {
    pub name: &'static str,
    pub readiness: Readiness,
    pub surface: Option<SurfaceInput>,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Readiness {
    LaunchArgument,
    AfterLaunch { ready_delay: Duration },
}
pub(super) struct SurfaceInput {
    pub submit_count: usize,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub newline: terminal::ConsoleNewline,
}
pub(super) struct TurnContext<'a> {
    pub kind: TurnKind,
    pub directory: &'a Path,
    pub session_id: &'a str,
    pub claim_token: &'a str,
    pub surface: &'a terminal::TerminalSession,
    pub provider_path: &'a Path,
    pub bridge_executable: &'a Path,
    pub prompt: &'a str,
    pub deadline: Instant,
}
pub(super) struct Prepared {
    pub prompt: String,
    pub correlation_id: Option<String>,
}
#[derive(Debug)]
pub(super) enum Outcome {
    Sent,
    NotSent(anyhow::Error),
    Uncertain(anyhow::Error),
}
pub(super) struct LaunchArgument;
pub(super) const LAUNCH_ARGUMENT: LaunchArgument = LaunchArgument;
impl Transport for LaunchArgument {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "launch-argument",
            readiness: Readiness::LaunchArgument,
            surface: None,
        }
    }
    fn prepare(&self, _: &TurnContext<'_>) -> Result<Prepared> {
        Ok(Prepared {
            prompt: String::new(),
            correlation_id: None,
        })
    }
    fn send(&self, _: &TurnContext<'_>, _: &Prepared, _: Option<&Path>) -> Outcome {
        unreachable!("launch arguments are delivered by the wrapper")
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

impl From<terminal::TerminalSendResult> for Outcome {
    fn from(result: terminal::TerminalSendResult) -> Self {
        match result {
            Ok(()) => Self::Sent,
            Err(failure) => {
                let uncertain = failure.delivery_may_have_occurred();
                let error = failure.into_error();
                if uncertain {
                    Self::Uncertain(error)
                } else {
                    Self::NotSent(error)
                }
            }
        }
    }
}

pub(in crate::native) struct RuntimeChecks;
impl Checks for RuntimeChecks {
    fn ready(&self, context: &DeliveryContext<'_>, delay: Duration) -> Result<()> {
        turn::wait_for_status(
            context.directory,
            SessionState::AwaitingInitialInput,
            context.deadline,
            context.requested,
        )?;
        let delay = initial_prompt_delay_within_budget(context.deadline, delay, context.requested)?;
        std::thread::sleep(delay);
        Ok(())
    }
    fn ownership(&self, context: &DeliveryContext<'_>) -> Result<()> {
        verify_terminal_surface_ownership_until(
            context.directory,
            context.session_id,
            context.surface,
            context.deadline,
            context.requested,
        )
    }
    fn remaining(&self, context: &DeliveryContext<'_>) -> Result<Duration> {
        remaining_turn_timeout(context.deadline, context.requested)
    }
    fn budget(
        &self,
        context: &DeliveryContext<'_>,
        count: usize,
        remaining: Duration,
    ) -> Result<()> {
        terminal::validate_terminal_send_budget(count, context.surface.kind, remaining)
    }
}

// Injection points for external checks; the real Store and Claim remain in tests.
pub(in crate::native) trait Checks {
    fn ready(&self, context: &DeliveryContext<'_>, delay: Duration) -> Result<()>;
    fn ownership(&self, context: &DeliveryContext<'_>) -> Result<()>;
    fn remaining(&self, context: &DeliveryContext<'_>) -> Result<Duration>;
    fn budget(
        &self,
        context: &DeliveryContext<'_>,
        count: usize,
        remaining: Duration,
    ) -> Result<()>;
}

impl DeliveryContext<'_> {
    fn turn_context<'a>(
        &'a self,
        initial: bool,
        token: &'a str,
        prompt: &'a str,
        executable: &'a Path,
    ) -> TurnContext<'a> {
        TurnContext {
            kind: if initial {
                TurnKind::Initial
            } else {
                TurnKind::FollowUp
            },
            directory: self.directory,
            session_id: self.session_id,
            claim_token: token,
            surface: self.surface,
            provider_path: self.provider_path,
            bridge_executable: executable,
            prompt,
            deadline: self.deadline,
        }
    }
}

/// A failed Delivery and whether its prompt may have reached the provider. Only the Turn
/// run's hook reads `uncertain`, to mark the context it already adds; nothing persists it.
pub(in crate::native) struct Undelivered {
    pub(in crate::native) error: anyhow::Error,
    pub(in crate::native) uncertain: bool,
}

impl Undelivered {
    fn not_uncertain(error: anyhow::Error) -> Self {
        Self {
            error,
            uncertain: false,
        }
    }
}

// Context is added only after settlement: status.error retains the provider/check cause.
// An uncertain send marks its outermost existing context; the text is unchanged.
#[derive(Clone, Copy)]
enum Stage {
    Bare,
    Prepare,
    Remaining,
    Budget,
    Send,
}
fn follow_up_error(
    error: anyhow::Error,
    stage: Stage,
    facts: &TransportFacts,
    context: &TurnContext<'_>,
    uncertain: bool,
) -> anyhow::Error {
    let name = facts.name;
    match stage {
        Stage::Prepare if facts.surface.is_some() => error.context(format!(
            "failed to prepare input for visible {} session {}", context.surface.kind.display_name(), context.session_id))
            .context(format!("provider follow-up transport {name} failed")),
        Stage::Send if facts.surface.is_some() => {
            let error = error.context(format!(
                "failed to type into visible {} session {}", context.surface.kind.display_name(), context.session_id));
            let message = format!("provider follow-up transport {name} failed");
            if uncertain {
                error.context(DeliveryUncertain::new(message))
            } else {
                error.context(message)
            }
        }
        Stage::Send if uncertain => error.context(DeliveryUncertain::new(format!(
            "provider follow-up transport {name} could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed"))),
        Stage::Send => error.context(format!("provider follow-up transport {name} failed")),
        Stage::Remaining if facts.surface.is_some() => error.context(format!(
            "provider follow-up transport {name} exhausted its total timeout before delivery")),
        Stage::Budget => error.context(format!(
            "provider follow-up transport {name} cannot start inside its remaining total timeout")),
        _ => error,
    }
}

// The plain form that delivery/tests.rs drives; the Turn run uses `deliver`.
#[cfg(test)]
pub(in crate::native) fn deliver_with(
    claim: turn::Claim,
    turn: Turn<'_>,
    context: DeliveryContext<'_>,
    transport: &dyn Transport,
    checks: &dyn Checks,
) -> Result<Delivered> {
    deliver(claim, turn, context, transport, checks).map_err(|undelivered| undelivered.error)
}

/// Deliver the Turn; a failure keeps whether its prompt may have been delivered.
pub(in crate::native) fn deliver(
    mut claim: turn::Claim,
    mut turn: Turn<'_>,
    context: DeliveryContext<'_>,
    transport: &dyn Transport,
    checks: &dyn Checks,
) -> std::result::Result<Delivered, Undelivered> {
    let facts = transport.facts();
    let initial = matches!(turn, Turn::Initial { .. });
    if initial && facts.readiness == Readiness::LaunchArgument {
        claim
            .settle_delivery(turn::Delivery::Sent)
            .map_err(Undelivered::not_uncertain)?;
        return Ok(Delivered {
            correlation_id: None,
        });
    }
    let token = claim.token().to_owned();
    let mut prompt = String::new();
    // Paste has never needed current_exe. Resolve it only for addressed transports.
    let mut executable = std::path::PathBuf::new();
    let mut prepared = None;
    let mut prompt_file = None;
    let mut stage = Stage::Bare;
    let outcome = (|| -> Result<Outcome> {
        if let Turn::Initial { gate } = &mut turn {
            if let Readiness::AfterLaunch { ready_delay } = facts.readiness {
                checks.ready(&context, ready_delay)?;
                gate(Gate::AfterLaunch)?;
            }
            prompt = Reader::open_unchecked(context.directory)
                .initial_prompt()
                .context("failed to read the preserved initial prompt")?;
        } else if let Turn::FollowUp { prompt: input } = turn {
            prompt = input.to_owned();
        }
        stage = Stage::Prepare;
        prepared = Some(transport.prepare(&context.turn_context(
            initial,
            &token,
            &prompt,
            &executable,
        ))?);
        if initial && facts.surface.is_none() {
            executable =
                std::env::current_exe().context("failed to locate the current executable")?;
        }
        if facts.surface.is_some() {
            let mut file = tempfile::Builder::new()
                .prefix("pending-prompt-")
                .suffix(".txt")
                .tempfile_in(context.directory)?;
            RecordStore::set_file_private(file.as_file())?;
            file.write_all(&terminal_input_bytes(
                context.surface.kind,
                &prepared.as_ref().unwrap().prompt,
            ))?;
            file.flush()?;
            prompt_file = Some(file);
            checks.ownership(&context)?;
        }
        stage = Stage::Remaining;
        let remaining = checks.remaining(&context)?;
        if let Some(surface) = &facts.surface {
            stage = Stage::Budget;
            checks.budget(&context, surface.submit_count, remaining)?;
        }
        stage = Stage::Bare;
        if !initial && facts.surface.is_none() {
            executable =
                std::env::current_exe().context("failed to locate the current executable")?;
        }
        if let Turn::Initial { gate } = &mut turn {
            gate(Gate::BeforeDelivery)?;
        }
        claim.begin_delivery()?;
        stage = Stage::Send;
        Ok(transport.send(
            &context.turn_context(initial, &token, &prompt, &executable),
            prepared.as_ref().unwrap(),
            prompt_file.as_ref().map(|file| file.path()),
        ))
    })()
    .unwrap_or_else(Outcome::NotSent);
    // Initial failure files historically disappear before rollback/uncertainty writes;
    // follow-up files live through settlement. Do not put a long-lived lock around send.
    if (initial || matches!(stage, Stage::Prepare)) && !matches!(outcome, Outcome::Sent) {
        drop(prompt_file.take());
    }
    match outcome {
        Outcome::Sent => {
            if initial {
                if let Err(error) = claim.complete_initial_delivery() {
                    drop(prompt_file.take());
                    let _ = claim.record_delivery_diagnostic(&error);
                    return Err(Undelivered::not_uncertain(error));
                }
            } else {
                drop(prompt_file.take());
                claim
                    .settle_delivery(turn::Delivery::Sent)
                    .map_err(Undelivered::not_uncertain)?;
            }
            Ok(Delivered {
                correlation_id: prepared.and_then(|p| p.correlation_id),
            })
        }
        Outcome::NotSent(error) => {
            transport.discard_prepared(
                &context.turn_context(initial, &token, &prompt, &executable),
                prepared.as_ref(),
            );
            let _ = claim.settle_delivery(turn::Delivery::NotSent(&error));
            Err(Undelivered::not_uncertain(if initial {
                error
            } else {
                follow_up_error(
                    error,
                    stage,
                    &facts,
                    &context.turn_context(initial, &token, &prompt, &executable),
                    false,
                )
            }))
        }
        Outcome::Uncertain(error) => {
            let _ = claim.settle_delivery(turn::Delivery::Uncertain(&error));
            Err(Undelivered {
                // Initial Delivery adds no context; the Turn run's hook marks its own.
                error: if initial {
                    error
                } else {
                    follow_up_error(
                        error,
                        stage,
                        &facts,
                        &context.turn_context(initial, &token, &prompt, &executable),
                        true,
                    )
                },
                uncertain: true,
            })
        }
    }
}

fn initial_prompt_delay_within_budget(
    deadline: Instant,
    delay: Duration,
    requested: Duration,
) -> Result<Duration> {
    let remaining = remaining_turn_timeout(deadline, requested)?;
    if delay >= remaining {
        bail!(
            "initial prompt readiness delay of {} seconds does not fit inside the {} second ask timeout",
            delay.as_secs(),
            requested.as_secs()
        )
    }
    Ok(delay)
}

pub(in crate::native) fn verify_terminal_surface_ownership_until(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    deadline: Instant,
    requested: Duration,
) -> Result<()> {
    let timeout = remaining_turn_timeout(deadline, requested)?;
    terminal::ownership::verify_terminal_surface_ownership_with_timeout(
        directory,
        expected_session_id,
        session,
        Some(timeout),
    )
}

pub(in crate::native) fn remaining_turn_timeout(
    deadline: Instant,
    requested: Duration,
) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .with_context(|| TimedOut::new(format!("timed out after {} seconds", requested.as_secs())))
}

pub(in crate::native) fn terminal_input_bytes(
    kind: terminal::TerminalKind,
    prompt: &str,
) -> Vec<u8> {
    match terminal::facts(kind).paste_envelope {
        terminal::facts::PasteEnvelope::Raw => prompt.as_bytes().to_vec(),
        terminal::facts::PasteEnvelope::Bracketed => {
            format!("\x1b[200~{prompt}\x1b[201~").into_bytes()
        }
    }
}

#[cfg(test)]
pub(in crate::native) mod test_support;
#[cfg(test)]
mod tests;
