use super::process;
use crate::native::session::{self, Store};
use crate::native::session::{Reader, RecordReader, RecordStore};
use crate::native::terminal;
#[cfg(test)]
use crate::native::terminal::ownership;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
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
    let mut command = Command::new(&client.executable);
    if client.inject_warpctrl {
        command.arg("--warpctrl");
    }
    command.arg("--output-format").arg("json").args(args);
    command
}

fn dispatch_command(client: &ControlClient, uri: &str) -> Command {
    let mut command = Command::new("/usr/bin/open");
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

// Compatibility entry point; routing can pass its one creation-time choice below.
#[allow(dead_code)]
pub(super) fn open_bound_tab<F, U>(
    command: &str,
    directory: &Path,
    deadline: Instant,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    open_bound_tab_with_mode(command, directory, deadline, false, bind, unbind)
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
                    "failed to persist the Warp control binding: {binding_error:#}; exact Warp handle retained={session:?}; exact surface cleanup also failed: {cleanup_error:#}; {}",
                    crate::native::launch::RESIDUAL_SURFACE_MARKER
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
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<String> {
    let directory = session_directory(session)?;
    let binding = load_binding(&directory, session)?;
    let offer: HostOffer = RecordReader::at(
        Reader::open_unchecked(&directory)
            .private(HOST_OFFER_FILE)
            .path(),
    )
    .read_adapter_record("Warp host offer", MAX_CONTROL_OUTPUT as u64)?;
    validate_offer(&offer, &binding.attempt, &directory)?;
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

pub(super) fn surface_present(session: &TerminalSession, timeout: Duration) -> Result<bool> {
    let directory = session_directory(session)?;
    let binding = load_binding(&directory, session)?;
    let mut runner = ProcessRunner;
    let deadline = deadline_from_timeout(timeout)?;
    let Some(client) = binding_client_if_present(&mut runner, &binding, deadline)? else {
        return Ok(false);
    };
    tab_present_with(&mut runner, &client, session, deadline)
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    close_session_until(session, deadline_from_timeout(CONTROL_TIMEOUT)?)
}

pub(super) fn close_session_until(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let directory = session_directory(session)?;
    let binding = load_binding(&directory, session)?;
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
    match runner.process_birth(binding.pid)? {
        None => Ok(false),
        Some(live) if live == birth => Ok(true),
        Some(_) => bail!("recorded Warp app PID was reused; control and absence are unverified"),
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
    require_exact_ids(session)?;
    let was_present = tab_present_with(runner, client, session, deadline)?;
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
        if !tab_present_with(runner, client, session, deadline)? {
            return Ok(if was_present {
                CloseOutcome::Closed
            } else {
                CloseOutcome::Missing
            });
        }
        runner.pause(POLL_INTERVAL);
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
            "no reachable authorized Warp Control endpoint was found; the target app must expose an official endpoint and authorize Scripting and required actions. Bundle or CLI version alone does not establish availability"
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
        "{} native-warp-host {} {}",
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
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
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

fn session_directory(session: &TerminalSession) -> Result<PathBuf> {
    let id = session
        .managed_session_id
        .as_deref()
        .context("Warp terminal handle is missing its managed session binding")?;
    Reader::session_directory(id)
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
    if !crate::native::valid_session_id(session_id) {
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

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

// Warp preserves normal close warnings. Stop only the fully attested foreground
// job before requesting tab.close; never suppress warnings or signal by tty name.
#[cfg(target_os = "macos")]
pub(in crate::native) fn prepare_warp_close(
    directory: &Path,
    id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
    shell: &NativeProcessIdentity,
    stop: impl FnOnce(u32) -> Result<()>,
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
    session::close::record_terminal_close_intent(
        &Store::open_unchecked(directory),
        id,
        session,
        owner,
    )?;
    stop(group)
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn terminate_owned_foreground_group(group: u32) -> Result<()> {
    terminate_owned_foreground_group_with(group, Duration::from_secs(3), |_| {})
}

#[cfg(target_os = "macos")]
fn terminate_owned_foreground_group_with(
    group: u32,
    timeout: Duration,
    mut observe: impl FnMut(Option<i32>),
) -> Result<()> {
    let target = terminal::macos::apple_terminal::process_group_signal_target(group)?;
    if unsafe { libc::kill(target, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error).context("could not stop the attested Warp foreground group");
    }
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(target, 0) } != 0 {
            let error = std::io::Error::last_os_error();
            observe(error.raw_os_error());
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // EPERM does not establish absence, including for a zombie-only group
            // (measured on macOS 26.6.2, 2026-10-07, issue #85); only ESRCH does.
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(error).context("could not observe the stopped Warp foreground group");
            }
        } else {
            observe(None);
        }
        if Instant::now() >= deadline {
            bail!("the attested Warp foreground group has not stopped; no tab close was sent");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, os::unix::process::CommandExt};

    use super::*;

    #[cfg(target_os = "macos")]
    struct ForegroundGroupChild(std::process::Child);

    #[cfg(target_os = "macos")]
    impl ForegroundGroupChild {
        fn spawn() -> Self {
            Self(
                Command::new("/bin/sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            )
        }

        fn wait_for_zombie(&self) {
            let target = -(self.0.id() as i32);
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if unsafe { libc::kill(target, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return;
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ForegroundGroupChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_waits_for_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        let (reap, ready) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let received = ready.recv_timeout(Duration::from_secs(5));
            drop(child);
            received
        });
        let mut probes = Vec::new();
        let mut eperm_probes = 0;
        let result =
            terminate_owned_foreground_group_with(group, Duration::from_secs(3), |errno| {
                probes.push(errno);
                if errno == Some(libc::EPERM) {
                    eperm_probes += 1;
                    if eperm_probes == 2 {
                        reap.send(()).unwrap();
                    }
                }
            });
        drop(reap);
        let reaped = waiter.join();
        assert!(result.is_ok(), "{result:?}");
        reaped.unwrap().unwrap();
        let gone = probes
            .iter()
            .position(|errno| *errno == Some(libc::ESRCH))
            .unwrap();
        assert!(
            probes[..gone]
                .iter()
                .filter(|errno| **errno == Some(libc::EPERM))
                .count()
                >= 2,
            "{probes:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_times_out_before_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let timeout = Duration::from_secs(1);
        let mut eperm_probes = 0;
        let started = Instant::now();
        let result = terminate_owned_foreground_group_with(child.0.id(), timeout, |errno| {
            if errno == Some(libc::EPERM) {
                eperm_probes += 1;
            }
        });
        let elapsed = started.elapsed();
        assert_eq!(
            result.as_ref().unwrap_err().to_string(),
            "the attested Warp foreground group has not stopped; no tab close was sent",
            "{result:?}"
        );
        assert!(eperm_probes > 0, "no EPERM probe was observed");
        assert!(elapsed >= timeout);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_refuses_failed_signal_to_zombie() {
        let child = ForegroundGroupChild::spawn();
        assert_eq!(
            unsafe { libc::kill(-(child.0.id() as i32), libc::SIGTERM) },
            0
        );
        child.wait_for_zombie();
        let error = terminate_owned_foreground_group(child.0.id()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "could not stop the attested Warp foreground group"
        );
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }

    struct FakeRunner {
        directory: PathBuf,
        attempt: String,
        dispatched: bool,
        ambiguous: bool,
        wrong_rename_identity: bool,
        wrong_inspect_identity: bool,
        tab_present: bool,
        window_present: bool,
        siblings: Vec<String>,
        inspect_error: Option<&'static str>,
        instance_inspect_error: Option<&'static str>,
        cancel_close: bool,
        close_removes_tab: bool,
        write_offer: bool,
        multiple_instances: bool,
        missing_action: bool,
        block_control_binding: bool,
        calls: Vec<Vec<String>>,
        process_birth: Option<(u64, u64)>,
        birth_unreadable: bool,
        proof_failures: usize,
        proof_reply: Option<&'static str>,
        proof_selector_error: Option<&'static str>,
        proof_calls: usize,
        require_session_binding: bool,
        proof_executed: usize,
        offered_title: String,
        rename_error: Option<&'static str>,
        close_error: Option<&'static str>,
        reuse_window: bool,
        no_window: bool,
        uri_new_window: bool,
        dispatch_uncertain: bool,
        // The fake follows pinned d5d23e: `launch` is always handled, `tab_config`
        // only with TabConfigs (uri/mod.rs:142, 153).
        tab_configs_enabled: bool,
        launch_uri: bool,
        created: bool,
        home_tab: bool,
        launch_into_existing: bool,
    }

    impl FakeRunner {
        fn new(directory: &Path, attempt: &str) -> Self {
            Self {
                directory: directory.to_owned(),
                attempt: attempt.to_owned(),
                dispatched: false,
                ambiguous: false,
                wrong_rename_identity: false,
                wrong_inspect_identity: false,
                tab_present: true,
                window_present: true,
                siblings: Vec::new(),
                inspect_error: None,
                instance_inspect_error: None,
                cancel_close: false,
                close_removes_tab: true,
                write_offer: true,
                multiple_instances: false,
                missing_action: false,
                block_control_binding: false,
                calls: Vec::new(),
                process_birth: Some((100, 123)),
                birth_unreadable: false,
                proof_failures: 0,
                proof_reply: None,
                proof_selector_error: None,
                proof_calls: 0,
                require_session_binding: false,
                proof_executed: 0,
                offered_title: format!("agent-bridge-offer-{attempt}"),
                rename_error: None,
                close_error: None,
                reuse_window: false,
                no_window: false,
                uri_new_window: false,
                dispatch_uncertain: false,
                tab_configs_enabled: true,
                launch_uri: false,
                created: false,
                home_tab: false,
                launch_into_existing: false,
            }
        }

        fn owned_window(&self) -> &'static str {
            if self.reuse_window && !self.no_window && !self.uri_new_window {
                "old-window"
            } else {
                "new-window"
            }
        }

        fn json(value: Value) -> CommandOutput {
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&value).unwrap(),
                stderr: Vec::new(),
            }
        }

        fn error(code: &str) -> CommandOutput {
            CommandOutput {
                success: false,
                stdout: serde_json::to_vec(&json!({
                    "ok": false,
                    "error": {"code": code, "message": "fake error"}
                }))
                .unwrap(),
                stderr: Vec::new(),
            }
        }
    }

    impl WarpRunner for FakeRunner {
        fn process_birth(&mut self, _pid: u32) -> Result<Option<(u64, u64)>> {
            if self.birth_unreadable {
                bail!("injected app birth unreadable");
            }
            Ok(self.process_birth)
        }

        fn control(
            &mut self,
            client: &ControlClient,
            args: &[String],
            deadline: Instant,
        ) -> Result<CommandOutput> {
            let command = control_command(client, args);
            assert_eq!(command.get_program(), client.executable.as_os_str());
            let mut expected = Vec::new();
            if client.inject_warpctrl {
                expected.push("--warpctrl".to_owned());
            }
            expected.extend(["--output-format".into(), "json".into()]);
            expected.extend_from_slice(args);
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                expected.iter().map(OsStr::new).collect::<Vec<_>>()
            );
            self.calls.push(args.to_vec());
            let command = args.iter().take(2).map(String::as_str).collect::<Vec<_>>();
            Ok(match command.as_slice() {
                ["instance", "list"] => {
                    let mut instances = vec![json!({
                        "instance_id": "instance-1", "pid": 42, "channel": "stable",
                        "app_id": "dev.warp.Warp-Stable", "protocol_version": 1
                    })];
                    if self.multiple_instances {
                        instances.push(json!({
                            "instance_id": "instance-2", "pid": 43, "channel": "stable",
                            "app_id": "dev.warp.Warp-Stable", "protocol_version": 1
                        }));
                    }
                    Self::json(json!({"instances": instances}))
                }
                ["instance", "inspect"] => {
                    if let Some(code) = self.instance_inspect_error {
                        return Ok(Self::error(code));
                    }
                    let instance = args.last().unwrap();
                    let actions = REQUIRED_ACTIONS
                        .iter()
                        .filter(|name| !self.missing_action || **name != "tab.close")
                        .map(|name| json!({"name": name}))
                        .collect::<Vec<_>>();
                    Self::json(json!({
                        "instance_id": instance,
                        "pid": if instance == "instance-1" { 42 } else { 43 },
                        "channel": "stable",
                        "app_id": "dev.warp.Warp-Stable",
                        "protocol_version": 1,
                        "actions": actions
                    }))
                }
                ["window", "list"] => {
                    if args.last().is_some_and(|instance| instance == "instance-2") {
                        return Ok(Self::json(
                            json!({"windows": [{"window_id": "second-window"}]}),
                        ));
                    }
                    let mut windows = if self.no_window {
                        Vec::new()
                    } else {
                        vec![json!({"window_id": "old-window"})]
                    };
                    if self.created && self.owned_window() == "new-window" {
                        windows.push(json!({"window_id": "new-window"}));
                    }
                    if self.dispatched && self.ambiguous {
                        windows.push(json!({"window_id": "other-window"}));
                    }
                    Self::json(json!({"windows": windows}))
                }
                ["tab", "list"] => {
                    let window = args.last().unwrap();
                    let tabs = match window.as_str() {
                        "old-window" => {
                            let mut tabs =
                                vec![json!({"tab_id": "old-tab", "window_id": "old-window"})];
                            if self.created
                                && self.owned_window() == "old-window"
                                && self.tab_present
                            {
                                tabs.push(json!({"tab_id": "new-tab", "window_id": "old-window"}));
                            }
                            tabs
                        }
                        "new-window" if !self.window_present => Vec::new(),
                        "new-window" => {
                            let mut tabs = self
                                .siblings
                                .iter()
                                .map(|id| json!({"tab_id": id, "window_id": "new-window"}))
                                .collect::<Vec<_>>();
                            if self.home_tab {
                                tabs.push(json!({"tab_id": "home-tab", "window_id": "new-window"}));
                            }
                            if self.tab_present {
                                tabs.insert(
                                    0,
                                    json!({"tab_id": "new-tab", "window_id": "new-window"}),
                                );
                            }
                            tabs
                        }
                        "other-window" => {
                            vec![json!({"tab_id": "other-tab", "window_id": "other-window"})]
                        }
                        "second-window" => {
                            vec![json!({"tab_id": "second-tab", "window_id": "second-window"})]
                        }
                        _ => Vec::new(),
                    };
                    Self::json(json!({"tabs": tabs}))
                }
                ["tab", "rename"] => {
                    if args.iter().any(|arg| arg == "--tab-title") {
                        self.proof_calls += 1;
                        assert!(
                            !self.directory.join(CONTROL_FILE).exists(),
                            "proof precedes binding"
                        );
                        assert_eq!(args[3], "instance-1");
                        assert_eq!(args[5], self.owned_window());
                        let title = &args[7];
                        assert_eq!(title, &format!("agent-bridge-offer-{}", self.attempt));
                        assert_eq!(
                            args.last().unwrap(),
                            title,
                            "same-title proof cannot change the offer title"
                        );
                        if let Some(code) = self.proof_selector_error {
                            return Ok(Self::error(code));
                        }
                        if self.offered_title != *title {
                            return Ok(Self::error("missing_target"));
                        }
                        // Upstream selects a unique exact display title, then sets that same title.
                        self.proof_executed += 1;
                        if self.proof_failures > 0 {
                            self.proof_failures -= 1;
                            return match self.proof_reply {
                                Some("malformed") => Ok(CommandOutput {
                                    success: true,
                                    stdout: b"{".to_vec(),
                                    stderr: Vec::new(),
                                }),
                                Some("timeout") => {
                                    thread::sleep(
                                        deadline.saturating_duration_since(Instant::now()),
                                    );
                                    bail!("executed same-title proof, lost reply at timeout");
                                }
                                _ => bail!("executed same-title proof, reply lost"),
                            };
                        }
                        if self.block_control_binding {
                            write_new_json(
                                &self.directory.join(CONTROL_FILE),
                                &json!({"occupied": true}),
                            )?;
                        }
                        return Ok(Self::json(json!({
                            "action": if self.proof_reply == Some("wrong_action") { "tab.close" } else { "tab.rename" },
                            "ok": self.proof_reply != Some("not_ok"),
                            "instance_id": if self.proof_reply == Some("wrong_instance") { "wrong-instance" } else { "instance-1" },
                            "window_id": if self.proof_reply == Some("wrong_window") { "wrong-window" } else { self.owned_window() },
                            "tab_id": if self.proof_reply == Some("wrong_tab") { "wrong-tab" } else { "new-tab" },
                        })));
                    }
                    assert!(
                        self.directory.join(CONTROL_FILE).exists(),
                        "binding precedes mutation"
                    );
                    if self.require_session_binding {
                        let binding: TerminalSession = read_record(
                            &self.directory.join("proof-session-binding.json"),
                            "test session binding",
                        )?;
                        assert_eq!(
                            binding,
                            session(),
                            "exact session binding precedes final rename"
                        );
                    }
                    if let Some(code) = self.rename_error {
                        if code == "timeout" {
                            bail!("injected rename timeout");
                        }
                        if code == "malformed" {
                            return Ok(CommandOutput {
                                success: true,
                                stdout: b"{".to_vec(),
                                stderr: Vec::new(),
                            });
                        }
                        return Ok(Self::error(code));
                    }
                    Self::json(json!({
                        "action": "tab.rename", "ok": true, "instance_id": "instance-1",
                        "window_id": self.owned_window(),
                        "tab_id": if self.wrong_rename_identity { "wrong-tab" } else { "new-tab" }
                    }))
                }
                ["tab", "inspect"] => {
                    if args.iter().any(|arg| arg == "--tab-title") {
                        return Ok(Self::error("invalid_selector"));
                    }
                    if let Some(code) = self.inspect_error {
                        Self::error(code)
                    } else if self.tab_present {
                        Self::json(json!({
                            "action": "tab.inspect",
                            "tab": {
                                "tab_id": if self.wrong_inspect_identity { "wrong-tab" } else { "new-tab" },
                                "window_id": self.owned_window()
                            }
                        }))
                    } else {
                        Self::error("stale_target")
                    }
                }
                ["window", "inspect"] => {
                    if self.window_present {
                        Self::json(json!({
                            "action": "window.inspect",
                            "window": {
                                "window_id": if self.wrong_inspect_identity { "wrong-window" } else { "new-window" }
                            }
                        }))
                    } else {
                        Self::error("missing_target")
                    }
                }
                ["tab", "close"] => {
                    if let Some(code) = self.close_error {
                        if self.close_removes_tab {
                            self.tab_present = false;
                        }
                        return Ok(Self::error(code));
                    }
                    if self.cancel_close {
                        Self::error("target_state_conflict")
                    } else {
                        if self.close_removes_tab {
                            self.tab_present = false;
                            if self.siblings.is_empty() && self.owned_window() != "old-window" {
                                self.window_present = false;
                            }
                        }
                        Self::json(json!({
                            "action": "tab.close", "ok": true, "instance_id": "instance-1"
                        }))
                    }
                }
                _ => bail!("unexpected fake Warp command: {args:?}"),
            })
        }

        fn dispatch_uri(
            &mut self,
            client: &ControlClient,
            uri: &str,
            directory: &Path,
            attempt: &str,
            _deadline: Instant,
        ) -> Result<()> {
            let command = dispatch_command(client, uri);
            assert_eq!(command.get_program(), OsStr::new("/usr/bin/open"));
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                vec![OsStr::new("-a"), client.bundle.as_os_str(), OsStr::new(uri)]
            );
            assert_eq!(directory, self.directory);
            assert_eq!(attempt, self.attempt);
            assert!(!self.dispatched, "one URI creation only");
            self.dispatched = true;
            let name = format!("agent-bridge-{attempt}");
            let tab_config = client.config_dir.join(format!("{name}.toml"));
            let launch_config = client
                .config_dir
                .with_file_name("launch_configurations")
                .join(format!("{name}.yaml"));
            // Both routes must carry the same gate in front of the same launch command.
            let plan: HostPlan = read_record(&directory.join(HOST_PLAN_FILE), "plan").unwrap();
            let gated = gated_launch_command(
                &format!(
                    "{} native-warp-host {} {}",
                    shell_quote(std::env::current_exe().unwrap().as_os_str()),
                    shell_quote(directory.as_os_str()),
                    shell_quote(OsStr::new(attempt))
                ),
                &plan.command,
            );
            self.launch_uri = uri == format!("warp://launch/{name}");
            let new_window = if self.launch_uri {
                assert!(!tab_config.exists(), "one creation config only");
                let text = fs::read_to_string(&launch_config).unwrap();
                // Raw, a YAML 1.1 reader folds these as line breaks or refuses them.
                assert!(!text.chars().any(|character| {
                    ('\u{7f}'..='\u{9f}').contains(&character)
                        || "\u{2028}\u{2029}\u{feff}\u{fffe}\u{ffff}".contains(character)
                }));
                // Pinned launch_config.rs:15-21, 37-47, 190-206, 278-290, 364-367.
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap(),
                    json!({
                        "name": name,
                        "active_window_index": 0,
                        "windows": [{
                            "active_tab_index": 0,
                            "tabs": [{
                                "title": format!("agent-bridge-offer-{attempt}"),
                                "layout": {"cwd": directory.to_str().unwrap()},
                                "commands": [{"exec": gated}]
                            }]
                        }]
                    })
                );
                assert_eq!(
                    fs::metadata(&launch_config).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                true
            } else {
                let new_window = uri == format!("warp://tab_config/{name}?new_window=true");
                assert!(new_window || uri == format!("warp://tab_config/{name}"));
                assert!(!launch_config.exists(), "one creation config only");
                let parsed: toml::Value = fs::read_to_string(&tab_config).unwrap().parse().unwrap();
                assert_eq!(
                    parsed
                        .as_table()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    ["name", "panes", "title"]
                );
                assert_eq!(
                    parsed["panes"][0]
                        .as_table()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    ["commands", "directory", "id", "type"]
                );
                assert_eq!(
                    parsed["title"].as_str().unwrap(),
                    format!("agent-bridge-offer-{attempt}")
                );
                assert_eq!(parsed["panes"].as_array().unwrap().len(), 1);
                assert_eq!(parsed["panes"][0]["id"].as_str(), Some("main"));
                assert_eq!(parsed["panes"][0]["type"].as_str(), Some("terminal"));
                assert_eq!(parsed["panes"][0]["directory"].as_str(), directory.to_str());
                assert_eq!(parsed["panes"][0]["commands"].as_array().unwrap().len(), 1);
                assert_eq!(
                    parsed["panes"][0]["commands"][0].as_str(),
                    Some(gated.as_str())
                );
                assert_eq!(
                    fs::metadata(&tab_config).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                new_window
            };
            if self.launch_uri || self.tab_configs_enabled {
                self.created = true;
                self.uri_new_window = new_window && !self.launch_into_existing;
                // A window opened for a Tab Config is an empty workspace, which has a
                // tab of its own beside the configured one (uri/mod.rs:838-849,
                // root_view.rs:1231-1243, workspace/view.rs:4430-4461). A Launch
                // Configuration window holds its template's tabs only (3939-4034).
                self.home_tab = !self.launch_uri && (new_window || self.no_window);
                if self.write_offer {
                    write_new_json(
                        &directory.join(HOST_OFFER_FILE),
                        &HostOffer {
                            schema: RECORD_SCHEMA,
                            attempt: attempt.into(),
                            directory: directory.to_owned(),
                            pid: 77,
                            tty: "/dev/ttys777".into(),
                        },
                    )?;
                }
            }
            if self.dispatch_uncertain {
                bail!("injected lost URI dispatch reply");
            }
            Ok(())
        }

        fn pause(&mut self, _duration: Duration) {}
    }

    fn fixture() -> (tempfile::TempDir, ControlClient, String) {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("Warp.app");
        let executable = bundle.join("Contents/Resources/bin/warpctrl");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        File::create(&executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let bundle = bundle.canonicalize().unwrap();
        let executable = executable.canonicalize().unwrap();
        let client = ControlClient {
            bundle,
            executable,
            inject_warpctrl: false,
            app_id: "dev.warp.Warp-Stable".into(),
            channel: "stable".into(),
            scheme: "warp".into(),
            config_dir: temp.path().join("tab_configs"),
        };
        (temp, client, "0123456789abcdef0123456789abcdef".into())
    }

    fn session() -> TerminalSession {
        TerminalSession {
            kind: TerminalKind::Warp,
            id: "instance-1".into(),
            tab_id: Some("new-tab".into()),
            window_id: Some("new-window".into()),
            managed_session_id: Some("session-test".into()),
            windows_process_identity: None,
            wezterm_mux: None,
        }
    }

    #[test]
    fn production_orchestration_binds_before_releasing_host() {
        let (temp, client, attempt) = fixture();
        let config_dir = client.config_dir.clone();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        let bound = Cell::new(false);
        let result = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: ". launch.sh",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |record| {
                assert_eq!(record.tab_id.as_deref(), Some("new-tab"));
                bound.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert!(bound.get());
        assert_eq!(result.kind, TerminalKind::Warp);
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Start);
        assert_eq!(fs::read_dir(config_dir).unwrap().count(), 0);
    }

    #[test]
    fn global_snapshot_supports_multiple_instances_in_the_selected_bundle() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.multiple_instances = true;
        let result = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |_| Ok(()),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(result.id, "instance-1");
        assert!(runner.calls.iter().any(|call| {
            call.starts_with(&["window".into(), "list".into()])
                && call.last().is_some_and(|value| value == "instance-2")
        }));
    }

    #[test]
    fn missing_host_offer_aborts_without_binding_or_provider_start() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.write_offer = false;
        let bound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_millis(100),
                cleanup_deadline: Instant::now() + Duration::from_secs(1),
                attempt: &attempt,
            },
            |_| {
                bound.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        assert!(!bound.get());
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    #[test]
    fn unavailable_required_control_action_refuses_before_dispatch() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.missing_action = true;
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(1),
                cleanup_deadline: Instant::now() + Duration::from_secs(2),
                attempt: &attempt,
            },
            |_| bail!("must not bind"),
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("tab.close"));
        assert!(!runner.dispatched);
        assert!(!directory.join(HOST_PLAN_FILE).exists());
        assert!(!directory.join(HOST_DECISION_FILE).exists());
    }

    #[test]
    fn ambiguous_delta_aborts_without_mutation_or_bind() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.ambiguous = true;
        let bound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |_| {
                bound.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exactly one new tab"));
        assert!(!bound.get());
        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|value| value == "rename"))
        );
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    #[test]
    fn wrong_claim_identity_is_rejected_after_proven_binding() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.wrong_rename_identity = true;
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |_| Ok(()),
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("wrong identity"));
        assert!(
            runner
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|value| value == "close"))
        );
    }

    #[test]
    fn binding_record_failure_aborts_then_cleans_the_exact_claim() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.block_control_binding = true;
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |_| bail!("must not bind"),
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("control binding"));
        assert!(
            runner
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|value| value == "close"))
        );
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    fn failed_binding_launch_fixture(directory: &Path) -> crate::native::session::Store {
        use crate::native::{SessionState, acquire_turn_claim, launch, update_status};
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(directory, SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        let store = crate::native::session::Store::open_unchecked(directory);
        launch::begin(&store, &token, Instant::now() + Duration::from_secs(30)).unwrap();
        store
    }

    fn failed_binding_launch(
        runner: &mut FakeRunner,
        client: ControlClient,
        attempt: &str,
    ) -> anyhow::Error {
        let directory = runner.directory.clone();
        runner.block_control_binding = true;
        open_bound_tab_with(
            runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt,
            },
            |_| panic!("binding write failed before bind"),
            || panic!("no binding to remove"),
        )
        .unwrap_err()
        .context("adapter creation failed")
    }

    #[test]
    fn failed_binding_cleanup_retains_only_unconfirmed_warp_surface() {
        use crate::native::{
            SessionState, launch, session::close, terminal::RetainedLaunchSurface,
        };
        for cancel_close in [false, true] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            let store = failed_binding_launch_fixture(&directory);
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.cancel_close = cancel_close;
            let error = failed_binding_launch(&mut runner, client, &attempt);
            assert_eq!(
                error.downcast_ref::<RetainedLaunchSurface>().is_some(),
                cancel_close
            );
            launch::terminal_failed(&store, &error).unwrap();
            assert_eq!(store.status().unwrap().state, SessionState::Failed);
            assert_eq!(runner.tab_present, cancel_close);
            assert_eq!(
                runner
                    .calls
                    .iter()
                    .filter(|args| args.get(1).is_some_and(|s| s == "close"))
                    .count(),
                1
            );
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Abort);
            if cancel_close {
                let retained = store.terminal().unwrap();
                assert_eq!(retained, session_with_managed_id("session"));
                let message = store.status().unwrap().error.unwrap();
                for expected in [
                    "instance-1",
                    "new-tab",
                    "new-window",
                    launch::RESIDUAL_SURFACE_MARKER,
                ] {
                    assert!(message.contains(expected), "{message}");
                }
                // A handle alone cannot replace Warp's missing app incarnation/control
                // record. Failed recovery must preserve the handle and failure evidence.
                assert!(
                    close::close(&store, None, |surface| {
                        load_binding(&directory, surface)?;
                        panic!("missing control binding grants no control access")
                    })
                    .is_err()
                );
                assert_eq!(store.terminal().unwrap(), retained);
                assert_eq!(store.status().unwrap().state, SessionState::Failed);
            } else {
                assert!(store.terminal().is_err());
                assert!(
                    !store
                        .status()
                        .unwrap()
                        .error
                        .unwrap()
                        .contains(launch::RESIDUAL_SURFACE_MARKER)
                );
            }
        }
    }

    fn session_with_managed_id(id: &str) -> TerminalSession {
        TerminalSession {
            managed_session_id: Some(id.to_owned()),
            ..session()
        }
    }

    #[test]
    fn failed_binding_warp_close_before_handoff_refuses_pending_creation() {
        use crate::native::{
            SessionState, launch,
            session::{CoreRecord, close},
        };
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        let store = failed_binding_launch_fixture(&directory);
        let before = fs::read(store.record(CoreRecord::Status).path()).unwrap();
        let claim = fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap();
        let error = close::close(&store, None, |_| panic!("no surface bound")).unwrap_err();
        assert!(format!("{error:#}").contains("launcher is still creating the surface"));
        assert_eq!(
            fs::read(store.record(CoreRecord::Status).path()).unwrap(),
            before
        );
        assert_eq!(
            fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap(),
            claim
        );
        assert!(store.closed_if_present().unwrap().is_none());
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.cancel_close = true;
        let error = failed_binding_launch(&mut runner, client, &attempt);
        launch::terminal_failed(&store, &error).unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert_eq!(
            store.terminal().unwrap(),
            session_with_managed_id("session")
        );
    }

    #[test]
    fn failed_binding_warp_late_handoff_keeps_closed_residual_warning() {
        use crate::native::{
            SessionState, launch,
            session::{CoreRecord, close},
        };
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        let store = failed_binding_launch_fixture(&directory);
        let mut pending = launch::read(&store).unwrap().unwrap();
        pending.deadline_unix_ms = 0;
        store
            .record(CoreRecord::Launch)
            .write_json(&pending)
            .unwrap();
        close::close(&store, Some("earlier reason".into()), |_| {
            panic!("no surface bound")
        })
        .unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.cancel_close = true;
        let error = failed_binding_launch(&mut runner, client, &attempt);
        let reported = launch::terminal_failed(&store, &error).unwrap_err();
        let message = format!("{reported:#}");
        for expected in [
            "closed during the launch",
            "instance-1",
            "new-tab",
            "new-window",
            launch::RESIDUAL_SURFACE_MARKER,
        ] {
            assert!(message.contains(expected), "{message}");
        }
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Closed);
        assert!(
            status
                .error
                .as_deref()
                .unwrap()
                .starts_with("earlier reason; ")
        );
        assert!(status.error.as_deref().unwrap().contains(&message));
        assert!(
            fs::read_to_string(directory.join(launch::LOG))
                .unwrap()
                .contains(&message)
        );
        for _ in 0..2 {
            close::close(&store, None, |_| {
                panic!("closed session grants no close authority")
            })
            .unwrap();
            assert!(store.terminal().is_err());
            assert_eq!(store.status().unwrap().error, status.error);
            assert_eq!(
                store.closed_if_present().unwrap().unwrap().error,
                status.error
            );
        }
        assert!(runner.tab_present);
        assert_eq!(
            runner
                .calls
                .iter()
                .filter(|args| args.get(1).is_some_and(|s| s == "close"))
                .count(),
            1
        );
    }

    #[test]
    fn bind_failure_aborts_then_closes_exact_claim() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        let unbound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |_| bail!("injected bind failure"),
            || {
                unbound.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected bind failure"));
        assert!(unbound.get());
        assert!(
            runner
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|value| value == "close"))
        );
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    fn failed_start_fixture(action: Option<HostAction>, cancel_close: bool) -> (bool, bool, bool) {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let durable_binding = directory.join("test-terminal-binding.json");
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.cancel_close = cancel_close;
        let unbound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(2),
                cleanup_deadline: Instant::now() + Duration::from_secs(3),
                attempt: &attempt,
            },
            |record| {
                record.managed_session_id = Some("session-test".into());
                write_new_json(&durable_binding, record)?;
                match action {
                    Some(action) => write_decision(&directory, &attempt, action)?,
                    None => fs::create_dir(directory.join(HOST_DECISION_FILE))?,
                }
                Ok(())
            },
            || {
                unbound.set(true);
                fs::remove_file(&durable_binding)?;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("failed to release the bound Warp host"));
        if let Some(action) = action {
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(
                decision.action, action,
                "exclusive decision must not be overwritten"
            );
        }
        if durable_binding.exists() {
            let retained: TerminalSession = read_record(&durable_binding, "test binding").unwrap();
            assert_eq!(retained, session());
            let binding = load_binding(&directory, &retained).unwrap();
            assert_eq!(binding.attempt, attempt);
        }
        let cleanup_attempted = runner.calls.iter().any(|call| {
            call.starts_with(&["tab".into(), "inspect".into()])
                && call.iter().any(|arg| arg == "--tab")
        });
        let outcome = (unbound.get(), durable_binding.exists(), cleanup_attempted);
        if cancel_close && durable_binding.exists() {
            runner.cancel_close = false;
            assert_eq!(
                close_exact(
                    &mut runner,
                    &load_binding(&directory, &session())
                        .unwrap()
                        .client()
                        .unwrap(),
                    &session(),
                    Instant::now() + Duration::from_secs(1)
                )
                .unwrap(),
                CloseOutcome::Closed,
            );
            fs::remove_file(&durable_binding).unwrap();
        }
        outcome
    }

    #[test]
    fn abort_is_published_before_cleanup_when_no_decision_exists() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = "0123456789abcdef0123456789abcdef";
        let mut guard = HostDecisionGuard::new(temp.path(), attempt);
        guard.abort_before_cleanup().unwrap();
        let decision: HostDecision =
            read_record(&temp.path().join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
        assert!(guard.published);
    }

    #[test]
    fn start_publication_failure_retains_binding_when_exact_cleanup_fails() {
        assert_eq!(
            failed_start_fixture(Some(HostAction::Abort), true),
            (false, true, true)
        );
    }

    #[test]
    fn start_publication_failure_unbinds_after_confirmed_cleanup() {
        assert_eq!(
            failed_start_fixture(Some(HostAction::Abort), false),
            (true, false, true)
        );
    }

    #[test]
    fn start_publication_failure_preserves_existing_start_decision() {
        assert_eq!(
            failed_start_fixture(Some(HostAction::Start), false),
            (false, true, false)
        );
    }

    #[test]
    fn start_publication_failure_does_not_clean_with_unverified_abort() {
        assert_eq!(failed_start_fixture(None, false), (false, true, false));
    }

    #[test]
    fn bind_that_outlives_startup_deadline_aborts_before_provider_start() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        let unbound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_millis(100),
                cleanup_deadline: Instant::now() + Duration::from_secs(1),
                attempt: &attempt,
            },
            |_| {
                thread::sleep(Duration::from_millis(150));
                Ok(())
            },
            || {
                unbound.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deadline expired"));
        assert!(unbound.get());
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    #[test]
    fn absence_is_distinct_from_control_failure() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.tab_present = false;
        assert!(
            !tab_present_with(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap()
        );
        runner.inspect_error = Some("unauthorized");
        assert!(
            tab_present_with(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("unauthorized")
        );
    }

    #[test]
    fn tab_first_regression_close_race_requires_exact_disappearance() {
        for code in ["stale_target", "missing_target"] {
            let (temp, client, attempt) = fixture();
            let mut runner = FakeRunner::new(temp.path(), &attempt);
            runner.close_error = Some(code);
            let outcome = close_exact(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_millis(100),
            );
            eprintln!("tab.close race {code}: {outcome:?}");
            assert_eq!(outcome.unwrap(), CloseOutcome::Closed);
            assert_eq!(runner.calls.last().unwrap()[..2], ["tab", "inspect"]);
        }
    }

    #[test]
    fn tab_first_regression_verified_death_needs_no_control_call() {
        let (temp, client, attempt) = fixture();
        let binding: ControlBinding = serde_json::from_value(json!({
            "schema": RECORD_SCHEMA, "attempt": attempt, "bundle": client.bundle,
            "executable": client.executable, "inject_warpctrl": false,
            "app_id": client.app_id, "channel": client.channel, "scheme": client.scheme,
            "instance_id": "instance-1", "pid": 42, "protocol_version": 1,
            "process_birth": [100, 123],
        }))
        .unwrap();
        let mut runner = FakeRunner::new(temp.path(), &attempt);
        runner.process_birth = None;
        let outcome = bound_instance_present(
            &mut runner,
            &client,
            &binding,
            Instant::now() + Duration::from_secs(1),
        );
        eprintln!(
            "verified process death: {outcome:?}; CLI calls={:?}",
            runner.calls
        );
        assert!(!outcome.unwrap());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn tab_first_verified_death_precedes_missing_bundle_resolution() {
        let (temp, client, attempt) = fixture();
        let binding: ControlBinding = serde_json::from_value(json!({
            "schema": RECORD_SCHEMA, "attempt": attempt, "bundle": client.bundle,
            "executable": client.executable, "inject_warpctrl": false,
            "app_id": client.app_id, "channel": client.channel, "scheme": client.scheme,
            "instance_id": "instance-1", "pid": 42, "protocol_version": 1,
            "process_birth": [100, 123],
        }))
        .unwrap();
        fs::remove_dir_all(&client.bundle).unwrap();
        let mut runner = FakeRunner::new(temp.path(), &attempt);
        runner.process_birth = None;
        assert!(
            binding_client_if_present(
                &mut runner,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap()
            .is_none()
        );
        assert!(runner.calls.is_empty());
        runner.process_birth = Some((100, 123));
        assert!(
            binding_client_if_present(
                &mut runner,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn tab_first_config_schema_round_trips_quoted_paths_and_commands() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("한글 'quoted' \"path\"\nline");
        fs::create_dir(&directory).unwrap();
        let command = "source '/a quoted/path'; printf '%s' \"line\\n\"";
        let path = write_tab_config(
            &client,
            "private-name",
            "private-title",
            &directory,
            &attempt,
            command,
        )
        .unwrap();
        let config: toml::Value = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(config["name"].as_str(), Some("private-name"));
        assert_eq!(config["title"].as_str(), Some("private-title"));
        assert_eq!(config["panes"][0]["directory"].as_str(), directory.to_str());
        assert!(
            config["panes"][0]["commands"][0]
                .as_str()
                .unwrap()
                .ends_with(command)
        );
        assert_eq!(config["panes"][0]["type"].as_str(), Some("terminal"));
        assert_eq!(path.extension().unwrap(), "toml");
    }

    #[test]
    fn tab_first_close_race_error_does_not_itself_prove_absence() {
        for code in ["stale_target", "missing_target", "unauthorized"] {
            let (temp, client, attempt) = fixture();
            let mut runner = FakeRunner::new(temp.path(), &attempt);
            runner.close_error = Some(code);
            runner.close_removes_tab = false;
            let error = close_exact(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_millis(15),
            )
            .unwrap_err();
            if code == "unauthorized" {
                assert!(error.to_string().contains("unauthorized"));
                assert_eq!(runner.calls.len(), 2);
            } else {
                // Either the loop or its inner Control call can observe the deadline.
                let message = error.to_string();
                assert!(
                    message.contains("remains present")
                        || message.contains("deadline is exhausted"),
                    "{message}"
                );
                assert!(runner.calls.len() > 2);
            }
        }
    }

    #[test]
    fn tab_first_existing_window_and_explicit_new_window_and_no_workspace() {
        for (no_window, force_new_window) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.reuse_window = true;
            runner.no_window = no_window;
            let bound = Cell::new(false);
            let result = open_bound_tab_with(
                &mut runner,
                OpenRequest {
                    clients: std::slice::from_ref(&client),
                    command: "ignored",
                    directory: &directory,
                    deadline: Instant::now() + Duration::from_secs(1),
                    cleanup_deadline: Instant::now() + Duration::from_secs(2),
                    attempt: &attempt,
                    force_new_window,
                },
                |session| {
                    assert!(directory.join(CONTROL_FILE).exists());
                    assert_eq!(
                        session.window_id.as_deref(),
                        Some(if no_window || force_new_window {
                            "new-window"
                        } else {
                            "old-window"
                        })
                    );
                    bound.set(true);
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
            assert!(bound.get());
            assert_eq!(runner.proof_calls, 1);
            // A window is a Launch Configuration; only a tab is a Tab Config.
            assert_eq!(runner.launch_uri, no_window || force_new_window);
            assert!(!creation_config_remains(&client, &attempt));
            close_exact(
                &mut runner,
                &client,
                &result,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            if !no_window && !force_new_window {
                assert!(
                    runner.window_present,
                    "existing workspace and sibling tab remain"
                );
            }
            assert!(
                !runner
                    .calls
                    .iter()
                    .any(|call| call.first().is_some_and(|s| s == "window")
                        && call.get(1).is_some_and(|s| s == "close"))
            );
            assert!(
                runner
                    .calls
                    .iter()
                    .filter(|call| call.get(1).is_some_and(|s| s == "close"))
                    .all(|call| call.last().is_some_and(|s| s == "new-tab"))
            );
        }
    }

    #[test]
    fn tab_first_concurrent_delta_in_existing_window_never_grants_authority() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.reuse_window = true;
        runner.ambiguous = true;
        let bound = Cell::new(false);
        assert!(proof_open_fixture(&mut runner, &client, &directory, &attempt, &bound).is_err());
        assert!(!bound.get());
        assert_eq!(runner.proof_calls, 0);
        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|s| s == "rename" || s == "close"))
        );
    }

    #[test]
    fn tab_first_uncertain_dispatch_and_unknown_capability_never_retry_creation() {
        for uncertain in [false, true] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.dispatch_uncertain = uncertain;
            runner.write_offer = false;
            let bound = Cell::new(false);
            let error =
                proof_open_fixture(&mut runner, &client, &directory, &attempt, &bound).unwrap_err();
            assert!(runner.dispatched && !runner.launch_uri && !bound.get());
            assert!(
                runner
                    .calls
                    .iter()
                    .all(|call| call.get(1).is_none_or(|s| s != "rename" && s != "close"))
            );
            if !uncertain {
                assert!(format!("{error:#}").contains("feature enablement is unverified"));
            }
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Abort);
        }
    }

    #[test]
    fn inspect_responses_must_name_the_exact_recorded_targets() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.wrong_inspect_identity = true;

        assert!(
            tab_present_with(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("wrong exact target identity")
        );
        assert!(
            window_present_with(
                &mut runner,
                &client,
                &session(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("wrong exact target identity")
        );
    }

    #[test]
    fn bound_instance_rejects_pid_reuse_and_distinguishes_actual_absence() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        let mut binding = ControlBinding {
            schema: RECORD_SCHEMA,
            attempt,
            bundle: client.bundle.clone(),
            executable: client.executable.clone(),
            inject_warpctrl: client.inject_warpctrl,
            app_id: client.app_id.clone(),
            channel: client.channel.clone(),
            scheme: client.scheme.clone(),
            instance_id: "instance-1".into(),
            pid: 99,
            protocol_version: 1,
            process_birth: Some((100, 123)),
        };
        let error = bound_instance_present(
            &mut runner,
            &client,
            &binding,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity no longer matches"));

        binding.pid = 42;
        runner.instance_inspect_error = Some("no_instance");
        runner.process_birth = None;
        assert!(
            !bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap()
        );
    }

    #[test]
    fn close_preserves_user_siblings_without_requiring_window_disappearance() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.siblings.push("user-tab".into());
        let outcome = close_exact(
            &mut runner,
            &client,
            &session(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(outcome, CloseOutcome::Closed);
        assert!(runner.window_present);
        assert_eq!(runner.siblings, ["user-tab"]);
        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.first().is_some_and(|value| value == "window")
                    && call.get(1).is_some_and(|value| value == "close"))
        );
    }

    #[test]
    fn close_warning_cancellation_is_an_error_and_record_corruption_is_rejected() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.cancel_close = true;
        let error = close_exact(
            &mut runner,
            &client,
            &session(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("target_state_conflict"));

        let corrupt = directory.join(CONTROL_FILE);
        fs::write(&corrupt, b"not-json").unwrap();
        assert!(read_record::<ControlBinding>(&corrupt, "binding").is_err());
        fs::remove_file(&corrupt).unwrap();
        assert!(read_record::<ControlBinding>(&corrupt, "binding").is_err());
    }

    #[test]
    fn close_ack_is_not_closure_until_exact_tab_disappears() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.close_removes_tab = false;
        let error = close_exact(
            &mut runner,
            &client,
            &session(),
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap_err();
        // Both deadline observations retain the surface; an acknowledgement is not absence.
        let message = error.to_string();
        assert!(
            message.contains("remains present") || message.contains("deadline is exhausted"),
            "{message}"
        );

        runner.close_removes_tab = true;
        let outcome = close_exact(
            &mut runner,
            &client,
            &session(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(outcome, CloseOutcome::Closed);
    }

    #[test]
    fn bundled_discovery_and_dispatch_ignore_invoking_terminal() {
        const CASE_ENV: &str = "AGENT_BRIDGE_WARP_HOST_FIXTURE";
        const TEST_NAME: &str = "native::terminal::macos::warp::tests::bundled_discovery_and_dispatch_ignore_invoking_terminal";
        let cases = [
            ("warp", Some("WarpTerminal"), false),
            ("iterm", Some("iTerm.app"), false),
            ("terminal", Some("Apple_Terminal"), false),
            ("wezterm", Some("WezTerm"), true),
            ("empty", None, false),
            ("unknown", Some("unknown-host"), false),
            ("conflict", Some("iTerm.app"), true),
        ];
        let Some(case) = std::env::var_os(CASE_ENV) else {
            // Child test processes isolate simulated host variables from this test
            // harness and from concurrent tests; no global environment is changed.
            for (name, term_program, conflict) in cases {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child.args(["--exact", TEST_NAME, "--test-threads=1", "--nocapture"]);
                child.env(CASE_ENV, name);
                for variable in [
                    "TERM_PROGRAM",
                    "TERM",
                    "ITERM_SESSION_ID",
                    "TERM_SESSION_ID",
                    "WEZTERM_PANE",
                    "WEZTERM_UNIX_SOCKET",
                    "WEZTERM_UNIX_DOMAIN",
                    "WEZTERM_EXECUTABLE",
                ] {
                    child.env_remove(variable);
                }
                if let Some(term_program) = term_program {
                    child.env("TERM_PROGRAM", term_program);
                }
                if name == "iterm" || name == "conflict" {
                    child.env("ITERM_SESSION_ID", "caller-iterm-session");
                }
                if name == "terminal" || name == "conflict" {
                    child.env("TERM_SESSION_ID", "caller-terminal-session");
                }
                if conflict {
                    child.env("WEZTERM_PANE", "unrelated-caller-pane");
                    child.env("WEZTERM_UNIX_SOCKET", "/not-an-owned-mux/socket");
                    child.env("WEZTERM_UNIX_DOMAIN", "unrelated-remote-domain");
                    child.env("WEZTERM_EXECUTABLE", "/not-an-owned-app/wezterm");
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "host case {name}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let case = case.to_str().unwrap();
        let (_, expected_term, _) = cases.iter().find(|(name, _, _)| *name == case).unwrap();
        assert_eq!(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            *expected_term
        );
        assert_eq!(
            crate::native::terminal::select(Some(TerminalKind::Warp)).unwrap(),
            TerminalKind::Warp
        );

        // Exercise both documented bundled routes without executing either file.
        for embedded in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let config_home = temp.path().join("config-home");
            let roots = [
                temp.path().join("system-applications"),
                temp.path().join("user-applications"),
            ];
            let bundle = roots[usize::from(embedded)].join("Warp.app");
            let executable = bundle.join(if embedded {
                "Contents/MacOS/stable"
            } else {
                "Contents/Resources/bin/warpctrl"
            });
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            // The fake reachable app has initialized its channel configuration root.
            fs::create_dir_all(config_home.join(".warp")).unwrap();
            File::create(&executable).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let clients = control_clients_in(&roots, &config_home).unwrap();
            assert_eq!(clients.len(), 1);
            assert_eq!(clients[0].bundle, bundle.canonicalize().unwrap());
            assert_eq!(clients[0].executable, executable.canonicalize().unwrap());
            assert_eq!(clients[0].inject_warpctrl, embedded);
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let attempt = "0123456789abcdef0123456789abcdef";
            let mut runner = FakeRunner::new(&directory, attempt);
            let result = open_bound_tab_with(
                &mut runner,
                OpenRequest {
                    force_new_window: false,
                    clients: &clients,
                    command: "ignored",
                    directory: &directory,
                    deadline: Instant::now() + Duration::from_secs(2),
                    cleanup_deadline: Instant::now() + Duration::from_secs(3),
                    attempt,
                },
                |record| {
                    assert_eq!(record.kind, TerminalKind::Warp);
                    assert_eq!(record.tab_id.as_deref(), Some("new-tab"));
                    assert_eq!(record.window_id.as_deref(), Some("new-window"));
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
            assert!(runner.dispatched);
            assert_eq!(result.kind, TerminalKind::Warp);
            assert_ne!(result.tab_id.as_deref(), Some("unrelated-caller-pane"));
            let binding = load_binding(&directory, &result).unwrap();
            assert_eq!(binding.executable, executable.canonicalize().unwrap());
            assert_eq!(binding.app_id, "dev.warp.Warp-Stable");
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Start);
        }
    }

    #[test]
    fn bundle_resolver_accepts_embedded_control_mode_without_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("Warp.app");
        let embedded = bundle.join("Contents/MacOS/stable");
        fs::create_dir_all(embedded.parent().unwrap()).unwrap();
        File::create(&embedded).unwrap();
        fs::set_permissions(&embedded, fs::Permissions::from_mode(0o700)).unwrap();
        let client = client_for_bundle(&bundle, BUNDLE_SPECS[0], temp.path())
            .unwrap()
            .unwrap();
        assert!(client.inject_warpctrl);
        assert_eq!(client.executable, embedded.canonicalize().unwrap());
    }

    #[test]
    fn preview_style_user_owned_config_symlink_resolves_to_private_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("stable-launch-configurations");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let preview = temp.path().join("preview-launch-configurations");
        symlink(&target, &preview).unwrap();
        assert_eq!(
            ensure_private_config_directory(&preview).unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[test]
    fn warp_input_is_never_staged() {
        let failure = send_file(&session(), Path::new("ignored"), Instant::now()).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert!(failure.error().to_string().contains("cannot submit"));
    }

    // This child executes the real host/attestation functions in its own PTY process.
    #[test]
    fn warp_r2_owned_process_probe() {
        let Ok(mode) = std::env::var("WARP_R2_PROBE_MODE") else {
            return;
        };
        let directory = PathBuf::from(std::env::var_os("WARP_R2_PROBE_DIR").unwrap());
        let attempt = "0123456789abcdef0123456789abcdef";
        if mode == "host" {
            if let Err(error) = run_host(&directory, attempt) {
                eprintln!("{error:#}");
                std::process::exit(7);
            }
        } else {
            let result = ownership::current_native_session_owner("session-warp-r2-probe");
            let value = match result {
                Ok(owner) => json!({"ok": true, "pid": owner.pid, "group": owner.process_group}),
                Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
            };
            write_new_json(&directory.join("owner-probe.json"), &value).unwrap();
        }
    }

    #[test]
    fn warp_r2_real_host_and_owner_attestation_through_private_pty() {
        real_host_fixture(Some(HostAction::Start), false);
        real_host_fixture(Some(HostAction::Abort), false);
        real_host_fixture(None, false);
        real_host_fixture(Some(HostAction::Start), true);
    }

    fn real_host_fixture(action: Option<HostAction>, invalid_plan: bool) {
        use std::os::fd::{AsRawFd, FromRawFd};
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("session-warp-r2-probe");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::env::current_exe().unwrap();
        let probe = format!(
            "{} --exact native::terminal::macos::warp::tests::warp_r2_owned_process_probe --nocapture --test-threads=1",
            shell_quote(executable.as_os_str())
        );
        let owner_command = format!("WARP_R2_PROBE_MODE=owner {probe}");
        write_new_json(
            &directory.join(HOST_PLAN_FILE),
            &HostPlan {
                schema: RECORD_SCHEMA,
                attempt: "0123456789abcdef0123456789abcdef".into(),
                command: format!("{owner_command}; :"),
                deadline_unix_ms: wall_ms().unwrap() + 2000,
                decision_timeout_ms: 200,
            },
        )
        .unwrap();
        if invalid_plan {
            fs::write(directory.join(HOST_PLAN_FILE), b"{").unwrap();
        }
        if let Some(action) = action {
            write_decision(&directory, "0123456789abcdef0123456789abcdef", action).unwrap();
        }
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let shell_command = format!(
            "{}; exit",
            gated_launch_command(&format!("WARP_R2_PROBE_MODE=host {probe}"), &owner_command)
        );
        let mut command = Command::new("/bin/zsh");
        command
            .args(["-f", "-i", "-c", &shell_command])
            .env("WARP_R2_PROBE_DIR", &directory)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        // Drain concurrently so a full PTY never blocks the owned child.
        let fd = master.as_raw_fd();
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut master = master;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut output = Vec::new();
        loop {
            let mut bytes = [0u8; 4096];
            if let Ok(n) = master.read(&mut bytes) {
                output.extend_from_slice(&bytes[..n]);
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!(
                    "owned host fixture timed out: {}",
                    String::from_utf8_lossy(&output)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        if action != Some(HostAction::Start) || invalid_plan {
            assert!(
                !directory.join("owner-probe.json").exists(),
                "aborted or timed-out gate must exit its launch shell"
            );
            assert!(!child.wait().unwrap().success());
            return;
        }
        let value: Value =
            serde_json::from_slice(&fs::read(directory.join("owner-probe.json")).unwrap()).unwrap();
        eprintln!(
            "actual host/current_native_session_owner: {value}; PTY: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            value["ok"], true,
            "real host must preserve a separate foreground native-session job: {value}"
        );
    }

    #[test]
    fn warp_r2_rename_failure_retains_proven_binding_and_aborts() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.wrong_rename_identity = true;
        runner.cancel_close = true;
        let bound = Cell::new(false);
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(1),
                cleanup_deadline: Instant::now() + Duration::from_secs(2),
                attempt: &attempt,
            },
            |_| {
                bound.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(
            bound.get(),
            "title proof must be durably bound before rename: {error:#}"
        );
        assert!(directory.join(CONTROL_FILE).exists());
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort);
    }

    #[test]
    fn warp_r2_proof_official_route_binds_before_final_rename() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.require_session_binding = true;
        let result = open_bound_tab_with(
            &mut runner,
            OpenRequest {
                force_new_window: false,
                clients: &[client],
                command: "ignored",
                directory: &directory,
                deadline: Instant::now() + Duration::from_secs(1),
                cleanup_deadline: Instant::now() + Duration::from_secs(2),
                attempt: &attempt,
            },
            |record| {
                assert!(directory.join(CONTROL_FILE).exists());
                assert!(!directory.join(HOST_DECISION_FILE).exists());
                record.managed_session_id = Some("session-test".into());
                write_new_json(&directory.join("proof-session-binding.json"), record)?;
                Ok(())
            },
            || Ok(()),
        );
        assert!(
            result.is_ok(),
            "pinned official same-title rename must provide proof instead of unsupported title inspect: {result:?}"
        );
        assert_eq!(runner.proof_executed, 1);
        let mutations = runner
            .calls
            .iter()
            .filter(|args| args.get(1).is_some_and(|arg| arg == "rename"))
            .collect::<Vec<_>>();
        assert_eq!(mutations.len(), 2);
        assert!(mutations[0].iter().any(|arg| arg == "--tab-title"));
        assert!(mutations[1].iter().any(|arg| arg == "--tab"));
    }

    #[test]
    fn warp_r2_rename_errors_fence_and_retain_exact_retry() {
        for code in ["target_state_conflict", "malformed", "timeout"] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.rename_error = Some(code);
            runner.cancel_close = true;
            let bound = Cell::new(false);
            let unbound = Cell::new(false);
            let error = open_bound_tab_with(
                &mut runner,
                OpenRequest {
                    force_new_window: false,
                    clients: std::slice::from_ref(&client),
                    command: "ignored",
                    directory: &directory,
                    deadline: Instant::now() + Duration::from_secs(1),
                    cleanup_deadline: Instant::now() + Duration::from_secs(2),
                    attempt: &attempt,
                },
                |_| {
                    bound.set(true);
                    Ok(())
                },
                || {
                    unbound.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(bound.get() && !unbound.get(), "{code}: {error:#}");
            let binding: ControlBinding =
                read_record(&directory.join(CONTROL_FILE), "binding").unwrap();
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Abort);
            runner.cancel_close = false;
            assert_eq!(
                close_bound_exact(
                    &mut runner,
                    &client,
                    &session(),
                    &binding,
                    Instant::now() + Duration::from_secs(1)
                )
                .unwrap(),
                CloseOutcome::Closed
            );
        }
    }

    #[test]
    fn warp_r2_app_birth_and_official_no_instance_semantics() {
        let (temp, client, attempt) = fixture();
        let mut runner = FakeRunner::new(temp.path(), &attempt);
        let mut binding = ControlBinding {
            schema: RECORD_SCHEMA,
            attempt,
            bundle: client.bundle.clone(),
            executable: client.executable.clone(),
            inject_warpctrl: false,
            app_id: client.app_id.clone(),
            channel: client.channel.clone(),
            scheme: client.scheme.clone(),
            instance_id: "instance-1".into(),
            pid: 42,
            protocol_version: 1,
            process_birth: Some((100, 123)),
        };
        runner.instance_inspect_error = Some("no_instance");
        assert!(
            bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        runner.process_birth = None;
        assert!(
            !bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap()
        );
        runner.process_birth = Some((101, 0));
        let calls = runner.calls.len();
        assert!(
            bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert_eq!(
            runner.calls.len(),
            calls,
            "reused app must never be controlled"
        );
        runner.birth_unreadable = true;
        assert!(
            bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        runner.birth_unreadable = false;
        binding.process_birth = None;
        assert!(
            bound_instance_present(
                &mut runner,
                &client,
                &binding,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn warp_r2_oss_channel_matches_pinned_upstream_display() {
        let spec = BUNDLE_SPECS
            .iter()
            .find(|spec| spec.bundle_name == "WarpOss.app")
            .unwrap();
        // crates/warp_core/src/channel/mod.rs:75-83 at d5d23e6119b39edb8c95580a60b68fcd4a2d5a1b.
        assert_eq!(spec.channel, "warp-oss");
    }

    struct ProofBirthRunner {
        fake: FakeRunner,
        injected_birth: Option<Option<(u64, u64)>>,
    }

    impl WarpRunner for ProofBirthRunner {
        fn process_birth(&mut self, pid: u32) -> Result<Option<(u64, u64)>> {
            match self.injected_birth {
                Some(birth) => Ok(birth),
                None => process::macos_process_start(pid),
            }
        }
        fn control(
            &mut self,
            client: &ControlClient,
            args: &[String],
            deadline: Instant,
        ) -> Result<CommandOutput> {
            self.fake.control(client, args, deadline)
        }
        fn dispatch_uri(
            &mut self,
            client: &ControlClient,
            uri: &str,
            directory: &Path,
            attempt: &str,
            deadline: Instant,
        ) -> Result<()> {
            self.fake
                .dispatch_uri(client, uri, directory, attempt, deadline)
        }
    }

    #[test]
    fn warp_r2_proof_regression_official_no_instance_after_verified_death() {
        let (temp, client, attempt) = fixture();
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let birth_result = process::macos_process_start(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        let birth = birth_result.unwrap().expect("owned probe alive");
        assert_eq!(
            process::macos_process_start(pid).unwrap(),
            None,
            "owned probe death must be verified independently"
        );
        let binding: ControlBinding = serde_json::from_value(json!({
            "schema": RECORD_SCHEMA, "attempt": attempt, "bundle": client.bundle, "executable": client.executable,
            "inject_warpctrl": false, "app_id": client.app_id, "channel": client.channel, "scheme": client.scheme,
            "instance_id": "instance-1", "pid": pid, "protocol_version": 1, "process_birth": birth,
        })).unwrap();
        let mut runner = ProofBirthRunner {
            fake: FakeRunner::new(temp.path(), &attempt),
            injected_birth: None,
        };
        runner.fake.instance_inspect_error = Some("no_instance");
        let observed = bound_instance_present(
            &mut runner,
            &client,
            &binding,
            Instant::now() + Duration::from_secs(1),
        );
        eprintln!(
            "verified owned process death pid={pid} birth={birth:?}; official no_instance result={observed:?}"
        );
        assert!(
            !observed.unwrap(),
            "verified dead recorded process must establish absence without any CLI"
        );
        assert!(runner.fake.calls.is_empty());
    }

    #[test]
    fn warp_r2_proof_regression_reused_app_pid_never_controls() {
        let (temp, client, attempt) = fixture();
        let binding: ControlBinding = serde_json::from_value(json!({
            "schema": RECORD_SCHEMA, "attempt": attempt, "bundle": client.bundle, "executable": client.executable,
            "inject_warpctrl": false, "app_id": client.app_id, "channel": client.channel, "scheme": client.scheme,
            "instance_id": "instance-1", "pid": 42, "protocol_version": 1, "process_birth": [100, 123],
        })).unwrap();
        let mut runner = ProofBirthRunner {
            fake: FakeRunner::new(temp.path(), &attempt),
            injected_birth: Some(Some((101, 0))),
        };
        let observed = bound_instance_present(
            &mut runner,
            &client,
            &binding,
            Instant::now() + Duration::from_secs(1),
        );
        eprintln!(
            "reused PID fixture old=(100,123) current=(101,0); result={observed:?}; control calls={:?}",
            runner.fake.calls
        );
        assert!(
            observed.is_err(),
            "reused PID must never be accepted by matching endpoint metadata"
        );
        assert!(
            runner.fake.calls.is_empty(),
            "reused PID must fail before any app control"
        );
    }

    fn proof_open_fixture(
        runner: &mut FakeRunner,
        client: &ControlClient,
        directory: &Path,
        attempt: &str,
        bound: &Cell<bool>,
    ) -> Result<TerminalSession> {
        mode_open_fixture(runner, client, directory, attempt, bound, false)
    }

    fn mode_open_fixture(
        runner: &mut FakeRunner,
        client: &ControlClient,
        directory: &Path,
        attempt: &str,
        bound: &Cell<bool>,
        force_new_window: bool,
    ) -> Result<TerminalSession> {
        open_bound_tab_with(
            runner,
            OpenRequest {
                force_new_window,
                clients: std::slice::from_ref(client),
                command: "ignored",
                directory,
                deadline: Instant::now() + Duration::from_millis(450),
                cleanup_deadline: Instant::now() + Duration::from_secs(1),
                attempt,
            },
            |_| {
                assert!(directory.join(CONTROL_FILE).exists());
                assert!(!directory.join(HOST_DECISION_FILE).exists());
                bound.set(true);
                Ok(())
            },
            || Ok(()),
        )
    }

    #[test]
    fn warp_r2_proof_uncertain_executed_reply_reacquires_same_title() {
        for code in ["lost", "malformed", "timeout"] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.proof_failures = 1;
            runner.proof_reply = Some(code);
            let bound = Cell::new(false);
            proof_open_fixture(&mut runner, &client, &directory, &attempt, &bound).unwrap();
            assert!(bound.get());
            assert_eq!(runner.proof_calls, 2);
            assert_eq!(runner.proof_executed, 2);
            assert_eq!(
                runner.offered_title,
                format!("agent-bridge-offer-{attempt}")
            );
            assert!(
                !runner
                    .calls
                    .iter()
                    .any(|args| args.get(1).is_some_and(|arg| arg == "close"))
            );
        }
    }

    #[test]
    fn warp_r2_proof_wrong_identity_or_ack_never_binds_or_closes() {
        for code in [
            "wrong_instance",
            "wrong_window",
            "wrong_tab",
            "wrong_action",
            "not_ok",
        ] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.proof_reply = Some(code);
            let bound = Cell::new(false);
            let error =
                proof_open_fixture(&mut runner, &client, &directory, &attempt, &bound).unwrap_err();
            assert!(format!("{error:#}").contains("wrong exact target"));
            assert!(!bound.get() && !directory.join(CONTROL_FILE).exists());
            assert_eq!(runner.proof_calls, 1);
            assert!(
                !runner
                    .calls
                    .iter()
                    .any(|args| args.get(1).is_some_and(|arg| arg == "close")
                        || args.iter().any(|arg| arg == "--tab")
                            && args.get(1).is_some_and(|arg| arg == "rename"))
            );
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Abort);
        }
    }

    #[test]
    fn warp_r2_proof_ambiguous_removed_title_and_exhausted_reply_preserve_unproven_surface() {
        for code in [
            "ambiguous_target",
            "missing_target",
            "removed",
            "lost",
            "timeout",
        ] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            match code {
                "removed" => runner.offered_title = "user-changed-title".into(),
                "lost" | "timeout" => {
                    runner.proof_failures = 99;
                    runner.proof_reply = Some(code);
                }
                _ => runner.proof_selector_error = Some(code),
            }
            let bound = Cell::new(false);
            let error =
                proof_open_fixture(&mut runner, &client, &directory, &attempt, &bound).unwrap_err();
            assert!(error.to_string().contains("residual launch tab"));
            assert!(!bound.get() && !directory.join(CONTROL_FILE).exists());
            assert!(runner.proof_calls <= 3);
            if code == "lost" {
                assert_eq!(runner.proof_calls, 3);
            }
            assert!(
                !runner
                    .calls
                    .iter()
                    .any(|args| args.get(1).is_some_and(|arg| arg == "close"))
            );
            let decision: HostDecision =
                read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
            assert_eq!(decision.action, HostAction::Abort);
        }
    }

    fn creation_config_remains(client: &ControlClient, attempt: &str) -> bool {
        client
            .config_dir
            .join(format!("agent-bridge-{attempt}.toml"))
            .exists()
            || client
                .config_dir
                .with_file_name("launch_configurations")
                .join(format!("agent-bridge-{attempt}.yaml"))
                .exists()
    }

    fn assert_unproven_surface_untouched(runner: &FakeRunner, directory: &Path, case: &str) {
        assert!(
            runner.calls.iter().all(|call| call
                .get(1)
                .is_none_or(|action| action != "rename" && action != "close")),
            "{case}: an unproven surface was mutated"
        );
        assert!(!directory.join(CONTROL_FILE).exists(), "{case}");
        let decision: HostDecision =
            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
        assert_eq!(decision.action, HostAction::Abort, "{case}");
    }

    // R3 finding 7. A window asked for, or needed because no window can take a tab,
    // must not depend on TabConfigs, and must hold the launch tab alone.
    #[test]
    fn g4_r20_window_route_is_the_ungated_launch_configuration() {
        let mut failures = Vec::new();
        for (no_window, force_new_window) in [(false, true), (true, true), (true, false)] {
            for tab_configs_enabled in [false, true] {
                let case = format!(
                    "no_window={no_window} force_new_window={force_new_window} tab_configs_enabled={tab_configs_enabled}"
                );
                let (temp, client, attempt) = fixture();
                let directory = temp.path().join("session");
                fs::create_dir(&directory).unwrap();
                let mut runner = FakeRunner::new(&directory, &attempt);
                runner.reuse_window = true;
                runner.no_window = no_window;
                runner.tab_configs_enabled = tab_configs_enabled;
                let bound = Cell::new(false);
                match mode_open_fixture(
                    &mut runner,
                    &client,
                    &directory,
                    &attempt,
                    &bound,
                    force_new_window,
                ) {
                    Ok(session) => {
                        assert!(bound.get() && runner.launch_uri, "{case}");
                        assert_eq!(session.window_id.as_deref(), Some("new-window"), "{case}");
                        assert_eq!(session.tab_id.as_deref(), Some("new-tab"), "{case}");
                        let decision: HostDecision =
                            read_record(&directory.join(HOST_DECISION_FILE), "decision").unwrap();
                        assert_eq!(decision.action, HostAction::Start, "{case}");
                    }
                    Err(error) => failures.push(format!("{case}: {error:#}")),
                }
                assert!(!creation_config_remains(&client, &attempt), "{case}");
            }
        }
        eprintln!("window route failures: {failures:#?}");
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // The tab route stays the first choice while a window exists. Its uncertainty is
    // never read as "no tab capability": one dispatch, no window, no mutation.
    #[test]
    fn g4_r20_uncertain_tab_request_never_becomes_a_window() {
        for case in ["tab_configs_off", "no_offer", "lost_reply", "ambiguous"] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.reuse_window = true;
            match case {
                "tab_configs_off" => runner.tab_configs_enabled = false,
                "no_offer" => runner.write_offer = false,
                "lost_reply" => runner.dispatch_uncertain = true,
                _ => runner.ambiguous = true,
            }
            let bound = Cell::new(false);
            let error =
                mode_open_fixture(&mut runner, &client, &directory, &attempt, &bound, false)
                    .unwrap_err();
            // The fake refuses a second dispatch, so this is the only creation request.
            assert!(
                runner.dispatched && !runner.launch_uri && !bound.get(),
                "{case}: {error:#}"
            );
            assert_unproven_surface_untouched(&runner, &directory, case);
            assert!(!creation_config_remains(&client, &attempt), "{case}");
            if case == "tab_configs_off" {
                assert!(format!("{error:#}").contains("feature enablement is unverified"));
            }
        }
    }

    #[test]
    fn g4_r20_window_route_partial_transitions_preserve_the_unproven_surface() {
        for case in ["no_offer", "lost_reply", "existing_window", "ambiguous"] {
            let (temp, client, attempt) = fixture();
            let directory = temp.path().join("session");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.reuse_window = true;
            match case {
                "no_offer" => runner.write_offer = false,
                "lost_reply" => runner.dispatch_uncertain = true,
                "existing_window" => runner.launch_into_existing = true,
                _ => runner.ambiguous = true,
            }
            let bound = Cell::new(false);
            let error = mode_open_fixture(&mut runner, &client, &directory, &attempt, &bound, true)
                .unwrap_err();
            assert!(
                runner.launch_uri,
                "{case}: a window must be requested through the Launch Configuration: {error:#}"
            );
            assert!(runner.dispatched && !bound.get(), "{case}: {error:#}");
            assert_unproven_surface_untouched(&runner, &directory, case);
            assert!(!creation_config_remains(&client, &attempt), "{case}");
            if case == "existing_window" {
                assert!(format!("{error:#}").contains("new-window request"));
            }
        }
    }

    #[test]
    fn g4_r20_creation_configs_carry_quoted_and_unicode_text_exactly() {
        for force_new_window in [false, true] {
            let (temp, client, attempt) = fixture();
            // NEL, LS and DEL are what a YAML 1.1 or TOML reader treats unlike JSON.
            let directory = temp
                .path()
                .join("한글 'quoted' \"path\"\nline \u{85}\u{2028}\u{7f} end");
            fs::create_dir(&directory).unwrap();
            let mut runner = FakeRunner::new(&directory, &attempt);
            runner.reuse_window = true;
            // The fake compares the parsed document with this directory and command.
            let result = open_bound_tab_with(
                &mut runner,
                OpenRequest {
                    force_new_window,
                    clients: std::slice::from_ref(&client),
                    command: "source '/a quoted/path'; printf '%s' \"line\\n\"",
                    directory: &directory,
                    deadline: Instant::now() + Duration::from_secs(1),
                    cleanup_deadline: Instant::now() + Duration::from_secs(2),
                    attempt: &attempt,
                },
                |_| Ok(()),
                || Ok(()),
            );
            assert_eq!(runner.launch_uri, force_new_window, "{result:?}");
            result.unwrap();
        }
    }
}
