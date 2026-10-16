//! Follow-up input to a managed session.
use super::args::{option_value, parse_timeout, read_prompt_option, set_flag_once, set_once};
use super::*;
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::{Reader, Store, turn};
use agent_bridge::{FirstPartyCli, checked_deadline_from, validate_terminal_input};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct TellRequest {
    id: String,
    pub(super) prompt: String,
    pub(super) timeout: Duration,
    detach: bool,
    pub(super) json: bool,
    pub(super) context_results: Vec<context::ContextResultRef>,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args.split_first().context("tell requires one session id")?;
    require_valid_session_id(id)?;
    let mut prompt = None;
    let mut prompt_file = None;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut context_results = Vec::new();
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--prompt-file" => set_once(
                &mut prompt_file,
                PathBuf::from(option_value(options, &mut index, "--prompt-file")?),
                "--prompt-file",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
            option => bail!("unknown tell option: {option}"),
        }
        index += 1;
    }
    let prompt = read_prompt_option(prompt, prompt_file, "tell")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
    Ok(NativeCommand::Tell(TellRequest {
        id: id.to_owned(),
        prompt,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
        context_results,
    }))
}

pub(super) fn run(request: TellRequest) -> Result<()> {
    let mut report = TurnReport::new(
        request.json,
        serde_json::Map::from_iter([("session".into(), serde_json::json!(request.id))]),
    );
    let outcome = run_tell_inner(request, &mut report);
    let mut extra = serde_json::Map::new();
    if let Some(gate) = outcome.as_ref().err().and_then(reopen_refusal_gate) {
        extra.insert("gate".to_owned(), serde_json::json!(gate));
    }
    report.finish(outcome, extra)
}

fn run_tell_inner(request: TellRequest, report: &mut TurnReport) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    // Attached results are resolved and pinned before the target is recovered, repaired,
    // claimed, or sent to, so a failed resolution leaves every session unchanged.
    let attached = context::resolve(&request.context_results)?;
    let directory = Reader::session_directory(&request.id)?;
    session::hold::permit(&Reader::open_unchecked(&directory), &request.id)?;
    Store::open_unchecked(&directory).converge_until(deadline)?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let terminal_session: terminal::TerminalSession =
        Reader::open_unchecked(&directory).terminal()?;
    delivery::verify_terminal_surface_ownership_until(
        &directory,
        &request.id,
        &terminal_session,
        deadline,
        request.timeout,
    )?;
    let previous_state =
        session::hold::follow_up_admission(&Reader::open_unchecked(&directory), &request.id)?;
    let prompt = native_delegation_prompt(
        &delegation_source(),
        &attached.prompt_with_attachments(&request.prompt),
    );
    let resumed_from = read_resumed_from(&directory)?;
    let (mut claim, baseline) = turn::claim_ready(
        &Store::open_unchecked(&directory),
        &request.id,
        &attached.sources,
    )?;
    report.addressed(request.id.clone(), claim.receipt().request_id.clone());
    refuse_follow_up_to_shared_conversation(
        provider,
        &directory,
        &request.id,
        resumed_from.as_ref(),
        deadline,
        &mut claim,
        &previous_state,
    )?;
    turn_run::run(
        report,
        claim,
        delivery::Turn::FollowUp { prompt: &prompt },
        &turn_run::TurnRun {
            facts: turn_run::SessionFacts {
                id: &request.id,
                directory: &directory,
                provider,
                provider_path: &manifest.provider_path,
                surface: &terminal_session,
            },
            timing: turn_run::Timing {
                deadline,
                requested: request.timeout,
            },
            output: turn_run::Output {
                detach: request.detach,
                json: request.json,
            },
            extra: &serde_json::Map::new(),
            baseline,
        },
        |error| error,
    )
}
