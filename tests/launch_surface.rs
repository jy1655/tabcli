//! The Surface a launch starts on, through the public commands, the same on every OS. `ask`
//! and `reopen` select it from the request and the caller's environment; the provider, with
//! its probed version, then decides between Herdr and the OS's own choice, and only a launch
//! that will use Herdr checks its caller, after the workspace, the source and the probe.
//! `doctor` reports that decision without the caller check. Most cases run the built binary as
//! if inside a Herdr terminal (hc gives every program `HC_TERMINAL_ID` and
//! `TERM_PROGRAM=ghostty`) whose core is absent, so a caller check fails; some run a provider
//! stub that only prints a version, and on macOS and Linux others answer the caller check from
//! a scripted core in this process. No real Herdr Core, state root, provider or terminal is
//! touched, and no test changes this process's environment.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
use serde_json::Value;
use std::{
    ffi::OsStr,
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
    // The socket's own short private directory under /tmp (macOS and Linux).
    #[cfg(unix)]
    _socket_directory: tempfile::TempDir,
    absent_core: String,
}

// A temporary directory only this user can change. Without explicit permissions tempfile
// creates it with the umask's mode (0o775 under umask 002), and in a shared directory (/tmp,
// or a TMPDIR shared by several users) another user could then create an entry that the
// cases need to be absent: the socket, the state root or the workspace. On Windows the
// directory keeps the access list of the user's temporary directory.
fn private_directory() -> tempfile::Builder<'static, 'static> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
}

impl Fixture {
    fn new() -> Self {
        let directory = private_directory().tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        // An endpoint where no core runs. On macOS and Linux it is a socket path, which the
        // adapter refuses by its length (above 103 bytes on macOS, 107 on Linux) before it
        // looks for a core, so it is not placed under the caller's temporary directory, whose
        // length the test does not control. On Windows it is a local pipe name nothing serves.
        #[cfg(unix)]
        let socket_directory = private_directory()
            .prefix("tls")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        let absent_core = socket_directory
            .path()
            .join("absent.sock")
            .to_string_lossy()
            .into_owned();
        #[cfg(windows)]
        let absent_core = format!(
            r"\\.\pipe\tabcli-launch-surface-test-{}-{}",
            std::process::id(),
            base.file_name().unwrap().to_string_lossy()
        );
        #[cfg(unix)]
        for path in [directory.path(), socket_directory.path()] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is not private", path.display());
        }
        Self {
            base,
            _directory: directory,
            #[cfg(unix)]
            _socket_directory: socket_directory,
            absent_core,
        }
    }

    fn state_root(&self) -> PathBuf {
        self.base.join("state")
    }

    fn run(&self, args: &[&str], caller: Caller) -> Output {
        self.run_with(args, caller, &[])
    }

    // `environment` adds to, or replaces, what every case sets.
    fn run_with(&self, args: &[&str], caller: Caller, environment: &[(&str, &OsStr)]) -> Output {
        self.run_from(&self.base, args, caller, environment)
    }

    // As `run_with`, invoked from `directory`.
    fn run_from(
        &self,
        directory: &std::path::Path,
        args: &[&str],
        caller: Caller,
        environment: &[(&str, &OsStr)],
    ) -> Output {
        let output = self
            .command_from(directory, args, caller, environment)
            .output()
            .unwrap();
        assert!(
            !self.state_root().exists(),
            "{args:?}: a state root was created"
        );
        output
    }

    // The command `run_from` runs.
    fn command_from(
        &self,
        directory: &std::path::Path,
        args: &[&str],
        caller: Caller,
        environment: &[(&str, &OsStr)],
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
        command
            .args(args)
            .current_dir(directory)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.state_root())
            .env("XDG_STATE_HOME", &self.base)
            .env("ZDOTDIR", &self.base)
            .env("HC_SOCKET", &self.absent_core)
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
        command.envs(environment.iter().copied());
        command
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

// The Surface each OS chooses outside Herdr, which an automatic choice inside hc falls back to
// where the provider is not admitted on Herdr: Ghostty on macOS (hc names it in
// `TERM_PROGRAM`), tmux on Linux and the managed console on Windows.
const FALLBACK: &str = if cfg!(target_os = "macos") {
    "ghostty"
} else if cfg!(windows) {
    "windows-console"
} else {
    "tmux"
};

