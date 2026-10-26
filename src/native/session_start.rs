//! One start operation; each source owns resolution, creation and provider-specific gates.
use super::args::{
    CommonStartOptions, option_value, parse_common_start_option, read_prompt_option, set_once,
};
use crate::native::failure_kind::DeliveryUncertain;
use crate::native::session::launch::wrapper::{check_provider_version_until, resolve_provider};
use crate::native::session::primitives::delegation_source;
use crate::native::session::{Reader, SessionState, Store, launch, requests, turn, update_status};
use crate::native::turn_report::TurnReport;
use crate::native::{
    CreatedSession, DEFAULT_TIMEOUT_SECS, NativeCommand, SessionSpec, consent, context,
    create_session, delivery, native_delegation_prompt, sanitize_title, session_start, terminal,
    turn_run,
};
use agent_bridge::{FirstPartyCli, checked_deadline_from, validate_terminal_input};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

pub(super) mod admission;

pub(super) type ResultExtra = serde_json::Map<String, serde_json::Value>;

pub(super) struct StartOptions {
    pub(super) timeout: Duration,
    pub(super) detach: bool,
    pub(super) json: bool,
}

pub(super) struct Resolved {
    // Attachments are applied, but delegation framing belongs to create.
    pub(super) spec: SessionSpec,
    pub(super) terminal_kind: terminal::TerminalKind,
    pub(super) context_sources: Vec<requests::ContextSource>,
    // Assessed by resolve, from the original requested path including aliases, before anything
    // is created; recorded with the session once it exists. None records nothing.
    pub(super) consent: Option<consent::Assessment>,
}

// Resolution and creation are completed before the shared Initial prompt operation.
// The source-specific value returned by create cannot exist without its Session.
impl Resolved {
    pub(super) fn create_with<T>(
        self,
        create: impl FnOnce(SessionSpec, Option<&consent::Assessment>) -> Result<(CreatedSession, T)>,
    ) -> Result<(Prepared, T)> {
        let provider = self.spec.provider;
        let (created, source) = create(self.spec, self.consent.as_ref())?;
        Ok((
            Prepared {
                created,
                provider,
                terminal_kind: self.terminal_kind,
                context_sources: self.context_sources,
            },
            source,
        ))
    }
}

pub(super) struct Prepared {
    pub(super) created: CreatedSession,
    pub(super) provider: FirstPartyCli,
    pub(super) terminal_kind: terminal::TerminalKind,
    pub(super) context_sources: Vec<requests::ContextSource>,
}

pub(super) enum Failure<'a> {
    Preparing(anyhow::Error),
    Launch {
        claim: &'a mut turn::Claim,
        error: anyhow::Error,
    },
    Delivery {
        error: anyhow::Error,
        deadline: Instant,
    },
}

// The real variation after creation: result fields, holder gates, and the treatment
// of a failed phase. Resolution, creation, and final settlement belong to the source.
pub(super) trait InitialSource {
    fn result_extra(&self) -> Result<ResultExtra>;
    fn gate(&self, directory: &Path, gate: delivery::Gate, deadline: Instant) -> Result<()>;
    fn failed(&self, created: &CreatedSession, failure: Failure<'_>) -> anyhow::Error;
}

pub(super) struct FreshSource;

impl InitialSource for FreshSource {
    fn result_extra(&self) -> Result<ResultExtra> {
        Ok(ResultExtra::new())
    }

    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        Ok(())
    }

    fn failed(&self, created: &CreatedSession, failure: Failure<'_>) -> anyhow::Error {
        match failure {
            Failure::Preparing(error) => {
                mark_start_failed(created, &error);
                error
            }
            Failure::Launch { error, .. } | Failure::Delivery { error, .. } => error,
        }
    }
}

