use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::process::{Child as StdChild, ChildStdin, Stdio};

use agent_bridge::{AgentDefinition, AgentId, TabSet, agents, handoff_text_from, session_title};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, after, never};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
#[cfg(not(windows))]
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

struct AgentSession {
    definition: AgentDefinition,
    title: String,
    workspace: PathBuf,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    alive: Arc<AtomicBool>,
    last_activity: Arc<Mutex<Instant>>,
    hook_adapter: Option<HookAdapter>,
    size: (u16, u16),
    #[cfg(windows)]
    process: WindowsSessionProcess,
    #[cfg(windows)]
    node_control: Option<Arc<Mutex<ChildStdin>>>,
    #[cfg(not(windows))]
    master: Box<dyn MasterPty + Send>,
    #[cfg(not(windows))]
    child: Box<dyn Child + Send + Sync>,
}

#[cfg(windows)]
enum WindowsSessionProcess {
    Conpty(conpty::Process),
    NodePty(StdChild),
}

#[cfg(windows)]
type WindowsSpawnParts = (
    WindowsSessionProcess,
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
    Option<Arc<Mutex<ChildStdin>>>,
);

#[cfg(windows)]
struct NodePtyControlWriter {
    input: Arc<Mutex<ChildStdin>>,
}

#[cfg(windows)]
impl Write for NodePtyControlWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut input = self
            .input
            .lock()
            .map_err(|_| std::io::Error::other("Claude PTY control channel poisoned"))?;
        writeln!(input, "{{\"t\":\"write\",\"d\":\"{}\"}}", hex(bytes))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.input
            .lock()
            .map_err(|_| std::io::Error::other("Claude PTY control channel poisoned"))?
            .flush()
    }
}

#[cfg(windows)]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(windows)]
const NODE_PTY_SIDECAR: &str = r#"
const readline = require('readline');
const pty = require('node-pty');
const program = process.env.AGENT_BRIDGE_CLAUDE_PROGRAM;
const args = JSON.parse(process.env.AGENT_BRIDGE_CLAUDE_ARGS || '[]');
const child = pty.spawn(program, args, {
  name: 'xterm-256color', cols: 100, rows: 32, cwd: process.cwd(), env: process.env,
  useConpty: true, useConptyDll: true
});
child.onData(data => process.stdout.write(data));
child.onExit(({exitCode}) => process.exit(exitCode || 0));
readline.createInterface({input: process.stdin, crlfDelay: Infinity}).on('line', line => {
  const message = JSON.parse(line);
  if (message.t === 'write') child.write(Buffer.from(message.d, 'hex').toString());
  if (message.t === 'resize') child.resize(message.cols, message.rows);
});
process.on('SIGTERM', () => child.kill());
"#;

struct HookAdapter {
    status_file: tempfile::NamedTempFile,
    settings_file: Option<tempfile::NamedTempFile>,
}

fn session_arguments(
    definition: AgentDefinition,
    yolo: bool,
    claude_settings: Option<&Path>,
    codex_notify_exe: Option<&Path>,
) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if yolo {
        arguments.push(OsString::from(match definition.id {
            AgentId::Codex => "--dangerously-bypass-approvals-and-sandbox",
            AgentId::Claude | AgentId::Agy => "--dangerously-skip-permissions",
        }));
    }
    if definition.id == AgentId::Claude
        && let Some(settings) = claude_settings
    {
        arguments.push(OsString::from("--settings"));
        arguments.push(settings.as_os_str().to_owned());
    }
    if definition.id == AgentId::Codex
        && let Some(exe) = codex_notify_exe
    {
        let exe_text = exe.display().to_string();
        if !exe_text.contains('\'') {
            arguments.push(OsString::from("-c"));
            arguments.push(OsString::from(format!(
                "notify=['{exe_text}','hook','finished']"
            )));
        }
    }
    arguments
}

