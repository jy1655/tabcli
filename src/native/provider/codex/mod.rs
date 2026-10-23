mod correlation;
mod diagnose;
mod hooks;
mod launch;
mod lifecycle;
pub(super) mod questions;
mod queue;
mod runner;
#[cfg(test)]
mod test_support;
mod transcript;
mod turn;
use self::correlation::{
    codex_input_correlates, codex_owned_string, codex_string, correlated_prompt,
    correlated_response, established_codex_thread,
};
use self::diagnose::diagnose_codex;
use self::launch::{codex_launch_arguments, send_initial_prompt, workspace_trust_override};
use self::queue::send_native_queue_message;
use super::{
    CrossSessionMessageContext, InitialPromptDisposition, LaunchContext, LaunchPlan,
    NativeProviderAdapter, ReopenSupport,
};
use crate::native::delivery::{
    self, Outcome, Prepared, Readiness, SurfaceInput, Transport, TransportFacts, TurnContext,
};
use crate::native::session::turn as session_turn;
use crate::native::session::{Reader, Store};
use crate::native::terminal;
use agent_bridge::{
    FirstPartyCli, provider_effort_args, provider_launch_args, provider_model_args,
};
use anyhow::{Context, Result, bail};
use semver::Version;
use std::{ffi::OsString, path::Path, time::Duration};

pub(super) static ADAPTER: CodexAdapter = CodexAdapter;

pub(super) struct CodexAdapter;

#[cfg(test)]
use self::turn::PENDING_TURN_FILE;
use self::turn::{PendingCodexTurn, cancel_pending_turn, read_pending_turn, write_pending_turn};

fn codex_version_supports_native_queue(output: &str) -> Result<bool> {
    Ok(codex_version(output)? >= Version::new(0, 149, 0))
}

fn codex_version(output: &str) -> Result<Version> {
    output
        .split_whitespace()
        .filter_map(|token| {
            let candidate = token
                .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .strip_prefix('v')
                .unwrap_or(
                    token.trim_matches(|character: char| !character.is_ascii_alphanumeric()),
                );
            Version::parse(candidate).ok()
        })
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not parse a semantic Codex version from {output:?}"))
}

// Reopen needs the thread-writer lock and queued-input gates plus a live resume-while-held check.
// See docs/verification/2026-10-12-reopen-source-notes.md.
const CODEX_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Codex reopen is not implemented in this slice; it requires the thread writer-lock and queued_items gates and a live check of resume-while-held behavior";

fn terminal_initial_prompt(directory: &Path, prompt: &str) -> Result<String> {
    let pending =
        read_pending_turn(directory)?.context("Codex initial turn correlation state is missing")?;
    Ok(correlated_prompt(prompt, &pending))
}