const BLANK_TITLE: &str = "--title cannot be empty";

fn assert_workspace_refused(reply: &Value, case: &str) {
    assert!(error(reply).starts_with(NO_WORKSPACE), "{case}: {reply}");
}

fn assert_caller_refused(reply: &Value, case: &str) {
    assert!(error(reply).starts_with(NO_CORE), "{case}: {reply}");
    assert!(error(reply).ends_with(CALLER_REFUSAL), "{case}: {reply}");
    assert!(reply.get("failure_kind").is_none(), "{case}: {reply}");
}

impl Fixture {
    // A provider executable with `body`, a POSIX shell script or, on Windows, a batch file,
    // alone in its own directory. Returns the PATH of a case that runs it: that directory,
    // then the system's own.
    fn provider_path(&self, name: &str, command: &str, body: &str) -> std::ffi::OsString {
        let directory = self.base.join(format!("bin-{name}"));
        std::fs::create_dir_all(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = directory.join(command);
            std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::env::join_paths([
                directory.as_path(),
                std::path::Path::new("/usr/bin"),
                std::path::Path::new("/bin"),
            ])
            .unwrap()
        }
        #[cfg(windows)]
        {
            std::fs::write(
                directory.join(format!("{command}.cmd")),
                format!("@echo off\r\n{body}\r\n"),
            )
            .unwrap();
            let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
            std::env::join_paths([directory, system]).unwrap()
        }
    }

    // A provider that prints `version` (plain words only, as both shells echo them).
    fn printing(&self, command: &str, version: &str) -> std::ffi::OsString {
        let name = format!(
            "{command}-{}",
            version.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        );
        if cfg!(windows) {
            self.provider_path(&name, command, &format!("echo {version}"))
        } else {
            self.provider_path(&name, command, &format!("echo '{version}'"))
        }
    }

    // A home, and the provider homes under it, of this case's own, so that no real provider
    // store is read.
    fn homes(&self) -> Vec<(&'static str, PathBuf)> {
        let home = self.base.join("home");
        let homes = vec![
            ("CODEX_HOME", home.join("codex")),
            ("CLAUDE_CONFIG_DIR", home.join("claude")),
            ("PI_CODING_AGENT_DIR", home.join("pi")),
        ];
        for (_, directory) in &homes {
            std::fs::create_dir_all(directory).unwrap();
        }
        let mut all = vec![("HOME", home.clone()), ("USERPROFILE", home)];
        all.extend(homes);
        all
    }

    // `ask` in an existing workspace with a blank title: a launch that passes its Surface
    // decision is refused right after it, before anything exists.
    fn ask_blank_title(
        &self,
        provider: &str,
        terminal: Option<&str>,
        path: &OsStr,
        socket: Option<&OsStr>,
    ) -> Value {
        let mut args = vec!["--title", "   "];
        if let Some(terminal) = terminal {
            args.extend(["--terminal", terminal]);
        }
        let mut environment = vec![("PATH", path)];
        if let Some(socket) = socket {
            environment.push(("HC_SOCKET", socket));
        }
        refusal(self.ask_with(provider, &args, &environment))
    }

    // `ask` from inside hc in an existing workspace with `args` and `environment` added.
    fn ask_with(&self, provider: &str, args: &[&str], environment: &[(&str, &OsStr)]) -> Output {
        let project = self.base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut all = vec![
            "ask",
            provider,
            "--workspace",
            project.to_str().unwrap(),
            "--prompt",
            "must not be delivered",
            "--json",
        ];
        all.extend(args);
        let homes = self.homes();
        let mut full: Vec<(&str, &OsStr)> = homes
            .iter()
            .map(|(name, path)| (*name, path.as_os_str()))
            .collect();
        full.extend(environment);
        self.run_with(&all, Caller::Herdr, &full)
    }

