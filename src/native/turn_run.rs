//! One Turn run from a held Claim to its Result for ask, tell and reopen: address the
//! report, deliver through the adapter's Transport, then detach or wait, and emit.
//! Callers keep everything before the run (admission, launch, consent, refusal gates)
//! and their own early report address; the run repeats that address idempotently.
use super::delivery::{self, Checks, DeliveryContext, RuntimeChecks, Transport, Turn};
use super::session_start::ResultExtra;
use crate::native::session::schema::SessionEvent;
use crate::native::session::{Store, requests, turn};
use crate::native::turn_report::TurnReport;
use crate::native::{envelope, provider, query, terminal};
use agent_bridge::{FirstPartyCli, PUBLIC_COMMAND, terminal_safe_text};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// The session the Turn is delivered to.
pub(super) struct SessionFacts<'a> {
    pub(super) id: &'a str,
    pub(super) directory: &'a Path,
    pub(super) provider: FirstPartyCli,
    pub(super) provider_path: &'a Path,
    pub(super) surface: &'a terminal::TerminalSession,
}

/// The command's budget: the absolute deadline and the timeout the user asked for.
#[derive(Clone, Copy)]
pub(super) struct Timing {
    pub(super) deadline: Instant,
    pub(super) requested: Duration,
}

#[derive(Clone, Copy)]
pub(super) struct Output {
    pub(super) detach: bool,
    pub(super) json: bool,
}

/// Everything a Turn run needs beside the Claim, the Turn and the report.
pub(super) struct TurnRun<'a> {
    pub(super) facts: SessionFacts<'a>,
    pub(super) timing: Timing,
    pub(super) output: Output,
    /// Fields added to the success rendering (reopen's `source_session`, `resumed_from`).
    pub(super) extra: &'a ResultExtra,
    /// Events already present when the Claim was taken; the wait looks past them.
    pub(super) baseline: usize,
}

/// Production run: the adapter's Transport for the Turn and the runtime checks.
pub(super) fn run(
    report: &mut TurnReport,
    claim: turn::Claim,
    turn: Turn<'_>,
    run: &TurnRun<'_>,
    on_delivery_error: impl FnOnce(anyhow::Error) -> anyhow::Error,
) -> Result<()> {
    let transport = match turn {
        Turn::Initial { .. } => provider::initial_transport(run.facts.provider),
        Turn::FollowUp { .. } => provider::follow_up_transport(run.facts.provider),
    };
    run_with(
        report,
        claim,
        turn,
        run,
        on_delivery_error,
        transport.as_ref(),
        &RuntimeChecks,
    )
}

/// In order: copy the Claim's token and Receipt; address the report; deliver through
/// `delivery::deliver_with`; on a Delivery error call `on_delivery_error` exactly once with
/// the unwrapped error and return its result (the caller adds its own context); on
/// success emit the accepted result (detach) or wait for the Claim-addressed Result and
/// emit it with the event. Wait and emission errors never reach `on_delivery_error`.
pub(super) fn run_with(
    report: &mut TurnReport,
    claim: turn::Claim,
    turn: Turn<'_>,
    run: &TurnRun<'_>,
    on_delivery_error: impl FnOnce(anyhow::Error) -> anyhow::Error,
    transport: &dyn Transport,
    checks: &dyn Checks,
) -> Result<()> {
    let TurnRun {
        facts,
        timing,
        output,
        extra,
        baseline,
    } = run;
    let claim_token = claim.token().to_owned();
    let receipt = claim.receipt().clone();
    report.addressed(facts.id.to_owned(), receipt.request_id.clone());
    let delivered = delivery::deliver_with(
        claim,
        turn,
        DeliveryContext {
            directory: facts.directory,
            session_id: facts.id,
            surface: facts.surface,
            provider_path: facts.provider_path,
            deadline: timing.deadline,
            requested: timing.requested,
        },
        transport,
        checks,
    );
    let expected_turn_id = match delivered {
        Ok(delivered) => delivered.correlation_id,
        Err(error) => return Err(on_delivery_error(error)),
    };

    if output.detach {
        return emit_session_result_with(
            output.json,
            facts.id,
            facts.surface,
            facts.provider,
            &receipt,
            None,
            extra,
        );
    }

    let event = turn::wait(
        &Store::open_unchecked(facts.directory),
        *baseline,
        expected_turn_id.as_deref(),
        Some(&claim_token),
        timing.deadline,
        timing.requested,
    )
    .with_context(|| {
        format!(
            "session {} in {} did not return a successful result; use `{PUBLIC_COMMAND} inspect {}` to inspect its recorded state",
            facts.id,
            facts.surface.kind.display_name(),
            facts.id
        )
    })?;
    emit_session_result_with(
        output.json,
        facts.id,
        facts.surface,
        facts.provider,
        &receipt,
        Some(&event),
        extra,
    )
}

pub(super) fn emit_session_result_with(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    receipt: &requests::Receipt,
    event: Option<&SessionEvent>,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let request_id = &receipt.request_id;
    let (elapsed, elapsed_reason) = query::request::observed_elapsed(Some(receipt), event);
    let mut value = envelope::ok(
        serde_json::json!({
                "session": id,
                "request_id": request_id,
                "context_sources": receipt.context_sources,
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
                "request_state": if event.is_some() { "completed" } else { "accepted" },
                "provider": provider.as_str(),
                "terminal": terminal_session.kind.as_str(),
                "terminal_session_id": terminal_session.id,
                "terminal_tab_id": terminal_session.tab_id,
                "terminal_window_id": terminal_session.window_id,
                "iterm_session_id": (terminal_session.kind == terminal::TerminalKind::Iterm2)
                    .then_some(terminal_session.id.as_str()),
                "result": event.map(|value| value.message.as_str()),
                "provider_session_id": event.and_then(|value| value.provider_session_id.as_deref()),
                "turn_id": event.and_then(|value| value.turn_id.as_deref()),
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    for (key, field) in extra {
        value[key.as_str()] = field.clone();
    }
    envelope::emit(json, &value, || {
        println!("session: {id}\nrequest: {request_id}");
        for (key, field) in extra {
            if let Some(field) = field.as_str() {
                println!("{}: {}", key, terminal_safe_text(field, false));
            }
        }
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