#[cfg(windows)]
fn resolve_windows_agent_command(
    definition: AgentDefinition,
    path: Option<&std::ffi::OsStr>,
    user_profile: Option<&std::ffi::OsStr>,
) -> PathBuf {
    let names = [
        format!("{}.exe", definition.command),
        format!("{}.cmd", definition.command),
        format!("{}.bat", definition.command),
        definition.command.to_owned(),
    ];
    if let Some(path) = path {
        for directory in std::env::split_paths(path) {
            for name in &names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    if definition.id == AgentId::Claude
        && let Some(user_profile) = user_profile
    {
        let candidate = PathBuf::from(user_profile)
            .join(".local")
            .join("bin")
            .join("claude.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(definition.command)
}

#[cfg(not(windows))]
fn resolve_unix_agent_command(
    definition: AgentDefinition,
    path: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(path) = path {
        for directory in std::env::split_paths(path) {
            let candidate = directory.join(definition.command);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if definition.id == AgentId::Claude
        && let Some(home) = home
    {
        let candidate = PathBuf::from(home)
            .join(".local")
            .join("bin")
            .join("claude");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(definition.command)
}

#[cfg(windows)]
fn global_node_modules() -> Result<PathBuf> {
    let output = Command::new("npm.cmd")
        .args(["root", "-g"])
        .output()
        .context("failed to locate global node modules")?;
    anyhow::ensure!(output.status.success(), "`npm root -g` failed");
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    anyhow::ensure!(
        path.join("node-pty").is_dir(),
        "Claude PTY dependency is missing; run `npm install -g node-pty`"
    );
    Ok(path)
}

fn help_text() -> String {
    format!(
        "agent-bridge {} — local PTY control room for first-party coding agent CLIs

Usage:
  agent-bridge [-yolo|--yolo] [--restore] [WORKSPACE]         start the TUI (default)
  agent-bridge open <agent> [--workspace PATH] [--prompt TEXT] [--title NAME]
  agent-bridge prompt <tab> [--wait [--until STATE]...] <text...>
  agent-bridge status <tab>
  agent-bridge read <tab> [--lines N]
  agent-bridge wait <tab> [--until STATE]... [--timeout-secs N]
  agent-bridge list
  agent-bridge close <tab>
  agent-bridge --help | --version

  Every delegation subcommand also accepts --json for machine-readable output.

TUI:
  Runs codex, claude, and agy in visible PTY tabs. Press F12 inside the app
  for the full key reference. WORKSPACE defaults to the current directory.
  -yolo, --yolo  DANGER: bypass approval and sandbox protections in every
                 spawned CLI session (forwards each CLI's official danger flag).
  --restore      recreate the previous session's tab layout at startup.

Delegation (drive a visible tab from a script or another agent):
  These subcommands talk to a RUNNING Agent Bridge TUI. Inside a tab, the
  session env (AGENT_BRIDGE_REQUESTS, AGENT_BRIDGE_TAB) routes requests to
  that instance; outside, the most recent TUI is discovered via
  ~/.agent-bridge/instance.json. If no TUI is running, commands fail fast —
  Agent Bridge never spawns agents invisibly.

  open    create a visible tab running <agent> (codex|claude|agy) and print
          the new tab title. --prompt injects TEXT after a short startup
          delay; --workspace sets the tab's working directory; --title picks
          a unique tab name.
  prompt  inject TEXT into an existing tab. Every injected prompt carries an
          \"[Agent Bridge delegation · from <tab>]\" provenance banner.
          --wait keeps polling afterwards until a settled state
          (default: finished).
  status  print the tab's state: working|waiting|idle|finished are
          hook-backed (claude; finished also codex). active|quiet|exited|
          unknown are observed only — quiet does not mean done.
  read    print the tab's currently visible terminal text; --lines N returns
          the most recent N lines including scrollback.
  wait    poll status until one of the given states (default: finished).
  list    print every tab as title, state, agent, workspace (tab-separated).
  close   close a tab and terminate its CLI session.

Example round trip:
  TAB=$(agent-bridge open codex --prompt \"review this diff\")
  agent-bridge wait \"$TAB\" --until finished --timeout-secs 900
  agent-bridge read \"$TAB\"
  agent-bridge prompt \"$TAB\" \"fix finding 2 only\"",
        env!("CARGO_PKG_VERSION")
    )
}

fn yolo_label(yolo: bool) -> &'static str {
    if yolo { " YOLO " } else { "" }
}

impl HookAdapter {
    fn status_path(&self) -> &Path {
        self.status_file.path()
    }

    fn settings_path(&self) -> Option<&Path> {
        self.settings_file
            .as_ref()
            .map(tempfile::NamedTempFile::path)
    }
}

fn new_status_file(initial: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut status_file = tempfile::Builder::new()
        .prefix("agent-bridge-")
        .suffix(".status")
        .tempfile_in(std::env::temp_dir())?;
    status_file.write_all(initial)?;
    Ok(status_file)
}

fn prepare_hook_adapter(definition: AgentDefinition) -> Result<Option<HookAdapter>> {
    match definition.id {
        AgentId::Claude => {
            let status_file = new_status_file(b"idle")?;
            let executable =
                std::env::current_exe().context("failed to locate agent-bridge executable")?;
            let settings = serde_json::to_vec_pretty(&claude_hook_settings(&executable))?;
            let mut settings_file = tempfile::Builder::new()
                .prefix("agent-bridge-")
                .suffix(".settings.json")
                .tempfile_in(std::env::temp_dir())?;
            settings_file.write_all(&settings)?;
            Ok(Some(HookAdapter {
                status_file,
                settings_file: Some(settings_file),
            }))
        }
        AgentId::Codex => Ok(Some(HookAdapter {
            status_file: new_status_file(b"")?,
            settings_file: None,
        })),
        AgentId::Agy => Ok(None),
    }
}

impl AgentSession {
    fn arguments(
        definition: AgentDefinition,
        yolo: bool,
        hook_adapter: Option<&HookAdapter>,
        extra_args: &[String],
    ) -> Vec<OsString> {
        let notify_exe = if definition.id == AgentId::Codex && hook_adapter.is_some() {
            std::env::current_exe().ok()
        } else {
            None
        };
        let mut arguments = session_arguments(
            definition,
            yolo,
            hook_adapter.and_then(HookAdapter::settings_path),
            notify_exe.as_deref(),
        );
        arguments.extend(extra_args.iter().map(OsString::from));
        arguments
    }

    #[cfg(windows)]
    fn spawn(
        definition: AgentDefinition,
        title: String,
        cwd: &Path,
        redraw: Sender<()>,
        yolo: bool,
        extra_args: &[String],
    ) -> Result<Self> {
        let hook_adapter = prepare_hook_adapter(definition)?;
        let program = resolve_windows_agent_command(
            definition,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("USERPROFILE").as_deref(),
        );
        let arguments = Self::arguments(definition, yolo, hook_adapter.as_ref(), extra_args);
        let (process, mut reader, writer, node_control): WindowsSpawnParts = if definition.id
            == AgentId::Claude
        {
            let node_path = global_node_modules()?;
            let arguments_json = serde_json::to_string(
                &arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy())
                    .collect::<Vec<_>>(),
            )?;
            let mut command = Command::new("node.exe");
            command
                .args(["-e", NODE_PTY_SIDECAR])
                .current_dir(cwd)
                .env("NODE_PATH", node_path)
                .env("AGENT_BRIDGE_CLAUDE_PROGRAM", &program)
                .env("AGENT_BRIDGE_CLAUDE_ARGS", arguments_json)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            if let Some(adapter) = &hook_adapter {
                command.env("AGENT_BRIDGE_STATUS_FILE", adapter.status_path());
            }
            if let Some(dir) = DELEGATION_DIR.get() {
                command.env("AGENT_BRIDGE_REQUESTS", dir);
                command.env("AGENT_BRIDGE_TAB", &title);
            }
            let mut child = command.spawn().context(
                "failed to start Claude PTY sidecar; install node-pty with `npm install -g node-pty`",
            )?;
            let reader = Box::new(
                child
                    .stdout
                    .take()
                    .context("Claude PTY stdout unavailable")?,
            );
            let node_control = Arc::new(Mutex::new(
                child.stdin.take().context("Claude PTY stdin unavailable")?,
            ));
            let writer = Box::new(NodePtyControlWriter {
                input: Arc::clone(&node_control),
            });
            (
                WindowsSessionProcess::NodePty(child),
                reader,
                writer,
                Some(node_control),
            )
        } else {
            let mut command = Command::new(&program);
            command.args(&arguments).current_dir(cwd);
            if let Some(adapter) = &hook_adapter {
                command.env("AGENT_BRIDGE_STATUS_FILE", adapter.status_path());
            }
            if let Some(dir) = DELEGATION_DIR.get() {
                command.env("AGENT_BRIDGE_REQUESTS", dir);
                command.env("AGENT_BRIDGE_TAB", &title);
            }
            let mut child = conpty::ProcessOptions::default()
                .set_console_size(Some((100, 32)))
                .spawn(command)
                .with_context(|| format!("failed to start {}", program.display()))?;
            let reader = Box::new(child.output()?);
            let writer = Box::new(child.input()?);
            (WindowsSessionProcess::Conpty(child), reader, writer, None)
        };
        let writer = Arc::new(Mutex::new(writer));
        let writer_for_reader = Arc::clone(&writer);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(32, 100, scrollback_rows())));
        let parser_for_reader = Arc::clone(&parser);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_reader = Arc::clone(&alive);
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let activity_for_reader = Arc::clone(&last_activity);
        thread::spawn(move || {
            let mut buffer = [0_u8; 8_192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                if let Some(response) = terminal_query_response(&buffer[..count])
                    && let Ok(mut input) = writer_for_reader.lock()
                {
                    let _ = input.write_all(response);
                    let _ = input.flush();
                }
                if let Ok(mut parser) = parser_for_reader.lock() {
                    process_output(&mut parser, &buffer[..count]);
                }
                if let Ok(mut activity) = activity_for_reader.lock() {
                    *activity = Instant::now();
                }
                let _ = redraw.try_send(());
            }
            alive_for_reader.store(false, Ordering::Release);
            let _ = redraw.try_send(());
        });
        Ok(Self {
            definition,
            title,
            workspace: cwd.to_path_buf(),
            parser,
            writer,
            alive,
            last_activity,
            hook_adapter,
            size: (100, 32),
            process,
            node_control,
        })
    }

    #[cfg(not(windows))]
    fn spawn(
        definition: AgentDefinition,
        title: String,
        cwd: &Path,
        redraw: Sender<()>,
        yolo: bool,
        extra_args: &[String],
    ) -> Result<Self> {
        let hook_adapter = prepare_hook_adapter(definition)?;
        let pair = native_pty_system().openpty(PtySize {
            rows: 32,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let program = resolve_unix_agent_command(
            definition,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        let mut command = CommandBuilder::new(program);
        command.cwd(cwd);
        for argument in Self::arguments(definition, yolo, hook_adapter.as_ref(), extra_args) {
            command.arg(argument);
        }
        if let Some(adapter) = &hook_adapter {
            command.env("AGENT_BRIDGE_STATUS_FILE", adapter.status_path());
        }
        if let Some(dir) = DELEGATION_DIR.get() {
            command.env("AGENT_BRIDGE_REQUESTS", dir);
            command.env("AGENT_BRIDGE_TAB", &title);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to start {}", definition.command))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let writer_for_reader = Arc::clone(&writer);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(32, 100, scrollback_rows())));
        let parser_for_reader = Arc::clone(&parser);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_reader = Arc::clone(&alive);
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let activity_for_reader = Arc::clone(&last_activity);
        thread::spawn(move || {
            let mut buffer = [0_u8; 8_192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                if let Some(response) = terminal_query_response(&buffer[..count])
                    && let Ok(mut input) = writer_for_reader.lock()
                {
                    let _ = input.write_all(response);
                    let _ = input.flush();
                }
                if let Ok(mut parser) = parser_for_reader.lock() {
                    process_output(&mut parser, &buffer[..count]);
                }
                if let Ok(mut activity) = activity_for_reader.lock() {
                    *activity = Instant::now();
                }
                let _ = redraw.try_send(());
            }
            alive_for_reader.store(false, Ordering::Release);
            let _ = redraw.try_send(());
        });
        Ok(Self {
            definition,
            title,
            workspace: cwd.to_path_buf(),
            parser,
            writer,
            alive,
            last_activity,
            hook_adapter,
            size: (100, 32),
            master: pair.master,
            child,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        if !self.is_alive() {
            anyhow::bail!("{} has exited", self.title);
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()?;
        if let Ok(mut activity) = self.last_activity.lock() {
            *activity = Instant::now();
        }
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if self.size == (cols, rows) {
            return Ok(());
        }
        #[cfg(windows)]
        match &mut self.process {
            WindowsSessionProcess::Conpty(process) => process.resize(cols as i16, rows as i16)?,
            WindowsSessionProcess::NodePty(_) => {
                let mut input = self
                    .node_control
                    .as_ref()
                    .context("Claude PTY control channel unavailable")?
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Claude PTY control channel poisoned"))?;
                writeln!(
                    input,
                    "{{\"t\":\"resize\",\"cols\":{cols},\"rows\":{rows}}}"
                )?;
                input.flush()?;
            }
        }
        #[cfg(not(windows))]
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?
            .screen_mut()
            .set_size(rows, cols);
        self.size = (cols, rows);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionActivity {
    Unknown,
    Active,
    Quiet,
    Exited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SemanticState {
    Working,
    Waiting,
    Idle,
    Finished,
}

impl SemanticState {
    const fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Finished => "finished",
        }
    }
}

fn parse_semantic_state(value: &str) -> Option<SemanticState> {
    match value.trim() {
        "working" => Some(SemanticState::Working),
        "waiting" => Some(SemanticState::Waiting),
        "idle" => Some(SemanticState::Idle),
        "finished" => Some(SemanticState::Finished),
        _ => None,
    }
}

fn notifications_enabled() -> bool {
    std::env::var("AGENT_BRIDGE_NOTIFICATIONS")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

const DEFAULT_SCROLLBACK_ROWS: usize = 2_000;
const MAX_SCROLLBACK_ROWS: usize = 100_000;

fn scrollback_rows() -> usize {
    scrollback_rows_from(std::env::var("AGENT_BRIDGE_SCROLLBACK").ok().as_deref())
}

fn scrollback_rows_from(value: Option<&str>) -> usize {
    value
        .and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|rows| *rows > 0)
        .map(|rows| rows.min(MAX_SCROLLBACK_ROWS))
        .unwrap_or(DEFAULT_SCROLLBACK_ROWS)
}

fn valid_hook_status_path(path: &Path) -> bool {
    path.parent() == Some(std::env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agent-bridge-") && name.ends_with(".status"))
}

fn write_hook_state(state: SemanticState) -> Result<()> {
    let path = PathBuf::from(
        std::env::var_os("AGENT_BRIDGE_STATUS_FILE")
            .context("AGENT_BRIDGE_STATUS_FILE is not set")?,
    );
    if !valid_hook_status_path(&path) {
        anyhow::bail!("refusing untrusted hook status path: {}", path.display());
    }
    fs::write(path, state.label()).context("failed to update hook status")
}

fn claude_hook_settings(executable: &Path) -> serde_json::Value {
    let handler = |state: &str| {
        serde_json::json!({
            "type": "command",
            "command": format!("\"{}\" hook {state}", executable.display()),
            "timeout": 5,
        })
    };
    serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [handler("idle")] }],
            "UserPromptSubmit": [{ "hooks": [handler("working")] }],
            "Notification": [{
                "matcher": "*",
                "hooks": [handler("waiting")]
            }],
            "Stop": [{ "hooks": [handler("finished")] }],
            "SessionEnd": [{ "hooks": [handler("finished")] }]
        }
    })
}

fn ensure_interactive_terminal(is_terminal: bool) -> Result<()> {
    if !is_terminal {
        anyhow::bail!("interactive terminal required");
    }
    Ok(())
}

fn process_output(parser: &mut vt100::Parser, bytes: &[u8]) {
    let pinned = parser.screen().scrollback();
    if pinned == 0 {
        parser.process(bytes);
        return;
    }
    parser.screen_mut().set_scrollback(usize::MAX);
    let old_maximum = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(pinned);
    parser.process(bytes);
    parser.screen_mut().set_scrollback(usize::MAX);
    let new_maximum = parser.screen().scrollback();
    parser
        .screen_mut()
        .set_scrollback(pinned.saturating_add(new_maximum.saturating_sub(old_maximum)));
}

fn terminal_query_response(bytes: &[u8]) -> Option<&'static [u8]> {
    if bytes.windows(4).any(|window| window == b"\x1b[>c") {
        Some(b"\x1b[>0;276;0c")
    } else if bytes.windows(3).any(|window| window == b"\x1b[c") {
        Some(b"\x1b[?1;2c")
    } else {
        None
    }
}

impl SessionActivity {
    const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Active => "active",
            Self::Quiet => "quiet",
            Self::Exited => "exited",
        }
    }
}

fn classify_activity(alive: bool, elapsed: Duration) -> SessionActivity {
    if !alive {
        SessionActivity::Exited
    } else if elapsed < Duration::from_secs(2) {
        SessionActivity::Active
    } else {
        SessionActivity::Quiet
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        #[cfg(windows)]
        match &mut self.process {
            WindowsSessionProcess::Conpty(process) => {
                let _ = process.exit(0);
            }
            WindowsSessionProcess::NodePty(child) => {
                let _ = child.kill();
            }
        }
        #[cfg(not(windows))]
        let _ = self.child.kill();
    }
}

trait SessionIo {
    fn definition(&self) -> AgentDefinition;
    fn title(&self) -> &str;
    fn workspace(&self) -> &Path;
    fn parser(&self) -> &Arc<Mutex<vt100::Parser>>;
    fn is_alive(&self) -> bool;
    fn write(&self, bytes: &[u8]) -> Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn scrollback(&self) -> Result<usize>;
    fn set_scrollback(&self, rows: usize) -> Result<()>;
    fn activity(&self) -> SessionActivity;
    fn semantic_state(&self) -> Option<SemanticState>;
    fn clear_finished_state(&self);
    fn find_text(&self, query: &str) -> Result<Option<usize>>;
    fn application_cursor(&self) -> bool;
    fn bracketed_paste(&self) -> bool;
    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding);
}

const RELAY_ENTER_DELAY: Duration = Duration::from_millis(50);
const RELAY_BRACKETED_CONFIRM_DELAY: Duration = Duration::from_millis(300);

fn relay_write_plan(bracketed: bool, message: &str) -> Vec<(Duration, Vec<u8>)> {
    let mut plan = vec![(
        Duration::ZERO,
        if bracketed {
            format!("\x1b[200~{message}\x1b[201~").into_bytes()
        } else {
            message.as_bytes().to_vec()
        },
    )];
    plan.push((RELAY_ENTER_DELAY, b"\r".to_vec()));
    if bracketed {
        plan.push((RELAY_BRACKETED_CONFIRM_DELAY, b"\r".to_vec()));
    }
    plan
}

fn visible_terminal_context(session: &dyn SessionIo, max_chars: usize) -> Result<String> {
    let parser = session
        .parser()
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?;
    let text = parser.screen().contents();
    let count = text.chars().count();
    if count <= max_chars {
        return Ok(text.trim().to_owned());
    }
    let tail = text.chars().skip(count - max_chars).collect::<String>();
    Ok(format!(
        "[... earlier visible context omitted ...]\n{}",
        tail.trim()
    ))
}

impl SessionIo for AgentSession {
    fn definition(&self) -> AgentDefinition {
        self.definition
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn parser(&self) -> &Arc<Mutex<vt100::Parser>> {
        &self.parser
    }

    fn is_alive(&self) -> bool {
        self.is_alive()
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        self.write(bytes)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.resize(cols, rows)
    }

    fn scrollback(&self) -> Result<usize> {
        Ok(self
            .parser
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?
            .screen()
            .scrollback())
    }

    fn set_scrollback(&self, rows: usize) -> Result<()> {
        self.parser
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?
            .screen_mut()
            .set_scrollback(rows);
        Ok(())
    }

    fn activity(&self) -> SessionActivity {
        let Ok(last_activity) = self.last_activity.lock() else {
            return SessionActivity::Unknown;
        };
        let elapsed = last_activity.elapsed();
        classify_activity(self.is_alive(), elapsed)
    }

    fn semantic_state(&self) -> Option<SemanticState> {
        let adapter = self.hook_adapter.as_ref()?;
        parse_semantic_state(&fs::read_to_string(adapter.status_path()).ok()?)
    }

    fn clear_finished_state(&self) {
        let Some(adapter) = self.hook_adapter.as_ref() else {
            return;
        };
        if self.semantic_state() == Some(SemanticState::Finished) {
            let _ = fs::write(adapter.status_path(), b"");
        }
    }

    fn find_text(&self, query: &str) -> Result<Option<usize>> {
        parser_find_text(&self.parser, query)
    }

    fn application_cursor(&self) -> bool {
        self.parser
            .lock()
            .is_ok_and(|parser| parser.screen().application_cursor())
    }

    fn bracketed_paste(&self) -> bool {
        self.parser
            .lock()
            .is_ok_and(|parser| parser.screen().bracketed_paste())
    }

    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        self.parser.lock().map_or(
            (
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Default,
            ),
            |parser| {
                (
                    parser.screen().mouse_protocol_mode(),
                    parser.screen().mouse_protocol_encoding(),
                )
            },
        )
    }
}

fn parser_find_text(parser: &Arc<Mutex<vt100::Parser>>, query: &str) -> Result<Option<usize>> {
    let query = query.to_lowercase();
    let mut parser = parser
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?;
    let screen = parser.screen_mut();
    let original = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let maximum = screen.scrollback();
    let page = usize::from(screen.size().0).max(1);
    let mut offset = 0;
    let mut found = None;
    loop {
        screen.set_scrollback(offset);
        if screen.contents().to_lowercase().contains(&query) {
            found = Some(offset);
            break;
        }
        if offset >= maximum {
            break;
        }
        offset = offset.saturating_add(page).min(maximum);
    }
    screen.set_scrollback(original);
    Ok(found)
}

fn parser_recent_lines(parser: &Arc<Mutex<vt100::Parser>>, wanted: usize) -> Result<String> {
    let mut parser = parser
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?;
    let screen = parser.screen_mut();
    let original = screen.scrollback();
    let height = usize::from(screen.size().0).max(1);
    screen.set_scrollback(usize::MAX);
    let maximum = screen.scrollback();
    let mut collected: Vec<String> = Vec::new();
    let mut offset = maximum;
    let mut previous = maximum;
    loop {
        screen.set_scrollback(offset);
        let window = screen
            .contents()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if offset == maximum {
            collected = window;
        } else {
            let advanced = previous - offset;
            let start = window.len().saturating_sub(advanced);
            collected.extend(window.into_iter().skip(start));
        }
        if offset == 0 {
            break;
        }
        previous = offset;
        offset = offset.saturating_sub(height);
    }
    screen.set_scrollback(original);
    let start = collected.len().saturating_sub(wanted);
    Ok(collected[start..].join("\n"))
}

#[cfg(test)]
type LegacySessionSpawner =
    Box<dyn FnMut(AgentDefinition, String, &Path, Sender<()>) -> Result<Box<dyn SessionIo>>>;
type SessionSpawner = Box<
    dyn FnMut(
        AgentDefinition,
        String,
        &Path,
        Sender<()>,
        bool,
        &[String],
    ) -> Result<Box<dyn SessionIo>>,
>;

enum Mode {
    Terminal,
    Help,
    PassThrough,
    Scrollback,
    Search {
        input: String,
    },
    Diff {
        text: String,
        offset: u16,
    },
    Add {
        selected: usize,
        workspace: String,
    },
    Relay {
        target: usize,
        input: String,
        confirm: bool,
        override_busy: bool,
        context: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitContext {
    branch: String,
    dirty: bool,
}

fn parse_git_context(branch: &str, status: &str) -> GitContext {
    GitContext {
        branch: branch.trim().to_owned(),
        dirty: !status.trim().is_empty(),
    }
}

fn read_git_context(cwd: &Path) -> Option<GitContext> {
    let branch = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .output()
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    Some(parse_git_context(
        &String::from_utf8_lossy(&branch.stdout),
        &String::from_utf8_lossy(&status.stdout),
    ))
}

fn spawn_git_context_reader(
    workspace: Arc<Mutex<PathBuf>>,
) -> Receiver<(PathBuf, Option<GitContext>)> {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        loop {
            let target = match workspace.lock() {
                Ok(path) => path.clone(),
                Err(_) => break,
            };
            let context = read_git_context(&target);
            if sender.send((target, context)).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
    receiver
}

fn read_git_diff(cwd: &Path) -> Result<String> {
    let has_head = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .is_ok_and(|output| output.status.success());
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(cwd)
        .args(["diff", "--no-ext-diff", "--no-color"]);
    if has_head {
        command.arg("HEAD");
    }
    let output = command
        .arg("--")
        .output()
        .context("failed to run git diff")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(if text.is_empty() {
        "No tracked changes.".to_owned()
    } else {
        text
    })
}

fn max_diff_offset(text: &str) -> u16 {
    text.lines()
        .count()
        .saturating_sub(1)
        .min(u16::MAX as usize) as u16
}

fn mouse_button_code(kind: MouseEventKind) -> Option<(u8, bool, bool)> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => Some((0, false, false)),
        MouseEventKind::Down(MouseButton::Middle) => Some((1, false, false)),
        MouseEventKind::Down(MouseButton::Right) => Some((2, false, false)),
        MouseEventKind::Up(_) => Some((3, true, false)),
        MouseEventKind::Drag(MouseButton::Left) => Some((32, false, true)),
        MouseEventKind::Drag(MouseButton::Middle) => Some((33, false, true)),
        MouseEventKind::Drag(MouseButton::Right) => Some((34, false, true)),
        MouseEventKind::Moved => Some((35, false, true)),
        MouseEventKind::ScrollUp => Some((64, false, false)),
        MouseEventKind::ScrollDown => Some((65, false, false)),
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => None,
    }
}

fn mouse_mode_reports(mode: vt100::MouseProtocolMode, kind: MouseEventKind) -> bool {
    match mode {
        vt100::MouseProtocolMode::None => false,
        vt100::MouseProtocolMode::Press => {
            matches!(
                kind,
                MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        }
        vt100::MouseProtocolMode::PressRelease => !matches!(
            kind,
            MouseEventKind::Drag(_)
                | MouseEventKind::Moved
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        vt100::MouseProtocolMode::ButtonMotion => !matches!(
            kind,
            MouseEventKind::Moved | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
        ),
        vt100::MouseProtocolMode::AnyMotion => !matches!(
            kind,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
        ),
    }
}

fn encode_mouse(
    mouse: MouseEvent,
    encoding: vt100::MouseProtocolEncoding,
    column: u16,
    row: u16,
) -> Option<Vec<u8>> {
    let (mut code, released, _) = mouse_button_code(mouse.kind)?;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => Some(
            format!(
                "\x1b[<{code};{column};{row}{}",
                if released { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        vt100::MouseProtocolEncoding::Default => {
            if column > 223 || row > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                code.saturating_add(32),
                (column as u8).saturating_add(32),
                (row as u8).saturating_add(32),
            ])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [
                u32::from(code) + 32,
                u32::from(column) + 32,
                u32::from(row) + 32,
            ] {
                let character = char::from_u32(value)?;
                let mut buffer = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
            Some(bytes)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SavedTab {
    agent: AgentId,
    workspace: PathBuf,
}

fn layout_manifest_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".agent-bridge")
            .join("last-layout.json"),
    )
}

fn agent_by_key(key: &str) -> Option<AgentId> {
    match key.to_ascii_lowercase().as_str() {
        "codex" => Some(AgentId::Codex),
        "claude" => Some(AgentId::Claude),
        "agy" => Some(AgentId::Agy),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct RegisteredAgent {
    definition: AgentDefinition,
    extra_args: Vec<String>,
}

fn default_registry() -> [RegisteredAgent; 3] {
    agents().map(|definition| RegisteredAgent {
        definition,
        extra_args: Vec::new(),
    })
}

fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn agents_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".agent-bridge")
            .join("agents.json"),
    )
}

fn registered_agents_from(text: Option<&str>) -> Result<[RegisteredAgent; 3], String> {
    let mut registry = default_registry();
    let Some(text) = text else {
        return Ok(registry);
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("agents.json parse error: {error}"))?;
    let Some(overrides) = value.get("agents").and_then(serde_json::Value::as_object) else {
        return Ok(registry);
    };
    for (key, spec) in overrides {
        let Some(id) = agent_by_key(key) else {
            return Err(format!(
                "agents.json: unknown agent {key:?} (expected codex, claude, or agy)"
            ));
        };
        let entry = &mut registry[agent_index(id)];
        if let Some(command) = spec.get("command").and_then(serde_json::Value::as_str) {
            entry.definition.command = leak_str(command.to_owned());
        }
        if let Some(role) = spec.get("role").and_then(serde_json::Value::as_str) {
            entry.definition.role = leak_str(role.to_owned());
        }
        if let Some(args) = spec.get("args").and_then(serde_json::Value::as_array) {
            entry.extra_args = args
                .iter()
                .filter_map(|argument| argument.as_str().map(str::to_owned))
                .collect();
        }
    }
    Ok(registry)
}

fn parse_layout(text: &str) -> Option<(bool, Vec<SavedTab>)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let yolo = value
        .get("yolo")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let tabs = value.get("tabs")?.as_array()?;
    let mut saved = Vec::new();
    for tab in tabs {
        let agent = agent_by_key(tab.get("agent")?.as_str()?)?;
        let workspace = PathBuf::from(tab.get("workspace")?.as_str()?);
        saved.push(SavedTab { agent, workspace });
    }
    Some((yolo, saved))
}

static DELEGATION_DIR: OnceLock<PathBuf> = OnceLock::new();

fn create_delegation_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("agent-bridge-{}-requests", std::process::id()));
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn write_json_atomically(path: &Path, value: &serde_json::Value) -> Result<()> {
    let payload = serde_json::to_vec_pretty(value)?;
    let part = path.with_extension("part");
    fs::write(&part, payload)?;
    fs::rename(&part, path)?;
    Ok(())
}

fn delegation_provenance(from: &str, text: &str) -> String {
    format!("[Agent Bridge delegation · from {from}] {text}")
}

fn instance_pointer_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".agent-bridge")
            .join("instance.json"),
    )
}

fn parse_instance_pointer(text: &str) -> Option<(u64, PathBuf)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let pid = value.get("pid").and_then(serde_json::Value::as_u64)?;
    let spool = PathBuf::from(value.get("spool").and_then(serde_json::Value::as_str)?);
    Some((pid, spool))
}

fn write_instance_pointer_at(pointer: &Path, pid: u64, spool: &Path) -> Result<()> {
    if let Some(parent) = pointer.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_atomically(
        pointer,
        &serde_json::json!({ "pid": pid, "spool": spool.to_string_lossy() }),
    )
}

fn clear_instance_pointer_at(pointer: &Path, our_pid: u64) {
    let Ok(text) = fs::read_to_string(pointer) else {
        return;
    };
    if parse_instance_pointer(&text).is_some_and(|(pid, _)| pid == our_pid) {
        let _ = fs::remove_file(pointer);
    }
}

fn clear_instance_pointer() {
    if let Some(pointer) = instance_pointer_path() {
        clear_instance_pointer_at(&pointer, u64::from(std::process::id()));
    }
}

struct PendingWrite {
    title: String,
    bytes: Vec<u8>,
    due: Instant,
}

struct App {
    sessions: TabSet<Box<dyn SessionIo>>,
    cwd: PathBuf,
    redraw: Sender<()>,
    spawner: SessionSpawner,
    git_contexts: HashMap<PathBuf, GitContext>,
    active_workspace: Arc<Mutex<PathBuf>>,
    ordinals: [u32; 3],
    mode: Mode,
    notice: String,
    notifications: bool,
    observed_states: HashMap<String, SemanticState>,
    yolo: bool,
    terminal_pane: Rect,
    rail_pane: Rect,
    footer_pane: Rect,
    pending_writes: Vec<PendingWrite>,
    layout_path: Option<PathBuf>,
    restorable_layout: Vec<SavedTab>,
    registry: [RegisteredAgent; 3],
    delegation_dir: Option<PathBuf>,
}

impl App {
    fn new(cwd: &Path, redraw: Sender<()>, yolo: bool) -> Self {
        let (registry, registry_warning) =
            match agents_config_path().and_then(|path| fs::read_to_string(path).ok()) {
                Some(text) => match registered_agents_from(Some(&text)) {
                    Ok(registry) => (registry, None),
                    Err(error) => (default_registry(), Some(error)),
                },
                None => (default_registry(), None),
            };
        if let Some(dir) = create_delegation_dir() {
            let _ = DELEGATION_DIR.set(dir);
        }
        let mut app = Self::new_with_spawner_yolo_registry(
            cwd,
            redraw,
            yolo,
            registry,
            Box::new(move |definition, title, cwd, redraw, yolo, extra| {
                Ok(Box::new(AgentSession::spawn(
                    definition, title, cwd, redraw, yolo, extra,
                )?))
            }),
        );
        app.delegation_dir = DELEGATION_DIR.get().cloned();
        if let (Some(pointer), Some(spool)) =
            (instance_pointer_path(), app.delegation_dir.as_deref())
        {
            let _ = write_instance_pointer_at(&pointer, u64::from(std::process::id()), spool);
        }
        if let Some(warning) = registry_warning {
            app.notice = format!("{warning}; using built-in agents");
        }
        if let Some(path) = layout_manifest_path() {
            if let Some((saved_yolo, tabs)) = fs::read_to_string(&path)
                .ok()
                .as_deref()
                .and_then(parse_layout)
                && !tabs.is_empty()
            {
                app.notice = format!(
                    "previous layout: {} tab(s) — F3, then Ctrl+L to restore{}",
                    tabs.len(),
                    if saved_yolo && !app.yolo {
                        " (saved as YOLO; restore keeps current mode)"
                    } else {
                        ""
                    }
                );
                app.restorable_layout = tabs;
            }
            app.layout_path = Some(path);
            app.save_layout();
        }
        app
    }

    #[cfg(test)]
    fn new_with_spawner(cwd: &Path, redraw: Sender<()>, mut spawner: LegacySessionSpawner) -> Self {
        Self::new_with_spawner_yolo(
            cwd,
            redraw,
            false,
            Box::new(move |definition, title, cwd, redraw, _, _| {
                spawner(definition, title, cwd, redraw)
            }),
        )
    }

    #[cfg(test)]
    fn new_with_spawner_yolo(
        cwd: &Path,
        redraw: Sender<()>,
        yolo: bool,
        spawner: SessionSpawner,
    ) -> Self {
        Self::new_with_spawner_yolo_registry(cwd, redraw, yolo, default_registry(), spawner)
    }

    fn new_with_spawner_yolo_registry(
        cwd: &Path,
        redraw: Sender<()>,
        yolo: bool,
        registry: [RegisteredAgent; 3],
        spawner: SessionSpawner,
    ) -> Self {
        let mut app = Self {
            sessions: TabSet::new(),
            cwd: cwd.to_path_buf(),
            redraw,
            spawner,
            git_contexts: read_git_context(cwd)
                .map(|context| HashMap::from([(cwd.to_path_buf(), context)]))
                .unwrap_or_default(),
            active_workspace: Arc::new(Mutex::new(cwd.to_path_buf())),
            ordinals: [0; 3],
            mode: Mode::Terminal,
            notice: format!("workspace: {}", cwd.display()),
            notifications: notifications_enabled(),
            observed_states: HashMap::new(),
            yolo,
            terminal_pane: Rect::default(),
            rail_pane: Rect::default(),
            footer_pane: Rect::default(),
            pending_writes: Vec::new(),
            layout_path: None,
            restorable_layout: Vec::new(),
            registry,
            delegation_dir: None,
        };
        if let Err(error) = app.add_session(AgentId::Codex) {
            app.notice = format!("{error:#}");
        }
        app
    }

    fn add_session(&mut self, id: AgentId) -> Result<()> {
        let workspace = self.cwd.clone();
        self.add_session_at(id, &workspace)
    }

    fn add_session_at(&mut self, id: AgentId, workspace: &Path) -> Result<()> {
        self.add_session_at_titled(id, workspace, None)
    }

    fn add_session_at_titled(
        &mut self,
        id: AgentId,
        workspace: &Path,
        title: Option<String>,
    ) -> Result<()> {
        let ordinal_index = agent_index(id);
        let agent = self.registry[ordinal_index].clone();
        let (title, bump_ordinal) = match title {
            Some(title) => (title, false),
            None => (session_title(id, self.ordinals[ordinal_index] + 1), true),
        };
        let session = (self.spawner)(
            agent.definition,
            title.clone(),
            workspace,
            self.redraw.clone(),
            self.yolo,
            &agent.extra_args,
        )
        .with_context(|| format!("failed to create {title}"))?;
        if bump_ordinal {
            self.ordinals[ordinal_index] += 1;
        }
        self.sessions.push(session);
        self.notice = format!("created {title}");
        self.save_layout();
        Ok(())
    }

    fn restart_active(&mut self) -> Result<()> {
        let Some(session) = self.sessions.active() else {
            anyhow::bail!("no session to restart");
        };
        if session.is_alive() {
            anyhow::bail!("{} is still running", session.title());
        }
        let definition = session.definition();
        let title = session.title().to_owned();
        let workspace = session.workspace().to_path_buf();
        let extra_args = self.registry[agent_index(definition.id)].extra_args.clone();
        let replacement = (self.spawner)(
            definition,
            title.clone(),
            &workspace,
            self.redraw.clone(),
            self.yolo,
            &extra_args,
        )
        .with_context(|| format!("failed to restart {title}"))?;
        self.sessions
            .replace_active(replacement)
            .expect("active session exists");
        self.observed_states.remove(&title);
        self.notice = format!("restarted {title} (fresh session)");
        Ok(())
    }

    fn find_session(&self, query: &str) -> Result<Option<(usize, Option<usize>)>> {
        if query.trim().is_empty() || self.sessions.is_empty() {
            return Ok(None);
        }
        let query_lower = query.to_lowercase();
        let start = self.sessions.active_index().unwrap_or(0);
        for step in 1..=self.sessions.len() {
            let index = (start + step) % self.sessions.len();
            let session = self.sessions.get(index).expect("session index");
            if session.title().to_lowercase().contains(&query_lower) {
                return Ok(Some((index, None)));
            }
            if let Some(offset) = session.find_text(query)? {
                return Ok(Some((index, Some(offset))));
            }
        }
        Ok(None)
    }

    fn poll_semantic_notifications(&mut self) -> bool {
        let mut bell = false;
        for session in self.sessions.items() {
            let Some(state) = session.semantic_state() else {
                self.observed_states.remove(session.title());
                continue;
            };
            let previous = self
                .observed_states
                .insert(session.title().to_owned(), state);
            if self.notifications
                && previous.is_some_and(|old| old != state)
                && matches!(state, SemanticState::Waiting | SemanticState::Finished)
            {
                self.notice = format!("{} is {}", session.title(), state.label());
                bell = true;
            }
        }
        bell
    }

    fn next_write_deadline(&self) -> Option<Instant> {
        self.pending_writes.iter().map(|write| write.due).min()
    }

    fn flush_due_writes(&mut self, now: Instant) {
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.pending_writes.len() {
            if self.pending_writes[index].due <= now {
                due.push(self.pending_writes.remove(index));
            } else {
                index += 1;
            }
        }
        due.sort_by_key(|write| write.due);
        for write in due {
            let Some(session) = self
                .sessions
                .items()
                .iter()
                .find(|session| session.title() == write.title)
            else {
                self.notice = format!("relay dropped: {} is gone", write.title);
                continue;
            };
            if !session.is_alive() {
                self.notice = format!("relay dropped: {} has exited", write.title);
                continue;
            }
            if write.bytes.contains(&b'\r') {
                session.clear_finished_state();
            }
            if let Err(error) = session.write(&write.bytes) {
                self.notice = format!("relay to {} failed: {error}", write.title);
            }
        }
    }

    fn enqueue_delegation_prompt(&mut self, title: &str, from: &str, text: &str, delay: Duration) {
        let message = delegation_provenance(from, text);
        let now = Instant::now();
        for (offset, bytes) in relay_write_plan(false, &message) {
            self.pending_writes.push(PendingWrite {
                title: title.to_owned(),
                bytes,
                due: now + delay + offset,
            });
        }
        self.notice = format!("delegation: {from} → {title}");
    }

    fn handle_delegation(&mut self, request: &serde_json::Value) -> serde_json::Value {
        let from = request
            .get("from")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("external")
            .to_owned();
        let target = request
            .get("target")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match request.get("kind").and_then(serde_json::Value::as_str) {
            Some("open") => {
                let Some(id) = request
                    .get("agent")
                    .and_then(serde_json::Value::as_str)
                    .and_then(agent_by_key)
                else {
                    return serde_json::json!({
                        "ok": false,
                        "error": "unknown agent (expected codex, claude, or agy)"
                    });
                };
                let workspace = request
                    .get("workspace")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.cwd.clone());
                if !workspace.is_dir() {
                    return serde_json::json!({
                        "ok": false,
                        "error": format!("workspace is not a directory: {}", workspace.display())
                    });
                }
                let requested_title = request
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if let Some(requested) = &requested_title
                    && self
                        .sessions
                        .items()
                        .iter()
                        .any(|session| session.title() == requested)
                {
                    return serde_json::json!({
                        "ok": false,
                        "error": format!("tab title already exists: {requested}")
                    });
                }
                if let Err(error) = self.add_session_at_titled(id, &workspace, requested_title) {
                    return serde_json::json!({ "ok": false, "error": format!("{error:#}") });
                }
                let title = self
                    .sessions
                    .active()
                    .map(|session| session.title().to_owned())
                    .unwrap_or_default();
                if let Some(prompt) = request.get("prompt").and_then(serde_json::Value::as_str) {
                    self.enqueue_delegation_prompt(
                        &title,
                        &from,
                        prompt,
                        Duration::from_millis(2_500),
                    );
                }
                serde_json::json!({ "ok": true, "tab": title })
            }
            Some("prompt") => {
                let exists = self
                    .sessions
                    .items()
                    .iter()
                    .any(|session| session.title() == target);
                if !exists {
                    return serde_json::json!({ "ok": false, "error": format!("no such tab: {target}") });
                }
                let Some(text) = request.get("prompt").and_then(serde_json::Value::as_str) else {
                    return serde_json::json!({ "ok": false, "error": "prompt text is required" });
                };
                self.enqueue_delegation_prompt(&target, &from, text, Duration::ZERO);
                self.flush_due_writes(Instant::now());
                serde_json::json!({ "ok": true, "tab": target })
            }
            Some("status") => {
                let Some(session) = self
                    .sessions
                    .items()
                    .iter()
                    .find(|session| session.title() == target)
                else {
                    return serde_json::json!({ "ok": false, "error": format!("no such tab: {target}") });
                };
                let state = if !session.is_alive() {
                    "exited".to_owned()
                } else {
                    self.observed_states
                        .get(&target)
                        .copied()
                        .map(|state| state.label().to_owned())
                        .unwrap_or_else(|| session.activity().label().to_owned())
                };
                serde_json::json!({ "ok": true, "tab": target, "state": state })
            }
            Some("read") => {
                let Some(session) = self
                    .sessions
                    .items()
                    .iter()
                    .find(|session| session.title() == target)
                else {
                    return serde_json::json!({ "ok": false, "error": format!("no such tab: {target}") });
                };
                let lines = request
                    .get("lines")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|lines| *lines > 0);
                match lines {
                    Some(lines) => {
                        match parser_recent_lines(session.parser(), lines.min(10_000) as usize) {
                            Ok(output) => serde_json::json!({
                                "ok": true,
                                "tab": target,
                                "output": output
                            }),
                            Err(error) => {
                                serde_json::json!({ "ok": false, "error": error.to_string() })
                            }
                        }
                    }
                    None => match session.parser().lock() {
                        Ok(parser) => serde_json::json!({
                            "ok": true,
                            "tab": target,
                            "output": parser.screen().contents()
                        }),
                        Err(_) => {
                            serde_json::json!({ "ok": false, "error": "terminal parser poisoned" })
                        }
                    },
                }
            }
            Some("list") => {
                let tabs = self
                    .sessions
                    .items()
                    .iter()
                    .map(|session| {
                        let state = if !session.is_alive() {
                            "exited".to_owned()
                        } else {
                            self.observed_states
                                .get(session.title())
                                .copied()
                                .map(|state| state.label().to_owned())
                                .unwrap_or_else(|| session.activity().label().to_owned())
                        };
                        serde_json::json!({
                            "tab": session.title(),
                            "agent": session.definition().command,
                            "state": state,
                            "workspace": session.workspace().display().to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({ "ok": true, "tabs": tabs })
            }
            Some("close") => {
                let Some(index) = self
                    .sessions
                    .items()
                    .iter()
                    .position(|session| session.title() == target)
                else {
                    return serde_json::json!({
                        "ok": false,
                        "error": format!("no such tab: {target}")
                    });
                };
                let removed = self.sessions.remove(index).expect("indexed session");
                let title = removed.title().to_owned();
                drop(removed);
                self.observed_states.remove(&title);
                self.save_layout();
                self.notice = format!("delegation: {from} closed {title}");
                serde_json::json!({ "ok": true, "tab": title })
            }
            _ => serde_json::json!({ "ok": false, "error": "unknown request kind" }),
        }
    }

    fn process_delegation_requests(&mut self) {
        let Some(dir) = self.delegation_dir.clone() else {
            return;
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with("req-") || !name.ends_with(".json") || name.ends_with(".rsp.json")
            {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let _ = fs::remove_file(&path);
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let response = self.handle_delegation(&request);
            let Some(reply) = request
                .get("reply")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
            else {
                continue;
            };
            if reply.parent() == Some(dir.as_path()) {
                let _ = write_json_atomically(&reply, &response);
            }
        }
    }

    fn layout_manifest(&self) -> serde_json::Value {
        let tabs = self
            .sessions
            .items()
            .iter()
            .map(|session| {
                serde_json::json!({
                    "agent": session.definition().id.name().to_ascii_lowercase(),
                    "workspace": session.workspace().display().to_string(),
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "version": 1, "yolo": self.yolo, "tabs": tabs })
    }

    fn save_layout(&mut self) {
        let Some(path) = self.layout_path.clone() else {
            return;
        };
        let manifest = self.layout_manifest();
        let result = serde_json::to_vec_pretty(&manifest)
            .map_err(anyhow::Error::from)
            .and_then(|payload| {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, payload)?;
                Ok(())
            });
        if let Err(error) = result {
            self.notice = format!("layout save failed: {error}");
        }
    }

    fn restore_saved_layout(&mut self) {
        let saved = std::mem::take(&mut self.restorable_layout);
        let mut restored = 0_usize;
        let mut skipped = 0_usize;
        for tab in &saved {
            if tab.workspace.is_dir() && self.add_session_at(tab.agent, &tab.workspace).is_ok() {
                restored += 1;
            } else {
                skipped += 1;
            }
        }
        self.notice = format!("restored {restored} tab(s), skipped {skipped}");
    }

    fn sync_active_workspace(&self) {
        let target = self
            .sessions
            .active()
            .map(|session| session.workspace().to_path_buf())
            .unwrap_or_else(|| self.cwd.clone());
        if let Ok(mut workspace) = self.active_workspace.lock()
            && *workspace != target
        {
            *workspace = target;
        }
    }

    fn handle_paste(&mut self, text: &str) -> Result<()> {
        let pass_through = matches!(self.mode, Mode::PassThrough);
        match &mut self.mode {
            Mode::Terminal | Mode::PassThrough => {
                if let Some(session) = self.sessions.active() {
                    let bytes = if session.bracketed_paste() {
                        format!("\x1b[200~{text}\x1b[201~").into_bytes()
                    } else {
                        text.as_bytes().to_vec()
                    };
                    if let Err(error) = session.write(&bytes) {
                        self.notice = format!("failed to write to {}: {error}", session.title());
                    }
                }
            }
            Mode::Search { input } => input.push_str(text),
            Mode::Relay {
                input,
                confirm,
                override_busy,
                context,
                ..
            } => {
                input.push_str(text);
                *confirm = false;
                *override_busy = false;
                *context = None;
            }
            Mode::Help | Mode::Scrollback | Mode::Diff { .. } | Mode::Add { .. } => {}
        }
        if pass_through {
            self.mode = Mode::Terminal;
        }
        Ok(())
    }

    fn handle_rail_mouse(&mut self, mouse: MouseEvent) -> bool {
        let rail = self.rail_pane;
        if rail.width < 3 || rail.height < 3 {
            return false;
        }
        let inside = mouse.column > rail.x
            && mouse.column < rail.right().saturating_sub(1)
            && mouse.row > rail.y
            && mouse.row < rail.bottom().saturating_sub(1);
        if !inside {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp if matches!(self.mode, Mode::Terminal | Mode::Scrollback) => {
                self.sessions.move_active(-1);
            }
            MouseEventKind::ScrollDown
                if matches!(self.mode, Mode::Terminal | Mode::Scrollback) =>
            {
                self.sessions.move_active(1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(index) = rail_row_to_session_index(rail, mouse.row, self.sessions.len())
                else {
                    return true;
                };
                match &mut self.mode {
                    Mode::Relay {
                        target,
                        confirm,
                        override_busy,
                        context,
                        ..
                    } => {
                        *target = index;
                        *confirm = false;
                        *override_busy = false;
                        *context = None;
                    }
                    Mode::Terminal | Mode::Scrollback | Mode::PassThrough => {
                        if self.sessions.set_active(index)
                            && let Some(session) = self.sessions.active()
                        {
                            self.notice = format!("switched to {}", session.title());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        true
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.handle_rail_mouse(mouse) {
            return Ok(());
        }
        if let Mode::Diff { text, offset } = &mut self.mode {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    *offset = offset.saturating_sub(3);
                    return Ok(());
                }
                MouseEventKind::ScrollDown => {
                    *offset = offset.saturating_add(3).min(max_diff_offset(text));
                    return Ok(());
                }
                _ => {}
            }
        }
        if let Mode::Add { selected, .. } = &mut self.mode
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && mouse.row == self.footer_pane.y.saturating_add(1)
            && let Some(index) = agent_chip_at(mouse.column, self.footer_pane.x)
        {
            *selected = index;
            return Ok(());
        }
        let Some(session) = self.sessions.active() else {
            return Ok(());
        };
        let (protocol_mode, protocol_encoding) = session.mouse_protocol();
        if protocol_mode != vt100::MouseProtocolMode::None {
            let pane = self.terminal_pane;
            if mouse.column <= pane.x
                || mouse.column >= pane.right().saturating_sub(1)
                || mouse.row <= pane.y
                || mouse.row >= pane.bottom().saturating_sub(1)
            {
                return Ok(());
            }
            if mouse_mode_reports(protocol_mode, mouse.kind)
                && let Some(bytes) = encode_mouse(
                    mouse,
                    protocol_encoding,
                    mouse.column - pane.x,
                    mouse.row - pane.y,
                )
            {
                session.write(&bytes)?;
            }
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                session.set_scrollback(session.scrollback()?.saturating_add(3))?;
                self.mode = Mode::Scrollback;
            }
            MouseEventKind::ScrollDown => {
                session.set_scrollback(session.scrollback()?.saturating_sub(3))?;
                if session.scrollback()? == 0 {
                    self.mode = Mode::Terminal;
                } else {
                    self.mode = Mode::Scrollback;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Terminal);
        match mode {
            Mode::Terminal => match key.code {
                KeyCode::F(12) => self.mode = Mode::Help,
                KeyCode::F(11) => self.mode = Mode::PassThrough,
                KeyCode::F(10) => return Ok(true),
                KeyCode::F(1) => match self.sessions.active().map(|session| session.workspace()) {
                    Some(workspace) => match read_git_diff(workspace) {
                        Ok(text) => self.mode = Mode::Diff { text, offset: 0 },
                        Err(error) => self.notice = error.to_string(),
                    },
                    None => self.notice = "no active session".to_owned(),
                },
                KeyCode::F(3) => {
                    self.mode = Mode::Add {
                        selected: 0,
                        workspace: self.cwd.display().to_string(),
                    }
                }
                KeyCode::F(4) => {
                    if let Some(session) = self.sessions.remove_active() {
                        self.observed_states.remove(session.title());
                        self.notice = format!("closed {}", session.title());
                        self.save_layout();
                    }
                }
                KeyCode::F(8) if !self.sessions.is_empty() => {
                    self.mode = Mode::Scrollback;
                    self.notice = "scrollback mode".to_owned();
                }
                KeyCode::F(7) => {
                    self.mode = Mode::Search {
                        input: String::new(),
                    };
                }
                KeyCode::F(9) => {
                    if let Err(error) = self.restart_active() {
                        self.notice = format!("{error:#}");
                    }
                }
                KeyCode::F(5) => self.sessions.move_active(-1),
                KeyCode::F(6) => self.sessions.move_active(1),
                KeyCode::F(2) if self.sessions.len() > 1 => {
                    self.mode = Mode::Relay {
                        target: (self.sessions.active_index().unwrap_or(0) + 1)
                            % self.sessions.len(),
                        input: String::new(),
                        confirm: false,
                        override_busy: false,
                        context: None,
                    };
                }
                KeyCode::F(2) => {
                    self.notice = "relay needs at least two sessions".to_owned();
                }
                _ => {
                    if let Some(session) = self.sessions.active()
                        && let Some(bytes) = encode_key(key, session.application_cursor())
                    {
                        if bytes.contains(&b'\r') {
                            session.clear_finished_state();
                        }
                        if let Err(error) = session.write(&bytes) {
                            self.notice = error.to_string();
                        }
                    }
                }
            },
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::F(12) => {}
                _ => self.mode = Mode::Help,
            },
            Mode::PassThrough => {
                if let Some(session) = self.sessions.active()
                    && let Some(bytes) = encode_key(key, session.application_cursor())
                {
                    if bytes.contains(&b'\r') {
                        session.clear_finished_state();
                    }
                    if let Err(error) = session.write(&bytes) {
                        self.notice = error.to_string();
                    }
                }
            }
            Mode::Scrollback => {
                let Some(session) = self.sessions.active() else {
                    return Ok(false);
                };
                match key.code {
                    KeyCode::Esc | KeyCode::F(8) => {
                        session.set_scrollback(0)?;
                    }
                    KeyCode::Up => {
                        session.set_scrollback(session.scrollback()?.saturating_add(1))?;
                        self.mode = Mode::Scrollback;
                    }
                    KeyCode::PageUp => {
                        session.set_scrollback(session.scrollback()?.saturating_add(10))?;
                        self.mode = Mode::Scrollback;
                    }
                    KeyCode::Down => {
                        session.set_scrollback(session.scrollback()?.saturating_sub(1))?;
                        self.mode = Mode::Scrollback;
                    }
                    KeyCode::PageDown => {
                        session.set_scrollback(session.scrollback()?.saturating_sub(10))?;
                        self.mode = Mode::Scrollback;
                    }
                    KeyCode::Home => {
                        session.set_scrollback(usize::MAX)?;
                        self.mode = Mode::Scrollback;
                    }
                    KeyCode::End => {
                        session.set_scrollback(0)?;
                        self.mode = Mode::Scrollback;
                    }
                    _ => self.mode = Mode::Scrollback,
                }
            }
            Mode::Diff { text, mut offset } => match key.code {
                KeyCode::Esc | KeyCode::F(1) => {}
                KeyCode::Up => {
                    offset = offset.saturating_sub(1);
                    self.mode = Mode::Diff { text, offset };
                }
                KeyCode::Down => {
                    offset = offset.saturating_add(1).min(max_diff_offset(&text));
                    self.mode = Mode::Diff { text, offset };
                }
                KeyCode::PageUp => {
                    offset = offset.saturating_sub(10);
                    self.mode = Mode::Diff { text, offset };
                }
                KeyCode::PageDown => {
                    offset = offset.saturating_add(10).min(max_diff_offset(&text));
                    self.mode = Mode::Diff { text, offset };
                }
                KeyCode::Home => self.mode = Mode::Diff { text, offset: 0 },
                _ => self.mode = Mode::Diff { text, offset },
            },
            Mode::Search { mut input } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::Search { input };
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(character);
                    self.mode = Mode::Search { input };
                }
                KeyCode::Enter => match self.find_session(&input) {
                    Ok(Some((index, offset))) => {
                        self.sessions.set_active(index);
                        let session = self.sessions.active().expect("matched session");
                        if let Some(rows) = offset.filter(|rows| *rows > 0) {
                            session.set_scrollback(rows)?;
                            self.notice =
                                format!("search matched {} (scrollback)", session.title());
                            self.mode = Mode::Scrollback;
                        } else {
                            self.notice = format!("search matched {}", session.title());
                        }
                    }
                    Ok(None) => {
                        self.notice = format!("no session matches {input:?}");
                        self.mode = Mode::Search { input };
                    }
                    Err(error) => {
                        self.notice = error.to_string();
                        self.mode = Mode::Search { input };
                    }
                },
                _ => self.mode = Mode::Search { input },
            },
            Mode::Add {
                mut selected,
                mut workspace,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                    selected = selected.checked_sub(1).unwrap_or(agents().len() - 1);
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    };
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                    selected = (selected + 1) % agents().len();
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    };
                }
                KeyCode::Backspace => {
                    workspace.pop();
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    };
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    workspace.clear();
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    };
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if self.restorable_layout.is_empty() {
                        self.notice = "no saved layout to restore".to_owned();
                        self.mode = Mode::Add {
                            selected,
                            workspace,
                        };
                    } else {
                        self.restore_saved_layout();
                    }
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    workspace.push(character);
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    };
                }
                KeyCode::Enter => {
                    let path = PathBuf::from(workspace.trim());
                    let result = if !path.is_dir() {
                        Err(anyhow::anyhow!(
                            "workspace is not a directory: {}",
                            path.display()
                        ))
                    } else {
                        self.add_session_at(agents()[selected].id, &path)
                    };
                    if let Err(error) = result {
                        self.notice = format!("{error:#}");
                        self.mode = Mode::Add {
                            selected,
                            workspace,
                        };
                    }
                }
                _ => {
                    self.mode = Mode::Add {
                        selected,
                        workspace,
                    }
                }
            },
            Mode::Relay {
                mut target,
                mut input,
                mut confirm,
                override_busy,
                mut context,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Left => {
                    target = target.checked_sub(1).unwrap_or(self.sessions.len() - 1);
                    confirm = false;
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm,
                        override_busy: false,
                        context: None,
                    };
                }
                KeyCode::Right | KeyCode::Tab => {
                    target = (target + 1) % self.sessions.len();
                    confirm = false;
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm,
                        override_busy: false,
                        context: None,
                    };
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm: false,
                        override_busy: false,
                        context: None,
                    };
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(character);
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm: false,
                        override_busy: false,
                        context: None,
                    };
                }
                KeyCode::Enter => {
                    let Some(source_session) = self.sessions.active() else {
                        return Ok(false);
                    };
                    let source_title = source_session.title().to_owned();
                    let source = format!(
                        "{} @ {}",
                        source_title,
                        source_session.workspace().display()
                    );
                    let captured = match context.take() {
                        Some(context) => context,
                        None => match visible_terminal_context(source_session.as_ref(), 6_000) {
                            Ok(context) => context,
                            Err(error) => {
                                self.notice = error.to_string();
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm,
                                    override_busy: false,
                                    context: None,
                                };
                                return Ok(false);
                            }
                        },
                    };
                    match handoff_text_from(&source, &input, &captured) {
                        Ok(message) => {
                            let destination = self.sessions.get(target).expect("relay target");
                            if !destination.is_alive() {
                                self.notice = format!("{} has exited", destination.title());
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm,
                                    override_busy: false,
                                    context: Some(captured),
                                };
                            } else if !confirm {
                                self.notice = format!(
                                    "captured {} context chars; Enter again to send to {} @ {}",
                                    captured.chars().count(),
                                    destination.title(),
                                    destination.workspace().display()
                                );
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm: true,
                                    override_busy: false,
                                    context: Some(captured),
                                };
                            } else {
                                let title = destination.title().to_owned();
                                if !override_busy
                                    && self.observed_states.get(&title)
                                        == Some(&SemanticState::Working)
                                {
                                    self.notice = format!(
                                        "⚠ {title} is working — Enter again to interrupt, Esc to cancel"
                                    );
                                    self.mode = Mode::Relay {
                                        target,
                                        input,
                                        confirm: true,
                                        override_busy: true,
                                        context: Some(captured),
                                    };
                                } else {
                                    let bracketed = destination.bracketed_paste();
                                    let now = Instant::now();
                                    for (offset, bytes) in relay_write_plan(bracketed, &message) {
                                        self.pending_writes.push(PendingWrite {
                                            title: title.clone(),
                                            bytes,
                                            due: now + offset,
                                        });
                                    }
                                    self.sessions.set_active(target);
                                    self.notice = format!("{source_title} → {title} relayed");
                                    self.flush_due_writes(now);
                                }
                            }
                        }
                        Err(error) => {
                            self.notice = error.to_string();
                            self.mode = Mode::Relay {
                                target,
                                input,
                                confirm,
                                override_busy: false,
                                context: Some(captured),
                            };
                        }
                    }
                }
                _ => {
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm,
                        override_busy,
                        context,
                    }
                }
            },
        }
        Ok(false)
    }
}

