//! One start operation; each source owns resolution, creation and provider-specific gates.
use super::args::{
    CommonStartOptions, OptionSet, ValidatedStartOptions, option_value, parse_common_start_option,
    set_once, validate_start_options,
};
use crate::native::failure_kind::DeliveryUncertain;
use crate::native::provider_installation::{check_provider_version_until, resolve_provider};
use crate::native::session::{Reader, SessionState, Store, launch, requests, turn, update_status};
use crate::native::turn_report::TurnReport;
use crate::native::{
    CreatedSession, DEFAULT_TIMEOUT_SECS, NativeCommand, SessionSpec, consent, context,
    create_session, delegation_prompt, delivery, sanitize_title, session_start, terminal, turn_run,
};
use agent_bridge::{FirstPartyCli, PUBLIC_COMMAND, checked_deadline_from};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

pub(super) mod admission;
pub(super) mod deadline;
mod provider_drift;

pub(super) type ResultExtra = serde_json::Map<String, serde_json::Value>;

pub(super) struct StartOptions {
    pub(super) timeout: Duration,
    pub(super) detach: bool,
    pub(super) json: bool,
}

// In production, only `resolve_at` constructs a Resolved.
pub(super) struct Resolved {
    // Attachments are applied, but delegation framing belongs to create.
    spec: SessionSpec,
    terminal_kind: terminal::TerminalKind,
    // The automatic fallback reason to log once the Session exists.
    herdr_refusal: Option<anyhow::Error>,
    // The initial-prompt Transport selected by post-probe surface admission, carried
    // straight into initial delivery so delivery cannot re-decide it independently.
    initial_transport: Box<dyn delivery::Transport>,
    context_sources: Vec<requests::ContextSource>,
    // Assessed by resolve, from the path its Resolution names (ask: the requested path,
    // aliases included; reopen: the canonical one), before Session creation; recorded
    // with the session once it exists. None records nothing.
    consent: Option<consent::Assessment>,
}

// Resolution and creation are completed before the shared Initial prompt operation.
// The source-specific value returned by create cannot exist without its Session.
impl Resolved {
    // A fixture that skips the resolution; tests reach the later phases through it.
    #[cfg(test)]
    pub(super) fn from_parts(
        spec: SessionSpec,
        terminal_kind: terminal::TerminalKind,
        context_sources: Vec<requests::ContextSource>,
        consent: Option<consent::Assessment>,
    ) -> Self {
        let initial_transport =
            crate::native::provider::admit_surface(spec.provider, terminal_kind, None)
                .expect("test fixture selects an admitted surface");
        Self {
            spec,
            terminal_kind,
            herdr_refusal: None,
            initial_transport,
            context_sources,
            consent,
        }
    }

    // Only the Windows launch-recovery fixtures adjust a resolved spec.
    #[cfg(all(test, windows))]
    pub(super) fn spec_mut(&mut self) -> &mut SessionSpec {
        &mut self.spec
    }

