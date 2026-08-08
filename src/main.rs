use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Child as StdChild, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agent_bridge::{AgentDefinition, AgentId, TabSet, agents, relay_text_from, session_title};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
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
    settings_file: tempfile::NamedTempFile,
}

fn session_arguments(
    definition: AgentDefinition,
    yolo: bool,
    claude_settings: Option<&Path>,
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
        "agent-bridge {}\n\nUsage: agent-bridge [--yolo] [WORKSPACE]\n\nOptions:\n  --yolo  DANGER: bypass approval and sandbox protections in every spawned CLI session",
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

    fn settings_path(&self) -> &Path {
        self.settings_file.path()
    }
}

fn prepare_hook_adapter(definition: AgentDefinition) -> Result<Option<HookAdapter>> {
    if definition.id != AgentId::Claude {
        return Ok(None);
    }
    let mut status_file = tempfile::Builder::new()
        .prefix("agent-bridge-")
        .suffix(".status")
        .tempfile_in(std::env::temp_dir())?;
    status_file.write_all(b"idle")?;
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let settings = serde_json::to_vec_pretty(&claude_hook_settings(&executable))?;
    let mut settings_file = tempfile::Builder::new()
        .prefix("agent-bridge-")
        .suffix(".settings.json")
        .tempfile_in(std::env::temp_dir())?;
    settings_file.write_all(&settings)?;
    Ok(Some(HookAdapter {
        status_file,
        settings_file,
    }))
}

impl AgentSession {
    fn arguments(
        definition: AgentDefinition,
        yolo: bool,
        hook_adapter: Option<&HookAdapter>,
    ) -> Vec<OsString> {
        session_arguments(
            definition,
            yolo,
            hook_adapter.map(HookAdapter::settings_path),
        )
    }