struct CodexInitialPaste;
impl Transport for CodexInitialPaste {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "terminal-paste-fallback",
            readiness: Readiness::AfterLaunch {
                // Cold-start MCP/extension redraws can ignore early Returns even
                // after the composer appears; keep the existing readiness delay.
                ready_delay: Duration::from_secs(12),
            },
            // Shift+Return preserves LF only in the default editable composer; replace with a native input API.
            // Custom bindings/modes are outside this fallback; see docs/verification/2026-10-12-windows-0.4.8.md.
            surface: Some(SurfaceInput {
                // Legacy not-injected route only; injected sends explicitly override
                // this with zero and own their single guarded Return (#124).
                submit_count: 2,
                newline: terminal::ConsoleNewline::ShiftReturn,
            }),
        }
    }
    fn prepare(&self, turn: &TurnContext<'_>) -> Result<Prepared> {
        // No timing/UI-independent literal Tab encoding is proven for Codex's
        // console composer. Keep this restriction out of the native queue path.
        if turn.surface.kind == terminal::TerminalKind::WindowsConsole && turn.prompt.contains('\t')
        {
            bail!(
                "Codex Windows console paste cannot preserve tabs; replace tab characters with spaces"
            );
        }
        Ok(Prepared {
            prompt: terminal_initial_prompt(turn.directory, turn.prompt)?,
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, _: &Prepared, file: Option<&Path>) -> Outcome {
        send_initial_prompt(
            turn.surface,
            file.expect("surface transport has a prompt file"),
            turn.deadline,
        )
        .into()
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

// The queue addresses the established thread; it never selects the active TUI thread.
struct CodexQueue;
impl Transport for CodexQueue {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "provider-native-queue",
            readiness: Readiness::AfterLaunch {
                ready_delay: Duration::ZERO,
            },
            surface: None,
        }
    }
    fn prepare(&self, _: &TurnContext<'_>) -> Result<Prepared> {
        Ok(Prepared {
            prompt: String::new(),
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, _: &Prepared, _: Option<&Path>) -> Outcome {
        send_native_queue_message(
            CrossSessionMessageContext {
                bridge_executable: turn.bridge_executable,
                directory: turn.directory,
                provider_path: turn.provider_path,
                correlation_id: turn.claim_token,
                prompt: turn.prompt,
                deadline: turn.deadline,
            },
            &mut crate::native::clock::SystemClock,
        )
        .map_or_else(|outcome| outcome, |()| Outcome::Sent)
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

const APPLIED_MARKER: &str = "codex-config-override";

impl NativeProviderAdapter for CodexAdapter {
    fn result_timeout_diagnostic(
        &self,
    ) -> Option<super::super::diagnostics::ResultTimeoutDiagnostic> {
        None
    }

    fn launch_trust(&self) -> crate::native::consent::LaunchTrust {
        crate::native::consent::LaunchTrust {
            answers_launch_dialog: false,
            bypass_suppresses_dialog: false,
            applied_marker: Some(APPLIED_MARKER),
            dialog_settle: std::time::Duration::ZERO,
        }
    }

    fn initial_transport(&self) -> Box<dyn Transport> {
        initial_transport_for_platform(cfg!(windows))
    }
    fn follow_up_transport(&self) -> Box<dyn Transport> {
        Box::new(CodexQueue)
    }

    fn cancel_support(&self, _directory: &Path) -> Result<super::CancelSupport> {
        Ok(super::CancelSupport::Unsupported(
            "in this release the Bridge integration for codex does not support cancel".to_owned(),
        ))
    }

    fn workspace_trust_key(&self, _screen: &str, _workspace: &Path) -> Option<terminal::DialogKey> {
        None
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        env: &super::super::consent::Environment,
        store: Option<&Path>,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let store = match store {
            Some(store) => store.to_path_buf(),
            None => Home::resolve(env)?.config,
        };
        let Some(text) = consent::read_store(&store)? else {
            return Ok(Trust::Absent);
        };
        let config: toml::Value = text.parse().context("invalid Codex trust config")?;
        let mut key = consent::native_key(workspace)?;
        if cfg!(windows) {
            key = key.to_lowercase();
        }
        let Some(projects) = config.get("projects") else {
            return Ok(Trust::Absent);
        };
        let projects = projects
            .as_table()
            .context("Codex projects is not a table")?;
        let entry = projects.get(&key).or_else(|| {
            if cfg!(windows) {
                projects
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == key)
                    .map(|(_, v)| v)
            } else {
                None
            }
        });
        let entry = entry
            .map(|v| {
                v.as_table()
                    .context("Codex project trust entry is not a table")
            })
            .transpose()?;
        match entry.and_then(|v| v.get("trust_level")) {
            None => Ok(Trust::Absent),
            Some(v) if v.as_str() == Some("trusted") => Ok(Trust::Trusted(Evidence {
                provider: "codex".into(),
                store,
                key,
            })),
            Some(v) if v.as_str() == Some("untrusted") => Ok(Trust::Declined),
            _ => bail!("unknown Codex trust level"),
        }
    }

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // Codex derives no session identity from the caller's environment.
        &[]
    }

    fn diagnose(
        &self,
        context: super::super::diagnostics::Context<'_>,
    ) -> Vec<super::super::diagnostics::Check> {
        diagnose_codex(context)
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        prepare_launch_for_platform(context, cfg!(windows), super::super::consent::authorized)
    }

    fn reopen(&self) -> Result<&'static dyn ReopenSupport> {
        bail!("{CODEX_REOPEN_UNSUPPORTED}")
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        match codex_string(payload, "hook_event_name") {
            Some("UserPromptSubmit") => {
                return turn::bind(&Store::open_unchecked(directory), payload);
            }
            Some("Stop") => return turn::stop(&Store::open_unchecked(directory), payload),
            Some(_) => return Ok(()),
            None => {}
        }
        let pending = match turn::notify(&Store::open_unchecked(directory), payload)? {
            turn::Route::Bound | turn::Route::Stale => return Ok(()),
            turn::Route::Unbound(pending) => pending,
        };
        let raw_message = codex_string(payload, "last-assistant-message")
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex notify payload has no assistant result"))?;
        let Some(pending) = pending else {
            return Ok(());
        };
        let message = match correlated_response(raw_message, &pending) {
            Ok(message) => message,
            Err(_) if codex_input_correlates(payload, &pending) => raw_message,
            Err(_) => return Ok(()),
        };
        let thread_id = codex_owned_string(payload, "thread-id");
        let established_thread = established_codex_thread(directory)?;
        if established_thread
            .as_deref()
            .is_some_and(|established| thread_id.as_deref() != Some(established))
        {
            return Ok(());
        }
        session_turn::Report::for_claim(
            &Store::open_unchecked(directory),
            FirstPartyCli::Codex,
            Some(&pending.claim_token),
        )
        .with_provenance(Some("codex_notify"))
        .complete(message, thread_id, codex_owned_string(payload, "turn-id"))
        .context("failed to record the correlated Codex result")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Codex does not expose Agent Bridge provider controls")
    }
}

// Codex's home: `config.toml` under `CODEX_HOME`, or under `.codex` in the user home. An
// empty `CODEX_HOME` keeps the relative spelling, `config.toml` in the caller's working
// directory, and the user home is required even when `CODEX_HOME` is set.
#[derive(Debug)]
struct Home {
    config: std::path::PathBuf,
}

impl Home {
    fn resolve(env: &crate::native::consent::Environment) -> Result<Self> {
        let home = crate::native::consent::user_home(env)?;
        Ok(Self {
            config: env
                .codex_home
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(home).join(".codex"))
                .join("config.toml"),
        })
    }

    // The directory in which Codex keeps its memories and runs its background memory
    // consolidation, beside its `config.toml`.
    fn memory_workspace(&self) -> Result<std::path::PathBuf> {
        Ok(self
            .config
            .parent()
            .context("Codex config path has no parent")?
            .join("memories"))
    }
}

// The Codex memory workspace of the caller's environment. Bridge's launch admission
// refuses that workspace by default (issue #216); the location is Codex's, so it is
// declared here.
pub(super) fn memory_workspace(
    env: &crate::native::consent::Environment,
) -> Result<std::path::PathBuf> {
    Home::resolve(env)?.memory_workspace()
}

fn initial_transport_for_platform(windows: bool) -> Box<dyn Transport> {
    if windows {
        Box::new(CodexInitialPaste)
    } else {
        Box::new(delivery::LAUNCH_ARGUMENT)
    }
}

// The complete launch command: the requested policy in Codex's flags (the bypass only when
// requested, then effort, then model), then Codex's own arguments with the workspace trust
// override at their start when consent authorizes it. `authorized` is the consent gate;
// production passes `consent::authorized`, and the applied marker is recorded right after
// a positive answer, before the plan exists.
fn prepare_launch_for_platform(
    context: LaunchContext<'_>,
    windows: bool,
    authorized: fn(&Path, FirstPartyCli) -> Result<bool>,
) -> Result<LaunchPlan> {
    let claim_token =
        session_turn::current_claim_token(&Reader::open_unchecked(context.directory))?
            .context("Codex launch has no native turn claim")?;
    let pending = install_pending_turn_for_prompt(context.directory, &claim_token, context.prompt)?;
    let mut arguments = provider_launch_args(FirstPartyCli::Codex, context.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = context.effort {
        arguments.extend(
            provider_effort_args(FirstPartyCli::Codex, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = context.model {
        arguments.extend(
            provider_model_args(FirstPartyCli::Codex, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    let mut own = codex_launch_arguments(
        context.bridge_executable,
        context.workspace,
        context.prompt,
        &pending,
        windows,
    )?;
    let hook_override = hooks::prepare(context.directory, context.bridge_executable)?;
    if let Some(value) = hook_override {
        // Immediately after the unchanged notify override, before a positional prompt.
        own.splice(2..2, [OsString::from("-c"), OsString::from(value)]);
    }
    if authorized(context.directory, FirstPartyCli::Codex)? {
        // Official process-local TOML override; never edits config.toml or any
        // approval/sandbox setting. Insert before the positional prompt.
        own.splice(
            0..0,
            [
                OsString::from("-c"),
                OsString::from(workspace_trust_override(context.workspace)?),
            ],
        );
        super::super::consent::applied(context.directory, APPLIED_MARKER)?;
    }
    arguments.extend(own);
    Ok(LaunchPlan {
        arguments,
        initial_prompt: InitialPromptDisposition::of(
            initial_transport_for_platform(windows).as_ref(),
        ),
        watcher: Box::new(transcript::Watcher),
        environment_removals: &[],
    })
}

// A record without prompt sizes, as a binary before them wrote it; the tests use it for
// that shape and for turns whose prompt is not under test.
#[cfg(test)]
fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingCodexTurn> {
    let pending = PendingCodexTurn::new(claim_token)?;
    write_pending_turn(directory, &pending)?;
    Ok(pending)
}

// The initial turn's record, written once with the sizes of the prompt as Codex receives it
// (the launch argument on macOS, the paste on Windows: both are `correlated_prompt`).
fn install_pending_turn_for_prompt(
    directory: &Path,
    claim_token: &str,
    prompt: &str,
) -> Result<PendingCodexTurn> {
    let mut pending = PendingCodexTurn::new(claim_token)?;
    let framed = correlated_prompt(prompt, &pending);
    pending.record_framed_prompt(&framed);
    write_pending_turn(directory, &pending)?;
    Ok(pending)
}

#[cfg(test)]
mod tests;
