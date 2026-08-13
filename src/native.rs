#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge::FirstPartyCli;
    use std::path::PathBuf;

    #[test]
    fn ask_yolo_is_false_unless_the_child_request_contains_the_flag() {
        let command = parse_args([
            "ask",
            "codex",
            "--workspace",
            "/tmp/project",
            "--prompt",
            "review this",
        ])
        .unwrap();

        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                provider: FirstPartyCli::Codex,
                workspace,
                yolo: false,
                ..
            }) if workspace == PathBuf::from("/tmp/project")
        ));
    }

    #[test]
    fn ask_codex_accepts_a_request_scoped_model_and_claude_rejects_it() {
        let command = parse_args([
            "ask",
            "codex",
            "--prompt",
            "review this",
            "--model",
            "gpt-daybreak-blue-latest",
        ])
        .unwrap();

        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                model: Some(model),
                ..
            }) if model == "gpt-daybreak-blue-latest"
        ));
        assert!(
            parse_args([
                "ask",
                "claude",
                "--prompt",
                "review this",
                "--model",
                "gpt-daybreak-blue-latest",
            ])
            .is_err()
        );
    }

    #[test]
    fn close_is_rejected_without_the_explicit_flag() {
        assert!(parse_args(["close-session", "session-safe123"]).is_err());
        assert!(parse_args(["close-session", "session-safe123", "--explicit"]).is_ok());
    }

    #[test]
    fn internal_session_ids_cannot_escape_the_state_root() {
        assert!(valid_session_id("session-abCD_123-xyz"));
        assert!(!valid_session_id("../outside"));
        assert!(!valid_session_id("session/child"));
    }

    #[test]
    fn hook_payload_extracts_first_party_assistant_results() {
        let codex = serde_json::json!({ "last-assistant-message": "codex result" });
        let claude = serde_json::json!({ "last_assistant_message": "claude result" });

        assert_eq!(extract_assistant_message(&codex), Some("codex result"));
        assert_eq!(extract_assistant_message(&claude), Some("claude result"));
    }

    #[test]
    fn iterm_script_keeps_dynamic_values_in_argv() {
        assert!(!OPEN_ITERM_TAB_SCRIPT.contains("review this"));
        assert!(OPEN_ITERM_TAB_SCRIPT.contains("item 1 of argv"));
        assert!(OPEN_ITERM_TAB_SCRIPT.contains("write text bridgeCommand"));
    }

    #[test]
    fn iterm_follow_up_sends_an_explicit_carriage_return() {
        assert!(SEND_ITERM_TEXT_SCRIPT.contains("ASCII character 13"));
        assert!(SEND_ITERM_TEXT_SCRIPT.contains("newline NO"));
        assert!(!SEND_ITERM_TEXT_SCRIPT.contains("write text \"\""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iterm_applescripts_compile_without_opening_a_tab() {
        for script in [
            OPEN_ITERM_TAB_SCRIPT,
            SEND_ITERM_TEXT_SCRIPT,
            CLOSE_ITERM_SESSION_SCRIPT,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new("/usr/bin/osacompile")
                .arg("-o")
                .arg(directory.path().join("bridge.scpt"))
                .arg("-e")
                .arg(script)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "AppleScript did not compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn status_updates_replace_atomically() {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        update_status(directory.path(), "ready", None, None).unwrap();

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
    }

    #[test]
    fn claude_settings_capture_only_stop_for_the_native_session() {
        let settings = claude_hook_settings(Path::new("/opt/Agent Bridge/bin/agent-bridge"));
        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();

        assert_eq!(
            command,
            "'/opt/Agent Bridge/bin/agent-bridge' native-hook claude"
        );
        assert!(settings["hooks"]["PermissionRequest"].is_null());
    }

    #[test]
    fn shell_quoting_handles_apostrophes_without_executing_them() {
        assert_eq!(
            shell_quote(std::ffi::OsStr::new("/tmp/user's bridge")),
            "'/tmp/user'\"'\"'s bridge'"
        );
    }

    #[test]
    fn bridge_shell_command_quotes_the_workspace_and_executable() {
        assert_eq!(
            bridge_shell_command(
                Path::new("/tmp/project; touch nope"),
                Path::new("/tmp/state root"),
                Path::new("/tmp/Agent Bridge/bin"),
                "session-safe123",
            ),
            "cd '/tmp/project; touch nope' && AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'"
        );
    }

    #[test]
    fn follow_up_prompts_only_enter_a_completed_live_cli_turn() {
        assert!(session_accepts_prompt("ready"));
        for state in [
            "launching",
            "running",
            "working",
            "exited",
            "failed",
            "closed",
        ] {
            assert!(!session_accepts_prompt(state), "accepted {state}");
        }
    }

    #[test]
    fn follow_up_prompt_is_one_bracketed_paste_payload() {
        assert_eq!(
            terminal_paste_bytes("line one\nline two"),
            b"\x1b[200~line one\nline two\x1b[201~"
        );
    }

    #[test]
    fn native_turn_claim_is_exclusive_until_the_hook_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        assert!(acquire_turn_claim(directory.path()).is_err());

        claim.retain();
        assert!(acquire_turn_claim(directory.path()).is_err());
        release_turn_claim(directory.path()).unwrap();
        assert!(acquire_turn_claim(directory.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn provider_resolution_rejects_relative_path_entries() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::Builder::new()
            .prefix("relative-provider-")
            .tempdir_in(&cwd)
            .unwrap();
        let relative = root.path().strip_prefix(&cwd).unwrap();
        let executable = root.path().join("codex");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(resolve_provider_from_path(FirstPartyCli::Codex, relative.as_os_str()).is_err());
    }

    #[test]
    fn every_native_prompt_carries_a_sanitized_source_provenance() {
        assert_eq!(
            native_delegation_prompt("Codex parent\nforged", "review this"),
            "[Agent Bridge native delegation]\nSource: Codex parent forged\n\nreview this"
        );
    }

    #[test]
    fn terminal_titles_drop_control_characters_and_are_bounded() {
        assert_eq!(sanitize_title(" Review\nTab\t ").unwrap(), "Review Tab");
        assert_eq!(
            sanitize_title(&"x".repeat(200)).unwrap().chars().count(),
            80
        );
        assert!(sanitize_title("\n\t").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_session_executes_the_provider_with_policy_and_provenance() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let provider = root.path().join("fake-codex");
        fs::write(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'codex-cli 0.147.0\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\npwd > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/cwd.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-safe123".to_owned(),
                provider: "codex".to_owned(),
                provider_path: provider,
                provider_version: "codex-cli 0.147.0".to_owned(),
                workspace: workspace.clone(),
                title: "Codex test".to_owned(),
                model: Some("gpt-daybreak-blue-latest".to_owned()),
                yolo: true,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
        write_private(
            &directory.join("initial-prompt.txt"),
            native_delegation_prompt("parent", "review this").as_bytes(),
        )
        .unwrap();

        run_session_inner(&directory).unwrap();

        let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
        assert!(arguments.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(arguments.contains("--model\ngpt-daybreak-blue-latest"));
        assert!(arguments.contains("notify=["));
        assert!(arguments.contains("native-hook"));
        assert!(arguments.contains("[Agent Bridge native delegation]"));
        assert!(arguments.contains("Source: parent"));
        assert!(!directory.join("initial-prompt.txt").exists());
        assert_eq!(
            fs::read_to_string(directory.join("cwd.txt"))
                .unwrap()
                .trim(),
            workspace.canonicalize().unwrap().to_string_lossy()
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "exited");
    }
}
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agent_bridge::{
    FirstPartyCli, cli_version_is_supported, confirm_explicit_close, provider_launch_args,
    terminal_safe_text, validate_terminal_input,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const STATE_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_STATE_DIR";
const SESSION_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_SESSION_DIR";
const DEFAULT_TIMEOUT_SECS: u64 = 900;
const SESSION_SCHEMA: u32 = 1;
const TURN_CLAIM_FILE: &str = "turn.claim";

pub(crate) const OPEN_ITERM_TAB_SCRIPT: &str = r#"
on run argv
    set bridgeCommand to item 1 of argv
    set tabTitle to item 2 of argv
    tell application "iTerm2"
        activate
        if (count of windows) is 0 then
            set targetWindow to (create window with default profile)
            set targetSession to current session of targetWindow
        else
            set targetWindow to current window
            tell targetWindow
                set targetTab to (create tab with default profile)
                set targetSession to current session of targetTab
            end tell
        end if
        tell targetSession
            set name to tabTitle
            write text bridgeCommand
            return unique ID
        end tell
    end tell
end run
"#;

const SEND_ITERM_TEXT_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    set promptPath to item 2 of argv
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        tell targetSession
                            write contents of file promptPath
                            write text (ASCII character 13) newline NO
                        end tell
                        return "sent"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "Agent Bridge iTerm session not found"
end run
"#;

const CLOSE_ITERM_SESSION_SCRIPT: &str = r#"
on run argv
    set wantedId to item 1 of argv
    tell application "iTerm2"
        repeat with targetWindow in windows
            repeat with targetTab in tabs of targetWindow
                repeat with targetSession in sessions of targetTab
                    if unique ID of targetSession is wantedId then
                        close targetSession
                        return "closed"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "Agent Bridge iTerm session not found"
end run
"#;

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Ask(AskRequest),
    Tell(TellRequest),
    Sessions {
        json: bool,
    },
    Close(CloseRequest),
    RunSession {
        id: String,
    },
    Hook {
        provider: FirstPartyCli,
        payload: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct AskRequest {
    pub(crate) provider: FirstPartyCli,
    pub(crate) workspace: PathBuf,
    pub(crate) prompt: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) yolo: bool,
    pub(crate) timeout: Duration,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

#[derive(Debug)]
pub(crate) struct TellRequest {
    id: String,
    prompt: String,
    timeout: Duration,
    detach: bool,
    json: bool,
}

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionManifest {
    schema: u32,
    id: String,
    provider: String,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    #[serde(default)]
    model: Option<String>,
    yolo: bool,
    created_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
struct TerminalRecord {
    iterm_session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionStatus {
    state: String,
    updated_unix_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionEvent {
    provider: String,
    message: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    created_unix_ms: u128,
}

struct CreatedSession {
    id: String,
    directory: PathBuf,
    manifest: SessionManifest,
}

struct SessionSpec {
    provider: FirstPartyCli,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    model: Option<String>,
    yolo: bool,
    prompt: String,
}

pub(crate) fn is_command(value: &str) -> bool {
    matches!(
        value,
        "ask" | "tell" | "sessions" | "close-session" | "native-session" | "native-hook"
    )
}

pub(crate) fn parse_args<I, S>(args: I) -> Result<NativeCommand>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    let (command, rest) = args.split_first().context("native command is required")?;
    match command.as_str() {
        "ask" => parse_ask(rest),
        "tell" => parse_tell(rest),
        "sessions" => parse_sessions(rest),
        "close-session" => parse_close(rest),
        "native-session" => {
            let id = one_positional(rest, "native-session requires one session id")?;
            require_valid_session_id(id)?;
            Ok(NativeCommand::RunSession { id: id.to_owned() })
        }
        "native-hook" => parse_hook(rest),
        _ => bail!("unknown native command: {command}"),
    }
}

fn parse_ask(args: &[String]) -> Result<NativeCommand> {
    let (provider, options) = args.split_first().context("ask requires codex or claude")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    let mut workspace = None;
    let mut prompt = None;
    let mut title = None;
    let mut model = None;
    let mut yolo = false;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--title" => set_once(
                &mut title,
                option_value(options, &mut index, "--title")?.to_owned(),
                "--title",
            )?,
            "--model" => set_once(
                &mut model,
                option_value(options, &mut index, "--model")?.to_owned(),
                "--model",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--yolo" => set_flag_once(&mut yolo, "--yolo")?,
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    let prompt = prompt.context("ask requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    if model.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--model cannot be empty");
    }
    if provider != FirstPartyCli::Codex && model.is_some() {
        bail!("--model is currently supported only for codex");
    }
    Ok(NativeCommand::Ask(AskRequest {
        provider,
        workspace,
        prompt,
        title,
        model,
        yolo,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_tell(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args.split_first().context("tell requires one session id")?;
    require_valid_session_id(id)?;
    let mut prompt = None;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown tell option: {option}"),
        }
        index += 1;
    }
    let prompt = prompt.context("tell requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
    Ok(NativeCommand::Tell(TellRequest {
        id: id.to_owned(),
        prompt,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_sessions(args: &[String]) -> Result<NativeCommand> {
    match args {
        [] => Ok(NativeCommand::Sessions { json: false }),
        [option] if option == "--json" => Ok(NativeCommand::Sessions { json: true }),
        _ => bail!("sessions accepts only --json"),
    }
}

fn parse_close(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("close-session requires one session id")?;
    require_valid_session_id(id)?;
    let mut explicit = false;
    let mut json = false;
    for option in options {
        match option.as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            _ => bail!("unknown close-session option: {option}"),
        }
    }
    confirm_explicit_close(explicit)?;
    Ok(NativeCommand::Close(CloseRequest {
        id: id.to_owned(),
        explicit,
        json,
    }))
}

fn parse_hook(args: &[String]) -> Result<NativeCommand> {
    let (provider, payload) = args
        .split_first()
        .context("native-hook requires codex or claude")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    if payload.len() > 1 {
        bail!("native-hook accepts at most one payload argument");
    }
    Ok(NativeCommand::Hook {
        provider,
        payload: payload.first().cloned(),
    })
}

fn one_positional<'a>(args: &'a [String], message: &str) -> Result<&'a str> {
    match args {
        [value] => Ok(value),
        _ => bail!("{message}"),
    }
}

fn option_value<'a>(args: &'a [String], index: &mut usize, option: &str) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{option} may only be specified once");
    }
    Ok(())
}

fn set_flag_once(flag: &mut bool, option: &str) -> Result<()> {
    if *flag {
        bail!("{option} may only be specified once");
    }
    *flag = true;
    Ok(())
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("invalid timeout: {value}"))?;
    if seconds == 0 {
        bail!("timeout must be greater than zero");
    }
    Ok(Duration::from_secs(seconds))
}

pub(crate) fn valid_session_id(value: &str) -> bool {
    value.starts_with("session-")
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn require_valid_session_id(value: &str) -> Result<()> {
    if !valid_session_id(value) {
        bail!("invalid Agent Bridge session id: {value:?}");
    }
    Ok(())
}

pub(crate) fn run(command: NativeCommand) -> Result<()> {
    match command {
        NativeCommand::Ask(request) => run_ask(request),
        NativeCommand::Tell(request) => run_tell(request),
        NativeCommand::Sessions { json } => run_sessions(json),
        NativeCommand::Close(request) => run_close(request),
        NativeCommand::RunSession { id } => run_session(&id),
        NativeCommand::Hook { provider, payload } => run_hook(provider, payload.as_deref()),
    }
}

fn run_ask(request: AskRequest) -> Result<()> {
    ensure_macos_iterm()?;
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
    let provider_version = check_provider_version(request.provider, &provider_path)?;
    let requested_title = request.title.unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        format!("{} · {workspace_name}", request.provider.as_str())
    });
    let title = sanitize_title(&requested_title)?;
    let created = create_session(SessionSpec {
        provider: request.provider,
        provider_path,
        provider_version,
        workspace,
        title,
        model: request.model,
        yolo: request.yolo,
        prompt: native_delegation_prompt(&delegation_source(), &request.prompt),
    })?;
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let bridge_command = bridge_shell_command(
        &created.manifest.workspace,
        created
            .directory
            .parent()
            .context("session directory has no state root")?,
        &executable,
        &created.id,
    );
    let iterm_session_id = match open_iterm_tab(&bridge_command, &created.manifest.title) {
        Ok(id) => id,
        Err(error) => {
            let _ = fs::remove_file(created.directory.join("initial-prompt.txt"));
            let _ = update_status(
                &created.directory,
                "failed",
                None,
                Some(format!("{error:#}")),
            );
            return Err(error)
                .with_context(|| format!("failed to open iTerm tab for session {}", created.id));
        }
    };
    write_json_atomic(
        &created.directory.join("terminal.json"),
        &TerminalRecord {
            iterm_session_id: iterm_session_id.clone(),
        },
    )?;

    if request.detach {
        return emit_session_result(
            request.json,
            &created.id,
            &iterm_session_id,
            request.provider,
            None,
        );
    }

    let event = wait_for_event(&created.directory, 0, request.timeout).with_context(|| {
        format!(
            "session {} remains open in iTerm; use `agent-bridge sessions` to inspect it",
            created.id
        )
    })?;
    emit_session_result(
        request.json,
        &created.id,
        &iterm_session_id,
        request.provider,
        Some(&event),
    )
}

fn run_tell(request: TellRequest) -> Result<()> {
    ensure_macos_iterm()?;
    let directory = session_directory(&request.id)?;
    let claim = acquire_turn_claim(&directory)?;
    let manifest = read_manifest(&directory)?;
    let terminal: TerminalRecord = read_json(&directory.join("terminal.json"))?;
    let baseline = event_paths(&directory)?.len();
    let previous_state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&previous_state) {
        bail!(
            "session {} is {previous_state}; tell requires the ready state",
            request.id
        );
    }
    update_status(&directory, "working", None, None)?;

    let mut prompt_file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(&directory)?;
    set_private_file_permissions(prompt_file.as_file())?;
    let prompt = native_delegation_prompt(&delegation_source(), &request.prompt);
    prompt_file.write_all(&terminal_paste_bytes(&prompt))?;
    prompt_file.flush()?;
    if let Err(error) = send_iterm_file(&terminal.iterm_session_id, prompt_file.path()) {
        let _ = update_status(
            &directory,
            &previous_state,
            None,
            Some(format!("{error:#}")),
        );
        return Err(error)
            .with_context(|| format!("failed to type into visible iTerm session {}", request.id));
    }
    claim.retain();

    if request.detach {
        return emit_session_result(
            request.json,
            &request.id,
            &terminal.iterm_session_id,
            FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
            None,
        );
    }
    let event = wait_for_event(&directory, baseline, request.timeout).with_context(|| {
        format!(
            "session {} remains open in iTerm; the requested turn did not report completion",
            request.id
        )
    })?;
    emit_session_result(
        request.json,
        &request.id,
        &terminal.iterm_session_id,
        FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
        Some(&event),
    )
}

fn run_sessions(json: bool) -> Result<()> {
    let root = state_root()?;
    let mut sessions = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_session_id(&id) {
                continue;
            }
            let directory = entry.path();
            let Ok(manifest) = read_manifest(&directory) else {
                continue;
            };
            let status = read_json::<SessionStatus>(&directory.join("status.json")).ok();
            let terminal = read_json::<TerminalRecord>(&directory.join("terminal.json")).ok();
            sessions.push(serde_json::json!({
                "id": manifest.id,
                "provider": manifest.provider,
                "workspace": manifest.workspace,
                "title": manifest.title,
                "yolo": manifest.yolo,
                "state": status.as_ref().map(|value| value.state.as_str()).unwrap_or("unknown"),
                "iterm_session_id": terminal.map(|value| value.iterm_session_id),
                "results": event_paths(&directory).map(|paths| paths.len()).unwrap_or(0),
            }));
        }
    }
    sessions.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else if sessions.is_empty() {
        println!("no native Agent Bridge sessions");
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{}\t{}\tyolo={}\t{} result(s)",
                session["id"].as_str().unwrap_or("?"),
                terminal_safe_text(session["state"].as_str().unwrap_or("unknown"), false),
                terminal_safe_text(session["provider"].as_str().unwrap_or("?"), false),
                terminal_safe_text(session["workspace"].as_str().unwrap_or("?"), false),
                session["yolo"].as_bool().unwrap_or(false),
                session["results"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn run_close(request: CloseRequest) -> Result<()> {
    ensure_macos_iterm()?;
    confirm_explicit_close(request.explicit)?;
    let directory = session_directory(&request.id)?;
    let terminal: TerminalRecord = read_json(&directory.join("terminal.json"))?;
    close_iterm_session(&terminal.iterm_session_id)
        .with_context(|| format!("failed to close visible iTerm session {}", request.id))?;
    update_status(&directory, "closed", None, None)?;
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": request.id,
                "closed": true,
            }))?
        );
    } else {
        println!("closed {}", request.id);
    }
    Ok(())
}

fn emit_session_result(
    json: bool,
    id: &str,
    iterm_session_id: &str,
    provider: FirstPartyCli,
    event: Option<&SessionEvent>,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": id,
                "provider": provider.as_str(),
                "iterm_session_id": iterm_session_id,
                "result": event.map(|value| value.message.as_str()),
                "provider_session_id": event.and_then(|value| value.provider_session_id.as_deref()),
                "turn_id": event.and_then(|value| value.turn_id.as_deref()),
            }))?
        );
    } else {
        println!("session: {id}");
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
    }
    Ok(())
}