fn resolve_fresh(request: &AskRequest, deadline: Instant) -> Result<Resolved> {
    // Attached results are resolved and pinned before any session, claim, receipt, terminal,
    // or delivery exists, so a failed resolution changes nothing.
    let attached = context::resolve(&request.context_results)?;
    let terminal_kind = terminal::select(request.terminal)?;
    let workspace = request.workspace.canonicalize().with_context(|| {
        format!(
            "workspace does not exist or cannot be resolved: {}",
            request.workspace.display()
        )
    })?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let root = Reader::state_root()?;
    admission::check(&root, &workspace)?;
    let provider_path = resolve_provider(request.provider)?;
    let provider_version =
        check_provider_version_until(request.provider, &provider_path, Some(deadline))?;
    let requested_title = request.title.clone().unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        format!("{} · {workspace_name}", request.provider.as_str())
    });
    let title = sanitize_title(&requested_title)?;
    let consent = consent::assess(&root, &request.workspace)?;
    Ok(Resolved {
        spec: SessionSpec {
            provider: request.provider,
            provider_path,
            provider_version,
            workspace,
            title,
            model: request.model.clone(),
            effort: request.effort.clone(),
            yolo: request.yolo,
            prompt: attached.prompt_with_attachments(&request.prompt),
        },
        terminal_kind,
        context_sources: attached.sources,
        consent: Some(consent),
    })
}

// Delegation framing and consent recording precede any source provenance binding.
pub(super) fn create_with_consent(
    mut spec: SessionSpec,
    assessment: Option<&consent::Assessment>,
) -> Result<CreatedSession> {
    spec.prompt = native_delegation_prompt(&delegation_source(), &spec.prompt);
    let created = create_session(spec)?;
    record_consent(&created, assessment)?;
    Ok(created)
}

// Marks a session that failed between its creation and its launch, so that nothing takes
// it for a launchable one. No provider process exists at that point.
pub(super) fn mark_start_failed(created: &CreatedSession, error: &anyhow::Error) {
    let _ = update_status(
        &created.directory,
        SessionState::Failed,
        None,
        Some(format!("{error:#}")),
    );
}

// Record the assessment with the session that create made, before anything else binds to
// that session: a reopen finalizes its source marker only after its create has returned, so
// a record that cannot be written releases the source like any creation failure. The session
// is marked failed, so that nothing takes it for a launchable one.
pub(super) fn record_consent(
    created: &CreatedSession,
    consent: Option<&consent::Assessment>,
) -> Result<()> {
    let Some(assessment) = consent else {
        return Ok(());
    };
    consent::record(&created.directory, assessment).map_err(|error| {
        let _ = update_status(
            &created.directory,
            SessionState::Failed,
            None,
            Some(format!("{error:#}")),
        );
        error
    })
}

// The deadline includes resolution and creation. Once a Session exists, run owns the
// Claim, address, launch, consent, and Initial prompt ordering through one operation.
pub(super) struct Start {
    options: StartOptions,
    deadline: Instant,
}

impl Start {
    pub(super) fn new(options: StartOptions) -> Result<Self> {
        let deadline = checked_deadline_from(Instant::now(), options.timeout)?;
        Ok(Self { options, deadline })
    }

    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(super) fn run(
        &self,
        prepared: &Prepared,
        source: &dyn InitialSource,
        report: &mut TurnReport,
    ) -> Result<()> {
        let created = &prepared.created;
        let provider = prepared.provider;
        let deadline = self.deadline;
        let (result_extra, mut initial_claim) = match source.result_extra().and_then(|extra| {
            turn::claim(
                &Store::open_unchecked(&created.directory),
                &prepared.context_sources,
            )
            .map(|claim| (extra, claim))
        }) {
            Ok(prepared) => prepared,
            Err(error) => return Err(source.failed(created, Failure::Preparing(error))),
        };
        let expected_claim_token = initial_claim.token().to_owned();
        let receipt = initial_claim.receipt().clone();
        report.addressed(created.id.clone(), receipt.request_id.clone());
        let terminal_session = match launch::launcher::start(
            created,
            prepared.terminal_kind,
            &mut initial_claim,
            &expected_claim_token,
            deadline,
        ) {
            Ok(surface) => surface,
            Err(error) => {
                return Err(source.failed(
                    created,
                    Failure::Launch {
                        claim: &mut initial_claim,
                        error,
                    },
                ));
            }
        };
        // Consent failure is neither a launcher failure nor a Delivery failure.
        consent::complete_launch(
            &created.directory,
            provider,
            &terminal_session,
            &expected_claim_token,
            deadline,
        )?;
        initial_claim.startup_confirmed();
        let mut gate = |gate| source.gate(&created.directory, gate, deadline);
        turn_run::run(
            report,
            initial_claim,
            delivery::Turn::Initial { gate: &mut gate },
            &turn_run::TurnRun {
                facts: turn_run::SessionFacts {
                    id: &created.id,
                    directory: &created.directory,
                    provider,
                    provider_path: &created.manifest.provider_path,
                    surface: &terminal_session,
                },
                timing: turn_run::Timing {
                    deadline,
                    requested: self.options.timeout,
                },
                output: turn_run::Output {
                    detach: self.options.detach,
                    json: self.options.json,
                },
                extra: &result_extra,
                baseline: 0,
            },
            |undelivered| {
                let delivery::Undelivered { error, uncertain } = undelivered;
                initial_delivery_failure(
                    source.failed(created, Failure::Delivery { error, deadline }),
                    uncertain,
                    provider,
                    &created.id,
                )
            },
        )
    }
}

