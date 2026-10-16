use super::OwnedSurfaceClose;
use super::ownership::verified_macos_terminal_owner;
use super::process;
use super::process::live_native_process_identity;
use super::{MacTerminalAdapter, MacosOpenMode};
use crate::native::private_to_user::owned_and_not_writable_by_others;
use crate::native::session::close::budget::close_remaining;
use crate::native::session::{self, Store};
use crate::native::session::{Reader, RecordReader, RecordStore};
use crate::native::terminal;
use crate::native::terminal::ownership;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
#[cfg(unix)]
use crate::native::terminal::shell_quote;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

#[cfg(test)]
use serde_json::Value;

use super::{CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession};

const HOST_COMMAND: &str = TerminalKind::Warp.registration().host_command;

const CONTROL_FILE: &str = "warp-control.json";
const HOST_PLAN_FILE: &str = "warp-host-plan.json";
const HOST_OFFER_FILE: &str = "warp-host-offer.json";
const HOST_DECISION_FILE: &str = "warp-host-decision.json";
const RECORD_SCHEMA: u32 = 1;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_CONTROL_OUTPUT: usize = 1024 * 1024;
// Names the adapter gives its exclusive records and its host directory in diagnostics; the
// session module publishes the bytes and knows nothing about Warp.
const EXCLUSIVE_RECORD_LABEL: &str = "Warp";
const EXCLUSIVE_TEMPORARY_PREFIX: &str = ".agent-bridge-warp-";
const HOST_SESSION_DIRECTORY_LABEL: &str = "Warp host session";
const TITLE_PROOF_ATTEMPTS: u32 = 3;

const REQUIRED_ACTIONS: &[&str] = &[
    "instance.inspect",
    "window.list",
    "window.inspect",
    "tab.list",
    "tab.inspect",
    "tab.rename",
    "tab.close",
];

#[derive(Clone, Copy)]
struct BundleSpec {
    bundle_name: &'static str,
    app_id: &'static str,
    channel: &'static str,
    scheme: &'static str,
    wrapper: &'static str,
    embedded: &'static str,
    config_home: &'static str,
}

const BUNDLE_SPECS: &[BundleSpec] = &[
    BundleSpec {
        bundle_name: "Warp.app",
        app_id: "dev.warp.Warp-Stable",
        channel: "stable",
        scheme: "warp",
        wrapper: "warpctrl",
        embedded: "stable",
        config_home: ".warp",
    },
    BundleSpec {
        bundle_name: "WarpPreview.app",
        app_id: "dev.warp.Warp-Preview",
        channel: "preview",
        scheme: "warppreview",
        wrapper: "warpctrl-preview",
        embedded: "preview",
        config_home: ".warp-preview",
    },
    BundleSpec {
        bundle_name: "WarpDev.app",
        app_id: "dev.warp.Warp-Dev",
        channel: "dev",
        scheme: "warpdev",
        wrapper: "warpctrl-dev",
        embedded: "dev",
        config_home: ".warp-dev",
    },
    BundleSpec {
        bundle_name: "WarpLocal.app",
        app_id: "dev.warp.Warp-Local",
        channel: "local",
        scheme: "warplocal",
        wrapper: "warpctrl-local",
        embedded: "warp",
        config_home: ".warp-local",
    },
    BundleSpec {
        bundle_name: "WarpOss.app",
        app_id: "dev.warp.WarpOss",
        channel: "warp-oss",
        scheme: "warposs",
        wrapper: "warpctrl-oss",
        embedded: "warp-oss",
        config_home: ".warp-oss",
    },
];