fn run_session(id: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("native-session must run in a visible interactive terminal");
    }
    let directory = session_directory(id)?;
    let result = run_session_inner(&directory);
    let _ = release_turn_claim(&directory);
    if let Err(error) = &result {
        let _ = update_status(&directory, "failed", None, Some(format!("{error:#}")));
    }
    result
}

fn run_session_inner(directory: &Path) -> Result<()> {
    let manifest = read_manifest(directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = directory.join("initial-prompt.txt");
    let prompt = fs::read_to_string(&prompt_path).context("failed to read initial prompt")?;
    fs::remove_file(&prompt_path).context("failed to remove consumed initial prompt")?;

    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut arguments = provider_launch_args(provider, manifest.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    match provider {
        FirstPartyCli::Codex => {
            if let Some(model) = &manifest.model {
                arguments.push(OsString::from("--model"));
                arguments.push(OsString::from(model));
            }
            let notify = serde_json::to_string(&[
                executable.to_string_lossy().as_ref(),
                "native-hook",
                "codex",
            ])?;
            arguments.push(OsString::from("-c"));
            arguments.push(OsString::from(format!("notify={notify}")));
            arguments.push(OsString::from("-C"));
            arguments.push(manifest.workspace.as_os_str().to_owned());
        }
        FirstPartyCli::Claude => {
            let settings_path = directory.join("claude-settings.json");
            write_json_atomic(&settings_path, &claude_hook_settings(&executable))?;
            arguments.push(OsString::from("--settings"));
            arguments.push(settings_path.into_os_string());
            arguments.push(OsString::from("--name"));
            arguments.push(OsString::from(&manifest.title));
        }
    }
    arguments.push(OsString::from(prompt));

    update_status(directory, "running", None, None)?;
    let status = Command::new(&manifest.provider_path)
        .args(arguments)
        .current_dir(&manifest.workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", &manifest.id)
        .status()
        .with_context(|| {
            format!(
                "failed to start {} at {}",
                provider.as_str(),
                manifest.provider_path.display()
            )
        })?;
    update_status(directory, "exited", status.code(), None)?;
    if !status.success() {
        bail!("{} exited with {status}", provider.as_str());
    }
    Ok(())
}

fn run_hook(provider: FirstPartyCli, argument_payload: Option<&str>) -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os(SESSION_DIR_ENV).context("native hook session directory is not set")?,
    );
    validate_hook_directory(&directory)?;
    let manifest = read_manifest(&directory)?;
    if manifest.provider != provider.as_str() {
        bail!(
            "hook provider {} does not match session provider {}",
            provider.as_str(),
            manifest.provider
        );
    }

    let payload_text = if let Some(payload) = argument_payload {
        payload.to_owned()
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    let payload: serde_json::Value =
        serde_json::from_str(&payload_text).context("native hook received invalid JSON")?;
    let message = extract_assistant_message(&payload)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("native hook payload did not contain an assistant result")?;
    let provider_session_id = json_string(&payload, &["session_id", "thread-id", "thread_id"]);
    let turn_id = json_string(&payload, &["turn_id", "turn-id"]);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: message.to_owned(),
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    write_event(&directory, &event)?;
    update_status(&directory, "ready", None, None)?;
    release_turn_claim(&directory)
}

pub(crate) fn extract_assistant_message(payload: &serde_json::Value) -> Option<&str> {
    ["last-assistant-message", "last_assistant_message"]
        .into_iter()
        .find_map(|key| payload.get(key).and_then(serde_json::Value::as_str))
}

fn json_string(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn claude_hook_settings(executable: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("{} native-hook claude", shell_quote(executable.as_os_str())),
                    "timeout": 10
                }]
            }]
        }
    })
}

