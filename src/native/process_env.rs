//! Environment policy and deadline-bounded command execution for Bridge children.
//! Provider adapters own their removal lists; helpers must not pass any caller's
//! session identity into another application.
#[cfg(not(windows))]
use crate::native::session::primitives::{SESSION_DIR_ENV, STATE_DIR_ENV};
use std::{
    ffi::OsStr,
    ffi::OsString,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

#[cfg(any(not(windows), test))]
use super::provider;
use super::provider_process;
#[cfg(not(windows))]
const BRIDGE_SESSION_VARIABLES: [&str; 4] = [
    STATE_DIR_ENV,
    SESSION_DIR_ENV,
    "AGENT_BRIDGE_NATIVE_SESSION_ID",
    "AGENT_BRIDGE_EXECUTABLE",
];

pub(super) fn helper_command(program: impl AsRef<OsStr>) -> Command {
    let command = Command::new(program);
    // Native Windows helpers retain their inherited environment until issue #111.
    #[cfg(not(windows))]
    let command = {
        let mut command = command;
        // Query each adapter, rather than maintaining a second copy of its markers.
        for provider in agent_bridge::supported_clis() {
            remove(
                &mut command,
                provider::probe_environment_removals(*provider),
            );
        }
        remove(&mut command, &BRIDGE_SESSION_VARIABLES);
        command
    };
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
        return Err(CommandOutputFailure::not_started(anyhow::anyhow!(
            "{label} timed out before it started"
        )));
    }
    let mut stdout = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let mut stderr = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let child_stdout = stdout
        .try_clone()
        .with_context(|| format!("failed to clone bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let child_stderr = stderr
        .try_clone()
        .with_context(|| format!("failed to clone bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    if Instant::now() >= deadline {
        return Err(CommandOutputFailure::not_started(anyhow::anyhow!(
            "{label} timed out before it started"
        )));
    }
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr))
        .spawn()
        .with_context(|| format!("failed to start {label}"))
        .map_err(CommandOutputFailure::not_started)?;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(20)));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    CommandOutputFailure::started(anyhow::anyhow!("{label} timed out"))
                        .ended_at_deadline(),
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandOutputFailure::started(
                    anyhow::Error::new(error).context(format!("failed to wait for {label}")),
                ));
            }
        }
    };
    stdout
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    stderr
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .with_context(|| format!("failed to read bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    stderr
        .read_to_end(&mut stderr_bytes)
        .with_context(|| format!("failed to read bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
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

        assert!(format!("{error:#}").contains("test child timed out"));
        assert!(started.elapsed() < Duration::from_millis(500));
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
        assert!(
            format!("{:#}", failure.into_error()).contains("failed to start missing test child")
        );
    }

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
        #[cfg(not(windows))]
        {
            for provider in agent_bridge::supported_clis() {
                for name in provider::probe_environment_removals(*provider) {
                    expected.insert(OsStr::new(name), None);
                }
            }
            for name in BRIDGE_SESSION_VARIABLES {
                expected.insert(OsStr::new(name), None);
            }
        }
        for (name, value) in extras {
            expected.insert(OsStr::new(name), *value);
        }
        assert_eq!(command.get_envs().collect::<BTreeMap<_, _>>(), expected);
        #[cfg(not(windows))]
        for name in ["AI_AGENT", "CLAUDE_CODE_BRIDGE_SESSION_ID"] {
            assert_eq!(expected.get(OsStr::new(name)), Some(&None), "{name}");
        }
        // No override/removal of user configuration: it remains inherited.
        for name in [
            "PATH",
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
        ] {
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
        let command = crate::native::bridge_shell_command(
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
    #[cfg(not(windows))]
    #[test]
    fn helpers_remove_only_adapter_markers_and_bridge_session_variables() {
        assert_helper_environment(&helper_command("unused"), &[]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_helpers_keep_the_inherited_environment_until_issue_111() {
        assert_eq!(helper_command("unused").get_envs().count(), 0);
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
