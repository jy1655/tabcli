//! Environment policy and deadline-bounded command execution for Bridge children.
//! Provider adapters own their removal lists; helpers must not pass any caller's
//! session identity into another application.
use crate::native::session::primitives::{SESSION_DIR_ENV, STATE_DIR_ENV};
use std::{
    ffi::OsStr,
    ffi::OsString,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use super::failure_kind::TimedOut;
use super::provider;
use super::provider_process;
const BRIDGE_SESSION_VARIABLES: [&str; 4] = [
    STATE_DIR_ENV,
    SESSION_DIR_ENV,
    "AGENT_BRIDGE_NATIVE_SESSION_ID",
    "AGENT_BRIDGE_EXECUTABLE",
];

pub(super) fn non_provider_removals() -> impl Iterator<Item = &'static str> {
    // Query each adapter, rather than maintaining a second copy of its markers.
    agent_bridge::supported_clis()
        .iter()
        .flat_map(|provider| {
            provider::probe_environment_removals(*provider)
                .iter()
                .copied()
        })
        .chain(BRIDGE_SESSION_VARIABLES)
}

pub(super) fn helper_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for variable in non_provider_removals() {
        command.env_remove(variable);
    }
    command
}

pub(super) fn provider_command(
    executable: &Path,
    directory: &Path,
    arguments: Vec<OsString>,
    removals: &[&str],
) -> Result<Command> {
    let mut command = provider_process::command(executable, directory, arguments)?;
    remove(&mut command, removals);
    Ok(command)
}

pub(super) fn provider_version_command(executable: &Path, removals: &[&str]) -> Result<Command> {
    let mut command = provider_process::version_command(executable)?;
    remove(&mut command, removals);
    Ok(command)
}

fn remove(command: &mut Command, removals: &[&str]) {
    for variable in removals {
        command.env_remove(variable);
    }
}

#[derive(Debug)]
pub(in crate::native) struct CommandOutputFailure {
    error: anyhow::Error,
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", test)),
        allow(dead_code)
    )]
    process_started: bool,
    // The child ran until the deadline and was ended there. Only the macOS AppleScript
    // runner asks; the other platforms carry the fact without reading it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    timed_out: bool,
}

impl CommandOutputFailure {
    pub(in crate::native) fn not_started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: false,
            timed_out: false,
        }
    }

    pub(in crate::native) fn started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: true,
            timed_out: false,
        }
    }

    fn ended_at_deadline(mut self) -> Self {
        self.timed_out = true;
        self
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(in crate::native) fn timed_out(&self) -> bool {
        self.timed_out
    }

    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", test)),
        allow(dead_code)
    )]
    pub(in crate::native) fn process_started(&self) -> bool {
        self.process_started
    }

    pub(in crate::native) fn into_error(self) -> anyhow::Error {
        self.error
    }
}

// Both checks before the spawn; the process flags stay false, the error carries the deadline.
fn not_started_at_deadline(label: &str) -> CommandOutputFailure {
    CommandOutputFailure::not_started(anyhow::Error::new(TimedOut::new(format!(
        "{label} timed out before it started"
    ))))
}

pub(in crate::native) fn command_output_until(
    command: &mut Command,
    deadline: Instant,
    label: &str,
) -> Result<Output> {
    command_output_until_classified(command, deadline, label)
        .map_err(CommandOutputFailure::into_error)
}

pub(in crate::native) fn command_output_until_classified(
    command: &mut Command,
    deadline: Instant,
    label: &str,
) -> std::result::Result<Output, CommandOutputFailure> {
    command_output_with_stdin_until_classified(command, Stdio::null(), deadline, label)
}

pub(in crate::native) fn command_output_with_stdin_until_classified(
    command: &mut Command,
    stdin: Stdio,
    deadline: Instant,
    label: &str,
) -> std::result::Result<Output, CommandOutputFailure> {
    if Instant::now() >= deadline {
        return Err(not_started_at_deadline(label));
    }
    let execution = Execution::Command { label };
    let storage = OutputStorage::new(&execution).map_err(CommandOutputFailure::not_started)?;
    let child_streams = storage
        .child_streams(&execution)
        .map_err(CommandOutputFailure::not_started)?;
    if Instant::now() >= deadline {
        return Err(not_started_at_deadline(label));
    }
    bounded_output(command, stdin, deadline, execution, storage, child_streams)
}

pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT: u64 = 64 * 1024;

