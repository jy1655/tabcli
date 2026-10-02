use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
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

    fn pause(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

struct ProcessRunner;

impl WarpRunner for ProcessRunner {
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
}

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
    let clients = installed_control_clients()?;
    let attempt = random_token()?;
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("Warp startup timeout leaves no room for exact surface cleanup")?;
    open_bound_tab_with(
        &mut ProcessRunner,
        OpenRequest {
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

    let (deadline_unix_ms, decision_timeout_ms) = host_deadlines(deadline)?;
    let plan = HostPlan {
        schema: RECORD_SCHEMA,
        attempt: attempt.to_owned(),
        command: command.to_owned(),
        deadline_unix_ms,
        decision_timeout_ms,
    };
    write_new_json(&directory.join(HOST_PLAN_FILE), &plan)?;
    let mut decision = HostDecisionGuard::new(directory, attempt);

    let config_name = format!("agent-bridge-{attempt}");
    let offer_title = format!("agent-bridge-offer-{attempt}");
    let bound_title = format!("agent-bridge-{attempt}");
    let config_path = write_launch_config(&client, &config_name, &offer_title, directory, attempt)?;
    let _config_guard = LaunchConfigGuard(config_path);
    let uri = format!("{}://launch/{config_name}", client.scheme);
    runner.dispatch_uri(&client, &uri, directory, attempt, deadline)?;

    let offer = wait_for_offer(runner, directory, attempt, deadline)?;
    let after_instances = list_matching_instances(runner, &client, deadline)?;
    if instance_map(&after_instances)? != instance_map(&before_instances)? {
        decision.publish(HostAction::Abort)?;
        bail!("Warp instance identity changed during launch; no surface was mutated");
    }
    let after = snapshot_instances(runner, &client, &after_instances, deadline)?;
    let mut new_windows = Vec::new();
    for (instance_id, snapshot) in &after.instances {
        let prior = before
            .instances
            .get(instance_id)
            .context("Warp instance appeared during launch")?;
        for (window_id, tabs) in &snapshot.windows {
            if !prior.windows.contains_key(window_id) {
                new_windows.push((instance_id.clone(), window_id.clone(), tabs.clone()));
            }
        }
    }
    if new_windows.len() != 1 || new_windows[0].2.len() != 1 {
        decision.publish(HostAction::Abort)?;
        bail!(
            "Warp launch did not produce exactly one new window with one tab; no surface was mutated"
        );
    }
    let (instance_id, window_id, tabs) = new_windows.pop().expect("one new window");
    let tab_id = tabs.into_iter().next().expect("one tab");
    let instance = after_instances
        .iter()
        .find(|instance| instance.instance_id == instance_id)
        .context("new Warp window has no matching instance identity")?;

    let renamed: RenameResponse = control_json(
        runner,
        &client,
        &[
            "tab".into(),
            "rename".into(),
            "--instance".into(),
            instance_id.clone(),
            "--window".into(),
            window_id.clone(),
            "--tab-title".into(),
            offer_title,
            bound_title,
        ],
        deadline,
    )
    .map_err(anyhow::Error::new)?;
    if !renamed.ok
        || renamed.action != "tab.rename"
        || renamed.instance_id != instance_id
        || renamed.window_id != window_id
        || renamed.tab_id != tab_id
    {
        decision.publish(HostAction::Abort)?;
        bail!("Warp returned the wrong identity for the one scoped title claim");
    }

    let mut session = TerminalSession {
        kind: TerminalKind::Warp,
        id: instance_id.clone(),
        tab_id: Some(tab_id),
        window_id: Some(window_id),
        managed_session_id: None,
        windows_process_identity: None,
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
    };
    if let Err(binding_error) = write_new_json(&directory.join(CONTROL_FILE), &control) {
        decision.publish(HostAction::Abort)?;
        return match close_exact(runner, &client, &session, cleanup_deadline) {
            Ok(_) => Err(binding_error).context("failed to persist the Warp control binding"),
            Err(cleanup_error) => Err(anyhow!(
                "failed to persist the Warp control binding: {binding_error:#}; exact surface cleanup also failed: {cleanup_error:#}"
            )),
        };
    }
    if let Err(bind_error) = bind(&mut session) {
        decision.publish(HostAction::Abort)?;
        let cleanup = close_exact(runner, &client, &session, cleanup_deadline);
        return finish_failed_bind(bind_error, cleanup, unbind);
    }
    if Instant::now() >= deadline {
        decision.publish(HostAction::Abort)?;
        let cleanup = close_exact(runner, &client, &session, cleanup_deadline);
        return finish_failed_bind(
            anyhow!("Warp launch deadline expired after binding and before provider start"),
            cleanup,
            unbind,
        );
    }
    if let Err(start_error) = decision.publish(HostAction::Start) {
        let start_error = start_error.context("failed to release the bound Warp host");
        if let Err(abort_error) = decision.abort_before_cleanup() {
            return Err(anyhow!(
                "{start_error:#}; abort could not be established: {abort_error:#}; durable exact binding retained"
            ));
        }
        let cleanup = close_exact(runner, &client, &session, cleanup_deadline);
        return finish_failed_bind(start_error, cleanup, unbind);
    }
    if offer.tty.is_empty() || offer.pid == 0 {
        bail!("invalid Warp host offer");
    }
    Ok(session)
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
    let offer: HostOffer = read_record(&directory.join(HOST_OFFER_FILE), "Warp host offer")?;
    validate_offer(&offer, &binding.attempt, &directory)?;
    let deadline = deadline_from_timeout(timeout.unwrap_or(CONTROL_TIMEOUT))?;
    let mut runner = ProcessRunner;
    let client = binding.client()?;
    if !bound_instance_present(&mut runner, &client, &binding, deadline)?
        || !tab_present_with(&mut runner, &client, session, deadline)?
    {
        bail!("managed Warp tab is missing");
    }
    Ok(offer.tty)
}

pub(super) fn surface_present(session: &TerminalSession, timeout: Duration) -> Result<bool> {
    let directory = session_directory(session)?;
    let binding = load_binding(&directory, session)?;
    let mut runner = ProcessRunner;
    let client = binding.client()?;
    let deadline = deadline_from_timeout(timeout)?;
    if !bound_instance_present(&mut runner, &client, &binding, deadline)? {
        return Ok(false);
    }
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
    let client = binding.client()?;
    if !bound_instance_present(&mut runner, &client, &binding, deadline)? {
        return Ok(CloseOutcome::Missing);
    }
    close_exact(&mut runner, &client, session, deadline)
}

fn bound_instance_present<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    binding: &ControlBinding,
    deadline: Instant,
) -> Result<bool> {
    let Some(inspect) = optional_control_json::<InstanceInspect, _>(
        runner,
        client,
        &[
            "instance".into(),
            "inspect".into(),
            "--instance".into(),
            binding.instance_id.clone(),
        ],
        deadline,
    )
    .map_err(anyhow::Error::new)?
    else {
        return Ok(false);
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

fn close_exact<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    require_exact_ids(session)?;
    let was_present = tab_present_with(runner, client, session, deadline)?;
    if was_present {
        let response: OkResponse =
            control_json(runner, client, &tab_args("close", session)?, deadline)
                .map_err(anyhow::Error::new)?;
        if !response.ok || response.action != "tab.close" || response.instance_id != session.id {
            bail!("Warp did not acknowledge the exact tab.close request");
        }
    }

    let mut last_tab_present = was_present;
    loop {
        if Instant::now() >= deadline {
            if last_tab_present {
                bail!("Warp acknowledged tab.close but the exact tab remains present");
            }
            bail!(
                "managed Warp tab is absent but dedicated-window disappearance was not proven before the deadline"
            );
        }
        if tab_present_with(runner, client, session, deadline)? {
            last_tab_present = true;
            runner.pause(POLL_INTERVAL);
            continue;
        }
        last_tab_present = false;
        if !window_present_with(runner, client, session, deadline)? {
            return Ok(if was_present {
                CloseOutcome::Closed
            } else {
                CloseOutcome::Missing
            });
        }
        let siblings = list_tabs(runner, client, session, deadline)?;
        if !siblings.is_empty() {
            bail!(
                "managed Warp tab is closed but its dedicated window contains another tab; it may be user-owned and was preserved"
            );
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

fn list_tabs<R: WarpRunner>(
    runner: &mut R,
    client: &ControlClient,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<Vec<TabDescription>> {
    let response = optional_control_json::<TabList, _>(
        runner,
        client,
        &[
            "tab".into(),
            "list".into(),
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
    Ok(response.map_or_else(Vec::new, |value| value.tabs))
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

fn write_launch_config(
    client: &ControlClient,
    name: &str,
    title: &str,
    directory: &Path,
    attempt: &str,
) -> Result<PathBuf> {
    let config_dir = ensure_private_config_directory(&client.config_dir)?;
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
    let configuration = json!({
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
    });
    let path = config_dir.join(format!("{name}.yaml"));
    write_new_json(&path, &configuration)?;
    Ok(path)
}

struct LaunchConfigGuard(PathBuf);

impl Drop for LaunchConfigGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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
            let decision: HostDecision = read_record(
                &self.directory.join(HOST_DECISION_FILE),
                "Warp host decision",
            )
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
    let path = directory.join(HOST_OFFER_FILE);
    loop {
        if path.exists() {
            let offer: HostOffer = read_record(&path, "Warp host offer")?;
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
    write_new_json(
        &directory.join(HOST_DECISION_FILE),
        &HostDecision {
            schema: RECORD_SCHEMA,
            attempt: attempt.to_owned(),
            action,
        },
    )
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("Warp record path has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-warp-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary
        .as_file_mut()
        .write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    fs::hard_link(temporary.path(), path)
        .with_context(|| format!("failed to publish exclusive Warp record {}", path.display()))?;
    // Once the link exists the waiting host may act on it, so a directory-sync failure
    // cannot be reported as "not published" and retried as the opposite decision.
    let _ = File::open(parent).and_then(|directory| directory.sync_all());
    Ok(())
}

fn read_record<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular {label}: {}", path.display());
    }
    if metadata.len() > MAX_CONTROL_OUTPUT as u64 {
        bail!("{label} exceeds the bounded record limit");
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {label}"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {label}"))
}

fn load_binding(directory: &Path, session: &TerminalSession) -> Result<ControlBinding> {
    require_exact_ids(session)?;
    let binding: ControlBinding =
        read_record(&directory.join(CONTROL_FILE), "Warp control binding")?;
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
    crate::native::session_directory(id)
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
        config_dir: home.join(spec.config_home).join("launch_configurations"),
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
    let stdout = read_bounded_file(stdout)?;
    let stderr = read_bounded_file(stderr)?;
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn read_bounded_file(mut file: File) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut retained = Vec::new();
    file.take(MAX_CONTROL_OUTPUT as u64 + 1)
        .read_to_end(&mut retained)?;
    Ok(retained)
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
    validate_private_session_directory(directory)?;
    let plan: HostPlan = read_record(&directory.join(HOST_PLAN_FILE), "Warp host plan")?;
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
    write_new_json(
        &directory.join(HOST_OFFER_FILE),
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
        let decision_path = directory.join(HOST_DECISION_FILE);
        if decision_path.exists() {
            let decision: HostDecision = read_record(&decision_path, "Warp host decision")?;
            if decision.schema != RECORD_SCHEMA || decision.attempt != attempt {
                bail!("Warp host decision does not match this launch attempt");
            }
            return match decision.action {
                HostAction::Abort => Ok(()),
                HostAction::Start => {
                    let error = Command::new("/bin/zsh")
                        .arg("-lc")
                        .arg(&plan.command)
                        .exec();
                    Err(error).context("failed to replace Warp host with launch command")
                }
            };
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn validate_private_session_directory(directory: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(directory).with_context(|| {
        format!(
            "no such Warp host session directory: {}",
            directory.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "refusing non-directory Warp host session path: {}",
            directory.display()
        );
    }
    // The hidden host executes the plan only from a session directory that another
    // account cannot replace or populate.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        bail!("Warp host session directory is not private to the current user");
    }
    Ok(())
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
mod tests {
    use std::cell::Cell;

    use super::*;

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
        fn control(
            &mut self,
            client: &ControlClient,
            args: &[String],
            _deadline: Instant,
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
                    let mut windows = vec![json!({"window_id": "old-window"})];
                    if self.dispatched {
                        windows.push(json!({"window_id": "new-window"}));
                        if self.ambiguous {
                            windows.push(json!({"window_id": "other-window"}));
                        }
                    }
                    Self::json(json!({"windows": windows}))
                }
                ["tab", "list"] => {
                    let window = args.last().unwrap();
                    let tabs = match window.as_str() {
                        "old-window" => {
                            vec![json!({"tab_id": "old-tab", "window_id": "old-window"})]
                        }
                        "new-window" if !self.window_present => Vec::new(),
                        "new-window" => {
                            let mut tabs = self
                                .siblings
                                .iter()
                                .map(|id| json!({"tab_id": id, "window_id": "new-window"}))
                                .collect::<Vec<_>>();
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
                    if self.block_control_binding {
                        write_new_json(
                            &self.directory.join(CONTROL_FILE),
                            &json!({"occupied": true}),
                        )?;
                    }
                    Self::json(json!({
                        "action": "tab.rename", "ok": true, "instance_id": "instance-1",
                        "window_id": "new-window",
                        "tab_id": if self.wrong_rename_identity { "wrong-tab" } else { "new-tab" }
                    }))
                }
                ["tab", "inspect"] => {
                    if let Some(code) = self.inspect_error {
                        Self::error(code)
                    } else if self.tab_present {
                        Self::json(json!({
                            "action": "tab.inspect",
                            "tab": {
                                "tab_id": if self.wrong_inspect_identity { "wrong-tab" } else { "new-tab" },
                                "window_id": "new-window"
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
                    if self.cancel_close {
                        Self::error("target_state_conflict")
                    } else {
                        if self.close_removes_tab {
                            self.tab_present = false;
                            if self.siblings.is_empty() {
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
            assert_eq!(uri, format!("warp://launch/agent-bridge-{attempt}"));
            let config = client
                .config_dir
                .join(format!("agent-bridge-{attempt}.yaml"));
            let parsed: Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
            assert_eq!(parsed["windows"].as_array().unwrap().len(), 1);
            assert_eq!(parsed["windows"][0]["tabs"].as_array().unwrap().len(), 1);
            assert_eq!(
                parsed["windows"][0]["tabs"][0]["commands"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            let command = parsed["windows"][0]["tabs"][0]["commands"][0]["exec"]
                .as_str()
                .unwrap();
            assert!(command.contains("native-warp-host"));
            assert!(command.contains(attempt));
            self.dispatched = true;
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
            config_dir: temp.path().join("launch_configurations"),
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
        assert!(error.to_string().contains("deadline"));
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
        assert!(error.to_string().contains("exactly one new window"));
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
    fn wrong_claim_identity_is_rejected_before_bind_or_close() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.wrong_rename_identity = true;
        let error = open_bound_tab_with(
            &mut runner,
            OpenRequest {
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
        assert!(error.to_string().contains("wrong identity"));
        assert!(
            !runner
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
        let cleanup_attempted = runner
            .calls
            .iter()
            .any(|call| call.starts_with(&["tab".into(), "inspect".into()]));
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
        runner.instance_inspect_error = Some("stale_target");
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
    fn close_preserves_user_siblings_and_reports_residual_window() {
        let (temp, client, attempt) = fixture();
        let directory = temp.path().join("session");
        fs::create_dir(&directory).unwrap();
        let mut runner = FakeRunner::new(&directory, &attempt);
        runner.siblings.push("user-tab".into());
        let error = close_exact(
            &mut runner,
            &client,
            &session(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("may be user-owned"));
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
    fn close_ack_is_not_closure_until_exact_tab_and_window_disappear() {
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
        assert!(error.to_string().contains("remains present"));

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
}
