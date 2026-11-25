//! The Surface a launch starts on, through the public commands, the same on every OS. `ask`
//! selects it and checks an automatic Herdr choice against its caller before it resolves the
//! workspace, and `reopen` before it looks up its source; the provider, with its probed
//! version, then decides between Herdr and the OS's own choice. `doctor` reports that decision
//! without the caller check. Most cases run the built binary as if inside a Herdr terminal (hc
//! gives every program `HC_TERMINAL_ID` and `TERM_PROGRAM=ghostty`) whose core is absent, so
//! the caller check fails; on macOS and Linux others answer it from a scripted core in this
//! process and run a provider stub that only prints a version. No real Herdr Core, state root,
//! provider or terminal is touched, and no test changes this process's environment.
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
        command
            .args(args)
            .current_dir(&self.base)
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
        let output = command.output().unwrap();
        assert!(
            !self.state_root().exists(),
            "{args:?}: a state root was created"
        );
        output
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

// Inside hc an automatic Herdr choice is checked against the caller on every OS, a provider
// not known yet (a reopen) included, except for a provider whose adapter refuses Herdr on
// this OS for every version (Codex on Windows): that one takes the OS's own choice at once.
fn caller_checked(provider: Option<&str>) -> bool {
    !(cfg!(windows) && provider == Some("codex"))
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
        // The automatic choice: an automatic Herdr is checked against the caller's terminal,
        // before the workspace (absent here) is resolved.
        let reply = fixture.ask(provider, None, Caller::Herdr);
        if caller_checked(Some(provider)) {
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
    // The source, and so its provider, is not known at selection: the caller is checked, on
    // Windows as on macOS and Linux.
    let reply = fixture.reopen(None);
    assert!(caller_checked(None));
    assert_caller_refused(&reply, "reopen");
    assert!(reply["gate"].is_null(), "{reply}");
    let reply = fixture.reopen(Some("herdr"));
    assert!(error(&reply).starts_with(NO_SOURCE), "{reply}");
}

// Without --probe, doctor reports what `ask` chooses before the version is known, and the
// fallback a version Herdr does not admit would get; it never runs the caller check.
#[test]
fn doctor_reports_the_selected_surface_without_the_caller_check() {
    let fixture = Fixture::new();
    for provider in PROVIDERS {
        let output = fixture.run(&["doctor", "--provider", provider, "--json"], Caller::Herdr);
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let selection = terminal_selection(&report);
        // The core is absent, so a caller check would have refused the automatic Herdr.
        assert_eq!(selection["availability"], "available", "{selection}");
        assert_eq!(selection["reason_code"], "terminal_selected", "{selection}");
        // Only Codex on Windows is refused on Herdr for every version.
        assert_eq!(
            selection["evidence"]["terminal"],
            if caller_checked(Some(provider)) {
                "herdr"
            } else {
                FALLBACK
            },
            "{provider}: {selection}"
        );
        assert_eq!(
            selection["evidence"]["fallback"], FALLBACK,
            "{provider}: {selection}"
        );
        // Outside hc there is no automatic Herdr choice, and so no fallback.
        let output = fixture.run(
            &["doctor", "--provider", provider, "--json"],
            Caller::InheritedMarker,
        );
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let selection = terminal_selection(&report);
        assert_ne!(selection["evidence"]["terminal"], "herdr", "{selection}");
        assert!(
            selection["evidence"].get("fallback").is_none(),
            "{selection}"
        );
    }
}

#[cfg(unix)]
const CODEX_HERDR_FLOOR: &str = "codex on Herdr is unsupported: Herdr requires stable Codex 0.169.0 or newer with --no-daemon; version: ";

#[cfg(unix)]
impl Fixture {
    // A provider executable that prints `version` for any arguments, alone in its own
    // directory: the PATH of a case that runs it, with only the system directories after it.
    fn provider_path(&self, command: &str, version: &str) -> std::ffi::OsString {
        use std::os::unix::fs::PermissionsExt;
        let directory = self.base.join(format!(
            "bin-{}",
            version.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join(command);
        std::fs::write(&executable, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::join_paths([
            directory.as_path(),
            std::path::Path::new("/usr/bin"),
            std::path::Path::new("/bin"),
        ])
        .unwrap()
    }

    // A home and a Codex home of this case's own, so that no real provider store is read.
    fn homes(&self) -> (PathBuf, PathBuf) {
        let home = self.base.join("home");
        let codex_home = home.join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        (home, codex_home)
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

// With --probe, doctor reports the Surface `ask` would choose with the version it read:
// inside hc a Codex that Herdr does not admit gets this OS's fallback, as `ask` gives it.
#[cfg(unix)]
#[test]
fn doctor_reports_the_surface_ask_chooses_for_the_probed_version() {
    let fixture = Fixture::new();
    let (home, codex_home) = fixture.homes();
    for (version, expected) in [
        ("codex-cli 0.168.0", FALLBACK),
        ("codex-cli 0.170.0", "herdr"),
        ("OpenAI Codex (v0.170.0)", "herdr"),
    ] {
        let path = fixture.provider_path("codex", version);
        let output = fixture.run_with(
            &["doctor", "--provider", "codex", "--probe", "--json"],
            Caller::Herdr,
            &[
                ("PATH", path.as_os_str()),
                ("HOME", home.as_os_str()),
                ("CODEX_HOME", codex_home.as_os_str()),
            ],
        );
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scope"]["current_version"], version, "{report}");
        let selection = terminal_selection(&report);
        assert_eq!(selection["reason_code"], "terminal_selected", "{selection}");
        assert_eq!(
            selection["evidence"]["terminal"], expected,
            "{version}: {selection}"
        );
        assert_eq!(
            selection["evidence"]["fallback"], FALLBACK,
            "{version}: {selection}"
        );
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
                            Self::serve(stream, program);
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

    fn serve(stream: std::os::unix::net::UnixStream, program: u32) {
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
                (Some("terminal.get"), Some("t7")) => serde_json::json!({"result": {
                    "address": "t7",
                    "state": "live",
                    "pid": program,
                    "argv": ["sh"],
                    "keep": false,
                    "created_ms": 1,
                }}),
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

// Inside a verified hc terminal the automatic choice is Herdr where the provider, with its
// probed version, is admitted there, and this OS's fallback otherwise; an explicit
// `--terminal herdr` is refused as before. A blank title, refused right after the Surface is
// admitted and before anything exists, ends each case.
#[cfg(unix)]
#[test]
fn an_automatic_herdr_gives_way_to_the_fallback_where_the_provider_version_is_not_admitted() {
    let fixture = Fixture::new();
    let core = ScriptedCore::start();
    let (home, codex_home) = fixture.homes();
    let project = fixture.base.join("project");
    std::fs::create_dir(&project).unwrap();
    let ask = |terminal: Option<&str>, path: &OsStr| {
        let mut args = vec![
            "ask",
            "codex",
            "--workspace",
            project.to_str().unwrap(),
            "--title",
            "   ",
            "--prompt",
            "must not be delivered",
            "--json",
        ];
        if let Some(terminal) = terminal {
            args.extend(["--terminal", terminal]);
        }
        refusal(fixture.run_with(
            &args,
            Caller::Herdr,
            &[
                ("PATH", path),
                ("HOME", home.as_os_str()),
                ("CODEX_HOME", codex_home.as_os_str()),
                ("HC_SOCKET", core.socket.as_os_str()),
            ],
        ))
    };
    for (version, admitted) in [
        ("codex-cli 0.170.0", true),
        ("codex-cli 0.168.0", false),
        ("codex-cli 0.170.0-alpha.1", false),
    ] {
        let path = fixture.provider_path("codex", version);
        let before = core.connections();
        let reply = ask(None, &path);
        assert_eq!(
            error(&reply),
            "--title cannot be empty",
            "{version}: {reply}"
        );
        assert_eq!(
            core.connections(),
            before + 1,
            "the caller was verified once"
        );
        let reply = ask(Some("herdr"), &path);
        if admitted {
            assert_eq!(
                error(&reply),
                "--title cannot be empty",
                "{version}: {reply}"
            );
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