    // `doctor --provider` from `directory` with the provider on `path` and `environment` added.
    fn doctor_from(
        &self,
        directory: &std::path::Path,
        provider: &str,
        probe: bool,
        caller: Caller,
        path: Option<&OsStr>,
        environment: &[(&str, &OsStr)],
    ) -> Value {
        let mut args = vec!["doctor", "--provider", provider, "--json"];
        if probe {
            args.push("--probe");
        }
        let homes = self.homes();
        let mut full: Vec<(&str, &OsStr)> = homes
            .iter()
            .map(|(name, path)| (*name, path.as_os_str()))
            .collect();
        if let Some(path) = path {
            full.push(("PATH", path));
        }
        full.extend(environment);
        let output = self.run_from(directory, &args, caller, &full);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    // A provider whose `--version` exits unsuccessfully.
    fn failing(&self, command: &str) -> std::ffi::OsString {
        let body = if cfg!(windows) { "exit /b 3" } else { "exit 3" };
        self.provider_path(&format!("{command}-failing"), command, body)
    }
}

fn terminal_selection(report: &Value) -> &Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "terminal_selection")
        .unwrap_or_else(|| panic!("{report}"))
}

// One rule places the caller check of an automatic Herdr choice. Where the adapter's answer
// does not depend on the version (Claude, Agy, Pi), Herdr is the choice before anything is
// resolved and the caller is checked first, before the workspace and the provider's probe, as
// in 0.4.49. Where it does (Codex), the caller is checked after the probe and only for a
// choice that ends on Herdr. An explicit Herdr is never checked against the caller.
#[test]
fn ask_checks_the_caller_first_where_no_version_can_change_the_choice() {
    let fixture = Fixture::new();
    for provider in PROVIDERS {
        let reply = fixture.ask(provider, None, Caller::Herdr);
        if provider == "codex" {
            assert_workspace_refused(&reply, provider);
        } else {
            assert_caller_refused(&reply, provider);
        }
        let reply = fixture.ask(provider, None, Caller::InheritedMarker);
        assert_workspace_refused(&reply, provider);
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
    }
    // A provider whose probe fails: Pi's choice needs no version, so the caller refusal comes
    // first and Pi is never run; Codex's needs its version, so its probe's failure comes first.
    let pi = fixture.failing("pi");
    let reply = refusal(fixture.ask_with("pi", &[], &[("PATH", pi.as_os_str())]));
    assert_caller_refused(&reply, "failing pi");
    let codex = fixture.failing("codex");
    let reply = refusal(fixture.ask_with("codex", &[], &[("PATH", codex.as_os_str())]));
    assert!(error(&reply).contains("--version exited with"), "{reply}");
}

// A caller that cannot be verified (the core is absent) refuses a launch that would use Herdr,
// and nothing else: on every OS a provider version Herdr does not admit takes the OS's own
// terminal without the check, as Codex on Windows does for every version.
#[test]
fn a_stale_caller_refuses_only_a_launch_that_uses_herdr() {
    let fixture = Fixture::new();
    // Pi runs on Herdr at every version on every OS.
    let pi = fixture.printing("pi", "1.5.0");
    assert_caller_refused(&fixture.ask_blank_title("pi", None, &pi, None), "pi");
    let reply = fixture.ask_blank_title("pi", Some("herdr"), &pi, None);
    assert_eq!(error(&reply), BLANK_TITLE, "{reply}");
    // Codex on both sides of the Herdr floor.
    for (version, herdr) in [
        ("codex-cli 0.168.0", false),
        ("codex-cli 0.170.0", !cfg!(windows)),
    ] {
        let codex = fixture.printing("codex", version);
        let reply = fixture.ask_blank_title("codex", None, &codex, None);
        if herdr {
            assert_caller_refused(&reply, version);
        } else {
            assert_eq!(error(&reply), BLANK_TITLE, "{version}: {reply}");
        }
    }
}

// A reopen reads its source and its provider's reopen support before any caller check: an
// absent source is reported as such inside a Herdr terminal whose caller cannot be verified.
#[test]
fn reopen_reports_its_source_before_any_caller_check() {
    let fixture = Fixture::new();
    for terminal in [None, Some("herdr")] {
        let reply = fixture.reopen(terminal);
        assert!(
            error(&reply).starts_with(NO_SOURCE),
            "{terminal:?}: {reply}"
        );
        assert!(reply["gate"].is_null(), "{reply}");
    }
}