// Probe only explicitly requested local CLI information. Scratch files (including Windows
// batch forwarders) live outside the session; contain descendants and cap time/output.
fn probe_command(
    executable: &Path,
    arguments: &[&str],
    scratch: &Path,
    environment_removals: &[&str],
) -> Result<std::process::Command> {
    let mut command = if arguments == ["--version"] {
        let mut command = provider_version_command(executable, environment_removals)?;
        command.arg("--version");
        command
    } else {
        provider_command(
            executable,
            scratch,
            arguments.iter().map(OsString::from).collect(),
            environment_removals,
        )?
    };
    command.env_remove(SESSION_DIR_ENV);
    Ok(command)
}

pub(super) fn probe(
    executable: &Path,
    arguments: &[&str],
    workspace: Option<&Path>,
    environment_removals: &[&str],
    deadline: Instant,
) -> Result<Output> {
    if Instant::now() >= deadline {
        anyhow::bail!("diagnostic probe deadline exhausted");
    }
    let scratch = tempfile::tempdir()?;
    let mut command = probe_command(executable, arguments, scratch.path(), environment_removals)?;
    command.current_dir(workspace.unwrap_or(scratch.path()));
    provider_process::configure_process_tree(&mut command);
    let execution = Execution::Probe;
    let storage = OutputStorage::new(&execution)?;
    if Instant::now() >= deadline {
        anyhow::bail!("diagnostic probe deadline exhausted before launch");
    }
    let child_streams = storage.child_streams(&execution)?;
    bounded_output(
        &mut command,
        Stdio::null(),
        deadline,
        execution,
        storage,
        child_streams,
    )
    .map_err(CommandOutputFailure::into_error)
}

// Entry points own preparation and launch-check ordering. The shared core uses
// this policy for diagnostics, polling, output limits and process containment.
enum Execution<'a> {
    Command { label: &'a str },
    Probe,
}

impl Execution<'_> {
    fn io<T>(&self, result: std::io::Result<T>, context: &str) -> Result<T> {
        match self {
            Self::Command { label } => result.with_context(|| format!("{context} {label}")),
            Self::Probe => Ok(result?),
        }
    }

    fn output_limit(&self) -> Option<u64> {
        match self {
            Self::Command { .. } => None,
            Self::Probe => Some(MAX_PROBE_OUTPUT),
        }
    }
}

struct OutputStorage {
    stdout: File,
    stderr: File,
}

impl OutputStorage {
    fn new(execution: &Execution<'_>) -> Result<Self> {
        let stdout = execution.io(
            tempfile::tempfile(),
            "failed to create bounded stdout storage for",
        )?;
        let stderr = execution.io(
            tempfile::tempfile(),
            "failed to create bounded stderr storage for",
        )?;
        Ok(Self { stdout, stderr })
    }

    fn child_streams(&self, execution: &Execution<'_>) -> Result<(File, File)> {
        let stdout = execution.io(
            self.stdout.try_clone(),
            "failed to clone bounded stdout storage for",
        )?;
        let stderr = execution.io(
            self.stderr.try_clone(),
            "failed to clone bounded stderr storage for",
        )?;
        Ok((stdout, stderr))
    }
}