fn create_session(spec: SessionSpec) -> Result<CreatedSession> {
    let root = state_root()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create state directory {}", root.display()))?;
    set_private_directory_permissions(&root)?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&root)?;
    let directory = temp.keep();
    set_private_directory_permissions(&directory)?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("session directory name is not UTF-8")?
        .to_owned();
    require_valid_session_id(&id)?;
    let events = directory.join("events");
    fs::create_dir(&events)?;
    set_private_directory_permissions(&events)?;
    let manifest = SessionManifest {
        schema: SESSION_SCHEMA,
        id: id.clone(),
        provider: spec.provider.as_str().to_owned(),
        provider_path: spec.provider_path,
        provider_version: spec.provider_version,
        workspace: spec.workspace,
        title: spec.title,
        model: spec.model,
        yolo: spec.yolo,
        created_unix_ms: unix_ms(),
    };
    write_json_atomic(&directory.join("manifest.json"), &manifest)?;
    write_private(
        &directory.join("initial-prompt.txt"),
        spec.prompt.as_bytes(),
    )?;
    update_status(&directory, "launching", None, None)?;
    Ok(CreatedSession {
        id,
        directory,
        manifest,
    })
}

fn state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(root));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    Ok(home.join(".agent-bridge").join("native-sessions"))
}