    #[cfg(windows)]
    fn spawn(
        definition: AgentDefinition,
        title: String,
        cwd: &Path,
        redraw: Sender<()>,
        yolo: bool,
    ) -> Result<Self> {
        let hook_adapter = prepare_hook_adapter(definition)?;
        let program = resolve_windows_agent_command(
            definition,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("USERPROFILE").as_deref(),
        );
        let arguments = Self::arguments(definition, yolo, hook_adapter.as_ref());
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
        let parser = Arc::new(Mutex::new(vt100::Parser::new(32, 100, 2_000)));
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
    ) -> Result<Self> {
        let hook_adapter = prepare_hook_adapter(definition)?;
        let pair = native_pty_system().openpty(PtySize {
            rows: 32,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let program = PathBuf::from(definition.command);
        let mut command = CommandBuilder::new(program);
        command.cwd(cwd);
        for argument in Self::arguments(definition, yolo, hook_adapter.as_ref()) {
            command.arg(argument);
        }
        if let Some(adapter) = &hook_adapter {
            command.env("AGENT_BRIDGE_STATUS_FILE", adapter.status_path());
        }
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to start {}", definition.command))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let writer_for_reader = Arc::clone(&writer);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(32, 100, 2_000)));
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
    fn parser(&self) -> &Arc<Mutex<vt100::Parser>>;
    fn is_alive(&self) -> bool;
    fn write(&self, bytes: &[u8]) -> Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn scrollback(&self) -> Result<usize>;
    fn set_scrollback(&self, rows: usize) -> Result<()>;
    fn activity(&self) -> SessionActivity;
    fn semantic_state(&self) -> Option<SemanticState>;
    fn contains_text(&self, query: &str) -> Result<bool>;
    fn application_cursor(&self) -> bool;
    fn bracketed_paste(&self) -> bool;
    fn mouse_protocol(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding);
}

impl SessionIo for AgentSession {
    fn definition(&self) -> AgentDefinition {
        self.definition
    }

    fn title(&self) -> &str {
        &self.title
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

    fn contains_text(&self, query: &str) -> Result<bool> {
        parser_contains_text(&self.parser, query)
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

fn parser_contains_text(parser: &Arc<Mutex<vt100::Parser>>, query: &str) -> Result<bool> {
    let query = query.to_lowercase();
    let mut parser = parser
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser poisoned"))?;
    let screen = parser.screen_mut();
    let original = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let maximum = screen.scrollback();
    let page = usize::from(screen.size().0).max(1);
    let mut offset = maximum;
    let mut found = false;
    loop {
        screen.set_scrollback(offset);
        if screen.contents().to_lowercase().contains(&query) {
            found = true;
            break;
        }
        if offset == 0 {
            break;
        }
        offset = offset.saturating_sub(page);
    }
    screen.set_scrollback(original);
    Ok(found)
}

type SessionSpawner =
    Box<dyn FnMut(AgentDefinition, String, &Path, Sender<()>) -> Result<Box<dyn SessionIo>>>;

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
    },
    Relay {
        target: usize,
        input: String,
        confirm: bool,
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

fn spawn_git_context_reader(cwd: PathBuf) -> Receiver<Option<GitContext>> {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        loop {
            if sender.send(read_git_context(&cwd)).is_err() {
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

struct App {
    sessions: TabSet<Box<dyn SessionIo>>,
    cwd: PathBuf,
    redraw: Sender<()>,
    spawner: SessionSpawner,
    git_context: Option<GitContext>,
    ordinals: [u32; 3],
    mode: Mode,
    notice: String,
    notifications: bool,
    observed_states: HashMap<String, SemanticState>,
    yolo: bool,
    terminal_pane: Rect,
}

impl App {
    fn new(cwd: &Path, redraw: Sender<()>, yolo: bool) -> Self {
        Self::new_with_spawner(
            cwd,
            redraw,
            Box::new(move |definition, title, cwd, redraw| {
                Ok(Box::new(AgentSession::spawn(
                    definition, title, cwd, redraw, yolo,
                )?))
            }),
        )
        .with_yolo(yolo)
    }

    fn new_with_spawner(cwd: &Path, redraw: Sender<()>, spawner: SessionSpawner) -> Self {
        let mut app = Self {
            sessions: TabSet::new(),
            cwd: cwd.to_path_buf(),
            redraw,
            spawner,
            git_context: read_git_context(cwd),
            ordinals: [0; 3],
            mode: Mode::Terminal,
            notice: format!("workspace: {}", cwd.display()),
            notifications: notifications_enabled(),
            observed_states: HashMap::new(),
            yolo: false,
            terminal_pane: Rect::default(),
        };
        if let Err(error) = app.add_session(AgentId::Codex) {
            app.notice = format!("{error:#}");
        }
        app
    }

    fn with_yolo(mut self, yolo: bool) -> Self {
        self.yolo = yolo;
        self
    }

    fn add_session(&mut self, id: AgentId) -> Result<()> {
        let definition = agents()
            .into_iter()
            .find(|definition| definition.id == id)
            .expect("registered agent");
        let ordinal_index = agent_index(id);
        let ordinal = self.ordinals[ordinal_index] + 1;
        let title = session_title(id, ordinal);
        let session = (self.spawner)(definition, title.clone(), &self.cwd, self.redraw.clone())
            .with_context(|| format!("failed to create {title}"))?;
        self.ordinals[ordinal_index] = ordinal;
        self.sessions.push(session);
        self.notice = format!("created {title}");
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
        let replacement = (self.spawner)(definition, title.clone(), &self.cwd, self.redraw.clone())
            .with_context(|| format!("failed to restart {title}"))?;
        self.sessions
            .replace_active(replacement)
            .expect("active session exists");
        self.observed_states.remove(&title);
        self.notice = format!("restarted {title} (fresh session)");
        Ok(())
    }

    fn find_session(&self, query: &str) -> Result<Option<usize>> {
        if query.trim().is_empty() || self.sessions.is_empty() {
            return Ok(None);
        }
        let query_lower = query.to_lowercase();
        let start = self.sessions.active_index().unwrap_or(0);
        for step in 1..=self.sessions.len() {
            let index = (start + step) % self.sessions.len();
            let session = self.sessions.get(index).expect("session index");
            if session.title().to_lowercase().contains(&query_lower)
                || session.contains_text(query)?
            {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    fn poll_semantic_notifications(&mut self) -> bool {
        let mut bell = false;
        for session in self.sessions.items() {
            let Some(state) = session.semantic_state() else {
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
            Mode::Relay { input, confirm, .. } => {
                input.push_str(text);
                *confirm = false;
            }
            Mode::Help | Mode::Scrollback | Mode::Diff { .. } | Mode::Add { .. } => {}
        }
        if pass_through {
            self.mode = Mode::Terminal;
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
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
                KeyCode::F(1) => match read_git_diff(&self.cwd) {
                    Ok(text) => self.mode = Mode::Diff { text, offset: 0 },
                    Err(error) => self.notice = error.to_string(),
                },
                KeyCode::F(3) => self.mode = Mode::Add { selected: 0 },
                KeyCode::F(4) => {
                    if let Some(session) = self.sessions.remove_active() {
                        self.observed_states.remove(session.title());
                        self.notice = format!("closed {}", session.title());
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
                    };
                }
                KeyCode::F(2) => {
                    self.notice = "relay needs at least two sessions".to_owned();
                }
                _ => {
                    if let Some(session) = self.sessions.active()
                        && let Some(bytes) = encode_key(key, session.application_cursor())
                        && let Err(error) = session.write(&bytes)
                    {
                        self.notice = error.to_string();
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
                    && let Err(error) = session.write(&bytes)
                {
                    self.notice = error.to_string();
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
                    Ok(Some(index)) => {
                        self.sessions.set_active(index);
                        self.notice =
                            format!("search matched {}", self.sessions.active().unwrap().title());
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
            Mode::Add { mut selected } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                    selected = selected.checked_sub(1).unwrap_or(agents().len() - 1);
                    self.mode = Mode::Add { selected };
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                    selected = (selected + 1) % agents().len();
                    self.mode = Mode::Add { selected };
                }
                KeyCode::Enter => {
                    if let Err(error) = self.add_session(agents()[selected].id) {
                        self.notice = format!("{error:#}");
                        self.mode = Mode::Add { selected };
                    }
                }
                _ => self.mode = Mode::Add { selected },
            },
            Mode::Relay {
                mut target,
                mut input,
                mut confirm,
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
                    };
                }
                KeyCode::Right | KeyCode::Tab => {
                    target = (target + 1) % self.sessions.len();
                    confirm = false;
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm,
                    };
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm: false,
                    };
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(character);
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm: false,
                    };
                }
                KeyCode::Enter => {
                    let Some(source_session) = self.sessions.active() else {
                        return Ok(false);
                    };
                    let source_title = source_session.title().to_owned();
                    match relay_text_from(&source_title, &input) {
                        Ok(message) => {
                            let destination = self.sessions.get(target).expect("relay target");
                            if !destination.is_alive() {
                                self.notice = format!("{} has exited", destination.title());
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm,
                                };
                            } else if !confirm {
                                self.notice = format!(
                                    "press Enter again to relay to {}",
                                    destination.title()
                                );
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm: true,
                                };
                            } else if let Err(error) =
                                destination.write(format!("{message}\r").as_bytes())
                            {
                                self.notice = error.to_string();
                                self.mode = Mode::Relay {
                                    target,
                                    input,
                                    confirm,
                                };
                            } else {
                                self.notice =
                                    format!("{source_title} → {} relayed", destination.title());
                                self.sessions.set_active(target);
                            }
                        }
                        Err(error) => {
                            self.notice = error.to_string();
                            self.mode = Mode::Relay {
                                target,
                                input,
                                confirm,
                            };
                        }
                    }
                }
                _ => {
                    self.mode = Mode::Relay {
                        target,
                        input,
                        confirm,
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

fn render(frame: &mut Frame, app: &App) {
    let (header, rail, terminal, footer) = app_layout(frame.area());
    let git_label = app
        .git_context
        .as_ref()
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
                 F2      relay prompt (Enter twice to send)\n\
                 F3      create session\n\
                 F4      close active session\n\
                 F5/F6   previous/next session\n\
                 F7      search titles and terminal history\n\
                 F8      browse scrollback\n\
                 F9      restart an exited session\n\
                 F10     quit Agent Bridge\n\
                 Ctrl+F11 send the next key directly to the CLI\n\
                 Shift+drag terminal-native text selection during mouse capture\n\
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
        Mode::Add { selected } => {
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
                    "{}←/→ choose CLI  ·  Enter create tab  ·  Esc cancel",
                    if app.yolo { "YOLO · " } else { "" }
                )),
            ]
        }
        Mode::Relay {
            target,
            input,
            confirm,
        } => {
            let target_name = app
                .sessions
                .get(*target)
                .map(|session| session.title())
                .unwrap_or("?");
            vec![
                Line::from(vec![
                    Span::styled(
                        format!(" RELAY → {target_name} "),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ),
                    Span::raw(format!(" {input}")),
                ]),
                Line::from(if *confirm {
                    " Enter confirm send  ·  edit/target change resets confirmation  ·  Esc cancel"
                } else {
                    " ←/→ target  ·  Enter review  ·  Esc cancel"
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
                session
                    .semantic_state()
                    .map(SemanticState::label)
                    .unwrap_or_else(|| activity.label())
            };
            ListItem::new(format!(" {marker} {} [{}]", session.title(), status)).style(style)
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
        " {} · {} · {} ",
        session.title(),
        definition.role,
        definition.command,
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

fn run(terminal: &mut DefaultTerminal, cwd: &Path, yolo: bool) -> Result<()> {
    let (redraw_sender, redraw_receiver) = crossbeam_channel::bounded(1);
    let event_receiver = spawn_event_reader();
    let heartbeat = crossbeam_channel::tick(Duration::from_secs(1));
    let git_context_receiver = spawn_git_context_reader(cwd.to_path_buf());
    let mut app = App::new(cwd, redraw_sender, yolo);
    let mut bell_pending = false;
    loop {
        let size = terminal.size()?;
        let (_, _, pane, _) = app_layout(Rect::new(0, 0, size.width, size.height));
        app.terminal_pane = pane;
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

        crossbeam_channel::select! {
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
            }
            recv(git_context_receiver) -> context => {
                app.git_context = context.context("git context reader stopped")?;
            }
        }
    }
    Ok(())
}

enum Launch {
    Run { cwd: PathBuf, yolo: bool },
    Help,
    Version,
    Hook(SemanticState),
}

fn parse_args_from(args: impl IntoIterator<Item = OsString>) -> Result<Launch> {
    let mut args = args.into_iter();
    let Some(argument) = args.next() else {
        return Ok(Launch::Run {
            cwd: std::env::current_dir()?,
            yolo: false,
        });
    };
    if argument == "hook" {
        let state = args
            .next()
            .and_then(|value| parse_semantic_state(&value.to_string_lossy()))
            .context("hook requires one of: working, waiting, idle, finished")?;
        if args.next().is_some() {
            anyhow::bail!("hook accepts exactly one state");
        }
        return Ok(Launch::Hook(state));
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
    for argument in std::iter::once(argument).chain(args) {
        if argument == "--yolo" {
            if yolo {
                anyhow::bail!("--yolo may only be specified once");
            }
            yolo = true;
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
    Ok(Launch::Run { cwd: path, yolo })
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
        Launch::Run { cwd, yolo } => {
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
            ratatui::run(|terminal| run(terminal, &cwd, yolo))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_screen_text(session: &dyn SessionIo, query: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if session.contains_text(query).unwrap_or(false) {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    struct TestSession {
        definition: AgentDefinition,
        title: String,
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

        fn contains_text(&self, query: &str) -> Result<bool> {
            parser_contains_text(&self.parser, query)
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
            parser: Arc::new(Mutex::new(vt100::Parser::new(32, 100, 2_000))),
            write_error,
            writes,
            alive: true,
            semantic_state: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn command_line_accepts_only_trusted_hook_states() {
        assert!(matches!(
            parse_args_from([OsString::from("hook"), OsString::from("waiting")]).unwrap(),
            Launch::Hook(SemanticState::Waiting)
        ));
        assert!(parse_args_from([OsString::from("hook"), OsString::from("done")]).is_err());
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
        assert_eq!(app.sessions.active_index(), Some(1));
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
                .contains_text("wheel-preserves-this")
                .unwrap()
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
    fn command_line_accepts_yolo_before_or_after_workspace() {
        for args in [
            vec![OsString::from("--yolo"), OsString::from(".")],
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
                session_arguments(*definition, false, None),
                Vec::<OsString>::new()
            );
            assert_eq!(
                session_arguments(*definition, true, None),
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
            session_arguments(definition, true, Some(Path::new("hook settings.json"))),
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