    pub(super) fn create_with<T>(
        self,
        create: impl FnOnce(SessionSpec, Option<&consent::Assessment>) -> Result<(CreatedSession, T)>,
    ) -> Result<(Prepared, T)> {
        let provider = self.spec.provider;
        let (created, source) = create(self.spec, self.consent.as_ref())?;
        if let Some(reason) = self.herdr_refusal {
            let reason = agent_bridge::terminal_safe_text(&format!("{reason:#}"), false);
            launch::log(
                &Store::open_unchecked(&created.directory),
                &format!("Herdr was not used for this launch: {reason}"),
            );
        }
        Ok((
            Prepared {
                created,
                provider,
                terminal_kind: self.terminal_kind,
                initial_transport: self.initial_transport,
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
    pub(super) initial_transport: Box<dyn delivery::Transport>,
    pub(super) context_sources: Vec<requests::ContextSource>,
}

pub(super) enum Failure<'a> {
    Preparing(anyhow::Error),
    Launch {
        claim: &'a mut turn::Claim,
        deadline: Instant,
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

    // A launch failure before the launcher fenced the Claim has no launcher settlement; one
    // after the fence keeps the launcher's own (the terminal failure handoff or the wait).
    fn failed(&self, created: &CreatedSession, failure: Failure<'_>) -> anyhow::Error {
        match failure {
            Failure::Preparing(error) => {
                mark_start_failed(created, &error);
                error
            }
            Failure::Launch { claim, error, .. } if !claim.launch_fenced() => {
                launch::settle_unfenced_start(
                    &Store::open_unchecked(&created.directory),
                    claim,
                    error,
                )
            }
            Failure::Launch { error, .. } | Failure::Delivery { error, .. } => error,
        }
    }
}

// Which path the consent assessment sees. `ask` passes the workspace as requested, aliases
// included, so an alias is recorded as one; `reopen` passes the canonical path, as it takes
// no path alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConsentPath {
    Requested,
    Canonical,
}

// How a start without `--title` names its session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DefaultTitle<'a> {
    // `{provider} · {workspace name}`
    Fresh,
    // `{provider} · {workspace name} (reopened {source})`
    Reopened { source: &'a str },
}

impl DefaultTitle<'_> {
    fn render(self, provider: FirstPartyCli, workspace_name: &str) -> String {
        match self {
            Self::Fresh => format!("{} · {workspace_name}", provider.as_str()),
            Self::Reopened { source } => format!(
                "{} · {workspace_name} (reopened {source})",
                provider.as_str()
            ),
        }
    }
}

// Inputs to shared resolution (`resolve_at`). Ask supplies its request, and reopen
// also uses the closed source.
pub(super) struct Resolution<'a> {
    pub(super) provider: FirstPartyCli,
    // The workspace as requested by ask or recorded in the reopen source manifest.
    pub(super) workspace: &'a Path,
    pub(super) consent_path: ConsentPath,
    pub(super) title: Option<String>,
    pub(super) default_title: DefaultTitle<'a>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) yolo: bool,
    pub(super) prompt: String,
    // Chosen by `choose_launch_surface` at the command's own place in its order, where 0.4.49
    // chose and checked it: `ask` after its context resolution, `reopen` before it reads the
    // source. The resolution decides an automatic Herdr choice with the provider
    // (`admit_choice`) and, when it is Herdr, with `caller`.
    pub(super) surface: terminal::Selection,
    // What the caller check found when the surface was chosen.
    pub(super) caller: CallerCheck,
    pub(super) context_sources: Vec<requests::ContextSource>,
    // Used by resolve_with (ask in production). Reopen checks its source workspace before recovery.
    pub(super) workspace_noun: &'static str,
    // How an admission refusal is reported: unchanged for `ask`, typed as reopen's
    // workspace-refused gate for a reopen.
    pub(super) admission_refusal: fn(anyhow::Error) -> anyhow::Error,
}

// The launchable spec of a start, after the command selected its terminal and checked its
// caller (`choose_launch_surface`), in 0.4.49's order: the Surface decision before the version
// (`choose_before_probe`), the canonical workspace directory, state root and launch admission,
// provider executable and version, version-aware surface admission (which decides an
// automatic Herdr choice that depends on the version, with the caller check's outcome), title,
// and workspace consent. The consent assessment may update shared records before a Session exists.
pub(super) fn resolve(resolution: Resolution<'_>, deadline: Instant) -> Result<Resolved> {
    resolve_with(resolution, |provider| probe_for_launch(provider, deadline))
}

pub(super) fn probe_for_launch(
    provider: FirstPartyCli,
    deadline: Instant,
) -> Result<(PathBuf, String)> {
    let provider_path = resolve_provider(provider)?;
    let provider_version = check_provider_version_until(provider, &provider_path, Some(deadline))?;
    Ok((provider_path, provider_version))
}

// `probe` is the resolution's one look at the installed CLI: its executable path and its
// supported version (`provider_installation::read_installed_version`, which doctor's report of
// the Surface reads too). Tests resolve without an installed provider through it.
pub(super) fn resolve_with(
    resolution: Resolution<'_>,
    probe: impl FnOnce(FirstPartyCli) -> Result<(PathBuf, String)>,
) -> Result<Resolved> {
    let _ = choose_before_probe(resolution.provider, resolution.surface)?;
    deadline::before("resolving the workspace")?;
    let workspace =
        LaunchWorkspace::read(resolution.workspace)?.directory(resolution.workspace_noun)?;
    let root = Reader::state_root().map_err(resolution.admission_refusal)?;
    check_launch_environment(&root)?;
    resolve_at(resolution, &root, workspace, probe)
}

// Reopen resolves its paths before it can repair an interrupted launch in the source's records.
pub(super) fn resolve_at(
    resolution: Resolution<'_>,
    root: &Path,
    workspace: PathBuf,
    probe: impl FnOnce(FirstPartyCli) -> Result<(PathBuf, String)>,
) -> Result<Resolved> {
    let Resolution {
        provider,
        workspace: requested,
        consent_path,
        title,
        default_title,
        model,
        effort,
        yolo,
        prompt,
        surface,
        caller,
        context_sources,
        workspace_noun: _,
        admission_refusal,
    } = resolution;
    deadline::before("checking launch admission")?;
    admission::check(root, &workspace).map_err(admission_refusal)?;
    let (provider_path, provider_version) = probe(provider)?;
    // Once the installed CLI version is known, the choice is made again with it, before
    // title, consent, session, or surface creation: a surface-specific version floor refuses
    // an explicit choice and gives an automatic Herdr choice its fallback. The admitted
    // Surface's initial Transport is kept for delivery.
    let Admitted {
        kind: terminal_kind,
        initial_transport,
        herdr_refusal,
    } = admit_choice(provider, surface, Some(&provider_version))?;
    // An automatic choice that ends on Herdr launches there only if its caller check passed.
    if terminal_kind == terminal::TerminalKind::Herdr
        && matches!(surface, terminal::Selection::Herdr { .. })
    {
        match caller {
            CallerCheck::Passed => {}
            CallerCheck::Failed(error) => return Err(error),
            CallerCheck::NotNeeded => {
                bail!("the automatic choice of Herdr was not checked against its caller")
            }
        }
    }
    let requested_title = title.unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        default_title.render(provider, workspace_name)
    });
    let title = sanitize_title(&requested_title)?;
    deadline::before("assessing workspace consent")?;
    let consent = consent::assess(
        root,
        match consent_path {
            ConsentPath::Requested => requested,
            ConsentPath::Canonical => &workspace,
        },
    )?;
    Ok(Resolved {
        spec: SessionSpec {
            provider,
            provider_path,
            provider_version,
            workspace,
            title,
            model,
            effort,
            yolo,
            prompt,
        },
        terminal_kind,
        initial_transport,
        context_sources,
        herdr_refusal,
        consent: Some(consent),
    })
}