fn session_directory(id: &str) -> Result<PathBuf> {
    require_valid_session_id(id)?;
    let directory = state_root()?.join(id);
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("no such Agent Bridge session: {id}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("refusing non-directory Agent Bridge session: {id}");
    }
    Ok(directory)
}

fn validate_hook_directory(directory: &Path) -> Result<()> {
    let root = state_root()?
        .canonicalize()
        .context("native state root is missing")?;
    let canonical = directory
        .canonicalize()
        .context("native hook session directory is missing")?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "refusing native hook directory outside state root: {}",
            directory.display()
        );
    }
    let id = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .context("native hook session id is not UTF-8")?;
    require_valid_session_id(id)
}

fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    let manifest: SessionManifest = read_json(&directory.join("manifest.json"))?;
    if manifest.schema != SESSION_SCHEMA {
        bail!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        );
    }
    let expected_id = directory.file_name().and_then(|name| name.to_str());
    if expected_id != Some(manifest.id.as_str()) {
        bail!("session manifest id does not match its directory");
    }
    Ok(manifest)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.flush()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn update_status(
    directory: &Path,
    state: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    write_json_atomic(
        &directory.join("status.json"),
        &SessionStatus {
            state: state.to_owned(),
            updated_unix_ms: unix_ms(),
            exit_code,
            error,
        },
    )
}