const fn agent_index(id: AgentId) -> usize {
    match id {
        AgentId::Codex => 0,
        AgentId::Claude => 1,
        AgentId::Agy => 2,
    }
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let mut bytes = match key.code {
        KeyCode::Char(character)
            if key.modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii_alphabetic() =>
        {
            vec![(character.to_ascii_lowercase() as u8) - b'a' + 1]
        }
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match character {
                ' ' | '@' => vec![0x00],
                '[' => vec![0x1b],
                '\\' => vec![0x1c],
                ']' => vec![0x1d],
                '^' => vec![0x1e],
                '_' => vec![0x1f],
                _ => character.to_string().into_bytes(),
            }
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => if application_cursor {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }
        .to_vec(),
        KeyCode::Down => if application_cursor {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }
        .to_vec(),
        KeyCode::Right => if application_cursor {
            b"\x1bOC"
        } else {
            b"\x1b[C"
        }
        .to_vec(),
        KeyCode::Left => if application_cursor {
            b"\x1bOD"
        } else {
            b"\x1b[D"
        }
        .to_vec(),
        KeyCode::Home => if application_cursor {
            b"\x1bOH"
        } else {
            b"\x1b[H"
        }
        .to_vec(),
        KeyCode::End => if application_cursor {
            b"\x1bOF"
        } else {
            b"\x1b[F"
        }
        .to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        _ => return None,
    };
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn agent_color(id: AgentId) -> Color {
    match id {
        AgentId::Codex => Color::Cyan,
        AgentId::Claude => Color::LightRed,
        AgentId::Agy => Color::Magenta,
    }
}

fn app_layout(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(4),
    ])
    .areas(area);
    let [rail, terminal] = Layout::horizontal([Constraint::Length(24), Constraint::Min(20)])
        .spacing(1)
        .areas(body);
    (header, rail, terminal, footer)
}

