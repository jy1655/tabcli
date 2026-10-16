//! The Codex memory workspace launch admission (#216), through the public command. Each
//! case runs the installed binary with an explicit HOME, USERPROFILE and CODEX_HOME, so no
//! test changes this process's environment. Linux has no terminal to select, and that
//! refusal comes before admission, so `ask` is exercised on macOS and Windows only.
#![cfg(any(target_os = "macos", windows))]
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _directory: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        Self {
            root: base.join("state"),
            base,
            _directory: directory,
        }
    }

    fn directory(&self, relative: &str) -> PathBuf {
        let path = self.base.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn command(&self, args: &[&str], environment: &[(&str, &Path)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
        command
            .args(args)
            .current_dir(&self.base)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &self.root)
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("CODEX_HOME")
            .stdin(std::process::Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    fn ask(&self, provider: &str, workspace: &Path, environment: &[(&str, &Path)]) -> Output {
        self.command(
            &[
                "ask",
                provider,
                "--workspace",
                workspace.to_str().unwrap(),
                "--prompt",
                "must not be delivered",
                "--json",
            ],
            environment,
        )
    }
}

fn refusal(workspace: &Path, memory: &Path) -> String {
    format!(
        "launch refused: workspace {} is within the Codex memory workspace {}; codex-memory-workspace is refuse. To allow launches here, run 'tabcli settings codex-memory-workspace allow' in the same state root.",
        workspace.display(),
        memory.display()
    )
}

fn error_of(output: &Output) -> String {
    assert!(!output.status.success(), "{output:?}");
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    reply["error"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn ask_refuses_the_memory_workspace_for_every_provider_before_any_record_exists() {
    let fixture = Fixture::new();
    let home = fixture.directory("home");
    let memory = fixture.directory("home/.codex/memories");
    let inside = fixture.directory("home/.codex/memories/rollouts");
    for provider in ["codex", "claude", "agy", "pi"] {
        for workspace in [&memory, &inside] {
            let output = fixture.ask(provider, workspace, &[("HOME", &home)]);
            let error = error_of(&output);
            assert!(error.contains(&refusal(workspace, &memory)), "{error}");
            assert!(!fixture.root.exists(), "{provider}: a record was created");
        }
    }
}

#[test]
fn the_caller_environment_names_the_codex_home_and_the_workspace_does_not() {
    let fixture = Fixture::new();
    let home = fixture.directory("home");
    let profile = fixture.directory("profile");
    // HOME is used before USERPROFILE, as for workspace trust.
    let home_memory = fixture.directory("home/.codex/memories");
    let profile_memory = fixture.directory("profile/.codex/memories");
    let output = fixture.ask(
        "codex",
        &home_memory,
        &[("HOME", &home), ("USERPROFILE", &profile)],
    );
    assert!(error_of(&output).contains(&refusal(&home_memory, &home_memory)));
    let output = fixture.ask("codex", &profile_memory, &[("USERPROFILE", &profile)]);
    assert!(error_of(&output).contains(&refusal(&profile_memory, &profile_memory)));

    // CODEX_HOME replaces the default home; a relative value is relative to the caller's
    // working directory, never to --workspace.
    let custom_memory = fixture.directory("custom/memories");
    let output = fixture.ask(
        "codex",
        &custom_memory,
        &[("HOME", &home), ("CODEX_HOME", Path::new("custom"))],
    );
    assert!(error_of(&output).contains(&refusal(&custom_memory, &custom_memory)));
    // An empty CODEX_HOME is not rejected: it names the caller's working directory.
    let cwd_memory = fixture.directory("memories");
    let output = fixture.ask(
        "codex",
        &cwd_memory,
        &[("HOME", &home), ("CODEX_HOME", Path::new(""))],
    );
    assert!(error_of(&output).contains(&refusal(&cwd_memory, &cwd_memory)));

    // Without HOME and USERPROFILE the Codex home cannot be resolved, even with CODEX_HOME.
    let output = fixture.ask(
        "codex",
        &custom_memory,
        &[("CODEX_HOME", &fixture.base.join("custom"))],
    );
    assert!(
        error_of(&output).contains(
            "cannot locate the Codex memory workspace: no user home for workspace trust lookup"
        ),
        "{output:?}"
    );
    assert!(!fixture.root.exists());
}

#[test]
fn the_settings_command_reports_and_changes_the_admission() {
    let fixture = Fixture::new();
    let shown = fixture.command(&["settings", "--json"], &[]);
    assert!(shown.status.success(), "{shown:?}");
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["codex_memory_workspace"], "refuse");
    assert!(!fixture.root.exists());

    let invalid = fixture.command(&["settings", "codex-memory-workspace", "yes"], &[]);
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .contains("codex-memory-workspace is refuse or allow, not \"yes\"")
    );
    assert!(!fixture.root.exists());

    let home = fixture.directory("home");
    let changed = fixture.command(
        &["settings", "codex-memory-workspace", "allow", "--json"],
        &[("HOME", &home)],
    );
    assert!(changed.status.success(), "{changed:?}");
    let changed: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["codex_memory_workspace"], "allow");
}