struct TurnClaim {
    path: PathBuf,
    retained: bool,
}

impl TurnClaim {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if !self.retained {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn acquire_turn_claim(directory: &Path) -> Result<TurnClaim> {
    let path = directory.join(TURN_CLAIM_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| "another tell request already owns this session turn")?;
    set_private_file_permissions(&file)?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(TurnClaim {
        path,
        retained: false,
    })
}

fn release_turn_claim(directory: &Path) -> Result<()> {
    match fs::remove_file(directory.join(TURN_CLAIM_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to release native turn claim"),
    }
}

fn write_event(directory: &Path, event: &SessionEvent) -> Result<()> {
    let events = directory.join("events");
    let name = format!(
        "event-{}-{}.json",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    write_json_atomic(&events.join(name), event)?;
    write_json_atomic(&directory.join("latest.json"), event)
}

fn event_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let events = directory.join("events");
    let mut paths = Vec::new();
    if !events.is_dir() {
        return Ok(paths);
    }
    for entry in fs::read_dir(events)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("event-") && name.ends_with(".json"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn wait_for_event(directory: &Path, baseline: usize, timeout: Duration) -> Result<SessionEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let paths = event_paths(directory)?;
        if paths.len() > baseline {
            return read_json(paths.last().context("event path disappeared")?);
        }
        if let Ok(status) = read_json::<SessionStatus>(&directory.join("status.json"))
            && matches!(status.state.as_str(), "failed" | "exited" | "closed")
        {
            let reason = status
                .error
                .unwrap_or_else(|| format!("session entered state {}", status.state));
            bail!("{reason}");
        }
        if Instant::now() >= deadline {
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn resolve_provider(provider: FirstPartyCli) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_provider_from_path(provider, &path)
}

fn resolve_provider_from_path(provider: FirstPartyCli, path: &std::ffi::OsStr) -> Result<PathBuf> {
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(provider.command());
        if candidate.is_file() && is_executable(&candidate) {
            let canonical = candidate.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize provider path {}",
                    candidate.display()
                )
            })?;
            if canonical.is_file() && is_executable(&canonical) {
                return Ok(canonical);
            }
        }
    }
    bail!(
        "{} was not found on an absolute PATH entry",
        provider.command()
    )
}

fn check_provider_version(provider: FirstPartyCli, executable: &Path) -> Result<String> {
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to query {} --version", executable.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            executable.display(),
            output.status
        );
    }
    let mut version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        version = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    if !cli_version_is_supported(provider, &version)? {
        bail!(
            "{} is too old: found {version:?}, require >= {}",
            provider.as_str(),
            provider.minimum_version()
        );
    }
    Ok(version)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn ensure_macos_iterm() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("native visible sessions currently require macOS and iTerm2");
    }
    Ok(())
}

