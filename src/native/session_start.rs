//! One start operation; each source owns resolution, creation and provider-specific gates.
use super::turn_report::emit_session_result_with;
use super::*;

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
    // Preserve the original requested path, including aliases. None skips assessment.
    pub(super) assess_consent: Option<PathBuf>,
}

pub(super) trait StartSource {
    fn resolve(&mut self, deadline: Instant) -> Result<Resolved>;
    fn create(&mut self, spec: SessionSpec) -> Result<CreatedSession>;
    fn gate(&self, directory: &Path, gate: delivery::Gate, deadline: Instant) -> Result<()>;
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
            assess_consent: Some(request.workspace.clone()),
        })
    }

    fn create(&mut self, mut spec: SessionSpec) -> Result<CreatedSession> {
        spec.prompt = native_delegation_prompt(&delegation_source(), &spec.prompt);
        create_session(spec)
    }

    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        Ok(())
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
    fn result_extra(&self) -> Result<ResultExtra> {
        Ok(ResultExtra::new())
    }
}

// Single-use protocol: resolve, create, optional assessment, extra, then claim/launch.
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
        assess_consent,
    } = source.resolve(deadline)?;
    let provider = spec.provider;
    let created = source.create(spec)?;
    if let Some(requested) = assess_consent {
        consent::prepare(&created.directory, &requested)?;
    }
    let result_extra = source.result_extra()?;
    let mut initial_claim =
        turn::claim(&Store::open_unchecked(&created.directory), &context_sources)?;
    let expected_claim_token = initial_claim.token().to_owned();
    let receipt = initial_claim.receipt().clone();
    report.addressed(created.id.clone(), receipt.request_id.clone());
    let launch_deadline = launch::begin(
        &Store::open_unchecked(&created.directory),
        &expected_claim_token,
        deadline,
    )?;
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    let bridge_command = bridge_shell_command(
        &created.manifest.workspace,
        created
            .directory
            .parent()
            .context("session directory has no state root")?,
        &executable,
        &created.id,
    )?;
    let bridge_command =
        launch::install_script(&Store::open_unchecked(&created.directory), &bridge_command)?;
    initial_claim.fence_launch();
    let terminal_session = match terminal::open_bound_tab(
        terminal_kind,
        &bridge_command,
        &created.directory,
        launch_deadline,
        |session| {
            session.managed_session_id = Some(created.id.clone());
            Store::open_unchecked(&created.directory).write_terminal(session)
        },
        || {
            Store::open_unchecked(&created.directory)
                .record(CoreRecord::Terminal)
                .remove()
        },
    ) {
        Ok(session) => session,
        Err(error) => {
            let error = launch::terminal_failure(&Store::open_unchecked(&created.directory), error);
            return Err(error).with_context(|| {
                format!(
                    "failed to open {} surface for session {}",
                    terminal_kind.display_name(),
                    created.id
                )
            });
        }
    };
    launch::wait(
        &Store::open_unchecked(&created.directory),
        &terminal_session,
        launch_deadline,
    )?;
    // A trust response is separate from model input. Only verified consent and an
    // adapter-recognized exact workspace dialog may produce one guarded response.
    consent::complete_launch(&created.directory, provider, &terminal_session, deadline)?;
    // The existing delivery paths own rollback/uncertainty after confirmed startup.
    initial_claim.startup_confirmed();
    let mut gate = |gate| source.gate(&created.directory, gate, deadline);
    let delivered = delivery::deliver(
        initial_claim,
        delivery::Turn::Initial { gate: &mut gate },
        delivery::DeliveryContext {
            provider,
            directory: &created.directory,
            session_id: &created.id,
            surface: &terminal_session,
            provider_path: &created.manifest.provider_path,
            deadline,
            requested: timeout,
        },
    );
    let expected_turn_id = match delivered {
        Ok(delivered) => delivered.correlation_id,
        Err(error) => {
            let error = source.delivery_failed(&created.directory, &created.id, error, deadline);
            return Err(error).with_context(|| {
                format!(
                    "failed to deliver the initial prompt to {} session {}",
                    provider.as_str(),
                    created.id
                )
            });
        }
    };

    if detach {
        return emit_session_result_with(
            json,
            &created.id,
            &terminal_session,
            provider,
            &receipt,
            None,
            &result_extra,
        );
    }

    let event = turn::wait(
        &Store::open_unchecked(&created.directory),
        0,
        expected_turn_id.as_deref(),
        Some(&expected_claim_token),
        deadline,
        timeout,
    )
    .with_context(|| {
        format!(
            "session {} in {} did not return a successful result; use `{PUBLIC_COMMAND} inspect {}` to inspect its recorded state",
            created.id,
            terminal_session.kind.display_name(),
            created.id
        )
    })?;
    emit_session_result_with(
        json,
        &created.id,
        &terminal_session,
        provider,
        &receipt,
        Some(&event),
        &result_extra,
    )
}

#[cfg(test)]
mod tests;