/// Paths embedded in a launch command must survive conversion to text exactly.
pub(in crate::native) fn launch_path_text<'a>(path: &'a Path, field: &str) -> Result<&'a str> {
    path.to_str()
        .with_context(|| format!("{field} path is not Unicode text"))
}

// Reopen checks the spelling before recovering a prior launch, but reports an absent
// workspace only after judging the source, as it does for other workspace failures.
pub(super) struct LaunchWorkspace<'a> {
    requested: &'a Path,
    canonical: std::io::Result<PathBuf>,
}

impl<'a> LaunchWorkspace<'a> {
    pub(super) fn read(requested: &'a Path) -> Result<Self> {
        launch_path_text(requested, "workspace")?;
        let canonical = requested.canonicalize();
        if let Ok(path) = &canonical {
            launch_path_text(path, "workspace")?;
        }
        Ok(Self {
            requested,
            canonical,
        })
    }

    pub(super) fn directory(self, noun: &str) -> Result<PathBuf> {
        let path = self.canonical.with_context(|| {
            format!(
                "{noun} does not exist or cannot be resolved: {}",
                self.requested.display()
            )
        })?;
        if !path.is_dir() {
            bail!("{noun} is not a directory: {}", path.display());
        }
        Ok(path)
    }
}

pub(super) fn check_launch_environment(root: &Path) -> Result<()> {
    // Session creation expands a relative state root against the current directory.
    if root.is_absolute() {
        launch_path_text(root, "state root")?;
    } else {
        launch_path_text(&std::env::current_dir()?.join(root), "state root")?;
    }
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    launch_path_text(&executable, "TAB executable")?;
    Ok(())
}