fn run_osascript(script: &str, arguments: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(arguments)
        .output()
        .context("failed to execute /usr/bin/osascript")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "iTerm2 automation failed: {}",
            if error.is_empty() {
                output.status.to_string()
            } else {
                error
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn open_iterm_tab(command: &str, title: &str) -> Result<String> {
    let id = run_osascript(OPEN_ITERM_TAB_SCRIPT, &[command, title])?;
    if id.is_empty() {
        bail!("iTerm2 did not return a session id");
    }
    Ok(id)
}

fn send_iterm_file(iterm_session_id: &str, prompt_path: &Path) -> Result<()> {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    let response = run_osascript(SEND_ITERM_TEXT_SCRIPT, &[iterm_session_id, prompt_path])?;
    if response != "sent" {
        bail!("unexpected iTerm2 send response: {response:?}");
    }
    Ok(())
}

fn close_iterm_session(iterm_session_id: &str) -> Result<()> {
    let response = run_osascript(CLOSE_ITERM_SESSION_SCRIPT, &[iterm_session_id])?;
    if response != "closed" {
        bail!("unexpected iTerm2 close response: {response:?}");
    }
    Ok(())
}

fn shell_quote(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn bridge_shell_command(
    workspace: &Path,
    state_root: &Path,
    executable: &Path,
    id: &str,
) -> String {
    format!(
        "cd {} && {}={} {} native-session {}",
        shell_quote(workspace.as_os_str()),
        STATE_DIR_ENV,
        shell_quote(state_root.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(OsString::from(id).as_os_str())
    )
}

fn session_accepts_prompt(state: &str) -> bool {
    state == "ready"
}

fn delegation_source() -> String {
    std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID")
        .or_else(|_| std::env::var("AGENT_BRIDGE_TAB"))
        .unwrap_or_else(|_| "external".to_owned())
}

fn native_delegation_prompt(source: &str, prompt: &str) -> String {
    let source = source
        .chars()
        .take(128)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let source = if source.trim().is_empty() {
        "external"
    } else {
        source.trim()
    };
    format!(
        "[Agent Bridge native delegation]\nSource: {}\n\n{}",
        source,
        prompt.trim()
    )
}

fn terminal_paste_bytes(prompt: &str) -> Vec<u8> {
    format!("\x1b[200~{prompt}\x1b[201~").into_bytes()
}

fn sanitize_title(value: &str) -> Result<String> {
    let title = value
        .chars()
        .take(80)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        bail!("--title cannot be empty");
    }
    Ok(title)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}
