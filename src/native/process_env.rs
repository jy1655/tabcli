//! Environment policy for Bridge children. Provider adapters own their removal lists;
//! helpers must not pass any caller's session identity into another application.
use std::{ffi::OsStr, ffi::OsString, path::Path, process::Command};

use anyhow::Result;

#[cfg(any(not(windows), test))]
use super::provider;
use super::provider_process;
#[cfg(not(windows))]
use super::{SESSION_DIR_ENV, STATE_DIR_ENV};

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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
            crate::native::shell_quote(root.as_os_str())
        )));
        assert!(command.contains(&format!(
            "native-session {}",
            crate::native::shell_quote(OsStr::new(id))
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