fn resolve_fresh(request: &AskRequest, deadline: Instant) -> Result<Resolved> {
    // Resolve attachments before Session creation. Subsequent workspace consent assessment may
    // update shared records even if the start is refused.
    let attached = context::resolve(&request.context_results)?;
    let (surface, caller) = choose_launch_surface(request.terminal, Some(request.provider))?;
    resolve(
        fresh_resolution(request, attached, surface, caller),
        deadline,
    )
}

// What the caller check of an automatic Herdr choice found where `choose_launch_surface` ran
// it: the place where 0.4.49 ran it, before the workspace, the probe and, for a reopen, the
// source. The resolution uses it once the choice is decided (`resolve_at`).
pub(super) enum CallerCheck {
    // Nothing to prove: an explicit choice, a choice outside hc, or an automatic choice the
    // provider takes on its fallback at every version.
    NotNeeded,
    Passed,
    // A check that failed where the provider's version decides the choice (Codex off
    // Windows): an automatic choice that ends on Herdr is refused with it, and one that the
    // version gives its fallback launches there.
    Failed(anyhow::Error),
}

// The terminal a launch asked for or its caller's environment selects, the one entry of `ask`
// and `reopen`, each at its own place in its order (`Resolution::surface`), and the caller
// check of an automatic Herdr choice, run there as in 0.4.49. Selection reads the environment
// only. Every platform selects an explicit `--terminal` (`requested`) or refuses it here
// before creating a session if it does not run that kind. Without an explicit request, an hc
// terminal selects Herdr with the platform's usual Surface as its fallback; elsewhere the
// platform selects its usual Surface.
//
// An automatic Herdr choice proves that this process runs in the caller's Herdr terminal on
// the running core (`terminal::admit_selection`, the only step that opens a connection). A
// failed check refuses the launch here, without opening another terminal, where the choice is
// already known to be Herdr: for `ask`, a provider whose adapter's answer does not depend on
// the version (Claude, Agy, Pi); for `reopen`, which does not know its provider before it
// reads its source, always, as in 0.4.49 (every provider that supports reopen runs on Herdr,
// so no reopen that would take the fallback is refused). Where the version decides (Codex off
// Windows), the outcome is kept for `resolve_at` only while the request has time left.
// A provider that takes its fallback at every version is not checked. Ask runs
// `choose_before_probe` before workspace resolution. Reopen checks the source workspace's
// spelling before child recovery and reports other workspace errors after `choose_before_probe`.
// Both use `admit_choice` in `resolve_at` with the probed version.
pub(super) fn choose_launch_surface(
    requested: Option<terminal::TerminalKind>,
    provider: Option<FirstPartyCli>,
) -> Result<(terminal::Selection, CallerCheck)> {
    let selection = terminal::select_for(requested)?;
    let caller = check_caller(selection, provider, terminal::admit_selection)?;
    Ok((selection, caller))
}

// The caller check of `selection` for `provider` (`None`: not known yet), by the rule above.
// `verify` is the check itself (`terminal::admit_selection`); tests check without a Herdr Core
// through it.
pub(super) fn check_caller(
    selection: terminal::Selection,
    provider: Option<FirstPartyCli>,
    verify: impl FnOnce(terminal::Selection) -> Result<()>,
) -> Result<CallerCheck> {
    if !matches!(selection, terminal::Selection::Herdr { .. }) {
        return Ok(CallerCheck::NotNeeded);
    }
    let version_decides = match provider
        .map(|p| choose_before_probe(p, selection))
        .transpose()?
    {
        Some(EarlyChoice::Known(admitted)) if admitted.kind != terminal::TerminalKind::Herdr => {
            return Ok(CallerCheck::NotNeeded);
        }
        Some(EarlyChoice::ByVersion { .. }) => true,
        _ => false,
    };
    let outcome = verify(selection);
    deadline::before("using the Herdr caller check")?;
    match outcome {
        Ok(()) => Ok(CallerCheck::Passed),
        Err(error) if version_decides => Ok(CallerCheck::Failed(error)),
        Err(error) => Err(error),
    }
}