// Without --probe, doctor reports the choice where the version does not decide it, with no
// word about a version: Claude, Agy and Pi on Herdr, Codex on Windows on the managed console.
// Where the version decides (Codex on macOS and Linux) it names no terminal, says the version
// was not read, and gives Herdr's requirement and the fallback. It never checks the caller.
#[test]
fn doctor_reports_the_selected_surface_without_the_caller_check() {
    let fixture = Fixture::new();
    for provider in PROVIDERS {
        let report = fixture.doctor_from(&fixture.base, provider, false, Caller::Herdr, None, &[]);
        let selection = terminal_selection(&report);
        assert_eq!(
            selection["evidence"]["fallback"], FALLBACK,
            "{provider}: {selection}"
        );
        if provider == "codex" && !cfg!(windows) {
            assert_eq!(selection["availability"], "unknown", "{selection}");
            assert_eq!(
                selection["reason_code"], "probe_not_requested",
                "{selection}"
            );
            assert!(selection["evidence"]["terminal"].is_null(), "{selection}");
            assert_eq!(
                selection["evidence"]["herdr_requires"], "stable Codex 0.169.0 or newer",
                "{selection}"
            );
            assert!(
                selection["next_action"]
                    .as_str()
                    .unwrap()
                    .contains("--probe"),
                "{selection}"
            );
        } else {
            assert_eq!(selection["availability"], "available", "{selection}");
            assert_eq!(selection["reason_code"], "terminal_selected", "{selection}");
            assert_eq!(
                selection["evidence"]["terminal"],
                if provider == "codex" {
                    FALLBACK
                } else {
                    "herdr"
                },
                "{provider}: {selection}"
            );
            assert!(
                selection["evidence"].get("herdr_requires").is_none(),
                "{selection}"
            );
            for text in ["detail", "next_action"] {
                assert!(
                    !selection[text].as_str().unwrap().contains("version"),
                    "{provider} {text}: {selection}"
                );
            }
        }
        // Outside hc there is no automatic Herdr choice, and so no fallback.
        let report = fixture.doctor_from(
            &fixture.base,
            provider,
            false,
            Caller::InheritedMarker,
            None,
            &[],
        );
        let selection = terminal_selection(&report);
        assert_eq!(selection["reason_code"], "terminal_selected", "{selection}");
        assert_ne!(selection["evidence"]["terminal"], "herdr", "{selection}");
        assert!(
            selection["evidence"].get("fallback").is_none(),
            "{selection}"
        );
    }
}

// With --probe, where the version decides, doctor reports the outcome of the version check as
// `ask` reaches it: the Surface for an admitted version, and no Surface where the probe failed
// or the general minimum refuses the launch before any terminal is chosen.
#[cfg(unix)]
#[test]
fn doctor_reports_the_surface_ask_chooses_for_the_probed_version() {
    let fixture = Fixture::new();
    for (version, availability, reason, terminal) in [
        (
            "codex-cli 0.168.0",
            "available",
            "terminal_selected",
            Some(FALLBACK),
        ),
        (
            "codex-cli 0.170.0",
            "available",
            "terminal_selected",
            Some("herdr"),
        ),
        (
            "OpenAI Codex (v0.170.0)",
            "available",
            "terminal_selected",
            Some("herdr"),
        ),
        (
            "codex-cli 0.146.0",
            "unavailable",
            "version_unsupported",
            None,
        ),
        ("codex-cli unknown", "unknown", "version_unrecognized", None),
    ] {
        let path = fixture.printing("codex", version);
        let report = fixture.doctor_from(
            &fixture.base,
            "codex",
            true,
            Caller::Herdr,
            Some(&path),
            &[],
        );
        assert_eq!(report["scope"]["current_version"], version, "{report}");
        let selection = terminal_selection(&report);
        assert_eq!(
            selection["availability"], availability,
            "{version}: {selection}"
        );
        assert_eq!(selection["reason_code"], reason, "{version}: {selection}");
        assert_eq!(
            selection["evidence"]["terminal"].as_str(),
            terminal,
            "{version}: {selection}"
        );
        assert_eq!(selection["evidence"]["fallback"], FALLBACK, "{selection}");
    }
    // A probe that runs but exits unsuccessfully decides nothing, and --probe was given.
    let path = fixture.provider_path("codex-exit", "codex", "exit 3");
    let report = fixture.doctor_from(
        &fixture.base,
        "codex",
        true,
        Caller::Herdr,
        Some(&path),
        &[],
    );
    let selection = terminal_selection(&report);
    assert_eq!(selection["availability"], "unknown", "{selection}");
    assert_eq!(
        selection["reason_code"], "version_probe_failed",
        "{selection}"
    );
    assert!(selection["evidence"]["terminal"].is_null(), "{selection}");
    assert!(
        !selection["next_action"]
            .as_str()
            .unwrap()
            .contains("Add --probe"),
        "{selection}"
    );
}