#[derive(Clone, Debug)]
struct ControlClient {
    bundle: PathBuf,
    executable: PathBuf,
    inject_warpctrl: bool,
    app_id: String,
    channel: String,
    scheme: String,
    config_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ControlBinding {
    schema: u32,
    attempt: String,
    bundle: PathBuf,
    executable: PathBuf,
    inject_warpctrl: bool,
    app_id: String,
    channel: String,
    scheme: String,
    instance_id: String,
    pid: u32,
    protocol_version: u32,
    #[serde(default)]
    process_birth: Option<(u64, u64)>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HostPlan {
    schema: u32,
    attempt: String,
    command: String,
    deadline_unix_ms: u64,
    decision_timeout_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct HostOffer {
    schema: u32,
    attempt: String,
    directory: PathBuf,
    pid: u32,
    tty: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostAction {
    Start,
    Abort,
}

#[derive(Debug, Deserialize, Serialize)]
struct HostDecision {
    schema: u32,
    attempt: String,
    action: HostAction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct InstanceSummary {
    instance_id: String,
    pid: u32,
    channel: String,
    app_id: String,
    protocol_version: u32,
}

#[derive(Debug, Deserialize)]
struct InstanceList {
    instances: Vec<InstanceSummary>,
}

#[derive(Debug, Deserialize)]
struct InstanceInspect {
    instance_id: String,
    pid: u32,
    channel: String,
    app_id: String,
    protocol_version: u32,
    actions: Vec<ActionDescription>,
}

#[derive(Debug, Deserialize)]
struct ActionDescription {
    name: String,
}

#[derive(Debug, Deserialize)]
struct WindowList {
    windows: Vec<WindowDescription>,
}

#[derive(Debug, Deserialize)]
struct WindowDescription {
    window_id: String,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabDescription>,
}

#[derive(Debug, Deserialize)]
struct TabDescription {
    tab_id: String,
    window_id: String,
}

#[derive(Debug, Deserialize)]
struct RenameResponse {
    action: String,
    ok: bool,
    instance_id: String,
    window_id: String,
    tab_id: String,
}

#[derive(Debug, Deserialize)]
struct OkResponse {
    action: String,
    ok: bool,
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct TabInspectResponse {
    action: String,
    tab: TabDescription,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct WindowInspectResponse {
    action: String,
    window: WindowDescription,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ControlErrorBody,
}

#[derive(Debug, Deserialize)]
struct ControlErrorBody {
    code: String,
    message: String,
}

#[derive(Debug)]
struct ControlFailure {
    code: Option<String>,
    message: String,
}

impl std::fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(formatter, "Warp Control {code}: {}", self.message),
            None => write!(formatter, "Warp Control: {}", self.message),
        }
    }
}

impl std::error::Error for ControlFailure {}

struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait WarpRunner {
    fn control(
        &mut self,
        client: &ControlClient,
        args: &[String],
        deadline: Instant,
    ) -> Result<CommandOutput>;

    fn dispatch_uri(
        &mut self,
        client: &ControlClient,
        uri: &str,
        directory: &Path,
        attempt: &str,
        deadline: Instant,
    ) -> Result<()>;

    fn process_birth(&mut self, pid: u32) -> Result<Option<(u64, u64)>>;

    fn pause(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

struct ProcessRunner;

impl WarpRunner for ProcessRunner {
    fn process_birth(&mut self, pid: u32) -> Result<Option<(u64, u64)>> {
        process::macos_process_start(pid)
    }

    fn control(
        &mut self,
        client: &ControlClient,
        args: &[String],
        deadline: Instant,
    ) -> Result<CommandOutput> {
        run_bounded(control_command(client, args), deadline)
    }

    fn dispatch_uri(
        &mut self,
        client: &ControlClient,
        uri: &str,
        _directory: &Path,
        _attempt: &str,
        deadline: Instant,
    ) -> Result<()> {
        let output = run_bounded(dispatch_command(client, uri), deadline)?;
        if !output.success {
            bail!(
                "Warp launch URI dispatch failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

fn control_command(client: &ControlClient, args: &[String]) -> Command {
    let mut command = crate::native::process_env::helper_command(&client.executable);
    if client.inject_warpctrl {
        command.arg("--warpctrl");
    }
    command.arg("--output-format").arg("json").args(args);
    command
}

fn dispatch_command(client: &ControlClient, uri: &str) -> Command {
    let mut command = crate::native::process_env::helper_command("/usr/bin/open");
    command.arg("-a").arg(&client.bundle).arg(uri);
    command
}

#[derive(Default)]
struct Snapshot {
    windows: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Default)]
struct GlobalSnapshot {
    instances: BTreeMap<String, Snapshot>,
}

struct OpenRequest<'a> {
    clients: &'a [ControlClient],
    command: &'a str,
    directory: &'a Path,
    deadline: Instant,
    cleanup_deadline: Instant,
    attempt: &'a str,
    force_new_window: bool,
}

// Chosen once, from what is known before the creation, and never changed after it.
// Interfaces at pinned d5d23e6119b39edb8c95580a60b68fcd4a2d5a1b.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CreationRoute {
    // `tab_config` URI: a new tab in the active window. uri/mod.rs:153 takes this host
    // only with TabConfigs, which is enabled per build (features.rs:454-455) and which
    // Warp Control does not report, so the route is proven only by the host offer it
    // produces.
    Tab,
    // `launch` URI: uri/mod.rs:142 takes this host without a feature gate, and
    // 227-241 with root_view.rs:589-622 always builds a new window from the template.
    Window,
}

pub(super) fn open_bound_tab_with_mode<F, U>(
    command: &str,
    directory: &Path,
    deadline: Instant,
    force_new_window: bool,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    let clients = installed_control_clients()?;
    let attempt = random_token()?;
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("Warp startup timeout leaves no room for exact surface cleanup")?;
    open_bound_tab_with(
        &mut ProcessRunner,
        OpenRequest {
            force_new_window,
            clients: &clients,
            command,
            directory,
            deadline: startup_deadline,
            cleanup_deadline: deadline,
            attempt: &attempt,
        },
        bind,
        unbind,
    )
}

fn open_bound_tab_with<R, F, U>(
    runner: &mut R,
    request: OpenRequest<'_>,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    R: WarpRunner,
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    let OpenRequest {
        force_new_window,
        clients,
        command,
        directory,
        deadline,
        cleanup_deadline,
        attempt,
    } = request;
    validate_attempt_token(attempt)?;
    ensure_time(deadline, "Warp launch")?;
    for name in [
        CONTROL_FILE,
        HOST_PLAN_FILE,
        HOST_OFFER_FILE,
        HOST_DECISION_FILE,
    ] {
        if directory.join(name).exists() {
            bail!("refusing pre-existing Warp lifecycle record {name}");
        }
    }

    let (client, before_instances) = discover_control_client(runner, clients, deadline)?;
    let before = snapshot_instances(runner, &client, &before_instances, deadline)?;
    // A window is the route when it is asked for, or when no window exists that could
    // take a tab. A Tab Config URI is not a window route: the window it opens is an
    // empty workspace with a tab of its own (uri/mod.rs:838-849, root_view.rs:1231-1243,
    // workspace/view.rs:4430-4461), so the launch tab would not be the only new one.
    let route = if force_new_window
        || before
            .instances
            .values()
            .all(|instance| instance.windows.is_empty())
    {
        CreationRoute::Window
    } else {
        CreationRoute::Tab
    };

    let (deadline_unix_ms, decision_timeout_ms) = host_deadlines(deadline)?;
    let plan = HostPlan {
        schema: RECORD_SCHEMA,
        attempt: attempt.to_owned(),
        command: command.to_owned(),
        deadline_unix_ms,
        decision_timeout_ms,
    };
    RecordStore::at(
        Reader::open_unchecked(directory)
            .private(HOST_PLAN_FILE)
            .path(),
    )
    .write_new_json(EXCLUSIVE_RECORD_LABEL, EXCLUSIVE_TEMPORARY_PREFIX, &plan)?;
    let mut decision = HostDecisionGuard::new(directory, attempt);

    let config_name = format!("agent-bridge-{attempt}");
    let offer_title = format!("agent-bridge-offer-{attempt}");
    let bound_title = format!("agent-bridge-{attempt}");
    let (config_path, uri_host, offer_missing) = match route {
        CreationRoute::Tab => (
            write_tab_config(
                &client,
                &config_name,
                &offer_title,
                directory,
                attempt,
                command,
            )?,
            "tab_config",
            "Warp Tab Config host offer was not verified; TabConfigs URI support/feature enablement is unverified on this app, or host startup did not complete; a residual launch tab may remain and was not closed; no creation retry or new-window fallback was dispatched",
        ),
        CreationRoute::Window => (
            write_launch_config(
                &client,
                &config_name,
                &offer_title,
                directory,
                attempt,
                command,
            )?,
            "launch",
            "Warp Launch Configuration host offer was not verified; the launch URI was not handled or host startup did not complete; a residual launch window may remain and was not closed; no creation retry was dispatched",
        ),
    };
    let _config_guard = ConfigFileGuard(config_path);
    // One URI is one creation request. A missing, late or lost reply is never read as a
    // missing capability: no retry and no other route is dispatched after this.
    let uri = format!("{}://{uri_host}/{config_name}", client.scheme);
    if let Err(error) = runner.dispatch_uri(&client, &uri, directory, attempt, deadline) {
        decision.abort_before_cleanup().context("Warp dispatch failed and Abort could not be established; unproven residual surface was preserved")?;
        return Err(error).context("Warp dispatch failed; ownership is unproven and a residual launch tab may remain; no surface was closed");
    }

    wait_for_offer(runner, directory, attempt, deadline).context(offer_missing)?;
    let after_instances = list_matching_instances(runner, &client, deadline).context(
        "Warp instance ownership is unproven; a residual launch tab may remain and was preserved",
    )?;
    if instance_map(&after_instances)? != instance_map(&before_instances)? {
        decision.publish(HostAction::Abort)?;
        bail!(
            "Warp instance identity changed during launch; unproven residual launch tab may remain; no surface was mutated"
        );
    }
    let after = snapshot_instances(runner, &client, &after_instances, deadline).context(
        "Warp surface ownership is unproven; a residual launch tab may remain and was preserved",
    )?;
    let mut new_tabs = Vec::new();
    for (instance_id, snapshot) in &after.instances {
        let prior = before
            .instances
            .get(instance_id)
            .context("Warp instance appeared during launch")?;
        for (window_id, tabs) in &snapshot.windows {
            for tab_id in tabs {
                if !prior
                    .windows
                    .get(window_id)
                    .is_some_and(|old| old.contains(tab_id))
                {
                    new_tabs.push((instance_id.clone(), window_id.clone(), tab_id.clone()));
                }
            }
        }
    }
    if new_tabs.len() != 1 {
        decision.publish(HostAction::Abort)?;
        bail!(
            "Warp launch did not produce exactly one new tab; unproven residual launch tab may remain; no surface was mutated"
        );
    }
    let (instance_id, window_id, tab_id) = new_tabs.pop().expect("one new tab");
    if route == CreationRoute::Window
        && before.instances[&instance_id]
            .windows
            .contains_key(&window_id)
    {
        decision.publish(HostAction::Abort)?;
        bail!("Warp did not honor the explicit new-window request; unproven tab was preserved");
    }
    let instance = after_instances
        .iter()
        .find(|instance| instance.instance_id == instance_id)
        .context("new Warp tab has no matching instance identity")?;

    let process_birth = runner.process_birth(instance.pid)?
        .context("Warp instance process ended before its launch binding; unproven residual surface preserved")?;

    let mut session = TerminalSession {
        kind: TerminalKind::Warp,
        id: instance_id.clone(),
        tab_id: Some(tab_id),
        window_id: Some(window_id),
        managed_session_id: None,
        windows_process_identity: None,
        wezterm_mux: None,
    };

    let control = ControlBinding {
        schema: RECORD_SCHEMA,
        attempt: attempt.to_owned(),
        bundle: client.bundle.clone(),
        executable: client.executable.clone(),
        inject_warpctrl: client.inject_warpctrl,
        app_id: client.app_id.clone(),
        channel: client.channel.clone(),
        scheme: client.scheme.clone(),
        instance_id: instance_id.clone(),
        pid: instance.pid,
        protocol_version: instance.protocol_version,
        process_birth: Some(process_birth),
    };
    if let Err(error) = prove_offer_title(
        runner,
        &client,
        instance,
        &session,
        &offer_title,
        process_birth,
        deadline,
    ) {
        decision.abort_before_cleanup().context("Warp title proof failed and Abort could not be established; unproven residual surface preserved")?;
        return Err(error).context("Warp random title ownership remains unproven; a residual launch tab may remain; no final rename or close was attempted");
    }
    if let Err(binding_error) = RecordStore::at(
        Reader::open_unchecked(directory)
            .private(CONTROL_FILE)
            .path(),
    )
    .write_new_json(EXCLUSIVE_RECORD_LABEL, EXCLUSIVE_TEMPORARY_PREFIX, &control)
    {
        decision.publish(HostAction::Abort)?;
        return match close_bound_exact(runner, &client, &session, &control, cleanup_deadline) {
            Ok(_) => Err(binding_error).context("failed to persist the Warp control binding"),
            Err(cleanup_error) => {
                // Keep the proven ids even though the missing control binding cannot
                // authorize a later Warp close. The shared handoff preserves both the
                // failed launch and a late residual surface without reopening a session.
                let message = format!(
                    "failed to persist the Warp control binding: {binding_error:#}; exact Warp handle retained={session:?}; exact surface cleanup also failed: {cleanup_error:#}"
                );
                Err(
                    crate::native::terminal::RetainedLaunchSurface::new(session, message)
                        .with_unverified_cleanup()
                        .into(),
                )
            }
        };
    }
    if let Err(bind_error) = bind(&mut session) {
        decision.publish(HostAction::Abort)?;
        let cleanup = close_bound_exact(runner, &client, &session, &control, cleanup_deadline);
        return finish_failed_bind(bind_error, cleanup, unbind);
    }
    if Instant::now() >= deadline {
        decision.publish(HostAction::Abort)?;
        let cleanup = close_bound_exact(runner, &client, &session, &control, cleanup_deadline);
        return finish_failed_bind(
            anyhow!("Warp launch deadline expired after binding and before provider start"),
            cleanup,
            unbind,
        );
    }
    let rename_result = (|| -> Result<()> {
        if !bound_instance_present(runner, &client, &control, deadline)? {
            bail!("Warp instance ended before rename");
        }
        let mut args = tab_args("rename", &session)?;
        args.push(bound_title);
        let renamed: RenameResponse =
            control_json(runner, &client, &args, deadline).map_err(anyhow::Error::new)?;
        if !renamed.ok
            || renamed.action != "tab.rename"
            || renamed.instance_id != instance_id
            || renamed.window_id != session.window_id.as_deref().expect("checked")
            || renamed.tab_id != session.tab_id.as_deref().expect("checked")
        {
            bail!("Warp returned the wrong identity for the one scoped title claim");
        }
        Ok(())
    })();
    if let Err(error) = rename_result {
        decision.abort_before_cleanup().context(
            "Warp rename failed and Abort could not be established; exact binding retained",
        )?;
        let cleanup = close_bound_exact(runner, &client, &session, &control, cleanup_deadline);
        return finish_failed_bind(error.context("Warp scoped rename failed"), cleanup, unbind);
    }
    if let Err(start_error) = decision.publish(HostAction::Start) {
        let start_error = start_error.context("failed to release the bound Warp host");
        if let Err(abort_error) = decision.abort_before_cleanup() {
            return Err(anyhow!(
                "{start_error:#}; abort could not be established: {abort_error:#}; durable exact binding retained"
            ));
        }
        let cleanup = close_bound_exact(runner, &client, &session, &control, cleanup_deadline);
        return finish_failed_bind(start_error, cleanup, unbind);
    }
    Ok(session)
}

// Pinned d5d23e metadata tab.inspect rejects title selectors. Official tab.rename
// selects one exact display title and returns its actual instance/window/tab IDs.
// Setting the same unpredictable offer title is still a mutation. A lost reply may
// be reacquired by repeating this same-title claim; it never advances the title or
// releases the host. Replace this proof when Warp exposes read-only title/creation
// identity. No tab delta, error, or unvalidated reply grants close authority.
fn prove_offer_title<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    instance: &InstanceSummary,
    session: &TerminalSession,
    offer_title: &str,
    process_birth: (u64, u64),
    deadline: Instant,
) -> Result<()> {
    require_exact_ids(session)?;
    let args = vec![
        "tab".into(),
        "rename".into(),
        "--instance".into(),
        session.id.clone(),
        "--window".into(),
        session.window_id.clone().expect("checked"),
        "--tab-title".into(),
        offer_title.to_owned(),
        offer_title.to_owned(),
    ];
    let mut last_error = None;
    for attempt in 0..TITLE_PROOF_ATTEMPTS {
        ensure_time(deadline, "Warp same-title proof")?;
        if runner.process_birth(instance.pid)? != Some(process_birth) {
            bail!("Warp app process changed or ended before title proof; no control authority");
        }
        // Reserve time for reacquisition after a hung/lost first reply.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let call_budget = (remaining / (TITLE_PROOF_ATTEMPTS - attempt)).min(CONTROL_TIMEOUT);
        let call_deadline = Instant::now()
            .checked_add(call_budget)
            .context("Warp title proof deadline overflow")?
            .min(deadline);
        match control_json::<RenameResponse, _>(runner, client, &args, call_deadline) {
            Ok(proof) => {
                if !proof.ok
                    || proof.action != "tab.rename"
                    || proof.instance_id != session.id
                    || proof.window_id != session.window_id.as_deref().expect("checked")
                    || proof.tab_id != session.tab_id.as_deref().expect("checked")
                {
                    bail!(
                        "Warp same-title proof returned the wrong exact target identity or acknowledgement"
                    );
                }
                if runner.process_birth(instance.pid)? != Some(process_birth) {
                    bail!(
                        "Warp app process changed or ended during title proof; no control authority"
                    );
                }
                return Ok(());
            }
            Err(error) if error.code.is_some() => {
                return Err(anyhow::Error::new(error))
                    .context("Warp rejected the unique exact random-title proof");
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow::Error::new(
        last_error.context("Warp title proof returned no response")?,
    ))
    .context("Warp same-title proof reply could not be reacquired within three attempts")
}

fn finish_failed_bind<U>(
    bind_error: anyhow::Error,
    cleanup: Result<CloseOutcome>,
    unbind: U,
) -> Result<TerminalSession>
where
    U: FnOnce() -> Result<()>,
{
    match cleanup {
        Ok(_) => match unbind() {
            Ok(()) => Err(bind_error).context("failed to bind the created Warp surface"),
            Err(error) => Err(anyhow!(
                "failed to bind the created Warp surface: {bind_error:#}; exact cleanup succeeded but the durable binding could not be removed: {error:#}"
            )),
        },
        Err(error) => Err(anyhow!(
            "failed to bind the created Warp surface: {bind_error:#}; exact cleanup also failed: {error:#}"
        )),
    }
}

pub(super) fn send_file(
    _session: &TerminalSession,
    _prompt_path: &Path,
    _deadline: Instant,
) -> TerminalSendResult {
    Err(TerminalSendFailure::not_sent(anyhow!(
        "Warp Control input.insert/input.replace only stage text and cannot submit it; managed Warp terminal input is unsupported"
    )))
}

pub(super) fn verify_surface(
    directory: &Path,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    validate_session_directory(directory, session)?;
    let binding = load_binding(directory, session)?;
    let offer: HostOffer = RecordReader::at(
        Reader::open_unchecked(directory)
            .private(HOST_OFFER_FILE)
            .path(),
    )
    .read_adapter_record("Warp host offer", MAX_CONTROL_OUTPUT as u64)?;
    validate_offer(&offer, &binding.attempt, directory)?;
    let deadline = deadline_from_timeout(timeout.unwrap_or(CONTROL_TIMEOUT))?;
    let mut runner = ProcessRunner;
    let Some(client) = binding_client_if_present(&mut runner, &binding, deadline)? else {
        bail!("managed Warp tab is missing");
    };
    if !tab_present_with(&mut runner, &client, session, deadline)? {
        bail!("managed Warp tab is missing");
    }
    Ok(offer.tty)
}

pub(super) fn surface_present(
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    validate_session_directory(directory, session)?;
    let binding = load_binding(directory, session)?;
    let mut runner = ProcessRunner;
    let deadline = deadline_from_timeout(timeout)?;
    let Some(client) = binding_client_if_present(&mut runner, &binding, deadline)? else {
        return Ok(false);
    };
    tab_present_with(&mut runner, &client, session, deadline)
}

pub(super) fn close_session_until(
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    validate_session_directory(directory, session)?;
    let binding = load_binding(directory, session)?;
    let mut runner = ProcessRunner;
    let Some(client) = binding_client_if_present(&mut runner, &binding, deadline)? else {
        return Ok(CloseOutcome::Missing);
    };
    close_exact(&mut runner, &client, session, deadline)
}

fn recorded_process_present<R: WarpRunner>(
    runner: &mut R,
    binding: &ControlBinding,
) -> Result<bool> {
    let birth = binding.process_birth.context("Warp control binding is missing the app process birth; absence and control authority are unverified")?;
    use crate::native::process_identity::{self, Gone, ProcessObservation};
    match process_identity::classify(
        binding.pid,
        Some(&birth),
        || false, // A recorded incarnation never uses PID-only liveness.
        || runner.process_birth(binding.pid),
    ) {
        ProcessObservation::Live(_) => Ok(true),
        ProcessObservation::Gone(Gone::Exited) => Ok(false),
        ProcessObservation::Gone(Gone::Reused(_)) => {
            bail!("recorded Warp app PID was reused; control and absence are unverified")
        }
        ProcessObservation::Unknown(reason) => bail!("{reason}"),
    }
}

fn binding_client_if_present<R: WarpRunner>(
    runner: &mut R,
    binding: &ControlBinding,
    deadline: Instant,
) -> Result<Option<ControlClient>> {
    if !recorded_process_present(runner, binding)? {
        return Ok(None);
    }
    let client = binding.client()?;
    Ok(bound_instance_present(runner, &client, binding, deadline)?.then_some(client))
}

fn bound_instance_present<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    binding: &ControlBinding,
    deadline: Instant,
) -> Result<bool> {
    if !recorded_process_present(runner, binding)? {
        return Ok(false);
    }
    let inspect: InstanceInspect = match control_json(
        runner,
        client,
        &[
            "instance".into(),
            "inspect".into(),
            "--instance".into(),
            binding.instance_id.clone(),
        ],
        deadline,
    ) {
        Ok(inspect) => inspect,
        Err(error) => {
            return Err(anyhow::Error::new(error))
                .context("Warp instance absence is unverified; exact binding retained");
        }
    };
    if inspect.instance_id != binding.instance_id
        || inspect.pid != binding.pid
        || inspect.channel != binding.channel
        || inspect.app_id != binding.app_id
        || inspect.protocol_version != binding.protocol_version
    {
        bail!("recorded Warp instance identity no longer matches the live control endpoint");
    }
    Ok(true)
}

fn close_bound_exact<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    binding: &ControlBinding,
    deadline: Instant,
) -> Result<CloseOutcome> {
    if !bound_instance_present(runner, client, binding, deadline)? {
        return Ok(CloseOutcome::Missing);
    }
    close_exact(runner, client, session, deadline)
}

fn close_exact<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    ensure_time(
        deadline,
        "Warp exact tab disappearance; tab remains present or absence is unverified",
    )?;
    require_exact_ids(session)?;
    let was_present = tab_present_with(runner, client, session, deadline)?;
    ensure_time(
        deadline,
        "Warp exact tab disappearance; tab remains present or absence is unverified",
    )?;
    if was_present {
        match control_json::<OkResponse, _>(runner, client, &tab_args("close", session)?, deadline)
        {
            Ok(response) => {
                if !response.ok
                    || response.action != "tab.close"
                    || response.instance_id != session.id
                {
                    bail!("Warp did not acknowledge the exact tab.close request");
                }
            }
            Err(error)
                if matches!(
                    error.code.as_deref(),
                    Some("stale_target" | "missing_target")
                ) => {}
            Err(error) => return Err(anyhow::Error::new(error)),
        }
    }
    loop {
        ensure_time(
            deadline,
            "Warp exact tab disappearance; tab remains present or absence is unverified",
        )?;
        let present = tab_present_with(runner, client, session, deadline)?;
        ensure_time(
            deadline,
            "Warp exact tab disappearance; tab remains present or absence is unverified",
        )?;
        if !present {
            return Ok(if was_present {
                CloseOutcome::Closed
            } else {
                CloseOutcome::Missing
            });
        }
        runner.pause(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn tab_present_with<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<bool> {
    let response = optional_control_json::<TabInspectResponse, _>(
        runner,
        client,
        &tab_args("inspect", session)?,
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    let Some(response) = response else {
        return Ok(false);
    };
    if response.action != "tab.inspect"
        || response.tab.tab_id != session.tab_id.as_deref().expect("checked")
        || response.tab.window_id != session.window_id.as_deref().expect("checked")
    {
        bail!("Warp tab.inspect returned the wrong exact target identity");
    }
    Ok(true)
}

#[cfg(test)]
fn window_present_with<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<bool> {
    let response = optional_control_json::<WindowInspectResponse, _>(
        runner,
        client,
        &[
            "window".into(),
            "inspect".into(),
            "--instance".into(),
            session.id.clone(),
            "--window".into(),
            session
                .window_id
                .clone()
                .context("Warp window id is missing")?,
        ],
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    let Some(response) = response else {
        return Ok(false);
    };
    if response.action != "window.inspect"
        || response.window.window_id != session.window_id.as_deref().expect("checked")
    {
        bail!("Warp window.inspect returned the wrong exact target identity");
    }
    Ok(true)
}

fn tab_args(action: &str, session: &TerminalSession) -> Result<Vec<String>> {
    require_exact_ids(session)?;
    Ok(vec![
        "tab".into(),
        action.into(),
        "--instance".into(),
        session.id.clone(),
        "--window".into(),
        session.window_id.clone().expect("checked"),
        "--tab".into(),
        session.tab_id.clone().expect("checked"),
    ])
}

fn require_exact_ids(session: &TerminalSession) -> Result<()> {
    if session.kind != TerminalKind::Warp {
        bail!("not a Warp terminal session");
    }
    if session.id.is_empty()
        || session.tab_id.as_deref().is_none_or(str::is_empty)
        || session.window_id.as_deref().is_none_or(str::is_empty)
    {
        bail!("Warp terminal handle is missing exact instance/window/tab identity");
    }
    Ok(())
}

fn discover_control_client<R: WarpRunner>(
    runner: &mut R,
    clients: &[ControlClient],
    deadline: Instant,
) -> Result<(ControlClient, Vec<InstanceSummary>)> {
    let mut active_clients = Vec::new();
    let mut errors = Vec::new();
    for client in clients {
        match list_matching_instances(runner, client, deadline) {
            Ok(instances) if !instances.is_empty() => {
                active_clients.push((client.clone(), instances));
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("{}: {error:#}", client.bundle.display())),
        }
    }
    if !errors.is_empty() {
        bail!(
            "Warp Control endpoint discovery failed closed: {}",
            errors.join("; ")
        );
    }
    if active_clients.is_empty() {
        bail!(
            "no reachable authorized Warp Control endpoint was found; Warp publishes one only while its Scripting setting is on, and that setting is behind Warp's internal-only WarpControlCli feature flag, so a Stable or Preview build cannot expose it (see docs/terminals.md). Bundle or CLI version alone does not establish availability"
        );
    }
    if active_clients.len() != 1 {
        bail!(
            "Warp requires exactly one reachable authorized official bundle/channel; found {}",
            active_clients.len(),
        );
    }
    let (client, instances) = active_clients.remove(0);
    for instance in &instances {
        validate_capabilities(runner, &client, instance, deadline)?;
    }
    Ok((client, instances))
}

fn instance_map(instances: &[InstanceSummary]) -> Result<BTreeMap<String, InstanceSummary>> {
    let mut mapped = BTreeMap::new();
    for instance in instances {
        if mapped
            .insert(instance.instance_id.clone(), instance.clone())
            .is_some()
        {
            bail!("Warp returned a duplicate instance identity");
        }
    }
    Ok(mapped)
}

fn list_matching_instances<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    deadline: Instant,
) -> Result<Vec<InstanceSummary>> {
    let response: InstanceList = control_json(
        runner,
        client,
        &["instance".into(), "list".into()],
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    Ok(response
        .instances
        .into_iter()
        .filter(|instance| instance.app_id == client.app_id && instance.channel == client.channel)
        .collect())
}

fn validate_capabilities<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    instance: &InstanceSummary,
    deadline: Instant,
) -> Result<()> {
    let inspect: InstanceInspect = control_json(
        runner,
        client,
        &[
            "instance".into(),
            "inspect".into(),
            "--instance".into(),
            instance.instance_id.clone(),
        ],
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    if inspect.instance_id != instance.instance_id
        || inspect.pid != instance.pid
        || inspect.channel != client.channel
        || inspect.app_id != client.app_id
        || inspect.protocol_version != instance.protocol_version
    {
        bail!("Warp instance.inspect identity does not match instance.list");
    }
    let actions = inspect
        .actions
        .into_iter()
        .map(|action| action.name)
        .collect::<BTreeSet<_>>();
    let missing = REQUIRED_ACTIONS
        .iter()
        .filter(|action| !actions.contains(**action))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "reachable Warp Control endpoint lacks required actions: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn snapshot_instance<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    instance_id: &str,
    deadline: Instant,
) -> Result<Snapshot> {
    let windows: WindowList = control_json(
        runner,
        client,
        &[
            "window".into(),
            "list".into(),
            "--instance".into(),
            instance_id.to_owned(),
        ],
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    let mut snapshot = Snapshot::default();
    for window in windows.windows {
        let tabs: TabList = control_json(
            runner,
            client,
            &[
                "tab".into(),
                "list".into(),
                "--instance".into(),
                instance_id.to_owned(),
                "--window".into(),
                window.window_id.clone(),
            ],
            deadline,
        )
        .map_err(anyhow::Error::new)?;
        let mut ids = BTreeSet::new();
        for tab in tabs.tabs {
            if tab.window_id != window.window_id || !ids.insert(tab.tab_id) {
                bail!("Warp returned inconsistent tab.list identity");
            }
        }
        if snapshot.windows.insert(window.window_id, ids).is_some() {
            bail!("Warp returned a duplicate window identity");
        }
    }
    Ok(snapshot)
}

fn snapshot_instances<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    instances: &[InstanceSummary],
    deadline: Instant,
) -> Result<GlobalSnapshot> {
    let mut snapshot = GlobalSnapshot::default();
    for instance in instances {
        if snapshot
            .instances
            .insert(
                instance.instance_id.clone(),
                snapshot_instance(runner, client, &instance.instance_id, deadline)?,
            )
            .is_some()
        {
            bail!("Warp returned a duplicate instance identity");
        }
    }
    Ok(snapshot)
}

fn control_json<T: DeserializeOwned, R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    args: &[String],
    deadline: Instant,
) -> std::result::Result<T, ControlFailure> {
    ensure_time(deadline, "Warp Control call").map_err(|error| ControlFailure {
        code: None,
        message: format!("{error:#}"),
    })?;
    let output = runner
        .control(client, args, deadline)
        .map_err(|error| ControlFailure {
            code: None,
            message: format!("{error:#}"),
        })?;
    if output.stdout.len() > MAX_CONTROL_OUTPUT || output.stderr.len() > MAX_CONTROL_OUTPUT {
        return Err(ControlFailure {
            code: None,
            message: "response exceeded the bounded output limit".into(),
        });
    }
    if !output.success {
        if let Ok(envelope) = serde_json::from_slice::<ErrorEnvelope>(&output.stdout) {
            return Err(ControlFailure {
                code: Some(envelope.error.code),
                message: envelope.error.message,
            });
        }
        return Err(ControlFailure {
            code: None,
            message: format!(
                "command failed without a documented JSON error: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|error| ControlFailure {
        code: None,
        message: format!("invalid JSON response: {error}"),
    })
}

fn optional_control_json<T: DeserializeOwned, R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    args: &[String],
    deadline: Instant,
) -> std::result::Result<Option<T>, ControlFailure> {
    match control_json(runner, client, args, deadline) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some("missing_target" | "stale_target")
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn write_tab_config(
    client: &ControlClient,
    name: &str,
    title: &str,
    directory: &Path,
    attempt: &str,
    command: &str,
) -> Result<PathBuf> {
    let config_dir = ensure_private_config_directory(&client.config_dir)?;
    let (directory_text, host_command) = host_launch_command(directory, attempt, command)?;
    // Interface only: upstream tab_config.rs:144-164 and workspace/view.rs:7312-7326
    // pass the rendered title to add_tab_with_pane_layout, which sets custom_title
    // (13061-13063). metadata_config.rs:393-399 selects that exact display title.
    // JSON string escaping is TOML basic-string escaping except for DEL.
    let configuration = format!(
        "name = {}\ntitle = {}\n\n[[panes]]\nid = \"main\"\ntype = \"terminal\"\ndirectory = {}\ncommands = [{}]\n",
        serde_json::to_string(name)?,
        serde_json::to_string(title)?,
        serde_json::to_string(directory_text)?,
        serde_json::to_string(&host_command)?,
    );
    let path = config_dir.join(format!("{name}.toml"));
    RecordStore::at(&path).write_new_bytes(
        EXCLUSIVE_RECORD_LABEL,
        EXCLUSIVE_TEMPORARY_PREFIX,
        escape_unportable(&configuration).as_bytes(),
    )?;
    Ok(path)
}

// The window route that R2 used, restored: it needs no TabConfigs. Replace both
// routes when Warp Control can create a surface that runs a given command; at the
// pinned commit tab.create and window.create take only a tab type
// (crates/local_control/src/protocol.rs:170-173).
fn write_launch_config(
    client: &ControlClient,
    name: &str,
    title: &str,
    directory: &Path,
    attempt: &str,
    command: &str,
) -> Result<PathBuf> {
    // user_config/mod.rs:210-216 keeps launch_configurations beside tab_configs.
    let config_dir = ensure_private_config_directory(
        &client.config_dir.with_file_name("launch_configurations"),
    )?;
    let (directory_text, host_command) = host_launch_command(directory, attempt, command)?;
    // Interface only: launch_config.rs:15-21, 37-47, 190-206, 278-290, 364-367. The
    // URI finds the document by its `name` (uri/mod.rs:787-811) among the .yaml files
    // that serde_yaml reads (user_config/util.rs:20, 74-84); JSON is YAML flow style.
    // workspace/view.rs:4018-4023 passes the tab title to add_tab_with_pane_layout, so
    // the same-title proof holds as on the Tab Config route.
    let configuration = serde_json::to_string_pretty(&json!({
        "name": name,
        "windows": [{
            "active_tab_index": 0,
            "tabs": [{
                "title": title,
                "layout": { "cwd": directory_text },
                "commands": [{ "exec": host_command }]
            }]
        }],
        "active_window_index": 0
    }))?;
    let path = config_dir.join(format!("{name}.yaml"));
    RecordStore::at(&path).write_new_bytes(
        EXCLUSIVE_RECORD_LABEL,
        EXCLUSIVE_TEMPORARY_PREFIX,
        escape_unportable(&configuration).as_bytes(),
    )?;
    Ok(path)
}

// The session directory as text and the one command of the launch tab; both creation
// routes carry the same pair.
fn host_launch_command<'a>(
    directory: &'a Path,
    attempt: &str,
    command: &str,
) -> Result<(&'a str, String)> {
    let executable =
        std::env::current_exe().context("failed to resolve Agent Bridge executable")?;
    let executable = executable
        .to_str()
        .context("Agent Bridge executable path is not valid UTF-8")?;
    let directory_text = directory
        .to_str()
        .context("Warp session directory is not valid UTF-8")?;
    let host_command = format!(
        "{} {HOST_COMMAND} {} {}",
        shell_quote(OsStr::new(executable)),
        shell_quote(directory.as_os_str()),
        shell_quote(OsStr::new(attempt))
    );
    // These are distinct interactive-shell jobs; sourcing the launch command here
    // preserves the existing owner/shell foreground-group attestation.
    Ok((directory_text, gated_launch_command(&host_command, command)))
}

// serde_json writes these characters raw, and a reader of the two documents may not
// take them raw: TOML refuses DEL in a basic string, and YAML 1.1 treats NEL, LS and
// PS as line breaks and excludes DEL, the C1 controls, a byte order mark and the
// noncharacters. `\uXXXX` is the same character to a JSON, TOML or YAML reader, and
// both documents hold such a character only inside a quoted string.
fn escape_unportable(document: &str) -> String {
    let mut escaped = String::with_capacity(document.len());
    for character in document.chars() {
        match character {
            '\u{7f}'..='\u{9f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{feff}'
            | '\u{fffe}'
            | '\u{ffff}' => escaped.push_str(&format!("\\u{:04x}", u32::from(character))),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn gated_launch_command(host_command: &str, command: &str) -> String {
    format!("{host_command} || exit; {command}")
}

struct ConfigFileGuard(PathBuf);

impl Drop for ConfigFileGuard {
    fn drop(&mut self) {
        let _ = RecordStore::at(&self.0).remove_raw();
    }
}

struct HostDecisionGuard<'a> {
    directory: &'a Path,
    attempt: &'a str,
    published: bool,
}

impl<'a> HostDecisionGuard<'a> {
    fn new(directory: &'a Path, attempt: &'a str) -> Self {
        Self {
            directory,
            attempt,
            published: false,
        }
    }

    fn publish(&mut self, action: HostAction) -> Result<()> {
        write_decision(self.directory, self.attempt, action)?;
        self.published = true;
        Ok(())
    }

    fn abort_before_cleanup(&mut self) -> Result<()> {
        if let Err(publish_error) = self.publish(HostAction::Abort) {
            // write_new_json never fails after publishing its hard link, but another
            // decision can already occupy the path. Never replace that decision.
            let decision: HostDecision = RecordReader::at(
                Reader::open_unchecked(self.directory)
                    .private(HOST_DECISION_FILE)
                    .path(),
            )
            .read_adapter_record("Warp host decision", MAX_CONTROL_OUTPUT as u64)
            .with_context(|| format!("failed to publish Warp host abort: {publish_error:#}"))?;
            if decision.schema != RECORD_SCHEMA || decision.attempt != self.attempt {
                bail!("existing Warp host decision does not match this launch attempt");
            }
            self.published = true;
            if decision.action != HostAction::Abort {
                bail!(
                    "Warp host start decision is already published and cannot be replaced by abort"
                );
            }
        }
        Ok(())
    }
}

impl Drop for HostDecisionGuard<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = write_decision(self.directory, self.attempt, HostAction::Abort);
        }
    }
}

fn ensure_private_config_directory(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                if metadata.uid() != unsafe { libc::geteuid() } {
                    bail!("Warp launch configuration symlink is not owned by the current user");
                }
            } else if !metadata.is_dir() {
                bail!("refusing non-directory Warp launch configuration path");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("failed to create {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error.into()),
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir()
        || !owned_and_not_writable_by_others(&metadata, unsafe { libc::geteuid() })
    {
        bail!("Warp launch configuration directory is not privately writable");
    }
    Ok(canonical)
}

fn wait_for_offer<R: WarpRunner>(
    runner: &mut R,
    directory: &Path,
    attempt: &str,
    deadline: Instant,
) -> Result<HostOffer> {
    let path = Reader::open_unchecked(directory)
        .private(HOST_OFFER_FILE)
        .path()
        .to_owned();
    loop {
        if path.exists() {
            let offer: HostOffer = RecordReader::at(&path)
                .read_adapter_record("Warp host offer", MAX_CONTROL_OUTPUT as u64)?;
            validate_offer(&offer, attempt, directory)?;
            return Ok(offer);
        }
        ensure_time(deadline, "Warp host offer")?;
        runner.pause(POLL_INTERVAL);
    }
}

fn validate_offer(offer: &HostOffer, attempt: &str, directory: &Path) -> Result<()> {
    if offer.schema != RECORD_SCHEMA
        || offer.attempt != attempt
        || offer.directory != directory
        || offer.pid == 0
        || offer.tty.is_empty()
    {
        bail!("invalid or mismatched Warp host offer");
    }
    Ok(())
}

fn write_decision(directory: &Path, attempt: &str, action: HostAction) -> Result<()> {
    RecordStore::at(
        Reader::open_unchecked(directory)
            .private(HOST_DECISION_FILE)
            .path(),
    )
    .write_new_json(
        EXCLUSIVE_RECORD_LABEL,
        EXCLUSIVE_TEMPORARY_PREFIX,
        &HostDecision {
            schema: RECORD_SCHEMA,
            attempt: attempt.to_owned(),
            action,
        },
    )
}

fn load_binding(directory: &Path, session: &TerminalSession) -> Result<ControlBinding> {
    require_exact_ids(session)?;
    let binding: ControlBinding = RecordReader::at(
        Reader::open_unchecked(directory)
            .private(CONTROL_FILE)
            .path(),
    )
    .read_adapter_record("Warp control binding", MAX_CONTROL_OUTPUT as u64)?;
    if binding.schema != RECORD_SCHEMA || binding.instance_id != session.id {
        bail!("Warp control binding does not match the terminal handle");
    }
    validate_attempt_token(&binding.attempt)?;
    Ok(binding)
}

impl ControlBinding {
    fn client(&self) -> Result<ControlClient> {
        let bundle = self
            .bundle
            .canonicalize()
            .context("recorded Warp bundle is unavailable")?;
        let executable = self
            .executable
            .canonicalize()
            .context("recorded Warp Control executable is unavailable")?;
        if bundle != self.bundle
            || executable != self.executable
            || !executable.starts_with(&bundle)
        {
            bail!("recorded Warp bundle/control executable identity changed");
        }
        if !is_executable(&executable) {
            bail!("recorded Warp Control executable is not executable");
        }
        Ok(ControlClient {
            bundle,
            executable,
            inject_warpctrl: self.inject_warpctrl,
            app_id: self.app_id.clone(),
            channel: self.channel.clone(),
            scheme: self.scheme.clone(),
            config_dir: PathBuf::new(),
        })
    }
}

fn validate_session_directory(directory: &Path, session: &TerminalSession) -> Result<()> {
    let id = session
        .managed_session_id
        .as_deref()
        .context("Warp terminal handle is missing its managed session binding")?;
    if directory.file_name().and_then(|name| name.to_str()) != Some(id) {
        bail!("Warp managed session binding does not match the supplied session directory");
    }
    crate::native::session::primitives::require_valid_session_id(id)?;
    Reader::open_unchecked(directory)
        .validate_private_session_directory(HOST_SESSION_DIRECTORY_LABEL)
}

fn installed_control_clients() -> Result<Vec<ControlClient>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    control_clients_in(
        &[PathBuf::from("/Applications"), home.join("Applications")],
        &home,
    )
}

fn control_clients_in(roots: &[PathBuf], config_home: &Path) -> Result<Vec<ControlClient>> {
    let mut clients = Vec::new();
    for root in roots {
        for spec in BUNDLE_SPECS {
            let bundle = root.join(spec.bundle_name);
            if let Some(client) = client_for_bundle(&bundle, *spec, config_home)? {
                clients.push(client);
            }
        }
    }
    if clients.is_empty() {
        bail!("no official Warp application bundle with a Warp Control route was found");
    }
    Ok(clients)
}

fn client_for_bundle(
    bundle: &Path,
    spec: BundleSpec,
    home: &Path,
) -> Result<Option<ControlClient>> {
    let metadata = match fs::symlink_metadata(bundle) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing non-directory Warp bundle: {}", bundle.display());
    }
    let bundle = bundle.canonicalize()?;
    let macos = bundle.join("Contents/MacOS");
    let wrapper = bundle.join("Contents/Resources/bin").join(spec.wrapper);
    let embedded = macos.join(spec.embedded);
    let (executable, inject_warpctrl) = if is_executable(&wrapper) {
        (wrapper.canonicalize()?, false)
    } else if is_executable(&embedded) {
        (embedded.canonicalize()?, true)
    } else {
        return Ok(None);
    };
    if !executable.starts_with(&bundle) {
        bail!("Warp Control executable resolves outside its application bundle");
    }
    Ok(Some(ControlClient {
        bundle,
        executable,
        inject_warpctrl,
        app_id: spec.app_id.into(),
        channel: spec.channel.into(),
        scheme: spec.scheme.into(),
        config_dir: home.join(spec.config_home).join("tab_configs"),
    }))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn run_bounded(mut command: Command, deadline: Instant) -> Result<CommandOutput> {
    ensure_time(deadline, "subprocess")?;
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let mut child = command
        .spawn()
        .context("failed to start bounded subprocess")?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("bounded subprocess timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = RecordReader::read_bounded_file(stdout, MAX_CONTROL_OUTPUT as u64)?;
    let stderr = RecordReader::read_bounded_file(stderr, MAX_CONTROL_OUTPUT as u64)?;
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(in crate::native) fn valid_attempt_token(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_attempt_token(value: &str) -> Result<()> {
    if !valid_attempt_token(value) {
        bail!("invalid Warp launch attempt token");
    }
    Ok(())
}

pub(in crate::native) fn run_host(directory: &Path, attempt: &str) -> Result<()> {
    validate_attempt_token(attempt)?;
    if !directory.is_absolute() {
        bail!("Warp host session directory must be absolute");
    }
    let session_id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("Warp host session directory has no UTF-8 session id")?;
    if !crate::native::session::primitives::valid_session_id(session_id) {
        bail!("Warp host session directory has an invalid session id");
    }
    Reader::open_unchecked(directory)
        .validate_private_session_directory(HOST_SESSION_DIRECTORY_LABEL)?;
    let plan: HostPlan = RecordReader::at(
        Reader::open_unchecked(directory)
            .private(HOST_PLAN_FILE)
            .path(),
    )
    .read_adapter_record("Warp host plan", MAX_CONTROL_OUTPUT as u64)?;
    if plan.schema != RECORD_SCHEMA || plan.attempt != attempt {
        bail!("Warp host plan does not match this launch attempt");
    }
    if plan.decision_timeout_ms == 0 {
        bail!("Warp host plan has an invalid decision timeout");
    }
    let monotonic_deadline = Instant::now()
        .checked_add(Duration::from_millis(plan.decision_timeout_ms))
        .context("Warp host decision timeout is too large")?;
    let tty = controlling_tty()?;
    RecordStore::at(
        Reader::open_unchecked(directory)
            .private(HOST_OFFER_FILE)
            .path(),
    )
    .write_new_json(
        EXCLUSIVE_RECORD_LABEL,
        EXCLUSIVE_TEMPORARY_PREFIX,
        &HostOffer {
            schema: RECORD_SCHEMA,
            attempt: attempt.to_owned(),
            directory: directory.to_owned(),
            pid: std::process::id(),
            tty,
        },
    )?;
    loop {
        if Instant::now() >= monotonic_deadline || wall_ms()? >= plan.deadline_unix_ms {
            bail!("Warp host decision timed out");
        }
        let decision_path = Reader::open_unchecked(directory)
            .private(HOST_DECISION_FILE)
            .path()
            .to_owned();
        if decision_path.exists() {
            let decision: HostDecision = RecordReader::at(&decision_path)
                .read_adapter_record("Warp host decision", MAX_CONTROL_OUTPUT as u64)?;
            if decision.schema != RECORD_SCHEMA || decision.attempt != attempt {
                bail!("Warp host decision does not match this launch attempt");
            }
            return match decision.action {
                HostAction::Abort => bail!("Warp host launch was aborted"),
                HostAction::Start => Ok(()),
            };
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn controlling_tty() -> Result<String> {
    let mut buffer = vec![0_i8; 1024];
    // SAFETY: buffer is writable for its declared length and fd 0 remains borrowed.
    let result = unsafe { libc::ttyname_r(0, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        bail!("Warp host stdin is not attached to a tty (errno {result})");
    }
    // SAFETY: ttyname_r wrote a NUL-terminated string on success.
    let value = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Ok(value.to_string_lossy().into_owned())
}

fn deadline_from_timeout(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .context("Warp automation timeout is too large")
}

fn ensure_time(deadline: Instant, label: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("{label} deadline is exhausted");
    }
    Ok(())
}

fn wall_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis()
        .try_into()
        .context("system time does not fit u64")
}

fn host_deadlines(deadline: Instant) -> Result<(u64, u64)> {
    let timeout_ms = deadline
        .checked_duration_since(Instant::now())
        .context("Warp launch deadline is exhausted")?
        .as_millis()
        .try_into()
        .context("Warp launch duration does not fit u64")?;
    if timeout_ms == 0 {
        bail!("Warp launch deadline leaves no host decision time");
    }
    let deadline_unix_ms = wall_ms()?
        .checked_add(timeout_ms)
        .context("Warp host deadline overflow")?;
    Ok((deadline_unix_ms, timeout_ms))
}

#[cfg(test)]
fn read_record<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T> {
    crate::native::session::RecordReader::at(path)
        .read_adapter_record(label, MAX_CONTROL_OUTPUT as u64)
}

#[cfg(test)]
fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    crate::native::session::RecordStore::at(path).write_new_json(
        EXCLUSIVE_RECORD_LABEL,
        EXCLUSIVE_TEMPORARY_PREFIX,
        value,
    )
}

pub(super) struct Adapter;
pub(super) static ADAPTER: Adapter = Adapter;
impl MacTerminalAdapter for Adapter {
    fn open_bound_tab(
        &self,
        mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: super::BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession> {
        open_bound_tab_with_mode(
            command,
            directory,
            deadline,
            mode == MacosOpenMode::NewWindow,
            bind,
            unbind,
        )
    }
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult {
        send_file(session, prompt_path, deadline)
    }
    fn verify_surface(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>> {
        verify_surface(directory, session, timeout).map(Some)
    }
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool> {
        surface_present(directory, session, timeout)
    }
    // Screen reads and guarded dialog input are declared unsupported in the registration.
    fn close_session_until(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        close_session_until(directory, session, deadline)
    }
    fn close_owned_session(
        &self,
        directory: &Path,
        id: &str,
        session: &TerminalSession,
        close: OwnedSurfaceClose,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if close == OwnedSurfaceClose::LiveOwner {
            close_remaining(deadline)?;
            let (owner, live) = verified_macos_terminal_owner(directory, id, session, None)?;
            let shell = live_native_process_identity(live.parent_pid)?;
            prepare_warp_close(
                directory,
                id,
                session,
                &owner,
                &live,
                &shell,
                |group| {
                    ownership::terminate_owned_foreground_group_until(group, deadline).context(
                        "the attested Warp foreground group was not stopped; no tab close was sent",
                    )
                },
                deadline,
            )?;
        }
        close_session_until(directory, session, deadline)
    }
}
#[cfg(test)]
mod tests;

// Warp preserves normal close warnings. Stop only the fully attested foreground
// job before requesting tab.close; never suppress warnings or signal by tty name.
#[allow(clippy::too_many_arguments)]
pub(in crate::native) fn prepare_warp_close(
    directory: &Path,
    id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
    shell: &NativeProcessIdentity,
    stop: impl FnOnce(u32) -> Result<()>,
    deadline: std::time::Instant,
) -> Result<()> {
    session.verify_managed_session(id)?;
    if session.kind != terminal::TerminalKind::Warp
        || owner.managed_session_id.as_deref() != Some(id)
        || !native_owner_identity_matches(owner, live)
    {
        bail!("Warp close owner identity changed");
    }
    let group = verified_terminal_owner_process_group(owner, live)?;
    verified_terminal_shell_process_group(owner, live, shell)?;
    close_remaining(deadline)?;
    session::close::record_terminal_close_intent(
        &Store::open_unchecked(directory),
        id,
        session,
        owner,
    )?;
    close_remaining(deadline)?;
    stop(group)
}