// The Surface a selection gives a provider, and the initial Transport its adapter selected
// there.
pub(super) struct Admitted {
    pub(super) kind: terminal::TerminalKind,
    pub(super) initial_transport: Box<dyn delivery::Transport>,
    // Why an automatic Herdr choice took its fallback (`kind`), when it did.
    pub(super) herdr_refusal: Option<anyhow::Error>,
}

// The provider's admission of a selection. `version` is `None` before the CLI version is
// probed and `Some` once it is known. A selected kind is admitted or refused as it is, so an
// explicit `--terminal herdr` keeps its refusal; an automatic Herdr choice is Herdr where the
// adapter admits it, and otherwise its fallback, admitted the same way.
pub(super) fn admit_choice(
    provider: FirstPartyCli,
    selection: terminal::Selection,
    version: Option<&str>,
) -> Result<Admitted> {
    use crate::native::provider::admit_surface;
    let (kind, herdr_refusal) = match selection {
        terminal::Selection::Kind(kind) => (kind, None),
        terminal::Selection::Herdr { fallback } => {
            match admit_surface(provider, terminal::TerminalKind::Herdr, version) {
                Ok(initial_transport) => {
                    return Ok(Admitted {
                        kind: terminal::TerminalKind::Herdr,
                        initial_transport,
                        herdr_refusal: None,
                    });
                }
                Err(refusal) => (fallback, Some(refusal)),
            }
        }
    };
    Ok(Admitted {
        kind,
        initial_transport: admit_surface(provider, kind, version)?,
        herdr_refusal,
    })
}

// The Surface decision before the provider's version is read, shared by `resolve` and doctor's
// report. A choice no version can change is `Known`, with its admission; an automatic Herdr
// choice whose adapter declares a version requirement (`surface_version_requirement`) is
// `ByVersion`: Herdr where the installed version meets `requirement`, and `fallback` otherwise,
// which `admit_choice` with the version decides. An explicit choice the adapter refuses here
// for every version is refused.
pub(super) enum EarlyChoice {
    Known(Admitted),
    ByVersion {
        fallback: terminal::TerminalKind,
        requirement: String,
    },
}

pub(super) fn choose_before_probe(
    provider: FirstPartyCli,
    selection: terminal::Selection,
) -> Result<EarlyChoice> {
    if let terminal::Selection::Herdr { fallback } = selection
        && crate::native::provider::admit_surface(provider, terminal::TerminalKind::Herdr, None)
            .is_ok()
        && let Some(requirement) = crate::native::provider::surface_version_requirement(
            provider,
            terminal::TerminalKind::Herdr,
        )
    {
        return Ok(EarlyChoice::ByVersion {
            fallback,
            requirement,
        });
    }
    Ok(EarlyChoice::Known(admit_choice(provider, selection, None)?))
}

// What `ask` would start `provider` on without `--terminal`: its selection and its decision
// before the version. `doctor` reports it, decides a `ByVersion` choice with the version it
// reads through the launch's own probe (`provider_installation::read_installed_version`) and
// `admit_choice`, and runs no caller check.
pub(super) fn automatic_surface(
    provider: FirstPartyCli,
) -> Result<(terminal::Selection, EarlyChoice)> {
    let selection = terminal::select_for(None)?;
    Ok((selection, choose_before_probe(provider, selection)?))
}

// A fresh start's resolution: consent sees the workspace as requested, the default title
// names provider and workspace, and the workspace errors and the admission refusal keep
// their own words.
pub(super) fn fresh_resolution(
    request: &AskRequest,
    attached: context::ResolvedContext,
    surface: terminal::Selection,
    caller: CallerCheck,
) -> Resolution<'_> {
    Resolution {
        provider: request.provider,
        workspace: &request.workspace,
        consent_path: ConsentPath::Requested,
        title: request.title.clone(),
        default_title: DefaultTitle::Fresh,
        model: request.model.clone(),
        effort: request.effort.clone(),
        yolo: request.yolo,
        prompt: attached.prompt_with_attachments(&request.prompt),
        surface,
        caller,
        context_sources: attached.sources,
        workspace_noun: "workspace",
        admission_refusal: std::convert::identity,
    }
}