fn bounded_output(
    command: &mut Command,
    stdin: Stdio,
    deadline: Instant,
    execution: Execution<'_>,
    storage: OutputStorage,
    (child_stdout, child_stderr): (File, File),
) -> std::result::Result<Output, CommandOutputFailure> {
    let OutputStorage {
        mut stdout,
        mut stderr,
    } = storage;
    let mut child = execution
        .io(
            command
                .stdin(stdin)
                .stdout(child_stdout)
                .stderr(child_stderr)
                .spawn(),
            "failed to start",
        )
        .map_err(CommandOutputFailure::not_started)?;
    let tree = if matches!(execution, Execution::Probe) {
        match provider_process::ProviderProcessTree::attach(&child) {
            Ok(tree) => Some(tree),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandOutputFailure::started(error));
            }
        }
    } else {
        None
    };
    let mut reaped = false;
    let outcome = (|| {
        if let Some(tree) = &tree {
            tree.resume(&child).map_err(CommandOutputFailure::started)?;
        }
        let status = loop {
            if let Some(limit) = execution.output_limit() {
                let size = (|| -> Result<u64> {
                    Ok(stdout
                        .metadata()?
                        .len()
                        .saturating_add(stderr.metadata()?.len()))
                })()
                .map_err(CommandOutputFailure::started)?;
                if size > limit {
                    return Err(CommandOutputFailure::started(anyhow::anyhow!(
                        "diagnostic probe output limit exceeded"
                    )));
                }
            }
            if let Some(status) = execution
                .io(child.try_wait(), "failed to wait for")
                .map_err(CommandOutputFailure::started)?
            {
                reaped = true;
                break status;
            }
            if Instant::now() >= deadline {
                return Err(match execution {
                    Execution::Command { label } => CommandOutputFailure::started(
                        anyhow::Error::new(TimedOut::new(format!("{label} timed out"))),
                    )
                    .ended_at_deadline(),
                    Execution::Probe => {
                        CommandOutputFailure::started(anyhow::anyhow!("diagnostic probe timed out"))
                    }
                });
            }
            let interval = Duration::from_millis(20);
            thread::sleep(match execution {
                Execution::Command { .. } => deadline
                    .saturating_duration_since(Instant::now())
                    .min(interval),
                Execution::Probe => interval,
            });
        };
        if let Some(tree) = &tree {
            tree.terminate();
        }
        execution
            .io(
                stdout.seek(SeekFrom::Start(0)),
                "failed to rewind bounded stdout storage for",
            )
            .map_err(CommandOutputFailure::started)?;
        execution
            .io(
                stderr.seek(SeekFrom::Start(0)),
                "failed to rewind bounded stderr storage for",
            )
            .map_err(CommandOutputFailure::started)?;
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let read_limit = execution.output_limit().map_or(u64::MAX, |limit| limit + 1);
        execution
            .io(
                stdout.take(read_limit).read_to_end(&mut stdout_bytes),
                "failed to read bounded stdout storage for",
            )
            .map_err(CommandOutputFailure::started)?;
        execution
            .io(
                stderr.take(read_limit).read_to_end(&mut stderr_bytes),
                "failed to read bounded stderr storage for",
            )
            .map_err(CommandOutputFailure::started)?;
        if execution.output_limit().is_some_and(|limit| {
            stdout_bytes.len().saturating_add(stderr_bytes.len()) as u64 > limit
        }) {
            return Err(CommandOutputFailure::started(anyhow::anyhow!(
                "diagnostic probe output limit exceeded"
            )));
        }
        Ok(Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    })();
    if let Some(tree) = &tree {
        tree.terminate();
    }
    // Probes always clean up. Once try_wait reaps an ordinary command, even
    // a later rewind/read failure must not kill or wait for it again.
    if matches!(execution, Execution::Probe) || !reaped {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome
}

#[cfg(all(test, unix))]
mod probe_tests {
    #[test]
    fn doctor_commands_keep_only_adapter_removals_and_the_session_directory_removal() {
        for provider in agent_bridge::supported_clis() {
            let removals = provider::probe_environment_removals(*provider);
            for args in [vec!["--version"], vec!["diagnostic"]] {
                let command =
                    probe_command(Path::new("unused"), &args, Path::new("."), removals).unwrap();
                crate::native::process_env::tests::assert_provider_environment(
                    &command,
                    removals,
                    &[(SESSION_DIR_ENV, None)],
                );
            }
        }
    }

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn local_probes_bound_output_and_stop_a_hung_cli() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("provider");
        fs::write(&executable, "#!/bin/sh\nprintf '%066000d' 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let error = probe(
            &executable,
            &["--version"],
            Some(directory.path()),
            &[],
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("output limit"), "{error:#}");
        fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let error = probe(
            &executable,
            &["--version"],
            Some(directory.path()),
            &[],
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // WezTerm's fake runner needs the same timeout classification as the real runner.
    // Keep the production mutator private to this module.
    #[cfg(target_os = "macos")]
    pub(in crate::native) fn timed_out_failure(error: anyhow::Error) -> CommandOutputFailure {
        CommandOutputFailure::started(error).ended_at_deadline()
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_output_terminates_a_hung_child_at_the_deadline() {
        let mut command = Command::new("/bin/sh");
        // The background child inherits stdout/stderr. A pipe-reader implementation
        // would still block after killing only the direct shell process.
        command.args(["-c", "sleep 1 & wait"]);
        let started = Instant::now();

        let failure = command_output_until_classified(
            &mut command,
            started + Duration::from_millis(100),
            "test child",
        )
        .unwrap_err();
        assert!(failure.process_started());
        assert!(failure.timed_out());
        let error = failure.into_error();

        assert_eq!(format!("{error:#}"), "test child timed out");
        assert_eq!(
            crate::native::failure_kind::classify(&error),
            Some(crate::native::failure_kind::FailureKind::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_output_preserves_output_larger_than_the_probe_limit() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%066000d' 0; printf '%066000d' 0 >&2"]);
        let output = command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(5),
            "large output child",
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![b'0'; 66_000]);
        assert_eq!(output.stderr, output.stdout);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_output_identifies_a_pre_spawn_failure() {
        let mut command = Command::new("/definitely/not/an/agent-bridge-executable");

        let failure = command_output_until_classified(
            &mut command,
            Instant::now() + Duration::from_secs(1),
            "missing test child",
        )
        .unwrap_err();

        assert!(!failure.process_started());
        assert!(!failure.timed_out());
        let error = failure.into_error();
        assert!(format!("{error:#}").contains("failed to start missing test child"));
        // A spawn failure is no deadline.
        assert_eq!(crate::native::failure_kind::classify(&error), None);
    }

    #[test]
    fn a_deadline_before_the_spawn_is_typed_but_starts_and_ends_nothing() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let failure = command_output_until_classified(&mut command, Instant::now(), "test child")
            .unwrap_err();
        // Existing readers keep both process facts false before a spawn.
        assert!(!failure.process_started());
        assert!(!failure.timed_out());
        let error = failure.into_error();
        assert_eq!(
            format!("{error:#}"),
            "test child timed out before it started"
        );
        assert_eq!(
            format!("{error:#?}"),
            format!("{:?}", "test child timed out before it started")
        );
        assert_eq!(
            crate::native::failure_kind::classify(&error),
            Some(crate::native::failure_kind::FailureKind::TimedOut)
        );
        // The second check, after the output files exist, builds the same failure.
        let second = not_started_at_deadline("test child");
        assert!(!second.process_started() && !second.timed_out());
        assert_eq!(
            format!("{:#}", second.into_error()),
            "test child timed out before it started"
        );
    }

    // The Codex caller markers as Codex 0.166.0 spells them. Fixed here, not read from the
    // adapter, so that a dropped entry fails every test that uses them.
    pub(crate) const CODEX_CALLER_MARKERS: [&str; 5] = [
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_TOOL_CALL_ID",
        "CODEX_DAEMON_SHUTDOWN_FILE",
        "CODEX_DAEMON_SHUTDOWN_SOCKET",
    ];
    // Fixed spellings of both providers' caller identities. Non-Claude commands keep
    // Claude's execution configuration, but must not inherit its session or inbox.
    pub(crate) const NON_CLAUDE_CALLER_MARKERS: [&str; 15] = [
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_TOOL_CALL_ID",
        "CODEX_DAEMON_SHUTDOWN_FILE",
        "CODEX_DAEMON_SHUTDOWN_SOCKET",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CODEX_COMPANION_SESSION_ID",
        "CODEX_COMPANION_TRANSCRIPT_PATH",
    ];
    pub(crate) const NON_CLAUDE_CONTEXT_KEPT: [&str; 9] = [
        "AI_AGENT",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_EFFORT",
        "CLAUDE_CONFIG_DIR",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "CLAUDE_USER_CUSTOM_SETTING",
    ];
    // Codex configuration, credentials, install hints, informational and sandbox or proxy
    // context, and an unknown name: none is a caller marker, so every launch inherits them.
    pub(crate) const CODEX_CONTEXT_KEPT: [&str; 14] = [
        "CODEX_HOME",
        "CODEX_SQLITE_HOME",
        "CODEX_CA_CERTIFICATE",
        "CODEX_API_KEY",
        "CODEX_MANAGED_BY_NPM",
        "CODEX_MANAGED_PACKAGE_ROOT",
        "CODEX_CI",
        "CODEX_VERSION",
        "CODEX_PERMISSION_PROFILE",
        "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
        "CODEX_SANDBOX",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_NETWORK_PROXY_ACTIVE",
        "CODEX_USER_CUSTOM_SETTING",
    ];

    pub(crate) fn assert_provider_environment(
        command: &Command,
        removals: &[&str],
        extras: &[(&str, Option<&OsStr>)],
    ) {
        let mut expected: BTreeMap<_, _> = removals
            .iter()
            .map(|name| (OsStr::new(name), None))
            .collect();
        for (name, value) in extras {
            expected.insert(OsStr::new(name), *value);
        }
        assert_eq!(command.get_envs().collect::<BTreeMap<_, _>>(), expected);
    }
    pub(crate) fn assert_helper_environment(command: &Command, extras: &[(&str, Option<&OsStr>)]) {
        let mut expected = BTreeMap::new();
        for provider in agent_bridge::supported_clis() {
            for name in provider::probe_environment_removals(*provider) {
                expected.insert(OsStr::new(name), None);
            }
        }
        for name in BRIDGE_SESSION_VARIABLES {
            expected.insert(OsStr::new(name), None);
        }
        for (name, value) in extras {
            expected.insert(OsStr::new(name), *value);
        }
        assert_eq!(command.get_envs().collect::<BTreeMap<_, _>>(), expected);
        for name in ["AI_AGENT", "CLAUDE_CODE_BRIDGE_SESSION_ID"]
            .into_iter()
            .chain(CODEX_CALLER_MARKERS)
        {
            assert_eq!(expected.get(OsStr::new(name)), Some(&None), "{name}");
        }
        // No override/removal of user configuration: it remains inherited.
        for name in [
            "PATH",
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
        ]
        .into_iter()
        .chain(CODEX_CONTEXT_KEPT)
        {
            assert!(!expected.contains_key(OsStr::new(name)), "{name}");
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn without_bridge_environment(module: &str, test: &str) -> bool {
        let name = format!("{}::{test}", module.split_once("::").unwrap().1);
        const CHILD: &str = "E1_ENVIRONMENT_TEST";
        if std::env::var(CHILD).ok().as_deref() == Some(&name) {
            for variable in BRIDGE_SESSION_VARIABLES {
                assert!(std::env::var_os(variable).is_none(), "{variable}");
            }
            return true;
        }
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &name);
        for variable in BRIDGE_SESSION_VARIABLES {
            child.env_remove(variable);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        false
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn wrapper_for(directory: &Path) -> String {
        let root = directory.parent().unwrap();
        let id = directory.file_name().unwrap().to_str().unwrap();
        let command = crate::native::session::launch::launcher::bridge_shell_command(
            Path::new("/workspace"),
            root,
            Path::new("/unused/bridge"),
            id,
        )
        .unwrap();
        assert!(command.contains(&format!(
            "{STATE_DIR_ENV}={}",
            crate::native::terminal::shell_quote(root.as_os_str())
        )));
        assert!(command.contains(&format!(
            "native-session {}",
            crate::native::terminal::shell_quote(OsStr::new(id))
        )));
        command
    }
    #[test]
    fn helpers_remove_only_adapter_markers_and_bridge_session_variables() {
        assert_helper_environment(&helper_command("unused"), &[]);
    }

    #[test]
    fn non_provider_removals_drop_the_codex_caller_markers_and_keep_codex_configuration() {
        let removals: std::collections::BTreeSet<_> = non_provider_removals().collect();
        for name in CODEX_CALLER_MARKERS {
            assert!(removals.contains(name), "{name}");
        }
        for name in CODEX_CONTEXT_KEPT {
            assert!(!removals.contains(name), "{name}");
        }
        let command = helper_command("unused");
        let environment: BTreeMap<_, _> = command.get_envs().collect();
        for name in CODEX_CALLER_MARKERS {
            assert_eq!(environment.get(OsStr::new(name)), Some(&None), "{name}");
        }
        assert!(!environment.contains_key(OsStr::new("CODEX_HOME")));
    }

    #[test]
    fn environment_removals_are_recorded_on_the_command_before_spawn() {
        let mut command = Command::new("provider");
        command.env("CLAUDE_CODE_CHILD_SESSION", "1");
        remove(&mut command, &["CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE"]);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [
                (OsStr::new("CLAUDECODE"), None),
                (OsStr::new("CLAUDE_CODE_CHILD_SESSION"), None),
            ]
        );
    }

    #[test]
    fn provider_builders_preserve_each_adapters_exact_environment() {
        for provider in agent_bridge::supported_clis() {
            let removals = provider::probe_environment_removals(*provider);
            let expected: BTreeMap<_, _> = removals
                .iter()
                .map(|name| (OsStr::new(name), None))
                .collect();
            for command in [
                provider_command(Path::new("unused"), Path::new("."), vec![], removals).unwrap(),
                provider_version_command(Path::new("unused"), removals).unwrap(),
            ] {
                assert_eq!(command.get_envs().collect::<BTreeMap<_, _>>(), expected);
            }
        }
    }
}