// The sentence ask and reopen add to every initial Delivery failure; an uncertain one
// marks it for the hint without changing its text or the source's error below it.
pub(super) fn initial_delivery_failure(
    failed: anyhow::Error,
    uncertain: bool,
    provider: FirstPartyCli,
    session: &str,
) -> anyhow::Error {
    let message = format!(
        "failed to deliver the initial prompt to {} session {session}",
        provider.as_str()
    );
    if uncertain {
        failed.context(DeliveryUncertain::new(message))
    } else {
        failed.context(message)
    }
}

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(crate) struct AskRequest {
    pub(crate) provider: FirstPartyCli,
    pub(crate) workspace: PathBuf,
    pub(crate) prompt: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) terminal: Option<terminal::TerminalKind>,
    pub(crate) yolo: bool,
    pub(crate) timeout: Duration,
    pub(crate) detach: bool,
    pub(crate) json: bool,
    pub(crate) context_results: Vec<context::ContextResultRef>,
}

pub(super) fn parse_ask(args: &[String]) -> Result<NativeCommand> {
    let (provider, options) = args
        .split_first()
        .context("ask requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    let mut workspace = None;
    let mut parsed = CommonStartOptions::default();
    let mut context_results = Vec::new();
    let mut context_mode = None;
    let mut index = 0;
    while index < options.len() {
        if parse_common_start_option(options, &mut index, &mut parsed)? {
            continue;
        }
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--context-mode" => set_once(
                &mut context_mode,
                context::parse_mode(option_value(options, &mut index, "--context-mode")?)?,
                "--context-mode",
            )?,
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
    context::apply_mode(&mut context_results, context_mode)?;
    let CommonStartOptions {
        prompt,
        prompt_file,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout,
        detach,
        json,
    } = parsed;
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    let prompt = read_prompt_option(prompt, prompt_file, "ask")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
    if model.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--model cannot be empty");
    }
    if effort.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--effort cannot be empty");
    }
    Ok(NativeCommand::Ask(AskRequest {
        provider,
        workspace,
        prompt,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
        context_results,
    }))
}

pub(super) fn run_ask(request: AskRequest) -> Result<()> {
    let mut report = TurnReport::new(request.json, serde_json::Map::new());
    let outcome = run_ask_inner(request, &mut report);
    report.finish(outcome, serde_json::Map::new())
}

fn run_ask_inner(request: AskRequest, report: &mut TurnReport) -> Result<()> {
    let options = session_start::StartOptions {
        timeout: request.timeout,
        detach: request.detach,
        json: request.json,
    };
    let start = Start::new(options)?;
    let resolved = resolve_fresh(&request, start.deadline())?;
    let (prepared, ()) = resolved.create_with(|spec, consent| {
        create_with_consent(spec, consent).map(|created| (created, ()))
    })?;
    start.run(&prepared, &FreshSource, report)
}