fn terminal_inner_size(area: Rect) -> (u16, u16) {
    const PTY_RIGHT_MARGIN: u16 = 2;
    (
        area.width.saturating_sub(2 + PTY_RIGHT_MARGIN).max(1),
        area.height.saturating_sub(2).max(1),
    )
}

fn rail_row_to_session_index(rail: Rect, row: u16, sessions: usize) -> Option<usize> {
    let first = rail.y.saturating_add(1);
    if row < first {
        return None;
    }
    let index = usize::from(row.saturating_sub(first)) / 2;
    (index < sessions).then_some(index)
}

fn agent_chip_at(column: u16, origin: u16) -> Option<usize> {
    let mut x = origin;
    for (index, definition) in agents().iter().enumerate() {
        let width = definition.id.name().len() as u16 + 2;
        if column >= x && column < x.saturating_add(width) {
            return Some(index);
        }
        x = x.saturating_add(width + 2);
    }
    None
}

fn render(frame: &mut Frame, app: &App) {
    let (header, rail, terminal, footer) = app_layout(frame.area());
    let git_label = app
        .sessions
        .active()
        .and_then(|session| app.git_contexts.get(session.workspace()))
        .map(|git| {
            format!(
                "  ·  git:{}{}",
                git.branch,
                if git.dirty { "*" } else { "" }
            )
        })
        .unwrap_or_default();

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " AGENT BRIDGE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                yolo_label(app.yolo),
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  local PTY control room  ·  "),
            Span::styled(&app.notice, Style::default().fg(Color::DarkGray)),
            Span::styled(git_label, Style::default().fg(Color::Yellow)),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        header,
    );

    render_tab_rail(frame, app, rail);
    if matches!(app.mode, Mode::Help) {
        frame.render_widget(
            Paragraph::new(
                "F1      tracked git diff HEAD\n\
                 F2      handoff request + recent source context (Enter twice)\n\
                 F3      create session\n\
                 F4      close active session\n\
                 F5/F6   previous/next session\n\
                 F7      search titles and terminal history\n\
                 F8      browse scrollback\n\
                 F9      restart an exited session\n\
                 F10     quit Agent Bridge\n\
                 Ctrl+F11 send the next key directly to the CLI\n\
                 Shift+drag terminal-native text selection during mouse capture\n\
                 Mouse   click a rail entry to focus it (during F2: pick the target),\n\
                         wheel over the rail switches sessions, wheel scrolls output/diff\n\
                 F12     close this help\n\n\
                 Other terminal input goes directly to the active CLI.",
            )
            .block(Block::default().title(" Help ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
            terminal,
        );
    } else if let Mode::Diff { text, offset } = &app.mode {
        frame.render_widget(
            Paragraph::new(text.as_str())
                .block(
                    Block::default()
                        .title(" Git diff HEAD ")
                        .borders(Borders::ALL),
                )
                .scroll((*offset, 0))
                .wrap(Wrap { trim: false }),
            terminal,
        );
    } else if let Some(session) = app.sessions.active() {
        render_session(frame, session.as_ref(), terminal);
    } else {
        frame.render_widget(
            Paragraph::new("No sessions. Press F3 to add Codex, Claude, or Agy.")
                .block(Block::default().title(" Terminal ").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            terminal,
        );
    }

    let footer_text = match &app.mode {
        Mode::Terminal => vec![
            Line::from(
                " F12 help  ·  Ctrl+F11 send next key  ·  F3 new tab  ·  F4 close  ·  F5/F6 switch  ·  F10 quit",
            ),
            Line::from(Span::styled(
                " All other keys go directly to the active CLI.",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        Mode::Help => vec![
            Line::from(" HELP  ·  complete keyboard reference"),
            Line::from(" Esc/F12 return to terminal"),
        ],
        Mode::PassThrough => vec![
            Line::from(" PASS THROUGH  ·  next key goes directly to the active CLI"),
            Line::from(" Includes Agent Bridge function-key shortcuts"),
        ],
        Mode::Scrollback => vec![
            Line::from(" SCROLLBACK  ·  ↑/↓ line  ·  PgUp/PgDn page  ·  Home/End bounds"),
            Line::from(" Esc/F8 return to live terminal"),
        ],
        Mode::Search { input } => vec![
            Line::from(vec![
                Span::styled(
                    " SEARCH ",
                    Style::default().fg(Color::Black).bg(Color::LightBlue),
                ),
                Span::raw(format!(" {input}")),
            ]),
            Line::from(" Enter next match  ·  Backspace edit  ·  Esc cancel"),
        ],
        Mode::Diff { .. } => vec![
            Line::from(" GIT DIFF  ·  ↑/↓ line  ·  PgUp/PgDn page  ·  Home top"),
            Line::from(" Esc/F1 return to terminal"),
        ],
        Mode::Add {
            selected,
            workspace,
        } => {
            let choices = agents()
                .iter()
                .enumerate()
                .flat_map(|(index, definition)| {
                    let style = if index == *selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(agent_color(definition.id))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    [
                        Span::styled(format!(" {} ", definition.id.name()), style),
                        Span::raw("  "),
                    ]
                })
                .collect::<Vec<_>>();
            vec![
                Line::from(choices),
                Line::from(format!(
                    "{}workspace: {}  ·  type/edit path  ·  ←/→ CLI  ·  Enter create{}",
                    if app.yolo { "YOLO · " } else { "" },
                    workspace,
                    if app.restorable_layout.is_empty() {
                        ""
                    } else {
                        "  ·  Ctrl+L restore last layout"
                    }
                )),
            ]
        }
        Mode::Relay {
            target,
            input,
            confirm,
            override_busy: _,
            context,
        } => {
            let target_name = app
                .sessions
                .get(*target)
                .map(|session| format!("{} @ {}", session.title(), session.workspace().display()))
                .unwrap_or_else(|| "?".to_owned());
            vec![
                Line::from(vec![
                    Span::styled(
                        format!(" HANDOFF → {target_name} "),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ),
                    Span::raw(format!(" {input}")),
                ]),
                Line::from(if *confirm {
                    format!(
                        " {} context chars captured  ·  Enter confirm handoff  ·  edit resets",
                        context.as_ref().map_or(0, |value| value.chars().count())
                    )
                } else {
                    " ←/→ target  ·  Enter capture recent context  ·  Esc cancel".to_owned()
                }),
            ]
        }
    };
    frame.render_widget(
        Paragraph::new(footer_text).block(Block::default().borders(Borders::TOP)),
        footer,
    );
}

fn render_tab_rail(frame: &mut Frame, app: &App, area: Rect) {
    let active = app.sessions.active_index();
    let items = app
        .sessions
        .items()
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let selected = active == Some(index);
            let activity = session.activity();
            let marker = if activity == SessionActivity::Exited {
                "×"
            } else if selected {
                "▶"
            } else {
                " "
            };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(agent_color(session.definition().id))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let status = if activity == SessionActivity::Exited {
                activity.label()
            } else {
                app.observed_states
                    .get(session.title())
                    .copied()
                    .map(SemanticState::label)
                    .unwrap_or_else(|| activity.label())
            };
            ListItem::new(format!(
                " {marker} {} [{}]\n   {}",
                session.title(),
                status,
                session.workspace().display()
            ))
            .style(style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" Sessions ({}) ", app.sessions.len()))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_session(frame: &mut Frame, session: &dyn SessionIo, area: Rect) {
    let definition = session.definition();
    let color = agent_color(definition.id);
    let title = format!(
        " {} · {} · {} · {} ",
        session.title(),
        definition.role,
        definition.command,
        session.workspace().display(),
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Ok(parser) = session.parser().lock() else {
        frame.render_widget(Paragraph::new("[terminal parser unavailable]"), inner);
        return;
    };
    let screen = parser.screen();
    let rows = inner.height.min(screen.size().0);
    let cols = inner.width.min(screen.size().1);
    for row in 0..rows {
        for col in 0..cols {
            let Some(source) = screen.cell(row, col) else {
                continue;
            };
            if source.is_wide_continuation() {
                continue;
            }
            let Some(target) = frame.buffer_mut().cell_mut((inner.x + col, inner.y + row)) else {
                continue;
            };
            let symbol = if source.has_contents() {
                source.contents()
            } else {
                " "
            };
            target.set_symbol(symbol).set_style(vt_style(source));
        }
    }
    if screen.scrollback() == 0 && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            frame.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
}

fn vt_style(cell: &vt100::Cell) -> Style {
    let mut foreground = vt_color(cell.fgcolor());
    let mut background = vt_color(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut foreground, &mut background);
    }
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifiers.insert(Modifier::UNDERLINED);
    }
    Style::default()
        .fg(foreground)
        .bg(background)
        .add_modifier(modifiers)
}

const fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn spawn_event_reader() -> Receiver<Result<Event>> {
    let (sender, receiver) = crossbeam_channel::unbounded();
    thread::spawn(move || {
        loop {
            let event = event::read().context("failed to read terminal event");
            let stop = event.is_err();
            if sender.send(event).is_err() || stop {
                break;
            }
        }
    });
    receiver
}

fn handle_event(app: &mut App, event: Event) -> Result<bool> {
    match event {
        Event::Key(key) => app.handle_key(key),
        Event::Paste(text) => app.handle_paste(&text).map(|()| false),
        Event::Mouse(mouse) => app.handle_mouse(mouse).map(|()| false),
        _ => Ok(false),
    }
}

fn emit_pending_bell(writer: &mut impl Write, pending: &mut bool) -> Result<()> {
    if *pending {
        writer.write_all(b"\x07")?;
        writer.flush()?;
        *pending = false;
    }
    Ok(())
}

fn run(terminal: &mut DefaultTerminal, cwd: &Path, yolo: bool, restore: bool) -> Result<()> {
    let (redraw_sender, redraw_receiver) = crossbeam_channel::bounded(1);
    let event_receiver = spawn_event_reader();
    let heartbeat = crossbeam_channel::tick(Duration::from_secs(1));
    let mut app = App::new(cwd, redraw_sender, yolo);
    let git_context_receiver = spawn_git_context_reader(Arc::clone(&app.active_workspace));
    if restore {
        if app.restorable_layout.is_empty() {
            app.notice = "no saved layout to restore".to_owned();
        } else {
            app.restore_saved_layout();
        }
    }
    let mut bell_pending = false;
    loop {
        app.sync_active_workspace();
        let size = terminal.size()?;
        let (_, rail, pane, footer) = app_layout(Rect::new(0, 0, size.width, size.height));
        app.rail_pane = rail;
        app.terminal_pane = pane;
        app.footer_pane = footer;
        let (cols, rows) = terminal_inner_size(pane);
        for session in app.sessions.items_mut() {
            if !session.is_alive() {
                continue;
            }
            if let Err(error) = session.resize(cols, rows) {
                app.notice = format!("failed to resize {}: {error}", session.title());
            }
        }
        terminal.draw(|frame| render(frame, &app))?;
        emit_pending_bell(terminal.backend_mut(), &mut bell_pending)?;

        let write_deadline = app
            .next_write_deadline()
            .map(|due| after(due.saturating_duration_since(Instant::now())))
            .unwrap_or_else(never);
        crossbeam_channel::select! {
            recv(write_deadline) -> _ => {
                app.flush_due_writes(Instant::now());
            }
            recv(event_receiver) -> event => {
                if handle_event(&mut app, event.context("terminal event reader stopped")??)? {
                    break;
                }
                for event in event_receiver.try_iter() {
                    if handle_event(&mut app, event?)? {
                        return Ok(());
                    }
                }
            }
            recv(redraw_receiver) -> _ => {
                while redraw_receiver.try_recv().is_ok() {}
            }
            recv(heartbeat) -> _ => {
                if app.poll_semantic_notifications() {
                    bell_pending = true;
                }
                app.process_delegation_requests();
            }
            recv(git_context_receiver) -> update => {
                let (workspace, context) = update.context("git context reader stopped")?;
                match context {
                    Some(context) => {
                        app.git_contexts.insert(workspace, context);
                    }
                    None => {
                        app.git_contexts.remove(&workspace);
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum DelegateCommand {
    Open {
        agent: String,
        workspace: Option<PathBuf>,
        prompt: Option<String>,
        title: Option<String>,
    },
    Prompt {
        target: String,
        text: String,
        wait: bool,
        until: Vec<String>,
        timeout_secs: u64,
    },
    Status {
        target: String,
    },
    Read {
        target: String,
        lines: Option<u64>,
    },
    Wait {
        target: String,
        until: Vec<String>,
        timeout_secs: u64,
    },
    List,
    Close {
        target: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct DelegateInvocation {
    command: DelegateCommand,
    json: bool,
}

fn parse_delegate_command(kind: &str, args: Vec<String>) -> Result<DelegateInvocation> {
    let mut rest = args;
    let before = rest.len();
    rest.retain(|value| value != "--json");
    let json = rest.len() != before;
    let command = match kind {
        "open" => {
            anyhow::ensure!(
                !rest.is_empty(),
                "open requires an agent (codex, claude, or agy)"
            );
            let agent = rest.remove(0);
            let mut workspace = None;
            let mut prompt = None;
            let mut title = None;
            while !rest.is_empty() {
                match rest[0].as_str() {
                    "--workspace" => {
                        anyhow::ensure!(rest.len() >= 2, "--workspace requires a path");
                        rest.remove(0);
                        workspace = Some(PathBuf::from(rest.remove(0)));
                    }
                    "--prompt" => {
                        anyhow::ensure!(rest.len() >= 2, "--prompt requires text");
                        rest.remove(0);
                        prompt = Some(rest.remove(0));
                    }
                    "--title" => {
                        anyhow::ensure!(rest.len() >= 2, "--title requires a name");
                        rest.remove(0);
                        title = Some(rest.remove(0));
                    }
                    other => anyhow::bail!("unknown open option: {other}"),
                }
            }
            DelegateCommand::Open {
                agent,
                workspace,
                prompt,
                title,
            }
        }
        "prompt" => {
            anyhow::ensure!(!rest.is_empty(), "prompt requires a tab title");
            let target = rest.remove(0);
            let mut wait = false;
            let mut until = Vec::new();
            let mut timeout_secs = 600;
            let mut text_parts = Vec::new();
            while !rest.is_empty() {
                match rest[0].as_str() {
                    "--wait" => {
                        rest.remove(0);
                        wait = true;
                    }
                    "--until" => {
                        anyhow::ensure!(rest.len() >= 2, "--until requires a state");
                        rest.remove(0);
                        until.push(rest.remove(0));
                        wait = true;
                    }
                    "--timeout-secs" => {
                        anyhow::ensure!(rest.len() >= 2, "--timeout-secs requires a number");
                        rest.remove(0);
                        timeout_secs = rest
                            .remove(0)
                            .parse()
                            .context("--timeout-secs expects a number")?;
                    }
                    _ => text_parts.push(rest.remove(0)),
                }
            }
            anyhow::ensure!(!text_parts.is_empty(), "prompt requires text");
            if until.is_empty() {
                until.push("finished".to_owned());
            }
            DelegateCommand::Prompt {
                target,
                text: text_parts.join(" "),
                wait,
                until,
                timeout_secs,
            }
        }
        "status" => {
            anyhow::ensure!(rest.len() == 1, "status requires exactly one tab title");
            DelegateCommand::Status {
                target: rest.remove(0),
            }
        }
        "read" => {
            anyhow::ensure!(!rest.is_empty(), "read requires a tab title");
            let target = rest.remove(0);
            let mut lines = None;
            while !rest.is_empty() {
                match rest[0].as_str() {
                    "--lines" => {
                        anyhow::ensure!(rest.len() >= 2, "--lines requires a number");
                        rest.remove(0);
                        lines = Some(rest.remove(0).parse().context("--lines expects a number")?);
                    }
                    other => anyhow::bail!("unknown read option: {other}"),
                }
            }
            DelegateCommand::Read { target, lines }
        }
        "wait" => {
            anyhow::ensure!(!rest.is_empty(), "wait requires a tab title");
            let target = rest.remove(0);
            let mut until = Vec::new();
            let mut timeout_secs = 600;
            while !rest.is_empty() {
                match rest[0].as_str() {
                    "--until" => {
                        anyhow::ensure!(rest.len() >= 2, "--until requires a state");
                        rest.remove(0);
                        until.push(rest.remove(0));
                    }
                    "--timeout-secs" => {
                        anyhow::ensure!(rest.len() >= 2, "--timeout-secs requires a number");
                        rest.remove(0);
                        timeout_secs = rest
                            .remove(0)
                            .parse()
                            .context("--timeout-secs expects a number")?;
                    }
                    other => anyhow::bail!("unknown wait option: {other}"),
                }
            }
            if until.is_empty() {
                until.push("finished".to_owned());
            }
            DelegateCommand::Wait {
                target,
                until,
                timeout_secs,
            }
        }
        "list" => {
            anyhow::ensure!(rest.is_empty(), "list takes no arguments");
            DelegateCommand::List
        }
        "close" => {
            anyhow::ensure!(rest.len() == 1, "close requires exactly one tab title");
            DelegateCommand::Close {
                target: rest.remove(0),
            }
        }
        _ => anyhow::bail!("unknown delegation command: {kind}"),
    };
    Ok(DelegateInvocation { command, json })
}

fn delegation_spool_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENT_BRIDGE_REQUESTS") {
        return Ok(PathBuf::from(dir));
    }
    let pointer = instance_pointer_path()
        .context("cannot locate the Agent Bridge state directory (HOME is not set)")?;
    let text = fs::read_to_string(&pointer).map_err(|_| {
        anyhow::anyhow!(
            "no running Agent Bridge found — start the TUI first (looked for {})",
            pointer.display()
        )
    })?;
    let (_, spool) = parse_instance_pointer(&text)
        .with_context(|| format!("instance pointer is malformed: {}", pointer.display()))?;
    anyhow::ensure!(
        spool.is_dir(),
        "Agent Bridge instance pointer is stale (spool missing: {}); restart the TUI",
        spool.display()
    );
    Ok(spool)
}

fn send_delegation_request(
    mut request: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let dir = delegation_spool_dir()?;
    let mut file = tempfile::Builder::new()
        .prefix("req-")
        .suffix(".part")
        .tempfile_in(&dir)?;
    let stem = file
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("request file name is not valid UTF-8")?
        .to_owned();
    let reply = dir.join(format!("{stem}.rsp.json"));
    request["from"] = serde_json::json!(
        std::env::var("AGENT_BRIDGE_TAB").unwrap_or_else(|_| "external".to_owned())
    );
    request["reply"] = serde_json::json!(reply.to_string_lossy());
    file.write_all(&serde_json::to_vec_pretty(&request)?)?;
    let target = dir.join(format!("{stem}.json"));
    file.persist(&target)
        .map_err(|error| anyhow::anyhow!("failed to submit delegation request: {error}"))?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if reply.exists() {
            let text = fs::read_to_string(&reply)?;
            let _ = fs::remove_file(&reply);
            return Ok(serde_json::from_str(&text)?);
        }
        thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for Agent Bridge to answer (is the TUI running?)")
}

fn print_delegation_response(response: &serde_json::Value) -> Result<()> {
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!(
            "{}",
            response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("delegation failed")
        );
    }
    for key in ["tab", "state", "output"] {
        if let Some(value) = response.get(key).and_then(serde_json::Value::as_str) {
            println!("{value}");
        }
    }
    if let Some(tabs) = response.get("tabs").and_then(serde_json::Value::as_array) {
        for tab in tabs {
            let field = |key: &str| {
                tab.get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_owned()
            };
            println!(
                "{}\t{}\t{}\t{}",
                field("tab"),
                field("state"),
                field("agent"),
                field("workspace")
            );
        }
    }
    Ok(())
}

fn emit_delegation_response(response: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
        anyhow::ensure!(
            response.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
            "delegation failed"
        );
        Ok(())
    } else {
        print_delegation_response(response)
    }
}

fn wait_for_state(
    target: &str,
    until: &[String],
    timeout: Duration,
    grace: Duration,
) -> Result<serde_json::Value> {
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        let response = send_delegation_request(
            serde_json::json!({ "kind": "status", "target": target }),
            Duration::from_secs(15),
        )?;
        if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            anyhow::bail!(
                "{}",
                response
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("delegation failed")
            );
        }
        let state = response
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if started.elapsed() >= grace && until.iter().any(|u| u.eq_ignore_ascii_case(&state)) {
            return Ok(response);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for {target}; last state: {state}"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

fn run_delegate(invocation: DelegateInvocation) -> Result<()> {
    let json = invocation.json;
    match invocation.command {
        DelegateCommand::Open {
            agent,
            workspace,
            prompt,
            title,
        } => {
            let mut request = serde_json::json!({ "kind": "open", "agent": agent });
            if let Some(workspace) = workspace {
                let workspace = if workspace.is_absolute() {
                    workspace
                } else {
                    std::env::current_dir()?.join(workspace)
                };
                request["workspace"] = serde_json::json!(workspace.to_string_lossy());
            }
            if let Some(prompt) = prompt {
                request["prompt"] = serde_json::json!(prompt);
            }
            if let Some(title) = title {
                request["title"] = serde_json::json!(title);
            }
            emit_delegation_response(
                &send_delegation_request(request, Duration::from_secs(15))?,
                json,
            )
        }
        DelegateCommand::Prompt {
            target,
            text,
            wait,
            until,
            timeout_secs,
        } => {
            let submitted = send_delegation_request(
                serde_json::json!({ "kind": "prompt", "target": target, "prompt": text }),
                Duration::from_secs(15),
            )?;
            if !wait || submitted.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                return emit_delegation_response(&submitted, json);
            }
            let settled = wait_for_state(
                &target,
                &until,
                Duration::from_secs(timeout_secs),
                Duration::from_secs(2),
            )?;
            emit_delegation_response(&settled, json)
        }
        DelegateCommand::Status { target } => emit_delegation_response(
            &send_delegation_request(
                serde_json::json!({ "kind": "status", "target": target }),
                Duration::from_secs(15),
            )?,
            json,
        ),
        DelegateCommand::Read { target, lines } => {
            let mut request = serde_json::json!({ "kind": "read", "target": target });
            if let Some(lines) = lines {
                request["lines"] = serde_json::json!(lines);
            }
            emit_delegation_response(
                &send_delegation_request(request, Duration::from_secs(15))?,
                json,
            )
        }
        DelegateCommand::Wait {
            target,
            until,
            timeout_secs,
        } => {
            let settled = wait_for_state(
                &target,
                &until,
                Duration::from_secs(timeout_secs),
                Duration::ZERO,
            )?;
            emit_delegation_response(&settled, json)
        }
        DelegateCommand::List => emit_delegation_response(
            &send_delegation_request(
                serde_json::json!({ "kind": "list" }),
                Duration::from_secs(15),
            )?,
            json,
        ),
        DelegateCommand::Close { target } => emit_delegation_response(
            &send_delegation_request(
                serde_json::json!({ "kind": "close", "target": target }),
                Duration::from_secs(15),
            )?,
            json,
        ),
    }
}

enum Launch {
    Run {
        cwd: PathBuf,
        yolo: bool,
        restore: bool,
    },
    Help,
    Version,
    Hook(SemanticState),
    Delegate(DelegateInvocation),
}

fn parse_args_from(args: impl IntoIterator<Item = OsString>) -> Result<Launch> {
    let mut args = args.into_iter();
    let Some(argument) = args.next() else {
        return Ok(Launch::Run {
            cwd: std::env::current_dir()?,
            yolo: false,
            restore: false,
        });
    };
    if argument == "hook" {
        let state = args
            .next()
            .and_then(|value| parse_semantic_state(&value.to_string_lossy()))
            .context("hook requires one of: working, waiting, idle, finished")?;
        let _codex_notify_payload = args.next();
        if args.next().is_some() {
            anyhow::bail!("hook accepts one state and an optional notify payload");
        }
        return Ok(Launch::Hook(state));
    }
    if matches!(
        argument.to_string_lossy().as_ref(),
        "open" | "prompt" | "status" | "read" | "wait"
    ) {
        let kind = argument.to_string_lossy().into_owned();
        let rest = args
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if rest.iter().any(|value| value == "--help" || value == "-h") {
            return Ok(Launch::Help);
        }
        return Ok(Launch::Delegate(parse_delegate_command(&kind, rest)?));
    }
    if argument == "--help" || argument == "-h" {
        if args.next().is_some() {
            anyhow::bail!("--help does not accept arguments");
        }
        return Ok(Launch::Help);
    }
    if argument == "--version" || argument == "-V" {
        if args.next().is_some() {
            anyhow::bail!("--version does not accept arguments");
        }
        return Ok(Launch::Version);
    }

    let mut workspace = None;
    let mut yolo = false;
    let mut restore = false;
    for argument in std::iter::once(argument).chain(args) {
        if argument == "--yolo" || argument == "-yolo" {
            if yolo {
                anyhow::bail!("--yolo may only be specified once");
            }
            yolo = true;
        } else if argument == "--restore" {
            restore = true;
        } else if argument.to_string_lossy().starts_with('-') {
            anyhow::bail!("unknown option: {}", argument.to_string_lossy());
        } else if workspace.replace(PathBuf::from(argument)).is_some() {
            anyhow::bail!("expected at most one workspace path");
        }
    }
    let path = workspace.unwrap_or(std::env::current_dir()?);
    if !path.exists() {
        anyhow::bail!("workspace does not exist: {}", path.display());
    }
    if !path.is_dir() {
        anyhow::bail!("workspace is not a directory: {}", path.display());
    }
    Ok(Launch::Run {
        cwd: path,
        yolo,
        restore,
    })
}

fn main() -> Result<()> {
    match parse_args_from(std::env::args_os().skip(1))? {
        Launch::Help => {
            println!("{}", help_text());
            Ok(())
        }
        Launch::Version => {
            println!("agent-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Launch::Hook(state) => write_hook_state(state),
        Launch::Delegate(invocation) => run_delegate(invocation),
        Launch::Run { cwd, yolo, restore } => {
            ensure_interactive_terminal(std::io::stdout().is_terminal())?;
            struct InputModesGuard;
            impl Drop for InputModesGuard {
                fn drop(&mut self) {
                    let _ = execute!(
                        std::io::stdout(),
                        DisableMouseCapture,
                        DisableBracketedPaste
                    );
                }
            }
            let _input_modes = InputModesGuard;
            execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
            let result = ratatui::run(|terminal| run(terminal, &cwd, yolo, restore));
            clear_instance_pointer();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit_relay(destination: &dyn SessionIo, message: &str) -> Result<()> {
        let mut elapsed = Duration::ZERO;
        for (offset, bytes) in relay_write_plan(destination.bracketed_paste(), message) {
            if offset > elapsed {
                thread::sleep(offset - elapsed);
                elapsed = offset;
            }
            destination.write(&bytes)?;
        }
        Ok(())
    }

    fn wait_for_screen_text(session: &dyn SessionIo, query: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if session
                .find_text(query)
                .map(|found| found.is_some())
                .unwrap_or(false)
            {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    struct TestSession {
        definition: AgentDefinition,
        title: String,
        workspace: PathBuf,
        parser: Arc<Mutex<vt100::Parser>>,
        write_error: bool,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        alive: bool,
        semantic_state: Arc<Mutex<Option<SemanticState>>>,
    }

    impl SessionIo for TestSession {
        fn definition(&self) -> AgentDefinition {
            self.definition
        }

        fn title(&self) -> &str {
            &self.title
        }

        fn workspace(&self) -> &Path {
            &self.workspace
        }

        fn parser(&self) -> &Arc<Mutex<vt100::Parser>> {
            &self.parser
        }

        fn is_alive(&self) -> bool {
            self.alive
        }

        fn write(&self, _bytes: &[u8]) -> Result<()> {
            if self.write_error {
                anyhow::bail!("test write failed");
            }
            self.writes.lock().unwrap().push(_bytes.to_vec());
            Ok(())
        }

        fn resize(&mut self, _cols: u16, _rows: u16) -> Result<()> {
            Ok(())
        }

        fn scrollback(&self) -> Result<usize> {
            Ok(self.parser.lock().unwrap().screen().scrollback())
        }

        fn set_scrollback(&self, rows: usize) -> Result<()> {
            self.parser
                .lock()
                .unwrap()
                .screen_mut()
                .set_scrollback(rows);
            Ok(())
        }

        fn activity(&self) -> SessionActivity {
            SessionActivity::Unknown
        }

        fn semantic_state(&self) -> Option<SemanticState> {
            *self.semantic_state.lock().unwrap()
        }

        fn clear_finished_state(&self) {
            let mut state = self.semantic_state.lock().unwrap();
            if *state == Some(SemanticState::Finished) {
                *state = None;
            }
        }

        fn find_text(&self, query: &str) -> Result<Option<usize>> {
            parser_find_text(&self.parser, query)
        }

        fn application_cursor(&self) -> bool {
            self.parser.lock().unwrap().screen().application_cursor()
        }

        fn bracketed_paste(&self) -> bool {
            self.parser.lock().unwrap().screen().bracketed_paste()
        }

        fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
            let parser = self.parser.lock().unwrap();
            (
                parser.screen().mouse_protocol_mode(),
                parser.screen().mouse_protocol_encoding(),
            )
        }
    }

    fn test_session(definition: AgentDefinition, title: String, write_error: bool) -> TestSession {
        test_session_with_writes(
            definition,
            title,
            write_error,
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    fn test_session_with_writes(
        definition: AgentDefinition,
        title: String,
        write_error: bool,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> TestSession {
        TestSession {
            definition,
            title,
            workspace: PathBuf::from("workspace"),
            parser: Arc::new(Mutex::new(vt100::Parser::new(32, 100, 2_000))),
            write_error,
            writes,
            alive: true,
            semantic_state: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn agents_config_overrides_builtins_and_rejects_unknown_agents() {
        let registry = registered_agents_from(None).unwrap();
        assert_eq!(registry[0].definition.command, "codex");
        assert!(registry[0].extra_args.is_empty());

        let registry = registered_agents_from(Some(
            r#"{ "agents": { "agy": { "command": "/opt/agy", "role": "helper", "args": ["--effort", "high"] } } }"#,
        ))
        .unwrap();
        let agy = &registry[agent_index(AgentId::Agy)];
        assert_eq!(agy.definition.command, "/opt/agy");
        assert_eq!(agy.definition.role, "helper");
        assert_eq!(agy.extra_args, vec!["--effort", "high"]);
        assert_eq!(
            registry[agent_index(AgentId::Codex)].definition.command,
            "codex"
        );

        assert!(registered_agents_from(Some(r#"{ "agents": { "gemini": {} } }"#)).is_err());
        assert!(registered_agents_from(Some("not json")).is_err());
    }

    #[test]
    fn overridden_registry_reaches_the_spawner_with_extra_args() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let registry = registered_agents_from(Some(
            r#"{ "agents": { "codex": { "command": "codex-nightly", "args": ["--profile", "fast"] } } }"#,
        ))
        .unwrap();
        let mut app = App::new_with_spawner_yolo_registry(
            Path::new("workspace"),
            redraw,
            false,
            registry,
            Box::new(move |definition, title, _, _, _, extra| {
                captured
                    .lock()
                    .unwrap()
                    .push((definition.command.to_owned(), extra.to_vec()));
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(
            observed[0],
            (
                "codex-nightly".to_owned(),
                vec!["--profile".to_owned(), "fast".to_owned()]
            )
        );
        assert_eq!(observed[1], ("claude".to_owned(), Vec::new()));
    }

    #[test]
    fn spawn_arguments_append_configured_extra_args_last() {
        let codex = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Codex)
            .unwrap();
        assert_eq!(
            AgentSession::arguments(
                codex,
                true,
                None,
                &["--profile".to_owned(), "fast".to_owned()]
            ),
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                OsString::from("--profile"),
                OsString::from("fast"),
            ]
        );
    }

    #[test]
    fn scrollback_rows_come_from_the_environment_with_safe_bounds() {
        assert_eq!(scrollback_rows_from(None), 2_000);
        assert_eq!(scrollback_rows_from(Some("500")), 500);
        assert_eq!(scrollback_rows_from(Some("0")), 2_000);
        assert_eq!(scrollback_rows_from(Some("junk")), 2_000);
        assert_eq!(scrollback_rows_from(Some("999999")), 100_000);
    }

    #[test]
    fn f7_search_positions_scrollback_at_an_older_match() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                let session = test_session(definition, title, false);
                {
                    let mut parser = session.parser.lock().unwrap();
                    parser.process(b"deep needle\r\n");
                    for index in 0..80 {
                        parser.process(format!("line {index}\r\n").as_bytes());
                    }
                }
                Ok(Box::new(session))
            }),
        );

        app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
            .unwrap();
        for character in "deep".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(app.sessions.active().unwrap().scrollback().unwrap() > 0);
        assert!(matches!(app.mode, Mode::Scrollback));
        assert!(app.notice.contains("scrollback"));
    }

    #[test]
    fn layout_manifest_saves_on_add_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let manifest_path = directory.path().join("state").join("last-layout.json");
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(|definition, title, cwd, _| {
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                Ok(Box::new(session))
            }),
        );
        app.layout_path = Some(manifest_path.clone());

        app.add_session_at(AgentId::Claude, Path::new("other-workspace"))
            .unwrap();

        let (yolo, tabs) = parse_layout(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert!(!yolo);
        assert_eq!(
            tabs,
            vec![
                SavedTab {
                    agent: AgentId::Codex,
                    workspace: PathBuf::from("launch-workspace"),
                },
                SavedTab {
                    agent: AgentId::Claude,
                    workspace: PathBuf::from("other-workspace"),
                },
            ]
        );
    }

    #[test]
    fn saved_layout_restores_once_from_the_add_screen() {
        let workspace = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(|definition, title, cwd, _| {
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                Ok(Box::new(session))
            }),
        );
        app.restorable_layout = vec![
            SavedTab {
                agent: AgentId::Claude,
                workspace: workspace.path().to_path_buf(),
            },
            SavedTab {
                agent: AgentId::Agy,
                workspace: PathBuf::from("does-not-exist-anywhere"),
            },
        ];

        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .unwrap();

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions.active().unwrap().workspace(), workspace.path());
        assert!(app.notice.contains("restored 1"));
        assert!(matches!(app.mode, Mode::Terminal));

        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.notice.contains("no saved layout"));
    }

    #[test]
    fn instance_pointer_round_trips_and_rejects_malformed_input() {
        let directory = tempfile::tempdir().unwrap();
        let pointer = directory.path().join("state").join("instance.json");
        let spool = directory.path().join("spool");

        write_instance_pointer_at(&pointer, 4242, &spool).unwrap();
        let (pid, parsed_spool) =
            parse_instance_pointer(&fs::read_to_string(&pointer).unwrap()).unwrap();
        assert_eq!(pid, 4242);
        assert_eq!(parsed_spool, spool);

        assert!(parse_instance_pointer("not json").is_none());
        assert!(parse_instance_pointer(r#"{ "pid": 1 }"#).is_none());
    }

    #[test]
    fn clearing_the_instance_pointer_only_removes_our_own() {
        let directory = tempfile::tempdir().unwrap();
        let pointer = directory.path().join("instance.json");
        let spool = directory.path().join("spool");

        write_instance_pointer_at(&pointer, 7, &spool).unwrap();
        clear_instance_pointer_at(&pointer, 8);
        assert!(pointer.exists(), "a newer instance's pointer must survive");

        clear_instance_pointer_at(&pointer, 7);
        assert!(!pointer.exists());
    }

    #[test]
    fn delegate_command_parsing_covers_all_subcommands() {
        assert_eq!(
            parse_delegate_command(
                "open",
                vec![
                    "codex".to_owned(),
                    "--workspace".to_owned(),
                    "/tmp/x".to_owned(),
                    "--title".to_owned(),
                    "Reviewer".to_owned(),
                    "--json".to_owned(),
                    "--prompt".to_owned(),
                    "review this".to_owned(),
                ],
            )
            .unwrap(),
            DelegateInvocation {
                command: DelegateCommand::Open {
                    agent: "codex".to_owned(),
                    workspace: Some(PathBuf::from("/tmp/x")),
                    prompt: Some("review this".to_owned()),
                    title: Some("Reviewer".to_owned()),
                },
                json: true,
            }
        );
        assert_eq!(
            parse_delegate_command(
                "prompt",
                vec![
                    "Claude 1".to_owned(),
                    "--wait".to_owned(),
                    "fix".to_owned(),
                    "the bug".to_owned(),
                ],
            )
            .unwrap(),
            DelegateInvocation {
                command: DelegateCommand::Prompt {
                    target: "Claude 1".to_owned(),
                    text: "fix the bug".to_owned(),
                    wait: true,
                    until: vec!["finished".to_owned()],
                    timeout_secs: 600,
                },
                json: false,
            }
        );
        assert_eq!(
            parse_delegate_command(
                "read",
                vec!["Codex 2".to_owned(), "--lines".to_owned(), "200".to_owned()],
            )
            .unwrap()
            .command,
            DelegateCommand::Read {
                target: "Codex 2".to_owned(),
                lines: Some(200),
            }
        );
        assert_eq!(
            parse_delegate_command("wait", vec!["Codex 2".to_owned()])
                .unwrap()
                .command,
            DelegateCommand::Wait {
                target: "Codex 2".to_owned(),
                until: vec!["finished".to_owned()],
                timeout_secs: 600,
            }
        );
        assert_eq!(
            parse_delegate_command("list", vec!["--json".to_owned()]).unwrap(),
            DelegateInvocation {
                command: DelegateCommand::List,
                json: true,
            }
        );
        assert_eq!(
            parse_delegate_command("close", vec!["Reviewer".to_owned()])
                .unwrap()
                .command,
            DelegateCommand::Close {
                target: "Reviewer".to_owned(),
            }
        );
        assert!(parse_delegate_command("open", Vec::new()).is_err());
        assert!(parse_delegate_command("status", Vec::new()).is_err());
        assert!(parse_delegate_command("list", vec!["extra".to_owned()]).is_err());
        assert!(matches!(
            parse_args_from([OsString::from("open"), OsString::from("claude")]).unwrap(),
            Launch::Delegate(DelegateInvocation {
                command: DelegateCommand::Open { .. },
                ..
            })
        ));
    }

    #[test]
    fn command_line_accepts_the_restore_flag() {
        assert!(matches!(
            parse_args_from([OsString::from("--restore"), OsString::from(".")]).unwrap(),
            Launch::Run { restore: true, .. }
        ));
        assert!(matches!(
            parse_args_from([OsString::from(".")]).unwrap(),
            Launch::Run { restore: false, .. }
        ));
    }

    #[test]
    fn delegation_read_with_lines_reaches_into_scrollback() {
        let spool = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                let session = test_session(definition, title, false);
                {
                    let mut parser = session.parser.lock().unwrap();
                    for index in 0..80 {
                        parser.process(format!("line {index}\r\n").as_bytes());
                    }
                }
                Ok(Box::new(session))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());

        let response = app.handle_delegation(&serde_json::json!({
            "kind": "read", "target": "Codex 1", "lines": 40
        }));

        let output = response["output"].as_str().unwrap();
        assert!(
            output.contains("line 45"),
            "expected a scrolled-off line in the output: {output:?}"
        );
        assert!(output.contains("line 79"));
        assert!(!output.contains("line 30\n"));
        assert!(output.lines().count() <= 40);
    }

    #[test]
    fn delegation_list_enumerates_every_tab() {
        let spool = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.delegation_dir = Some(spool.path().to_path_buf());
        app.observed_states
            .insert("Claude 1".to_owned(), SemanticState::Working);

        let response = app.handle_delegation(&serde_json::json!({ "kind": "list" }));
        let tabs = response["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0]["tab"], serde_json::json!("Codex 1"));
        assert_eq!(tabs[1]["tab"], serde_json::json!("Claude 1"));
        assert_eq!(tabs[1]["state"], serde_json::json!("working"));
        assert_eq!(tabs[1]["agent"], serde_json::json!("claude"));
    }

    #[test]
    fn delegation_close_removes_a_background_tab_without_stealing_focus() {
        let spool = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.add_session(AgentId::Claude).unwrap();
        assert_eq!(app.sessions.active().unwrap().title(), "Claude 2");
        app.delegation_dir = Some(spool.path().to_path_buf());

        let response = app.handle_delegation(&serde_json::json!({
            "kind": "close", "target": "Codex 1"
        }));
        assert_eq!(response["ok"], serde_json::json!(true));
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions.active().unwrap().title(), "Claude 2");

        let missing = app.handle_delegation(&serde_json::json!({
            "kind": "close", "target": "Codex 1"
        }));
        assert_eq!(missing["ok"], serde_json::json!(false));
    }

    #[test]
    fn delegation_open_with_a_custom_title_rejects_duplicates() {
        let spool = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());

        let request = serde_json::json!({
            "kind": "open",
            "agent": "claude",
            "workspace": workspace.path().to_string_lossy(),
            "title": "Reviewer",
        });
        let first = app.handle_delegation(&request);
        assert_eq!(first["tab"], serde_json::json!("Reviewer"));

        let duplicate = app.handle_delegation(&request);
        assert_eq!(duplicate["ok"], serde_json::json!(false));

        app.add_session(AgentId::Claude).unwrap();
        assert_eq!(app.sessions.active().unwrap().title(), "Claude 1");
    }

    #[test]
    fn delegation_open_request_creates_a_visible_tab_and_replies() {
        let spool = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(|definition, title, cwd, _| {
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                Ok(Box::new(session))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());
        let reply = spool.path().join("req-t1.rsp.json");
        fs::write(
            spool.path().join("req-t1.json"),
            serde_json::to_vec(&serde_json::json!({
                "kind": "open",
                "agent": "claude",
                "workspace": workspace.path().to_string_lossy(),
                "prompt": "review this diff",
                "from": "Codex 1",
                "reply": reply.to_string_lossy(),
            }))
            .unwrap(),
        )
        .unwrap();

        app.process_delegation_requests();

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions.active().unwrap().title(), "Claude 1");
        assert_eq!(app.sessions.active().unwrap().workspace(), workspace.path());
        let response: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&reply).unwrap()).unwrap();
        assert_eq!(response["ok"], serde_json::json!(true));
        assert_eq!(response["tab"], serde_json::json!("Claude 1"));
        assert!(!spool.path().join("req-t1.json").exists());
        assert!(app.pending_writes.iter().any(|write| {
            String::from_utf8_lossy(&write.bytes)
                .contains("[Agent Bridge delegation · from Codex 1] review this diff")
        }));
    }

    #[test]
    fn delegation_status_read_and_unknown_kinds_reply_accurately() {
        let spool = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                let session = test_session(definition, title, false);
                session.parser.lock().unwrap().process(b"hello output");
                Ok(Box::new(session))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());
        app.observed_states
            .insert("Codex 1".to_owned(), SemanticState::Working);

        let status = app.handle_delegation(&serde_json::json!({
            "kind": "status", "target": "Codex 1"
        }));
        assert_eq!(status["state"], serde_json::json!("working"));

        let read = app.handle_delegation(&serde_json::json!({
            "kind": "read", "target": "Codex 1"
        }));
        assert!(read["output"].as_str().unwrap().contains("hello output"));

        let missing = app.handle_delegation(&serde_json::json!({
            "kind": "status", "target": "Nope 9"
        }));
        assert_eq!(missing["ok"], serde_json::json!(false));

        let bogus = app.handle_delegation(&serde_json::json!({ "kind": "explode" }));
        assert_eq!(bogus["ok"], serde_json::json!(false));
    }

    #[test]
    fn delegation_reply_path_outside_the_spool_is_refused() {
        let spool = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());
        let escape = outside.path().join("stolen.json");
        fs::write(
            spool.path().join("req-evil.json"),
            serde_json::to_vec(&serde_json::json!({
                "kind": "status",
                "target": "Codex 1",
                "reply": escape.to_string_lossy(),
            }))
            .unwrap(),
        )
        .unwrap();

        app.process_delegation_requests();

        assert!(!escape.exists());
        assert!(!spool.path().join("req-evil.json").exists());
    }

    #[test]
    fn delegation_prompt_reaches_an_existing_tab_with_provenance() {
        let spool = tempfile::tempdir().unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&captured),
                )))
            }),
        );
        app.delegation_dir = Some(spool.path().to_path_buf());

        let response = app.handle_delegation(&serde_json::json!({
            "kind": "prompt",
            "target": "Codex 1",
            "prompt": "run the tests",
            "from": "Claude 1",
        }));

        assert_eq!(response["ok"], serde_json::json!(true));
        assert!(
            String::from_utf8_lossy(&writes.lock().unwrap()[0])
                .contains("[Agent Bridge delegation · from Claude 1] run the tests")
        );
    }

    #[test]
    fn command_line_accepts_only_trusted_hook_states() {
        assert!(matches!(
            parse_args_from([OsString::from("hook"), OsString::from("waiting")]).unwrap(),
            Launch::Hook(SemanticState::Waiting)
        ));
        assert!(parse_args_from([OsString::from("hook"), OsString::from("done")]).is_err());
        assert!(matches!(
            parse_args_from([
                OsString::from("hook"),
                OsString::from("finished"),
                OsString::from("{\"type\":\"agent-turn-complete\"}"),
            ])
            .unwrap(),
            Launch::Hook(SemanticState::Finished)
        ));
        assert!(
            parse_args_from([
                OsString::from("hook"),
                OsString::from("finished"),
                OsString::from("payload"),
                OsString::from("extra"),
            ])
            .is_err()
        );
    }

    #[test]
    fn codex_arguments_inject_a_session_notify_hook_for_finished() {
        let codex = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Codex)
            .unwrap();
        assert_eq!(
            session_arguments(codex, false, None, Some(Path::new("/opt/agent-bridge"))),
            vec![
                OsString::from("-c"),
                OsString::from("notify=['/opt/agent-bridge','hook','finished']"),
            ]
        );

        let claude = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();
        assert_eq!(
            session_arguments(claude, false, None, Some(Path::new("/opt/agent-bridge"))),
            Vec::<OsString>::new()
        );
    }

    #[test]
    fn enter_clears_a_stale_finished_state_before_submitting() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let state = Arc::new(Mutex::new(Some(SemanticState::Finished)));
        let session_state = Arc::clone(&state);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let mut session = test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&captured_writes),
                );
                session.semantic_state = Arc::clone(&session_state);
                Ok(Box::new(session))
            }),
        );
        app.observed_states
            .insert("Codex 1".to_owned(), SemanticState::Finished);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(*state.lock().unwrap(), None);
        assert_eq!(writes.lock().unwrap().as_slice(), [b"\r".to_vec()]);
        assert!(!app.poll_semantic_notifications());
        assert!(!app.observed_states.contains_key("Codex 1"));
    }

    #[test]
    fn semantic_notifications_fire_once_per_waiting_or_finished_transition() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let state = Arc::new(Mutex::new(Some(SemanticState::Idle)));
        let session_state = Arc::clone(&state);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let mut session = test_session(definition, title, false);
                session.semantic_state = Arc::clone(&session_state);
                Ok(Box::new(session))
            }),
        );
        app.notifications = true;

        assert!(!app.poll_semantic_notifications());
        *state.lock().unwrap() = Some(SemanticState::Waiting);
        assert!(app.poll_semantic_notifications());
        assert!(!app.poll_semantic_notifications());
        *state.lock().unwrap() = Some(SemanticState::Working);
        assert!(!app.poll_semantic_notifications());
        *state.lock().unwrap() = Some(SemanticState::Finished);
        assert!(app.poll_semantic_notifications());
    }

    #[test]
    fn heartbeat_poll_caches_semantic_state_for_rail_rendering() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let state = Arc::new(Mutex::new(Some(SemanticState::Working)));
        let session_state = Arc::clone(&state);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let mut session = test_session(definition, title, false);
                session.semantic_state = Arc::clone(&session_state);
                Ok(Box::new(session))
            }),
        );
        app.notifications = false;

        assert!(app.observed_states.is_empty());
        assert!(!app.poll_semantic_notifications());
        assert_eq!(
            app.observed_states.get("Codex 1"),
            Some(&SemanticState::Working)
        );
    }

    #[test]
    fn initial_spawn_failure_becomes_a_notice() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|_, _, _, _| anyhow::bail!("test spawn failed")),
        );

        assert!(app.sessions.is_empty());
        assert!(app.notice.contains("test spawn failed"));
    }

    #[test]
    fn terminal_write_failure_becomes_a_notice() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| Ok(Box::new(test_session(definition, title, true)))),
        );

        assert!(
            !app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .unwrap()
        );
        assert_eq!(app.notice, "test write failed");
    }

    #[test]
    fn f12_opens_help_while_question_mark_reaches_the_cli() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let session_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&session_writes),
                )))
            }),
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(*writes.lock().unwrap(), vec![b"?".to_vec()]);
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.mode, Mode::Help));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn ctrl_f11_sends_the_next_reserved_key_to_the_cli() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let session_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&session_writes),
                )))
            }),
        );

        app.handle_key(KeyEvent::new(KeyCode::F(11), KeyModifiers::CONTROL))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(*writes.lock().unwrap(), vec![b"\x1bOP".to_vec()]);
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn paste_respects_input_mode_and_inner_bracketed_paste() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let session_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let session =
                    test_session_with_writes(definition, title, false, Arc::clone(&session_writes));
                session.parser.lock().unwrap().process(b"\x1b[?2004h");
                Ok(Box::new(session))
            }),
        );

        app.handle_paste("first\nsecond").unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x1b[200~first\nsecond\x1b[201~".to_vec()]
        );

        app.mode = Mode::Search {
            input: String::new(),
        };
        app.handle_paste("needle").unwrap();
        assert!(matches!(app.mode, Mode::Search { ref input } if input == "needle"));
        assert_eq!(writes.lock().unwrap().len(), 1);

        app.mode = Mode::PassThrough;
        app.handle_paste("pasted").unwrap();
        assert_eq!(
            writes.lock().unwrap()[1],
            b"\x1b[200~pasted\x1b[201~".to_vec()
        );
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn control_c_cancels_add_and_relay_modes() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(ctrl_c).unwrap();
        assert!(matches!(app.mode, Mode::Terminal));

        app.add_session(AgentId::Claude).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(ctrl_c).unwrap();
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn relay_requires_confirmation_before_writing() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writes_for_sessions = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&writes_for_sessions),
                )))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);
        app.sessions
            .active()
            .unwrap()
            .parser()
            .lock()
            .unwrap()
            .process(b"SOURCE FINDING: retry path is unsafe");
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(writes.lock().unwrap().is_empty());
        assert!(matches!(app.mode, Mode::Relay { .. }));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(writes.lock().unwrap().len(), 1);
        assert!(
            String::from_utf8_lossy(&writes.lock().unwrap()[0])
                .contains("SOURCE FINDING: retry path is unsafe")
        );
        assert_eq!(app.sessions.active_index(), Some(1));

        app.flush_due_writes(Instant::now() + Duration::from_millis(400));
        assert_eq!(writes.lock().unwrap().len(), 2);
        assert_eq!(writes.lock().unwrap()[1], b"\r".to_vec());
    }

    #[test]
    fn handoff_to_a_working_claude_requires_an_explicit_interrupt_confirmation() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writes_for_sessions = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&writes_for_sessions),
                )))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);
        app.observed_states
            .insert("Claude 1".to_owned(), SemanticState::Working);

        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(writes.lock().unwrap().is_empty());
        assert!(app.notice.contains("working"));
        assert!(matches!(
            app.mode,
            Mode::Relay {
                override_busy: true,
                ..
            }
        ));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(!writes.lock().unwrap().is_empty());
        assert_eq!(app.sessions.active_index(), Some(1));
    }

    #[test]
    fn relay_write_plan_schedules_enter_after_paste_without_blocking() {
        assert_eq!(
            relay_write_plan(false, "go"),
            vec![
                (Duration::ZERO, b"go".to_vec()),
                (Duration::from_millis(50), b"\r".to_vec()),
            ]
        );
        let bracketed = relay_write_plan(true, "go");
        assert_eq!(bracketed[0].1, b"\x1b[200~go\x1b[201~".to_vec());
        assert_eq!(bracketed[1], (Duration::from_millis(50), b"\r".to_vec()));
        assert_eq!(bracketed[2], (Duration::from_millis(300), b"\r".to_vec()));
    }

    #[test]
    fn deferred_relay_writes_drop_when_the_target_is_gone() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                Ok(Box::new(test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&captured),
                )))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        let now = Instant::now();
        app.pending_writes.push(PendingWrite {
            title: "Claude 1".to_owned(),
            bytes: b"\r".to_vec(),
            due: now,
        });
        app.sessions.remove_active();

        app.flush_due_writes(now);

        assert!(app.notice.contains("relay dropped"));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn session_activity_uses_only_observed_liveness_and_recent_io() {
        assert_eq!(
            classify_activity(false, Duration::ZERO),
            SessionActivity::Exited
        );
        assert_eq!(
            classify_activity(true, Duration::from_millis(500)),
            SessionActivity::Active
        );
        assert_eq!(
            classify_activity(true, Duration::from_secs(3)),
            SessionActivity::Quiet
        );
    }

    #[test]
    fn f9_restarts_an_exited_session_in_place() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let spawn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_count_for_factory = Arc::clone(&spawn_count);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let mut session = test_session(definition, title, false);
                session.alive = spawn_count_for_factory.fetch_add(1, Ordering::SeqCst) > 0;
                Ok(Box::new(session))
            }),
        );
        assert!(!app.sessions.active().unwrap().is_alive());
        app.observed_states
            .insert("Codex 1".to_owned(), SemanticState::Finished);

        app.handle_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE))
            .unwrap();

        assert!(app.sessions.active().unwrap().is_alive());
        assert_eq!(app.sessions.active().unwrap().title(), "Codex 1");
        assert_eq!(spawn_count.load(Ordering::SeqCst), 2);
        assert!(!app.observed_states.contains_key("Codex 1"));
    }

    #[test]
    fn new_tabs_can_spawn_in_a_workspace_distinct_from_the_launch_workspace() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let spawned = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let captured = Arc::clone(&spawned);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(move |definition, title, cwd, _| {
                captured.lock().unwrap().push(cwd.to_path_buf());
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                Ok(Box::new(session))
            }),
        );

        app.add_session_at(AgentId::Claude, Path::new("other-workspace"))
            .unwrap();

        assert_eq!(
            *spawned.lock().unwrap(),
            vec![
                PathBuf::from("launch-workspace"),
                PathBuf::from("other-workspace")
            ]
        );
        assert_eq!(
            app.sessions.active().unwrap().workspace(),
            Path::new("other-workspace")
        );
    }

    #[test]
    fn restart_preserves_the_tabs_original_workspace() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let spawned = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let captured = Arc::clone(&spawned);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_for_factory = Arc::clone(&count);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(move |definition, title, cwd, _| {
                captured.lock().unwrap().push(cwd.to_path_buf());
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                session.alive = count_for_factory.fetch_add(1, Ordering::SeqCst) == 0;
                Ok(Box::new(session))
            }),
        );
        app.add_session_at(AgentId::Claude, Path::new("other-workspace"))
            .unwrap();

        app.restart_active().unwrap();

        assert_eq!(
            spawned.lock().unwrap().last(),
            Some(&PathBuf::from("other-workspace"))
        );
    }

    #[test]
    fn active_workspace_for_git_polling_follows_the_active_tab() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("launch-workspace"),
            redraw,
            Box::new(|definition, title, cwd, _| {
                let mut session = test_session(definition, title, false);
                session.workspace = cwd.to_path_buf();
                Ok(Box::new(session))
            }),
        );
        app.add_session_at(AgentId::Claude, Path::new("other-workspace"))
            .unwrap();

        app.sync_active_workspace();
        assert_eq!(
            *app.active_workspace.lock().unwrap(),
            PathBuf::from("other-workspace")
        );

        app.sessions.set_active(0);
        app.sync_active_workspace();
        assert_eq!(
            *app.active_workspace.lock().unwrap(),
            PathBuf::from("launch-workspace")
        );
    }

    #[test]
    fn f7_search_jumps_to_matching_session_output() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let spawn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_count_for_factory = Arc::clone(&spawn_count);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let session = test_session(definition, title, false);
                if spawn_count_for_factory.fetch_add(1, Ordering::SeqCst) == 1 {
                    session.parser.lock().unwrap().process(b"unique needle");
                }
                Ok(Box::new(session))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);

        app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
            .unwrap();
        for character in "NEEDLE".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.sessions.active_index(), Some(1));
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn git_context_parses_branch_and_dirty_state() {
        assert_eq!(
            parse_git_context("feature/search\n", " M src/main.rs\n"),
            GitContext {
                branch: "feature/search".to_owned(),
                dirty: true,
            }
        );
        assert_eq!(
            parse_git_context("main\n", ""),
            GitContext {
                branch: "main".to_owned(),
                dirty: false,
            }
        );
    }

    #[test]
    fn git_diff_works_before_the_first_commit() {
        let repository = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .arg("init")
            .arg(repository.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fs::write(repository.path().join("untracked.txt"), "new file").unwrap();

        assert_eq!(
            read_git_diff(repository.path()).unwrap(),
            "No tracked changes."
        );
    }

    #[test]
    fn claude_hook_settings_use_schema_valid_command_strings() {
        let settings = claude_hook_settings(Path::new("C:/tools/agent-bridge.exe"));
        assert_eq!(
            settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            serde_json::json!("\"C:/tools/agent-bridge.exe\" hook working")
        );
        assert!(settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["args"].is_null());
        assert!(settings["hooks"]["PermissionRequest"].is_null());
        assert_eq!(
            settings["hooks"]["Notification"][0]["matcher"],
            serde_json::json!("*")
        );
    }

    #[test]
    fn hook_adapter_does_not_overwrite_a_preclaimed_legacy_path() {
        let claimed = std::env::temp_dir().join(format!(
            "agent-bridge-{}-preclaimed.status",
            std::process::id()
        ));
        fs::write(&claimed, "attacker-owned").unwrap();

        let claude = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();
        let adapter = prepare_hook_adapter(claude).unwrap().unwrap();

        assert_eq!(fs::read_to_string(&claimed).unwrap(), "attacker-owned");
        drop(adapter);
        let _ = fs::remove_file(claimed);
    }

    #[test]
    fn tui_requires_an_interactive_terminal() {
        assert!(ensure_interactive_terminal(false).is_err());
        assert!(ensure_interactive_terminal(true).is_ok());
    }

    #[test]
    fn semantic_state_parser_rejects_untrusted_values() {
        assert_eq!(
            parse_semantic_state("waiting"),
            Some(SemanticState::Waiting)
        );
        assert_eq!(parse_semantic_state("idle\n"), Some(SemanticState::Idle));
        assert_eq!(parse_semantic_state("definitely-done"), None);
    }

    #[test]
    fn scrollback_mode_moves_through_history_and_returns_live() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                let session = test_session(definition, title, false);
                for index in 0..80 {
                    session
                        .parser
                        .lock()
                        .unwrap()
                        .process(format!("line {index}\r\n").as_bytes());
                }
                Ok(Box::new(session))
            }),
        );

        app.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.mode, Mode::Scrollback));
        assert!(app.sessions.active().unwrap().scrollback().unwrap() > 0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.mode, Mode::Terminal));
        assert_eq!(app.sessions.active().unwrap().scrollback().unwrap(), 0);
    }

    #[test]
    fn rail_rows_map_to_session_indices_within_bounds() {
        let rail = Rect::new(0, 3, 24, 30);
        assert_eq!(rail_row_to_session_index(rail, 3, 3), None);
        assert_eq!(rail_row_to_session_index(rail, 4, 3), Some(0));
        assert_eq!(rail_row_to_session_index(rail, 5, 3), Some(0));
        assert_eq!(rail_row_to_session_index(rail, 6, 3), Some(1));
        assert_eq!(rail_row_to_session_index(rail, 8, 3), Some(2));
        assert_eq!(rail_row_to_session_index(rail, 10, 3), None);
    }

    #[test]
    fn add_screen_agent_chips_resolve_by_column() {
        assert_eq!(agent_chip_at(0, 0), Some(0));
        assert_eq!(agent_chip_at(6, 0), Some(0));
        assert_eq!(agent_chip_at(7, 0), None);
        assert_eq!(agent_chip_at(9, 0), Some(1));
        assert_eq!(agent_chip_at(16, 0), Some(1));
        assert_eq!(agent_chip_at(19, 0), Some(2));
        assert_eq!(agent_chip_at(24, 0), None);
    }

    #[test]
    fn clicking_a_rail_entry_activates_that_session() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);
        app.rail_pane = Rect::new(0, 3, 24, 30);

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 6,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert_eq!(app.sessions.active_index(), Some(1));
        assert!(app.notice.contains("switched to"));
    }

    #[test]
    fn clicking_the_rail_during_relay_picks_the_target() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);
        app.rail_pane = Rect::new(0, 3, 24, 30);

        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.mode, Mode::Relay { confirm: true, .. }));

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 8,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert!(matches!(
            app.mode,
            Mode::Relay {
                target: 2,
                confirm: false,
                ..
            }
        ));
    }

    #[test]
    fn wheel_over_the_rail_switches_sessions() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.add_session(AgentId::Claude).unwrap();
        app.rail_pane = Rect::new(0, 3, 24, 30);

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 6,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert_eq!(app.sessions.active_index(), Some(0));
        assert!(matches!(app.mode, Mode::Terminal));
    }

    #[test]
    fn mouse_wheel_scrolls_the_diff_view() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|_, _, _, _| anyhow::bail!("unused")),
        );
        app.mode = Mode::Diff {
            text: "one\ntwo\nthree\nfour\nfive\nsix".to_owned(),
            offset: 0,
        };

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Diff { offset: 3, .. }));

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Diff { offset: 5, .. }));

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Diff { offset: 2, .. }));
    }

    #[test]
    fn clicking_an_agent_chip_selects_that_cli_on_the_add_screen() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|definition, title, _, _| {
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );
        app.footer_pane = Rect::new(0, 33, 120, 4);
        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))
            .unwrap();

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 34,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert!(matches!(app.mode, Mode::Add { selected: 1, .. }));
    }

    #[test]
    fn mouse_wheel_scrolls_history_without_writing_to_the_cli() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let session = test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&captured_writes),
                );
                for index in 0..80 {
                    session
                        .parser
                        .lock()
                        .unwrap()
                        .process(format!("line {index}\r\n").as_bytes());
                }
                Ok(Box::new(session))
            }),
        );

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert!(app.sessions.active().unwrap().scrollback().unwrap() > 0);
        assert!(matches!(app.mode, Mode::Scrollback));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn sgr_mouse_reporting_is_forwarded_with_terminal_relative_coordinates() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured_writes = Arc::clone(&writes);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(move |definition, title, _, _| {
                let session = test_session_with_writes(
                    definition,
                    title,
                    false,
                    Arc::clone(&captured_writes),
                );
                session
                    .parser
                    .lock()
                    .unwrap()
                    .process(b"\x1b[?1000h\x1b[?1006h");
                Ok(Box::new(session))
            }),
        );
        app.terminal_pane = Rect::new(25, 3, 95, 30);

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 30,
                row: 7,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert_eq!(writes.lock().unwrap().as_slice(), [b"\x1b[<64;5;4M"]);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "live smoke test: requires installed and authenticated Codex CLI"]
    fn live_codex_prompt_survives_mouse_wheel_scrolling() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new(Path::new(env!("CARGO_MANIFEST_DIR")), redraw, false);
        app.terminal_pane = Rect::new(25, 3, 95, 30);
        let session = app.sessions.active().expect("Codex session starts");
        assert!(
            wait_for_screen_text(session.as_ref(), "codex", Duration::from_secs(20)),
            "Codex prompt did not become ready"
        );
        session.write(b"wheel-preserves-this").unwrap();
        assert!(wait_for_screen_text(
            session.as_ref(),
            "wheel-preserves-this",
            Duration::from_secs(5)
        ));

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();

        assert!(
            app.sessions
                .active()
                .unwrap()
                .find_text("wheel-preserves-this")
                .unwrap()
                .is_some()
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "live smoke test: requires installed and authenticated Agy CLI"]
    fn live_agy_effort_label_survives_resize_with_right_margin() {
        let definition = AgentDefinition {
            id: AgentId::Agy,
            command: "agy --effort high",
            role: "live-smoke-test",
        };
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut session = AgentSession::spawn(
            definition,
            "Agy live smoke test".to_owned(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            redraw,
            false,
            &[],
        )
        .unwrap();
        assert!(
            wait_for_screen_text(&session, "High", Duration::from_secs(20)),
            "Agy did not render the full High effort label at 100 columns"
        );

        session.resize(68, 24).unwrap();
        assert!(
            wait_for_screen_text(&session, "High", Duration::from_secs(10)),
            "Agy did not retain the full High effort label after narrowing to 68 columns"
        );
        session.resize(100, 32).unwrap();
        assert!(
            wait_for_screen_text(&session, "High", Duration::from_secs(10)),
            "Agy did not retain the full High effort label after widening again"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "live smoke test: requires installed and authenticated Claude CLI"]
    fn live_claude_starts_through_conpty_compatible_runtime() {
        let definition = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut session = AgentSession::spawn(
            definition,
            "Claude live smoke test".to_owned(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            redraw,
            false,
            &[],
        )
        .unwrap();

        assert!(
            wait_for_screen_text(&session, "Claude Code", Duration::from_secs(20)),
            "Claude Code prompt did not become ready; alive: {}; screen: {:?}",
            session.is_alive(),
            session.parser.lock().unwrap().screen().contents()
        );
        assert!(
            !session
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("내부 또는 외부 명령")
        );
        session.resize(68, 24).unwrap();
        assert!(wait_for_screen_text(
            &session,
            "Claude Code",
            Duration::from_secs(10)
        ));
    }

    #[test]
    #[ignore = "live smoke test: requires installed and authenticated Codex and Claude CLIs"]
    fn live_relay_delivers_a_prompt_to_another_cli_session() {
        let (redraw, _) = crossbeam_channel::bounded(32);
        let mut app = App::new(Path::new(env!("CARGO_MANIFEST_DIR")), redraw, false);
        assert!(wait_for_screen_text(
            app.sessions.active().unwrap().as_ref(),
            "codex",
            Duration::from_secs(20)
        ));
        submit_relay(
            app.sessions.active().unwrap().as_ref(),
            "Output only the uppercase concatenation of: source, underscore, context, underscore, 731.",
        )
        .unwrap();
        let source_ready = wait_for_screen_text(
            app.sessions.active().unwrap().as_ref(),
            "SOURCE_CONTEXT_731",
            Duration::from_secs(30),
        );
        let source_screen = app.sessions.active().unwrap().parser().lock().unwrap();
        assert!(
            source_ready,
            "handoff source screen:\n{}",
            source_screen.screen().contents()
        );
        drop(source_screen);
        app.add_session(AgentId::Claude).unwrap();
        app.sessions.set_active(0);
        assert!(wait_for_screen_text(
            app.sessions.get(1).unwrap().as_ref(),
            "Claude Code",
            Duration::from_secs(20)
        ));

        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            .unwrap();
        app.handle_paste(
            "Find the marker beginning with SOURCE_CONTEXT in the recent source terminal context, append underscore followed by ACK, and output only the resulting value.",
        )
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.sessions.active_index(), Some(1));
        let delivered = wait_for_screen_text(
            app.sessions.active().unwrap().as_ref(),
            "SOURCE_CONTEXT_731_ACK",
            Duration::from_secs(30),
        );
        let screen = app.sessions.active().unwrap().parser().lock().unwrap();
        assert!(
            delivered,
            "relay target screen:\n{}",
            screen.screen().contents()
        );
    }

    #[test]
    fn incoming_output_keeps_a_scrollback_view_anchored() {
        let mut parser = vt100::Parser::new(4, 40, 100);
        for index in 0..20 {
            parser.process(format!("line {index}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(5);

        process_output(&mut parser, b"new line\r\n");

        assert_eq!(parser.screen().scrollback(), 6);
    }

    #[test]
    fn diff_scrolling_stops_at_the_last_line() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let mut app = App::new_with_spawner(
            Path::new("workspace"),
            redraw,
            Box::new(|_, _, _, _| anyhow::bail!("unused")),
        );
        app.mode = Mode::Diff {
            text: "one\ntwo\nthree".to_owned(),
            offset: 0,
        };
        for _ in 0..10 {
            app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
                .unwrap();
        }
        assert!(matches!(app.mode, Mode::Diff { offset: 2, .. }));
    }

    #[test]
    fn pending_bell_is_written_once_to_the_terminal_backend() {
        let mut output = Vec::new();
        let mut pending = true;
        emit_pending_bell(&mut output, &mut pending).unwrap();
        emit_pending_bell(&mut output, &mut pending).unwrap();
        assert_eq!(output, b"\x07");
    }

    #[test]
    fn command_line_supports_help_version_and_a_workspace() {
        assert!(matches!(
            parse_args_from([std::ffi::OsString::from("--help")]).unwrap(),
            Launch::Help
        ));
        assert!(matches!(
            parse_args_from([std::ffi::OsString::from("--version")]).unwrap(),
            Launch::Version
        ));
        assert!(matches!(
            parse_args_from([std::ffi::OsString::from(".")]).unwrap(),
            Launch::Run { yolo: false, .. }
        ));
    }

    #[test]
    fn command_line_without_a_workspace_inherits_process_cwd() {
        let expected = std::env::current_dir().unwrap();
        assert!(matches!(
            parse_args_from(Vec::<OsString>::new()).unwrap(),
            Launch::Run { cwd, yolo: false, .. } if cwd == expected
        ));
    }

    #[test]
    fn command_line_accepts_yolo_before_or_after_workspace() {
        for args in [
            vec![OsString::from("--yolo"), OsString::from(".")],
            vec![OsString::from("-yolo"), OsString::from(".")],
            vec![OsString::from("."), OsString::from("--yolo")],
            vec![OsString::from("--yolo")],
        ] {
            assert!(matches!(
                parse_args_from(args).unwrap(),
                Launch::Run { yolo: true, .. }
            ));
        }
    }

    #[test]
    fn yolo_mode_is_forwarded_to_initial_and_new_tab_spawns() {
        let (redraw, _) = crossbeam_channel::bounded(1);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_spawner = Arc::clone(&observed);
        let mut app = App::new_with_spawner_yolo(
            Path::new("workspace"),
            redraw,
            true,
            Box::new(move |definition, title, _, _, yolo, _| {
                observed_by_spawner.lock().unwrap().push(yolo);
                Ok(Box::new(test_session(definition, title, false)))
            }),
        );

        app.add_session(AgentId::Claude).unwrap();

        assert_eq!(*observed.lock().unwrap(), vec![true, true]);
    }

    #[test]
    fn session_arguments_add_only_the_selected_cli_danger_flag() {
        let definitions = agents();
        let expected = [
            (AgentId::Codex, "--dangerously-bypass-approvals-and-sandbox"),
            (AgentId::Claude, "--dangerously-skip-permissions"),
            (AgentId::Agy, "--dangerously-skip-permissions"),
        ];

        for (id, danger_flag) in expected {
            let definition = definitions.iter().find(|item| item.id == id).unwrap();
            assert_eq!(
                session_arguments(*definition, false, None, None),
                Vec::<OsString>::new()
            );
            assert_eq!(
                session_arguments(*definition, true, None, None),
                vec![OsString::from(danger_flag)]
            );
        }
    }

    #[test]
    fn claude_yolo_arguments_keep_session_settings() {
        let definition = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();
        assert_eq!(
            session_arguments(
                definition,
                true,
                Some(Path::new("hook settings.json")),
                None
            ),
            vec![
                OsString::from("--dangerously-skip-permissions"),
                OsString::from("--settings"),
                OsString::from("hook settings.json"),
            ]
        );
    }

    #[test]
    fn yolo_mode_has_a_persistent_warning_label() {
        assert_eq!(yolo_label(false), "");
        assert!(yolo_label(true).contains("YOLO"));
    }

    #[test]
    fn help_documents_the_delegation_surface_for_agents() {
        let help = help_text();
        for needle in [
            "open <agent>",
            "prompt <tab>",
            "status <tab>",
            "read <tab>",
            "wait <tab>",
            "agent-bridge list",
            "close <tab>",
            "--lines N",
            "--json",
            "--restore",
            "--title",
            "AGENT_BRIDGE_REQUESTS",
            "instance.json",
            "provenance",
            "quiet does not mean done",
            "Example round trip",
        ] {
            assert!(help.contains(needle), "help is missing {needle:?}");
        }
        assert!(matches!(
            parse_args_from([OsString::from("open"), OsString::from("--help")]).unwrap(),
            Launch::Help
        ));
        assert!(matches!(
            parse_args_from([OsString::from("wait"), OsString::from("-h")]).unwrap(),
            Launch::Help
        ));
    }

    #[test]
    fn help_describes_yolo_risk() {
        let help = help_text();
        assert!(help.contains("--yolo"));
        assert!(help.contains("bypass approval and sandbox protections"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolves_claude_from_official_user_install_when_path_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let install_dir = home.path().join(".local").join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let executable = install_dir.join("claude.exe");
        fs::write(&executable, b"test executable marker").unwrap();
        let definition = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();

        assert_eq!(
            resolve_windows_agent_command(
                definition,
                Some(std::ffi::OsStr::new("")),
                Some(home.path().as_os_str())
            ),
            executable
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_executable_suffix_over_extensionless_shell_shim() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("agy"), b"shell shim").unwrap();
        let executable = directory.path().join("agy.exe");
        fs::write(&executable, b"native executable").unwrap();
        let definition = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Agy)
            .unwrap();

        assert_eq!(
            resolve_windows_agent_command(definition, Some(directory.path().as_os_str()), None),
            executable
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_resolves_claude_from_official_user_install_when_path_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let install_dir = home.path().join(".local").join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let executable = install_dir.join("claude");
        fs::write(&executable, b"test executable marker").unwrap();
        let definition = agents()
            .into_iter()
            .find(|item| item.id == AgentId::Claude)
            .unwrap();

        assert_eq!(
            resolve_unix_agent_command(
                definition,
                Some(std::ffi::OsStr::new("")),
                Some(home.path().as_os_str())
            ),
            executable
        );
    }

    #[test]
    fn terminal_device_attribute_queries_receive_xterm_responses() {
        assert_eq!(
            terminal_query_response(b"\x1b[c"),
            Some(b"\x1b[?1;2c".as_slice())
        );
        assert_eq!(
            terminal_query_response(b"prefix\x1b[>csuffix"),
            Some(b"\x1b[>0;276;0c".as_slice())
        );
        assert_eq!(terminal_query_response(b"ordinary output"), None);
    }

    #[test]
    fn command_line_rejects_unknown_options_and_extra_arguments() {
        assert!(parse_args_from([std::ffi::OsString::from("--wat")]).is_err());
        assert!(
            parse_args_from([
                std::ffi::OsString::from("."),
                std::ffi::OsString::from("extra")
            ])
            .is_err()
        );
    }

    #[test]
    fn pty_size_matches_the_terminal_block_inner_area() {
        let area = Rect::new(24, 3, 96, 30);
        assert_eq!(terminal_inner_size(area), (92, 28));
    }

    #[test]
    fn pty_keeps_two_columns_clear_of_the_visible_right_margin() {
        let area = Rect::new(25, 3, 95, 30);
        let (pty_cols, _) = terminal_inner_size(area);
        let visible_cols = area.width.saturating_sub(2);
        assert_eq!(visible_cols.saturating_sub(pty_cols), 2);
    }

    #[test]
    fn shared_layout_drives_the_terminal_pane() {
        let area = Rect::new(0, 0, 120, 37);
        let (_, _, terminal, _) = app_layout(area);
        assert_eq!(terminal, Rect::new(25, 3, 95, 30));
    }

    #[test]
    fn alt_keys_receive_an_escape_prefix() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT), false),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), false),
            Some(b"\x1b\r".to_vec())
        );
    }

    #[test]
    fn non_alphabetic_control_keys_use_terminal_control_bytes() {
        for (character, expected) in [
            ('[', 0x1b),
            ('\\', 0x1c),
            (']', 0x1d),
            ('^', 0x1e),
            ('_', 0x1f),
            (' ', 0x00),
        ] {
            assert_eq!(
                encode_key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
                    false
                ),
                Some(vec![expected]),
                "Ctrl+{character:?}"
            );
        }
    }

    #[test]
    fn terminal_function_keys_are_encoded() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), false),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn application_cursor_mode_changes_cursor_sequences() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true),
            Some(b"\x1bOH".to_vec())
        );
    }
}
