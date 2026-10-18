//! One start operation; each source owns resolution, creation and provider-specific gates.
use super::args::{
    CommonStartOptions, option_value, parse_common_start_option, read_prompt_option, set_once,
};
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

pub(super) trait StartSource {
    fn resolve(&mut self, deadline: Instant) -> Result<Resolved>;
    // Creates the session and records the assessment with it (`record_consent`) before
    // anything else binds to the session.
    fn create(
        &mut self,
        spec: SessionSpec,
        consent: Option<&consent::Assessment>,
    ) -> Result<CreatedSession>;
    fn gate(&self, directory: &Path, gate: delivery::Gate, deadline: Instant) -> Result<()>;
    // A failure between create and the launch, when the session exists but no provider
    // process does: the source settles what it reserved for the session (`mark_start_failed`
    // at least) and returns the error to report.
    fn start_failed(&self, created: &CreatedSession, error: anyhow::Error) -> anyhow::Error;
    fn launch_failed(
        &self,
        created: &CreatedSession,
        initial_claim: &mut turn::Claim,
        error: anyhow::Error,
    ) -> anyhow::Error;
    fn delivery_failed(
        &self,
        directory: &Path,
        id: &str,
        error: anyhow::Error,
        deadline: Instant,
    ) -> anyhow::Error;
    fn result_extra(&self) -> Result<ResultExtra>;
}

pub(super) struct FreshSource(pub(super) AskRequest);

impl StartSource for FreshSource {
    fn resolve(&mut self, deadline: Instant) -> Result<Resolved> {
        let request = &self.0;
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

    fn create(
        &mut self,
        mut spec: SessionSpec,
        consent: Option<&consent::Assessment>,
    ) -> Result<CreatedSession> {
        spec.prompt = native_delegation_prompt(&delegation_source(), &spec.prompt);
        let created = create_session(spec)?;
        record_consent(&created, consent)?;
        Ok(created)
    }

    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        Ok(())
    }
    fn start_failed(&self, created: &CreatedSession, error: anyhow::Error) -> anyhow::Error {
        mark_start_failed(created, &error);
        error
    }
    fn delivery_failed(
        &self,
        _: &Path,
        _: &str,
        error: anyhow::Error,
        _: Instant,
    ) -> anyhow::Error {
        error
    }
    fn launch_failed(
        &self,
        _: &CreatedSession,
        _: &mut turn::Claim,
        error: anyhow::Error,
    ) -> anyhow::Error {
        error
    }
    fn result_extra(&self) -> Result<ResultExtra> {
        Ok(ResultExtra::new())
    }
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

// Single-use protocol: resolve (with the consent assessment), create (which records it),
// extra and claim (a failure there is `start_failed`), then launch.
pub(super) fn start(
    options: StartOptions,
    source: &mut dyn StartSource,
    report: &mut TurnReport,
) -> Result<()> {
    let StartOptions {
        timeout,
        detach,
        json,
    } = options;
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    let Resolved {
        spec,
        terminal_kind,
        context_sources,
        consent: assessment,
    } = source.resolve(deadline)?;
    let provider = spec.provider;
    let created = source.create(spec, assessment.as_ref())?;
    // Between create and the launch the session exists and no provider process does. A
    // failure here is settled by the source: the session is marked failed, and a reopen
    // records a launch-phase refusal that releases the source marker (#245).
    let (result_extra, mut initial_claim) = match source.result_extra().and_then(|extra| {
        turn::claim(&Store::open_unchecked(&created.directory), &context_sources)
            .map(|claim| (extra, claim))
    }) {
        Ok(prepared) => prepared,
        Err(error) => return Err(source.start_failed(&created, error)),
    };
    let expected_claim_token = initial_claim.token().to_owned();
    let receipt = initial_claim.receipt().clone();
    report.addressed(created.id.clone(), receipt.request_id.clone());
    let terminal_session = match launch::launcher::start(
        &created,
        terminal_kind,
        &mut initial_claim,
        &expected_claim_token,
        deadline,
    ) {
        Ok(surface) => surface,
        Err(error) => return Err(source.launch_failed(&created, &mut initial_claim, error)),
    };
    // A trust response is separate from model input. Only verified consent and an
    // adapter-recognized exact workspace dialog may produce one guarded response.
    consent::complete_launch(
        &created.directory,
        provider,
        &terminal_session,
        &expected_claim_token,
        deadline,
    )?;
    // The existing delivery paths own rollback/uncertainty after confirmed startup.
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
                requested: timeout,
            },
            output: turn_run::Output { detach, json },
            extra: &result_extra,
            baseline: 0,
        },
        |error| {
            let error = source.delivery_failed(&created.directory, &created.id, error, deadline);
            error.context(format!(
                "failed to deliver the initial prompt to {} session {}",
                provider.as_str(),
                created.id
            ))
        },
    )
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
            "--context-result" => context::push_option(
                &mut context_results,
                option_value(options, &mut index, "--context-result")?,
            )?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
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
    session_start::start(options, &mut session_start::FreshSource(request), report)
}
