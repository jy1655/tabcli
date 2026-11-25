//! The Surface a launch starts on, through the public commands. `ask` and `reopen` select it
//! and admit an automatic choice before anything else they check; `doctor` reports the
//! selection without admitting it. Each case runs the built binary as if inside a Herdr
//! terminal (hc gives every program `HC_TERMINAL_ID` and `TERM_PROGRAM=ghostty`) whose core
//! is absent, so admitting an automatic Herdr choice fails caller verification. No Herdr
//! Core, state root, provider or terminal is touched, and no test changes this process's
//! environment.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
};

const PROVIDERS: [&str; 4] = ["codex", "claude", "agy", "pi"];
const CALLER_REFUSAL: &str = "the automatic choice of Herdr was refused and no other terminal was opened. Run inside Herdr, or choose a terminal with --terminal";
const NO_CORE: &str = "no Herdr Core is running at ";
const NO_WORKSPACE: &str = "workspace does not exist or cannot be resolved: ";
const NO_SOURCE: &str = "no such TAB session: session-absent";

// The caller's terminal: inside hc, or a marker inherited into another terminal (which names
// that terminal, not Herdr).
#[derive(Clone, Copy, Debug)]
enum Caller {
    Herdr,
    InheritedMarker,
}

struct Fixture {
    _directory: tempfile::TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        Self {
            base,
            _directory: directory,
        }
    }

    fn state_root(&self) -> PathBuf {
        self.base.join("state")
    }

    fn run(&self, args: &[&str], caller: Caller) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
        command
            .args(args)
            .current_dir(&self.base)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.state_root())
            .env("XDG_STATE_HOME", &self.base)
            .env("ZDOTDIR", &self.base)
            .env("HC_SOCKET", self.absent_core())
            .env("HC_TERMINAL_ID", "t7")
            .env(
                "TERM_PROGRAM",
                match caller {
                    Caller::Herdr => "ghostty",
                    Caller::InheritedMarker => "WezTerm",
                },
            )
            .env("TERM", "xterm-256color")
            .env_remove("HC_SESSION")
            .env_remove("ITERM_SESSION_ID")
            .env_remove("TERM_SESSION_ID")
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .stdin(std::process::Stdio::null());
        let output = command.output().unwrap();
        assert!(
            !self.state_root().exists(),
            "{args:?}: a state root was created"
        );
        output
    }

    // An endpoint where no core runs: a socket path in this test's own private directory, or
    // on Windows a local pipe name that nothing serves.
    fn absent_core(&self) -> String {
        if cfg!(windows) {
            format!(
                r"\\.\pipe\tabcli-launch-surface-test-{}-{}",
                std::process::id(),
                self.base.file_name().unwrap().to_string_lossy()
            )
        } else {
            self.base.join("absent.sock").to_string_lossy().into_owned()
        }
    }

    fn ask(&self, provider: &str, terminal: Option<&str>, caller: Caller) -> Value {
        let workspace = self.base.join("absent-workspace");
        let mut args = vec![
            "ask",
            provider,
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            "must not be delivered",
            "--json",
        ];
        if let Some(terminal) = terminal {
            args.extend(["--terminal", terminal]);
        }
        refusal(self.run(&args, caller))
    }

    fn reopen(&self, terminal: Option<&str>) -> Value {
        let mut args = vec![
            "reopen",
            "session-absent",
            "--prompt",
            "must not be delivered",
            "--json",
        ];
        if let Some(terminal) = terminal {
            args.extend(["--terminal", terminal]);
        }
        refusal(self.run(&args, Caller::Herdr))
    }
}

fn refusal(output: Output) -> Value {
    assert!(!output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {output:?}"))
}

fn error(reply: &Value) -> &str {
    reply["error"].as_str().unwrap()
}

// Inside hc the automatic choice is Herdr on macOS and Linux for every provider, a provider
// not known yet included. On Windows it is Herdr only for a provider whose adapter runs there
// (not Codex), and the managed console otherwise.
fn automatic_herdr(provider: Option<&str>) -> bool {
    !cfg!(windows) || provider.is_some_and(|provider| provider != "codex")
}

fn assert_workspace_refused(reply: &Value, case: &str) {
    assert!(error(reply).starts_with(NO_WORKSPACE), "{case}: {reply}");
}

fn assert_caller_refused(reply: &Value, case: &str) {
    assert!(error(reply).starts_with(NO_CORE), "{case}: {reply}");
    assert!(error(reply).ends_with(CALLER_REFUSAL), "{case}: {reply}");
    assert!(reply.get("failure_kind").is_none(), "{case}: {reply}");
}

#[test]
fn ask_admits_its_selected_surface_before_it_resolves_the_workspace() {
    let fixture = Fixture::new();
    for provider in PROVIDERS {
        // The automatic choice: an automatic Herdr is admitted against the caller's terminal,
        // before the workspace (absent here) is resolved.
        let reply = fixture.ask(provider, None, Caller::Herdr);
        if automatic_herdr(Some(provider)) {
            assert_caller_refused(&reply, provider);
        } else {
            assert_workspace_refused(&reply, provider);
        }
        // An explicit Herdr is not verified against the caller; the provider's own admission
        // comes next, then the workspace.
        let reply = fixture.ask(provider, Some("herdr"), Caller::Herdr);
        if cfg!(windows) && provider == "codex" {
            assert!(
                error(&reply).starts_with("codex on Herdr is unsupported on this platform: "),
                "{reply}"
            );
            assert_eq!(reply["failure_kind"], "unsupported", "{reply}");
        } else {
            assert_workspace_refused(&reply, provider);
        }
        // A marker inherited into another terminal selects that terminal, not Herdr.
        let reply = fixture.ask(provider, None, Caller::InheritedMarker);
        assert_workspace_refused(&reply, provider);
    }
}

#[test]
fn reopen_selects_and_admits_its_surface_before_it_reads_its_source() {
    let fixture = Fixture::new();
    // The source, and so its provider, is not known at selection.
    let reply = fixture.reopen(None);
    if automatic_herdr(None) {
        assert_caller_refused(&reply, "reopen");
    } else {
        assert!(error(&reply).starts_with(NO_SOURCE), "{reply}");
    }
    assert!(reply["gate"].is_null(), "{reply}");
    let reply = fixture.reopen(Some("herdr"));
    assert!(error(&reply).starts_with(NO_SOURCE), "{reply}");
}

#[test]
fn doctor_reports_the_selected_surface_without_admitting_it() {
    let fixture = Fixture::new();
    for provider in PROVIDERS {
        let output = fixture.run(&["doctor", "--provider", provider, "--json"], Caller::Herdr);
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let selection = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "terminal_selection")
            .unwrap_or_else(|| panic!("{provider}: {report}"));
        // The core is absent, so an admission would have refused the automatic Herdr.
        assert_eq!(selection["availability"], "available", "{selection}");
        assert_eq!(selection["reason_code"], "terminal_selected", "{selection}");
        assert_eq!(
            selection["evidence"]["terminal"],
            if automatic_herdr(Some(provider)) {
                "herdr"
            } else {
                "windows-console"
            },
            "{provider}: {selection}"
        );
    }
}