// The probe that decides the reported Surface runs where `ask`'s does: in the directory doctor
// is invoked from. A shim that reports another version inside a project shows it.
#[cfg(unix)]
#[test]
fn doctor_reads_the_deciding_version_from_the_invoking_directory() {
    let fixture = Fixture::new();
    let project = fixture.base.join("pinned-project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join(".codex-pin"), b"0.168.0").unwrap();
    let path = fixture.provider_path(
        "codex-pinned",
        "codex",
        "if [ -f .codex-pin ]; then echo 'codex-cli 0.168.0'; else echo 'codex-cli 0.170.0'; fi",
    );
    for (directory, terminal) in [(&project, FALLBACK), (&fixture.base, "herdr")] {
        let report = fixture.doctor_from(directory, "codex", true, Caller::Herdr, Some(&path), &[]);
        let selection = terminal_selection(&report);
        assert_eq!(
            selection["evidence"]["terminal"],
            terminal,
            "{}: {selection}",
            directory.display()
        );
    }
    // The same for the environment: a shim that answers otherwise inside a managed session.
    let session_path = fixture.provider_path(
        "codex-session",
        "codex",
        "if [ -n \"$AGENT_BRIDGE_NATIVE_SESSION_DIR\" ]; then echo 'codex-cli 0.168.0'; else echo 'codex-cli 0.170.0'; fi",
    );
    let session = fixture.base.join("managed-session");
    let inside = [("AGENT_BRIDGE_NATIVE_SESSION_DIR", session.as_os_str())];
    let report = fixture.doctor_from(
        &fixture.base,
        "codex",
        true,
        Caller::Herdr,
        Some(&session_path),
        &inside,
    );
    assert_eq!(
        terminal_selection(&report)["evidence"]["terminal"],
        FALLBACK,
        "{report}"
    );
    let reply = refusal(fixture.ask_with(
        "codex",
        &["--title", "   "],
        &[inside[0], ("PATH", session_path.as_os_str())],
    ));
    assert_eq!(error(&reply), BLANK_TITLE, "{reply}");
    // And `ask` from that project takes the same fallback.
    let reply = refusal(
        fixture.run_from(
            &project,
            &[
                "ask",
                "codex",
                "--title",
                "   ",
                "--prompt",
                "must not be delivered",
                "--json",
            ],
            Caller::Herdr,
            &fixture
                .homes()
                .iter()
                .map(|(name, home)| (*name, home.as_os_str()))
                .chain([("PATH", path.as_os_str())])
                .collect::<Vec<_>>(),
        ),
    );
    assert_eq!(error(&reply), BLANK_TITLE, "{reply}");
}

// Outside hc the tmux row is probed before the provider's version, as in 0.4.49; inside hc,
// where the provider's version decides whether tmux is chosen, the version is read first.
#[cfg(target_os = "linux")]
#[test]
fn doctor_probes_the_provider_first_only_where_its_version_chooses_tmux() {
    let fixture = Fixture::new();
    let log = fixture.base.join("probes.log");
    let body = |line: &str, reply: &str| {
        format!("echo \"{line} $*\" >> \"$TABCLI_TEST_PROBE_LOG\"\necho '{reply}'")
    };
    let path = fixture.provider_path("probe-order", "codex", &body("codex", "codex-cli 0.168.0"));
    fixture.provider_path("probe-order", "tmux", &body("tmux", "tmux 3.6"));
    for (caller, first) in [
        (Caller::InheritedMarker, "tmux -V"),
        (Caller::Herdr, "codex --version"),
    ] {
        let _ = std::fs::remove_file(&log);
        let report = fixture.doctor_from(
            &fixture.base,
            "codex",
            true,
            caller,
            Some(&path),
            &[("TABCLI_TEST_PROBE_LOG", log.as_os_str())],
        );
        assert_eq!(terminal_selection(&report)["evidence"]["terminal"], "tmux");
        let order = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<_> = order
            .lines()
            .filter(|line| *line == "tmux -V" || *line == "codex --version")
            .collect();
        assert_eq!(lines.first(), Some(&first), "{caller:?}: {order}");
        assert!(lines.contains(&"tmux -V"), "{order}");
    }
}

// A core that holds the caller's terminal `t7` and names this test process as its program:
// the command under test runs as this process's child, so its caller check verifies. It
// answers `hello` and `terminal.get` only, and never creates a terminal.
#[cfg(unix)]
struct ScriptedCore {
    socket: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    server: Option<std::thread::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

#[cfg(unix)]
impl ScriptedCore {
    fn start() -> Self {
        Self::start_with(std::time::Duration::ZERO)
    }

    // A core that waits `delay` before it answers `terminal.get`.
    fn start_with(delay: std::time::Duration) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let directory = private_directory()
            .prefix("tlc")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("core.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let program = std::process::id();
        let server = {
            let (stop, connections) = (stop.clone(), connections.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connections.fetch_add(1, Ordering::SeqCst);
                            Self::serve(stream, program, delay);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            })
        };
        Self {
            socket,
            stop,
            connections,
            server: Some(server),
            _directory: directory,
        }
    }

    fn serve(stream: std::os::unix::net::UnixStream, program: u32, delay: std::time::Duration) {
        use std::io::{BufRead, Write};
        // An accepted socket can inherit the listener's non-blocking mode.
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        for line in std::io::BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut reply = match (
                request["method"].as_str(),
                request["params"]["terminal"].as_str(),
            ) {
                (Some("hello"), _) => serde_json::json!({"result": {
                    "generation": 1,
                    "version": "0.1.21",
                    "methods": ["terminal.run", "terminal.get", "terminal.kill", "terminal.rm",
                        "terminal.send", "terminal.read"],
                    "core": {"id": "core-launch-surface-test"},
                }}),
                (Some("terminal.get"), Some("t7")) => {
                    std::thread::sleep(delay);
                    serde_json::json!({"result": {
                        "address": "t7",
                        "state": "live",
                        "pid": program,
                        "argv": ["sh"],
                        "keep": false,
                        "created_ms": 1,
                    }})
                }
                (method, _) => serde_json::json!({"error": {
                    "code": -32601,
                    "message": format!("the test core does not answer {method:?}"),
                    "data": {"kind": "unknown_method"},
                }}),
            };
            reply["jsonrpc"] = "2.0".into();
            reply["id"] = request["id"].clone();
            if writeln!(writer, "{reply}").is_err() {
                break;
            }
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for ScriptedCore {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

// The caller check runs within the request's deadline: a core that answers after the deadline
// has passed refuses the launch as timed out, and nothing is created.
#[cfg(unix)]
#[test]
fn the_caller_check_ends_at_the_request_deadline() {
    let fixture = Fixture::new();
    let core = ScriptedCore::start_with(std::time::Duration::from_secs(3));
    let pi = fixture.printing("pi", "1.5.0");
    let started = std::time::Instant::now();
    let reply = refusal(fixture.ask_with(
        "pi",
        &["--timeout-secs", "1"],
        &[
            ("PATH", pi.as_os_str()),
            ("HC_SOCKET", core.socket.as_os_str()),
        ],
    ));
    assert!(error(&reply).ends_with(CALLER_REFUSAL), "{reply}");
    assert_eq!(reply["failure_kind"], "timed_out", "{reply}");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
}

// The Herdr configuration is read only as a regular file: one that is a FIFO (or anything
// else) is refused at once instead of waiting for a writer past the request's deadline.
#[cfg(unix)]
#[test]
fn a_herdr_configuration_that_is_not_a_file_refuses_without_waiting() {
    let fixture = Fixture::new();
    let config = fixture.base.join("config");
    std::fs::create_dir_all(config.join("herdr-core")).unwrap();
    let fifo = config.join("herdr-core").join("config.toml");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let pi = fixture.printing("pi", "1.5.0");
    let homes = fixture.homes();
    let mut environment: Vec<(&str, &OsStr)> = homes
        .iter()
        .map(|(name, path)| (*name, path.as_os_str()))
        .collect();
    environment.push(("PATH", pi.as_os_str()));
    environment.push(("XDG_CONFIG_HOME", config.as_os_str()));
    let project = fixture.base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut command = fixture.command_from(
        &fixture.base,
        &[
            "ask",
            "pi",
            "--workspace",
            project.to_str().unwrap(),
            "--prompt",
            "must not be delivered",
            "--json",
            "--timeout-secs",
            "1",
        ],
        Caller::Herdr,
        &environment,
    );
    command
        .env_remove("HC_SOCKET")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let started = std::time::Instant::now();
    let mut child = command.spawn().unwrap();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(6) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("ask still waited for the Herdr configuration after 6 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let reply = refusal(child.wait_with_output().unwrap());
    assert!(error(&reply).contains("is not a regular file"), "{reply}");
    assert!(error(&reply).ends_with(CALLER_REFUSAL), "{reply}");
    assert!(!fixture.state_root().exists());
}

// The probe that decides doctor's terminal row runs the launch's command under doctor's
// diagnostic limits: an output past the cap decides nothing, and a process it started does
// not outlive it.
#[cfg(unix)]
#[test]
fn doctor_runs_the_deciding_probe_under_its_diagnostic_limits() {
    let fixture = Fixture::new();
    let survivor = fixture.base.join("survivor");
    let path = fixture.provider_path(
        "codex-noisy",
        "codex",
        "if [ \"$1\" = --version ]; then\n  ( sleep 1; touch \"$TABCLI_TEST_SURVIVOR\" ) &\n  echo 'codex-cli 0.170.0'\n  printf '%66000s' ''\nfi",
    );
    let report = fixture.doctor_from(
        &fixture.base,
        "codex",
        true,
        Caller::Herdr,
        Some(&path),
        &[("TABCLI_TEST_SURVIVOR", survivor.as_os_str())],
    );
    let selection = terminal_selection(&report);
    assert_eq!(
        selection["reason_code"], "version_probe_failed",
        "{selection}"
    );
    assert!(selection["evidence"]["terminal"].is_null(), "{selection}");
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        !survivor.exists(),
        "a process the deciding probe started outlived doctor"
    );
}

#[cfg(unix)]
const CODEX_HERDR_FLOOR: &str = "codex on Herdr is unsupported: Herdr requires stable Codex 0.169.0 or newer with --no-daemon; version: ";

// Inside a verified hc terminal the automatic choice is Herdr where the provider, with its
// probed version, is admitted there, and this OS's fallback otherwise; only a launch that will
// use Herdr asks the core about its caller. An explicit `--terminal herdr` is refused as before.
// A blank title, refused right after the Surface is admitted and before anything exists, ends
// each case.
#[cfg(unix)]
#[test]
fn an_automatic_herdr_gives_way_to_the_fallback_where_the_provider_version_is_not_admitted() {
    let fixture = Fixture::new();
    let core = ScriptedCore::start();
    for (version, admitted) in [
        ("codex-cli 0.170.0", true),
        ("codex-cli 0.168.0", false),
        ("codex-cli 0.170.0-alpha.1", false),
    ] {
        let path = fixture.printing("codex", version);
        let socket = Some(core.socket.as_os_str());
        let before = core.connections();
        let reply = fixture.ask_blank_title("codex", None, &path, socket);
        assert_eq!(error(&reply), BLANK_TITLE, "{version}: {reply}");
        assert_eq!(
            core.connections(),
            before + usize::from(admitted),
            "{version}: the caller is verified only for a launch on Herdr"
        );
        let reply = fixture.ask_blank_title("codex", Some("herdr"), &path, socket);
        if admitted {
            assert_eq!(error(&reply), BLANK_TITLE, "{version}: {reply}");
        } else {
            assert_eq!(
                error(&reply),
                format!("{CODEX_HERDR_FLOOR}{version}"),
                "{version}: {reply}"
            );
            assert_eq!(reply["failure_kind"], "unsupported", "{reply}");
        }
    }
}