// The initial prompt and consent are recorded before any source provenance binding.
pub(super) fn create_with_consent(
    mut spec: SessionSpec,
    assessment: Option<&consent::Assessment>,
) -> Result<CreatedSession> {
    spec.prompt = delegation_prompt(&spec.prompt);
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
                        deadline,
                        error,
                    },
                ));
            }
        };
        // Consent failure is neither a launcher failure nor a Delivery failure; whether it
        // settles the Request is `settle_consent_failure`'s decision.
        if let Err(error) = consent::complete_launch(
            &created.directory,
            provider,
            &terminal_session,
            &expected_claim_token,
            deadline,
        ) {
            let error = settle_consent_failure(&created.directory, &mut initial_claim, error);
            return Err(match version_note(created, provider) {
                Some(note) => error.context(note),
                None => error,
            });
        }
        initial_claim.startup_confirmed();
        let mut gate = |gate| source.gate(&created.directory, gate, deadline);
        turn_run::run_initial(
            report,
            initial_claim,
            &mut gate,
            prepared.initial_transport.as_ref(),
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
                    version_note(created, provider),
                )
            },
        )
    }
}

// A consent failure after a confirmed launch sends no prompt. The Request is settled as a
// delivery refusal (`delivery_not_sent`, with the consent text and its note on whether a
// dialog key may have been sent) only when the session is still `awaiting-initial-input`
// (the wrapper recorded that this provider's prompt is delivered after launch, and only
// this start's delivery could have sent it), the Claim is still current in an unclosed
// session, and the publication succeeds. Every other outcome is left as recorded for
// inspection: an argument-delivered prompt (`running`) keeps its fenced Claim because the
// provider has it; an ended or closed session, an unreadable status or a Claim that is no
// longer current records nothing; an interrupted publication leaves the steps it reached,
// and a journal it left is completed by the next lifecycle recovery.
pub(super) fn settle_consent_failure(
    directory: &Path,
    claim: &mut turn::Claim,
    error: anyhow::Error,
) -> anyhow::Error {
    let undelivered = Reader::open_unchecked(directory)
        .status()
        .is_ok_and(|status| status.state == SessionState::AwaitingInitialInput);
    if !undelivered {
        return error;
    }
    match claim.settle_delivery(turn::Delivery::NotSent(&error)) {
        Ok(()) => error,
        // An interrupted settlement can stop at any of its steps, before or after the
        // event, the status or the Claim release: the records, not this error, say which.
        Err(refusal) => error.context(format!(
            "the not-sent settlement did not complete and may or may not have recorded the refusal; read this request's recorded outcome with `{PUBLIC_COMMAND} result` or `{PUBLIC_COMMAND} inspect`: {refusal:#}"
        )),
    }
}

// What the records say about the provider's version when a start's prompt was not delivered;
// see `provider_drift`. The note is added to the returned error only, never to a record.
fn version_note(created: &CreatedSession, provider: FirstPartyCli) -> Option<String> {
    provider_drift::note(
        created.directory.parent()?,
        &created.id,
        provider,
        &created.manifest.provider_version,
    )
}

// The sentence ask and reopen add to every initial Delivery failure; an uncertain one
// marks it for the hint without changing its text or the source's error below it.
pub(super) fn initial_delivery_failure(
    failed: anyhow::Error,
    uncertain: bool,
    provider: FirstPartyCli,
    session: &str,
    version_note: Option<String>,
) -> anyhow::Error {
    let mut message = format!(
        "failed to deliver the initial prompt to {} session {session}",
        provider.as_str()
    );
    if let Some(note) = version_note {
        message.push_str(&format!(" ({note})"));
    }
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
        if parse_common_start_option(options, &mut index, &mut parsed, OptionSet::Start)? {
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
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    let ValidatedStartOptions {
        prompt,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout,
        detach,
        json,
    } = validate_start_options(parsed, "ask")?;
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
    let (prepared, ()) = deadline::during(start.deadline(), || {
        let resolved = resolve_fresh(&request, start.deadline())?;
        resolved.create_with(|spec, consent| {
            create_with_consent(spec, consent).map(|created| (created, ()))
        })
    })?;
    start.run(&prepared, &FreshSource, report)
}
