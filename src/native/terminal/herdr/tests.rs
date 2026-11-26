use super::*;
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};

fn test_birth(start: u64) -> HerdrHostBirth {
    #[cfg(target_os = "macos")]
    return HerdrHostBirth::Macos {
        host_start_seconds: 1_800_000_000,
        host_start_microseconds: start,
    };
    #[cfg(target_os = "linux")]
    return HerdrHostBirth::Linux {
        linux_birth: crate::native::terminal::LinuxBirth {
            boot_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
            starttime_ticks: start,
        },
    };
    #[cfg(windows)]
    return HerdrHostBirth::Windows {
        host_creation_time: 134_000_000_000_000_000 + start,
    };
}

// The recorded endpoint each platform addresses: a socket path, or a local named pipe.
#[cfg(unix)]
const SOCKET: &str = "/state/herdr-core/default/default.sock";
#[cfg(windows)]
const SOCKET: &str = r"\\.\pipe\herdr-core-tester-default.sock";
const DIRECTORY: &str = "/state/session-herdr1";
const SESSION: &str = "session-herdr1";

/// What a scripted call does instead of, or after, its normal effect.
enum Injected {
    /// The core never acts: the call fails with this error.
    Instead(CallError),
    /// The core acts, then the reply is lost with this error.
    After(CallError),
}

#[derive(Default)]
struct FakeState {
    core_id: String,
    running: bool,
    terminals: BTreeMap<String, Value>,
    next: u32,
    calls: Vec<(String, Value)>,
    inject: BTreeMap<&'static str, VecDeque<Injected>>,
    births: BTreeMap<u32, HerdrHostBirth>,
    parents: BTreeMap<u32, u32>,
    unreadable_parent: Option<u32>,
    unknown_parent: Option<u32>,
    caller: Option<String>,
    current_pid: u32,
    /// Terminals of configured remote machines, which hc's list also returns.
    remote: Vec<Value>,
    after_call: Option<(&'static str, Mutation)>,
    /// Each session directory the launch host's admission was asked about, with the number
    /// of calls made before it, and the refusal that admission gives, if any.
    admitted: Vec<(PathBuf, usize)>,
    refused_directory: Option<String>,
}

type Mutation = Box<dyn FnOnce(&mut BTreeMap<String, Value>)>;

#[derive(Clone)]
struct FakeCore(Rc<RefCell<FakeState>>);

impl FakeCore {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(FakeState {
            core_id: "core-a".into(),
            running: true,
            next: 7,
            current_pid: 900,
            ..FakeState::default()
        })))
    }
    fn inject(&self, method: &'static str, injected: Injected) {
        self.0
            .borrow_mut()
            .inject
            .entry(method)
            .or_default()
            .push_back(injected);
    }
    fn methods(&self) -> Vec<String> {
        self.0
            .borrow()
            .calls
            .iter()
            .map(|(method, _)| method.clone())
            .collect()
    }
    fn params(&self, method: &str) -> Vec<Value> {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|(called, _)| called == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
    fn clear_calls(&self) {
        self.0.borrow_mut().calls.clear();
    }
    fn terminal(&self, address: &str) -> Option<Value> {
        self.0.borrow().terminals.get(address).cloned()
    }
    fn set(&self, address: &str, field: &str, value: Value) {
        self.0.borrow_mut().terminals.get_mut(address).unwrap()[field] = value;
    }
    /// Another same-user client changes the registry after the next named call takes effect.
    fn after_call(
        &self,
        method: &'static str,
        change: impl FnOnce(&mut BTreeMap<String, Value>) + 'static,
    ) {
        self.0.borrow_mut().after_call = Some((method, Box::new(change)));
    }
}

struct FakeConnection(Rc<RefCell<FakeState>>);

fn refused(kind: &str) -> CallError {
    CallError::Refused {
        kind: kind.into(),
        message: format!("{kind} by fake"),
    }
}

impl FakeConnection {
    fn act(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        let mut state = self.0.borrow_mut();
        let terminal = params["terminal"].as_str().unwrap_or_default().to_owned();
        match method {
            "hello" => Ok(json!({
                "generation": 1, "version": "0.1.21", "framed": false, "features": [],
                "core": {"id": state.core_id, "name": "fake"},
                "methods": REQUIRED_METHODS,
            })),
            "terminal.run" => {
                let address = format!("t{}", state.next);
                state.next += 1;
                let pid = 4000 + state.next;
                state.births.insert(pid, test_birth(u64::from(pid)));
                let value = json!({
                    "address": address, "state": "live", "pid": pid, "name": params["name"],
                    "argv": params["argv"], "keep": params["keep"], "rm": params["rm"],
                    "created_ms": 1_794_000_000_000_u64 + u64::from(pid),
                    "labels": params["labels"], "cwd": "/", "screen": "exact",
                    "cols": 200, "rows": 120,
                });
                state.terminals.insert(address.clone(), value);
                Ok(json!({"address": address, "state": "live"}))
            }
            "terminal.get" => {
                // An id names that terminal forever; a name names the live terminal that
                // holds it, else the latest finished one (herdr-core `Registry::resolve`).
                let by_name = |live: bool| {
                    state.terminals.values().rev().find(|value| {
                        value["name"] == json!(terminal) && (value["state"] == "live") == live
                    })
                };
                match state.terminals.get(&terminal) {
                    Some(value) => Ok(value.clone()),
                    None => by_name(true)
                        .or_else(|| by_name(false))
                        .cloned()
                        .ok_or_else(|| refused("not_found")),
                }
            }
            "terminal.list" => {
                let wanted = params["labels"].as_object().cloned().unwrap_or_default();
                let terminals: Vec<Value> = state
                    .terminals
                    .values()
                    .filter(|value| {
                        wanted
                            .iter()
                            .all(|(key, label)| value["labels"][key] == *label)
                    })
                    .cloned()
                    .chain(state.remote.iter().cloned())
                    .collect();
                Ok(json!({"terminals": terminals, "hidden_finished": 0, "machines": []}))
            }
            "terminal.kill" => {
                let value = state
                    .terminals
                    .get_mut(&terminal)
                    .ok_or_else(|| refused("not_found"))?;
                value["state"] = json!("finished");
                Ok(value.clone())
            }
            "terminal.rm" => match state.terminals.get(&terminal) {
                None => Err(refused("not_found")),
                Some(value) if value["state"] == "live" => Err(refused("status_mismatch")),
                Some(_) => {
                    state.terminals.remove(&terminal);
                    Ok(json!({"removed": terminal}))
                }
            },
            "terminal.send" => match state.terminals.get(&terminal) {
                None => Err(refused("not_found")),
                Some(_) => Ok(json!({"turn": 1})),
            },
            "terminal.read" => {
                Ok(json!({"text": "fake screen", "complete": true, "how": "screen"}))
            }
            other => Err(refused(&format!("unknown {other}"))),
        }
    }
}

impl Connection for FakeConnection {
    fn call(&mut self, method: &str, params: Value, _: Instant) -> Result<Value, CallError> {
        self.0
            .borrow_mut()
            .calls
            .push((method.to_owned(), params.clone()));
        let injected = self
            .0
            .borrow_mut()
            .inject
            .get_mut(method)
            .and_then(VecDeque::pop_front);
        let unanswered = match injected {
            Some(Injected::Instead(error)) => return Err(error),
            Some(Injected::After(error)) => Some(error),
            None => None,
        };
        let result = self.act(method, &params);
        if result.is_ok() {
            let mut state = self.0.borrow_mut();
            if state
                .after_call
                .as_ref()
                .is_some_and(|(after, _)| *after == method)
            {
                let (_, change) = state.after_call.take().unwrap();
                change(&mut state.terminals);
            }
        }
        match unanswered {
            Some(error) => Err(error),
            None => result,
        }
    }
    // The scripted core runs as this process.
    fn core_account(&self) -> Result<HostAccount> {
        HostAccount::current()
    }
}

impl Core for FakeCore {
    fn endpoint(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(SOCKET))
    }
    fn connect(&self, socket: &Path, _: Instant) -> Result<Box<dyn Connection + '_>> {
        if !self.0.borrow().running {
            return Err(not_running(socket));
        }
        self.0
            .borrow_mut()
            .calls
            .push(("connect".into(), json!(socket)));
        Ok(Box::new(FakeConnection(self.0.clone())))
    }
    fn executable(&self) -> Result<PathBuf> {
        Ok(PathBuf::from("/opt/tabcli"))
    }
    fn caller_terminal(&self) -> Option<String> {
        self.0.borrow().caller.clone()
    }
    fn current_pid(&self) -> u32 {
        self.0.borrow().current_pid
    }
    fn parent_pid(&self, pid: u32) -> Result<Parent> {
        let state = self.0.borrow();
        if state.unreadable_parent == Some(pid) {
            bail!("process {pid} is unreadable");
        }
        if state.unknown_parent == Some(pid) {
            return Ok(Parent::Unknown(format!(
                "the parent of process {pid} ended"
            )));
        }
        Ok(state
            .parents
            .get(&pid)
            .map_or(Parent::Root, |parent| Parent::Process(*parent)))
    }
    fn process_start(&self, pid: u32) -> Result<Option<HerdrHostBirth>> {
        Ok(self.0.borrow().births.get(&pid).copied())
    }
    fn launch_nonce(&self) -> Result<String> {
        Ok("0123456789abcdef0123456789abcdef".into())
    }
    fn admit_launch_directory(&self, directory: &Path, _: Option<&HostAccount>) -> Result<()> {
        let mut state = self.0.borrow_mut();
        let made = state.calls.len();
        state.admitted.push((directory.to_owned(), made));
        match &state.refused_directory {
            Some(reason) => bail!("{reason}"),
            None => Ok(()),
        }
    }
    fn pause(&self, _: Instant) {}
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn try_open(core: impl Core, command: &str, directory: &Path) -> Result<TerminalSession> {
    Adapter::with_core(core).open(
        command,
        directory,
        later(),
        |session| {
            session.managed_session_id = Some(SESSION.into());
            Ok(())
        },
        || panic!("no unbind on success"),
    )
}

fn opened(core: &FakeCore, directory: &Path) -> TerminalSession {
    try_open(core.clone(), ". '/state/launch.sh'", directory).unwrap()
}

fn created(core: &FakeCore) -> TerminalSession {
    opened(core, Path::new(DIRECTORY))
}

fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[test]
fn windows_herdr_open_refuses_a_foreign_kind_without_contacting_the_core() {
    use crate::native::terminal::windows_dispatch::WindowsTerminalAdapter;

    let core = FakeCore::new();
    let result = Adapter::with_core(core.clone()).open_bound_tab(
        TerminalKind::WindowsConsole,
        "launch",
        Path::new(DIRECTORY),
        later(),
        Box::new(|_| Ok(())),
        Box::new(|| Ok(())),
    );
    assert!(result.is_err(), "the adapter accepted a foreign kind");
    assert!(core.methods().is_empty());
}

#[cfg(unix)]
#[test]
fn endpoint_follows_hc_and_refuses_what_hc_would_refuse() {
    use super::unix::{EndpointInputs, resolve_socket};
    let no_config = |_: &Path| -> Result<Option<String>> { Ok(None) };
    let base = EndpointInputs {
        home: Some("/Users/u".into()),
        ..EndpointInputs::default()
    };
    let socket = |inputs: &EndpointInputs| {
        resolve_socket(inputs, &no_config).map(|path| path.display().to_string())
    };
    assert_eq!(
        socket(&base).unwrap(),
        "/Users/u/.local/state/herdr-core/default/default.sock"
    );
    let mut inputs = base.clone();
    inputs.hc_session = Some(" smoke ".into());
    inputs.xdg_state_home = Some("/xdg/state".into());
    assert_eq!(
        socket(&inputs).unwrap(),
        "/xdg/state/herdr-core/smoke/smoke.sock"
    );
    inputs.xdg_runtime_dir = Some("/run/me".into());
    assert_eq!(socket(&inputs).unwrap(), "/run/me/herdr-core/smoke.sock");
    let mut inputs = base.clone();
    inputs.run_user = Some("/run/user/501".into());
    assert_eq!(
        socket(&inputs).unwrap(),
        "/run/user/501/herdr-core/default.sock"
    );
    // HC_SOCKET wins over the config, which wins over the default.
    let config = |path: &Path| -> Result<Option<String>> {
        assert_eq!(path, Path::new("/Users/u/.config/herdr-core/config.toml"));
        Ok(Some("/configured/core.sock".into()))
    };
    assert_eq!(
        resolve_socket(&base, &config).unwrap(),
        Path::new("/configured/core.sock")
    );
    let mut inputs = base.clone();
    inputs.hc_socket = Some("/explicit/core.sock".into());
    assert_eq!(
        resolve_socket(&inputs, &|_| panic!("HC_SOCKET is read first")).unwrap(),
        Path::new("/explicit/core.sock")
    );
    // An empty value is unset, as in hc.
    let mut inputs = base.clone();
    inputs.hc_socket = Some("".into());
    inputs.hc_session = Some("  ".into());
    assert_eq!(
        socket(&inputs).unwrap(),
        "/Users/u/.local/state/herdr-core/default/default.sock"
    );
    for (field, value, expected) in [
        (
            "session",
            "a/b",
            "HC_SESSION \"a/b\" cannot name a Herdr Core",
        ),
        (
            "session",
            "..",
            "HC_SESSION \"..\" cannot name a Herdr Core",
        ),
        ("socket", "relative.sock", "is not an absolute path"),
        (
            "socket",
            &format!("/{}", "x".repeat(120)),
            "longer than this system allows",
        ),
    ] {
        let mut inputs = base.clone();
        match field {
            "session" => inputs.hc_session = Some(value.into()),
            _ => inputs.hc_socket = Some(value.into()),
        }
        let error = error_text(&resolve_socket(&inputs, &no_config).unwrap_err());
        assert!(error.contains(expected), "{error}");
    }
    let broken = |_: &Path| -> Result<Option<String>> { bail!("cannot parse") };
    assert!(resolve_socket(&base, &broken).is_err());
    let homeless = EndpointInputs::default();
    assert!(
        error_text(&resolve_socket(&homeless, &no_config).unwrap_err()).contains("HOME is not set")
    );
}

#[test]
fn config_socket_reads_only_a_string_setting() {
    assert_eq!(config_socket("").unwrap(), None);
    assert_eq!(config_socket("notes = false").unwrap(), None);
    assert_eq!(
        config_socket("socket = \"/a.sock\"").unwrap().as_deref(),
        Some("/a.sock")
    );
    assert_eq!(config_socket("socket = \"\"").unwrap(), None);
    assert!(config_socket("socket = 1").is_err());
    assert!(config_socket("socket = ").is_err());
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        read_config_socket(&directory.path().join("missing.toml")).unwrap(),
        None
    );
    let unreadable = directory.path().join("config.toml");
    fs::create_dir(&unreadable).unwrap();
    assert!(
        error_text(&read_config_socket(&unreadable).unwrap_err())
            .contains("does not guess the core's socket")
    );
}

// A configuration linked to the null device reads as an empty one, as hc reads it: no
// `socket` setting, so the default socket is used.
#[cfg(unix)]
#[test]
fn a_configuration_linked_to_the_null_device_is_empty() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::os::unix::fs::symlink("/dev/null", &config).unwrap();
    assert_eq!(read_config_socket(&config).unwrap(), None);
}

#[test]
fn replies_and_hello_are_validated_field_by_field() {
    assert_eq!(
        parse_reply(br#"{"jsonrpc":"2.0","id":3,"result":{"a":1}}"#, 3).unwrap(),
        json!({"a": 1})
    );
    match parse_reply(
        br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"no","data":{"kind":"blocked"}}}"#,
        3,
    ) {
        Err(CallError::Refused { kind, message }) => {
            assert_eq!((kind.as_str(), message.as_str()), ("blocked", "no"));
        }
        other => panic!("{other:?}"),
    }
    for line in [
        &br#"{"jsonrpc":"2.0","id":4,"result":{}}"#[..],
        br#"{"jsonrpc":"1.0","id":3,"result":{}}"#,
        br#"{"jsonrpc":"2.0","id":3}"#,
        br#"{"jsonrpc":"2.0","id":3,"error":{"message":"no kind"}}"#,
        b"not json",
    ] {
        assert!(
            matches!(parse_reply(line, 3), Err(CallError::Unanswered(_))),
            "{}",
            String::from_utf8_lossy(line)
        );
    }
    let hello = json!({"generation": 1, "version": "0.1.21", "core": {"id": "c"},
        "methods": REQUIRED_METHODS, "features": []});
    assert_eq!(validate_hello(&hello).unwrap(), "c");
    for (field, value) in [
        ("generation", json!(2)),
        ("version", Value::Null),
        ("core", json!({"id": ""})),
        ("methods", json!(["terminal.run"])),
    ] {
        let mut changed = hello.clone();
        changed[field] = value;
        assert!(validate_hello(&changed).is_err(), "{field}");
    }
}

#[test]
fn terminals_are_local_and_fully_described() {
    for address in ["t1", "t7", "t1b", "t10"] {
        assert!(valid_address(address), "{address}");
    }
    for address in [
        "",
        "t",
        "7",
        "T7",
        "vps/t7",
        "t-1",
        "t7 ",
        &format!("t{}", "1".repeat(40)),
    ] {
        assert!(!valid_address(address), "{address}");
    }
    let terminal = json!({"address": "t7", "state": "live", "pid": 10, "argv": ["a"],
        "keep": false, "created_ms": 5, "labels": {LAUNCH_LABEL: "n"}});
    let observed = parse_terminal(&terminal).unwrap();
    assert_eq!(observed.launch.as_deref(), Some("n"));
    assert_eq!(observed.pid, Some(10));
    for (field, value) in [
        ("address", json!("vps/t7")),
        ("state", json!("pending")),
        ("pid", json!(-1)),
        ("pid", json!(0)),
        ("argv", Value::Null),
        ("argv", json!([1])),
        ("keep", json!("no")),
        ("created_ms", Value::Null),
    ] {
        let mut changed = terminal.clone();
        changed[field] = value;
        assert!(parse_terminal(&changed).is_err(), "{field}");
    }
}

#[test]
fn creation_uses_the_platform_parent_policy_for_a_verified_caller() {
    let core = FakeCore::new();
    core.0.borrow_mut().terminals.insert(
        "t3".into(),
        json!({"address": "t3", "state": "live", "pid": 300, "argv": ["shell"],
               "created_ms": 1, "labels": {}}),
    );
    core.0.borrow_mut().caller = Some("t3".into());
    core.0.borrow_mut().parents = BTreeMap::from([(900, 800), (800, 300)]);
    verify_caller(&core, later()).unwrap();
    core.clear_calls();

    created(&core);
    let run = &core.params("terminal.run")[0];
    if cfg!(windows) {
        assert_eq!(run["no_parent"], true);
        assert!(run.get("parent").is_none());
        assert!(run.get("outlive_parent").is_none());
        return;
    }
    assert_eq!(run["parent"], json!("t3"));
    assert_eq!(run["outlive_parent"], true);
    assert_eq!(run["no_notify"], true);
    assert_eq!(run["keep"], false);
    assert!(run.get("no_parent").is_none());
    assert!(run.get("caller").is_none());
    assert_eq!(
        &core.methods()[..4],
        ["connect", "hello", "terminal.get", "terminal.run"]
    );
    assert_eq!(core.params("terminal.get")[0], json!({"terminal": "t3"}));
}

#[test]
fn creation_never_parents_to_a_stale_or_remote_marker() {
    for (marker, state, pid) in [
        ("t3", "live", 700),     // A different local program.
        ("t3", "finished", 900), // This PID on a finished terminal.
        ("t9", "live", 900),     // No such local terminal.
        ("remote/t3", "live", 900),
    ] {
        let core = FakeCore::new();
        core.0.borrow_mut().caller = Some(marker.into());
        core.0.borrow_mut().terminals.insert(
            "t3".into(),
            json!({"address": "t3", "state": state, "pid": pid, "argv": ["shell"],
                   "created_ms": 1}),
        );
        created(&core);
        let run = &core.params("terminal.run")[0];
        assert_eq!(run["no_parent"], true);
        assert!(run.get("parent").is_none());
        assert!(run.get("outlive_parent").is_none());
        assert!(
            core.params("terminal.get")
                .iter()
                .all(|p| p["terminal"] != "remote/t3")
        );
    }
}

#[cfg(unix)]
#[test]
fn explicit_creation_survives_a_caller_check_that_refuses_automatic_selection() {
    for unreadable_process in [true, false] {
        let core = FakeCore::new();
        core.0.borrow_mut().caller = Some("t3".into());
        core.0.borrow_mut().terminals.insert(
            "t3".into(),
            json!({"address": "t3", "state": "live", "pid": 300, "argv": ["shell"], "created_ms": 1}),
        );
        let arrange = || {
            if unreadable_process {
                core.0.borrow_mut().unreadable_parent = Some(900);
            } else {
                core.inject("terminal.get", Injected::Instead(refused("unavailable")));
            }
        };
        arrange();
        assert!(verify_caller(&core, later()).is_err());
        assert!(core.params("terminal.run").is_empty());
        arrange();
        core.clear_calls();
        created(&core);
        assert_eq!(
            &core.methods()[..6],
            [
                "connect",
                "hello",
                "terminal.get",
                "connect",
                "hello",
                "terminal.run"
            ]
        );
        let calls = core.params("terminal.run");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["no_parent"], true);
        assert!(calls[0].get("parent").is_none());
    }
}

#[test]
fn creation_outside_herdr_runs_the_launch_host_without_a_parent() {
    let core = FakeCore::new();
    let session = created(&core);
    assert_eq!(session.kind, TerminalKind::Herdr);
    assert_eq!(session.id, "t7");
    let recorded = session.herdr_core().unwrap();
    assert_eq!(recorded.socket, SOCKET);
    assert_eq!(recorded.core_id, "core-a");
    let terminal = core.terminal("t7").unwrap();
    assert_eq!(recorded.host_pid, terminal["pid"].as_u64().unwrap() as u32);
    assert_eq!(
        recorded.created_ms,
        terminal["created_ms"].as_u64().unwrap()
    );
    assert_eq!(recorded.birth, core.0.borrow().births[&recorded.host_pid]);
    let run = &core.params("terminal.run")[0];
    assert_eq!(
        run,
        &json!({
            "argv": ["/opt/tabcli", HOST_COMMAND, DIRECTORY, "--launch-command",
                     ". '/state/launch.sh'"],
            "name": "tabcli-0123456789abcdef01234567",
            "labels": {SESSION_LABEL: SESSION, LAUNCH_LABEL: "0123456789abcdef0123456789abcdef"},
            "no_parent": true, "no_notify": true, "keep": false, "rm": false,
        })
    );
    // Every call names its terminal only; none names a caller, and hello comes first.
    assert_eq!(
        core.methods(),
        [
            "connect",
            "hello",
            "terminal.run",
            "terminal.get",
            "terminal.get"
        ]
    );
    for (_, params) in core.0.borrow().calls.iter() {
        assert!(params.get("caller").is_none());
    }
}

#[test]
fn creation_needs_a_running_core_and_never_starts_one() {
    let core = FakeCore::new();
    core.0.borrow_mut().running = false;
    let error = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(error.contains("no Herdr Core is running"), "{error}");
    assert!(
        error.contains("does not start or restart a core"),
        "{error}"
    );
    assert!(core.methods().is_empty());
}

// The launch host is the core's program: a session directory that its admission would refuse,
// for the account of the process that serves the connection, is refused on that connection
// before `terminal.run`, so no terminal exists; an admitted directory goes on to the run.
#[test]
fn creation_refuses_a_directory_the_cores_launch_host_would_refuse() {
    let core = FakeCore::new();
    core.0.borrow_mut().refused_directory = Some(
        "the Herdr launch host would refuse the session directory /state/session-herdr1: Herdr launch directory is not private to the current user".into(),
    );
    let error = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert_eq!(
        error,
        "the Herdr terminal was not created: the Herdr launch host would refuse the session directory /state/session-herdr1: Herdr launch directory is not private to the current user"
    );
    assert_eq!(core.methods(), ["connect", "hello"]);
    assert_eq!(core.0.borrow().admitted, [(PathBuf::from(DIRECTORY), 2)]);
    assert!(core.0.borrow().terminals.is_empty());
    let core = FakeCore::new();
    created(&core);
    assert_eq!(core.0.borrow().admitted, [(PathBuf::from(DIRECTORY), 2)]);
    assert_eq!(core.methods()[..3], ["connect", "hello", "terminal.run"]);
}

// When the caller check fails on the first connection, the terminal is created on a new one.
// The directory is admitted on that connection, for the account that serves it, right before
// `terminal.run`, not on the connection that was given up.
#[cfg(unix)]
#[test]
fn creation_admits_the_directory_on_the_connection_that_runs_the_terminal() {
    let core = FakeCore::new();
    core.0.borrow_mut().caller = Some("t3".into());
    core.0.borrow_mut().terminals.insert(
        "t3".into(),
        json!({"address": "t3", "state": "live", "pid": 300, "argv": ["shell"], "created_ms": 1}),
    );
    core.inject("terminal.get", Injected::Instead(refused("unavailable")));
    created(&core);
    let methods = core.methods();
    let run = methods
        .iter()
        .position(|method| method == "terminal.run")
        .unwrap();
    assert_eq!(
        methods[..run],
        ["connect", "hello", "terminal.get", "connect", "hello"]
    );
    assert_eq!(core.0.borrow().admitted, [(PathBuf::from(DIRECTORY), run)]);
}

// The launcher's probe before startup is confirmed reads the exact terminal this launch
// created, with the presence proof: live is present, the core's `not_found` is absence, and a
// finished terminal is the end of its program, with its exit code when the core gives one. A
// finished terminal with another launch label, one set to keep, or one that is not this
// launch's is an inspection error, which ends nothing.
#[test]
fn the_launch_probe_reports_a_finished_launch_terminal() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let probe = || adapter.during_launch(Path::new(DIRECTORY), &session, Duration::from_secs(5));
    assert_eq!(
        probe().unwrap(),
        crate::native::terminal::LaunchSurface::Present
    );
    core.set("t7", "state", json!("finished"));
    assert_eq!(
        probe().unwrap(),
        crate::native::terminal::LaunchSurface::Finished("Herdr terminal t7 finished".into())
    );
    core.set("t7", "exit_code", json!(3));
    assert_eq!(
        probe().unwrap(),
        crate::native::terminal::LaunchSurface::Finished(
            "Herdr terminal t7 exited with code 3".into()
        )
    );
    let labels = core.terminal("t7").unwrap()["labels"].clone();
    core.set(
        "t7",
        "labels",
        json!({SESSION_LABEL: SESSION, LAUNCH_LABEL: "another"}),
    );
    assert!(error_text(&probe().unwrap_err()).contains("different launch label"));
    core.set("t7", "labels", labels);
    core.set("t7", "keep", json!(true));
    assert!(error_text(&probe().unwrap_err()).contains("set to keep"));
    core.set("t7", "keep", json!(false));
    core.set("t7", "created_ms", json!(1));
    assert!(error_text(&probe().unwrap_err()).contains("is not the terminal this session created"));
    core.0.borrow_mut().terminals.clear();
    assert_eq!(
        probe().unwrap(),
        crate::native::terminal::LaunchSurface::Absent
    );
}

#[test]
fn a_refused_run_creates_nothing_and_is_not_recovered() {
    let core = FakeCore::new();
    core.inject("terminal.run", Injected::Instead(refused("spawn_failed")));
    let error = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(error.contains("was not created"), "{error}");
    assert!(error.contains("spawn_failed"), "{error}");
    assert!(!core.methods().contains(&"terminal.list".to_owned()));
}

// The reply to `terminal.run` is lost after the core created the terminal: the exact
// terminal is read by this launch's name on the same core, checked and removed.
#[test]
fn a_lost_run_reply_removes_only_the_exact_labelled_terminal() {
    let core = FakeCore::new();
    // Another terminal of the user and another launch of TAB stay untouched.
    core.0.borrow_mut().terminals.insert(
        "t1".into(),
        json!({"address": "t1", "state": "live", "pid": 11, "argv": ["zsh"],
               "created_ms": 1, "labels": {}}),
    );
    core.0.borrow_mut().terminals.insert(
        "t2".into(),
        json!({"address": "t2", "state": "live", "pid": 12, "argv": ["/opt/tabcli"],
               "created_ms": 1, "labels": {LAUNCH_LABEL: "another"}}),
    );
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let error = try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err();
    assert!(error.downcast_ref::<RetainedLaunchSurface>().is_none());
    let text = error_text(&error);
    assert!(
        text.contains("t7 this launch created was removed"),
        "{text}"
    );
    assert!(text.contains("nothing was started"), "{text}");
    assert!(core.terminal("t7").is_none());
    assert!(core.terminal("t1").is_some() && core.terminal("t2").is_some());
    assert_eq!(core.params("terminal.kill"), [json!({"terminal": "t7"})]);
    assert!(core.params("terminal.list").is_empty());
    assert_eq!(
        core.params("terminal.get")[0],
        json!({"terminal": "tabcli-0123456789abcdef01234567"})
    );
}

#[test]
fn a_lost_run_reply_without_its_terminal_reports_nothing_created() {
    let core = FakeCore::new();
    core.inject(
        "terminal.run",
        Injected::Instead(CallError::Unanswered(anyhow!(
            "the Herdr Core closed the connection"
        ))),
    );
    let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(
        text.contains("none was created or it is already gone"),
        "{text}"
    );
    assert!(core.params("terminal.kill").is_empty());
}

#[test]
fn a_lost_run_reply_that_cannot_be_looked_up_keeps_the_fence_in_its_report() {
    let core = FakeCore::new();
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    core.inject(
        "terminal.get",
        Injected::Instead(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(text.contains("creation is uncertain"), "{text}");
    assert!(text.contains("starts nothing without a binding"), "{text}");
    assert!(
        text.contains("named tabcli-0123456789abcdef01234567"),
        "{text}"
    );
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.terminal("t7").is_some());
}

// The terminal was found by its name but could not be removed: its complete identity is retained, so
// that the launcher records the handle and a later close finishes it.
#[test]
fn a_found_terminal_that_cannot_be_removed_is_retained_with_its_identity() {
    let core = FakeCore::new();
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    core.inject("terminal.kill", Injected::Instead(refused("internal")));
    let error = try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err();
    let retained = error
        .downcast_ref::<RetainedLaunchSurface>()
        .expect("retained handle");
    assert_eq!(retained.surface.id, "t7");
    let recorded = retained.surface.herdr_core().unwrap();
    assert_eq!(recorded.core_id, "core-a");
    assert_eq!(
        recorded.host_pid,
        core.terminal("t7").unwrap()["pid"].as_u64().unwrap() as u32
    );
    // A later close of the retained handle finishes the removal.
    let mut handle = retained.surface.clone();
    handle.managed_session_id = Some(SESSION.into());
    assert_eq!(
        Adapter::with_core(core.clone())
            .close(Path::new(DIRECTORY), &handle, later())
            .unwrap(),
        CloseOutcome::Closed
    );
    assert!(core.terminal("t7").is_none());
}

// The run reply arrives, but the terminal cannot be confirmed as the requested one: it is
// removed by its address and argv.
#[test]
fn an_unconfirmed_creation_is_removed_by_its_address() {
    let core = FakeCore::new();
    core.inject("terminal.get", Injected::Instead(refused("internal")));
    let error = try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err();
    let text = error_text(&error);
    assert!(
        text.contains("t7 this launch created was removed"),
        "{text}"
    );
    assert!(core.terminal("t7").is_none());
    // The program ended before its birth was read: no identity, so nothing is retained,
    // but the exact terminal is still removed.
    let core = FakeCore::new();
    core.0.borrow_mut().births.clear();
    let state = core.0.clone();
    struct Forgetful(FakeCore);
    impl Core for Forgetful {
        fn endpoint(&self) -> Result<PathBuf> {
            self.0.endpoint()
        }
        fn connect(&self, socket: &Path, deadline: Instant) -> Result<Box<dyn Connection + '_>> {
            self.0.connect(socket, deadline)
        }
        fn executable(&self) -> Result<PathBuf> {
            self.0.executable()
        }
        fn caller_terminal(&self) -> Option<String> {
            None
        }
        fn current_pid(&self) -> u32 {
            1
        }
        fn parent_pid(&self, _: u32) -> Result<Parent> {
            Ok(Parent::Root)
        }
        fn process_start(&self, _: u32) -> Result<Option<HerdrHostBirth>> {
            Ok(None)
        }
        fn launch_nonce(&self) -> Result<String> {
            self.0.launch_nonce()
        }
        fn admit_launch_directory(
            &self,
            directory: &Path,
            account: Option<&HostAccount>,
        ) -> Result<()> {
            self.0.admit_launch_directory(directory, account)
        }
        fn pause(&self, _: Instant) {}
    }
    let error = try_open(Forgetful(FakeCore(state)), "launch", Path::new(DIRECTORY)).unwrap_err();
    assert!(error.downcast_ref::<RetainedLaunchSurface>().is_none());
    assert!(error_text(&error).contains("already ended"));
    assert!(core.terminal("t7").is_none());
}

#[test]
fn a_changed_or_unreachable_core_is_unknown_never_absent() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let directory = Path::new(DIRECTORY);
    core.0.borrow_mut().core_id = "core-b".into();
    core.clear_calls();
    for error in [
        adapter
            .present(directory, &session, Duration::from_secs(1))
            .unwrap_err(),
        adapter.close(directory, &session, later()).unwrap_err(),
        adapter.verify(&session, None).unwrap_err(),
        adapter.screen(&session, later()).unwrap_err(),
    ] {
        let text = error_text(&error);
        assert!(
            text.contains("a different Herdr Core (id core-b)"),
            "{text}"
        );
        assert!(text.contains("recorded core core-a"), "{text}");
    }
    // Only identity was read: nothing was asked of any terminal.
    assert!(
        core.methods()
            .iter()
            .all(|method| method == "connect" || method == "hello")
    );
    core.0.borrow_mut().core_id = "core-a".into();
    core.0.borrow_mut().running = false;
    let text = error_text(
        &adapter
            .present(directory, &session, Duration::from_secs(1))
            .unwrap_err(),
    );
    assert!(text.contains("no Herdr Core is running"), "{text}");
}

#[test]
fn a_removed_terminal_on_its_own_core_is_absent() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let directory = Path::new(DIRECTORY);
    core.0.borrow_mut().terminals.remove("t7");
    assert!(
        !adapter
            .present(directory, &session, Duration::from_secs(1))
            .unwrap()
    );
    assert_eq!(
        adapter.close(directory, &session, later()).unwrap(),
        CloseOutcome::Missing
    );
    assert!(adapter.verify(&session, None).is_err());
}

#[test]
fn close_kills_then_removes_and_repeats_idempotently() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let directory = Path::new(DIRECTORY);
    assert!(
        adapter
            .present(directory, &session, Duration::from_secs(1))
            .unwrap()
    );
    core.clear_calls();
    assert_eq!(
        adapter.close(directory, &session, later()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.kill"), [json!({"terminal": "t7"})]);
    assert_eq!(core.params("terminal.rm"), [json!({"terminal": "t7"})]);
    assert!(core.terminal("t7").is_none());
    core.clear_calls();
    assert_eq!(
        adapter.close(directory, &session, later()).unwrap(),
        CloseOutcome::Missing
    );
    assert!(core.params("terminal.kill").is_empty());
}

#[test]
fn ordinary_close_removes_a_finished_terminal_with_a_changed_launch_label() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.set("t7", "state", json!("finished"));
    let mut labels = core.terminal("t7").unwrap()["labels"].clone();
    labels[LAUNCH_LABEL] = json!("fedcba9876543210fedcba9876543210");
    core.set("t7", "labels", labels);
    core.clear_calls();
    assert_eq!(
        adapter
            .close(Path::new(DIRECTORY), &session, later())
            .unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.rm"), [json!({"terminal": "t7"})]);
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.terminal("t7").is_none());
}

// A close that ended the program but failed to remove the terminal is finished by the next
// close, which only removes; one whose kill reply was lost is not killed twice blindly.
#[test]
fn an_interrupted_close_is_finished_by_the_next_one() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let directory = Path::new(DIRECTORY);
    core.inject("terminal.rm", Injected::Instead(refused("internal")));
    assert!(adapter.close(directory, &session, later()).is_err());
    assert_eq!(core.terminal("t7").unwrap()["state"], "finished");
    core.clear_calls();
    assert_eq!(
        adapter.close(directory, &session, later()).unwrap(),
        CloseOutcome::Closed
    );
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.terminal("t7").is_none());

    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.inject(
        "terminal.kill",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&adapter.close(directory, &session, later()).unwrap_err());
    assert!(text.contains("sent but not answered"), "{text}");
    assert_eq!(
        adapter.close(directory, &session, later()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.kill").len(), 1);
}

// The Windows Repair policy and adapter lookup are portable. Only the owner's OS
// observation and the core connection are scripted here.
#[test]
fn windows_repair_closes_through_the_core_and_resumes_after_removal_fails() {
    use crate::native::process_identity::{Gone, ProcessObservation};
    use crate::native::session::close::repair_dead_owner_windows_with;
    use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
    use crate::native::session::test_support::lifecycle::write_owned_terminal_state;
    use crate::native::session::{CoreRecord, SessionState, Store};
    use crate::native::terminal::windows_dispatch::Dispatch;

    for (state, binding, initially_finished) in [
        ("working", Some(SESSION), false),
        ("exited", Some("session-foreign"), true),
        ("failed", None, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(SESSION);
        fs::create_dir(&directory).unwrap();
        write_owned_terminal_state(&directory, state, std::process::id());
        let store = Store::open_unchecked(&directory);
        let mut owner = store.owner().unwrap();
        owner.managed_session_id = binding.map(str::to_owned);
        store.write_owner(&owner).unwrap();
        let core = FakeCore::new();
        let surface = opened(&core, &directory);
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        let handle = store.record(CoreRecord::Terminal).bytes().unwrap();
        let claim = store.record(CoreRecord::TurnClaim).bytes().unwrap();
        if initially_finished {
            core.set("t7", "state", json!("finished"));
            core.set("t7", "labels", json!({}));
            // Start with a handle claimed by an interrupted Close.
            store.claim_terminal_handle().unwrap().unwrap();
        }
        core.clear_calls();
        core.inject("terminal.rm", Injected::Instead(refused("internal")));
        let console = FakeCore::new();
        let adapters = Dispatch::new(
            Adapter::with_core(console.clone()),
            Adapter::with_core(core.clone()),
        );
        let repair = || {
            let claimed_directory = directory.clone();
            let expected_handle = handle.clone();
            core.after_call("hello", move |_| {
                let store = Store::open_unchecked(&claimed_directory);
                assert_eq!(
                    store.record(CoreRecord::TerminalClosing).bytes().unwrap(),
                    expected_handle
                );
                assert!(
                    store
                        .record(CoreRecord::Terminal)
                        .bytes()
                        .unwrap()
                        .is_none()
                );
            });
            repair_dead_owner_windows_with(
                &store,
                |recorded| {
                    assert_eq!(recorded.managed_session_id.as_deref(), binding);
                    OwnerAttestation {
                        process: ProcessObservation::Gone(if initially_finished {
                            Gone::Reused("different birth".into())
                        } else {
                            Gone::Exited
                        }),
                        control: ControlAttestation::NotRecorded,
                        pid_alive: Some(initially_finished),
                    }
                },
                &adapters,
                later(),
            )
        };

        let text = error_text(&repair().unwrap_err());
        assert!(text.contains("its handle is retained"), "{text}");
        assert_eq!(core.terminal("t7").unwrap()["state"], "finished");
        assert_eq!(store.status().unwrap().state.as_str(), state);
        assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), handle);
        assert_eq!(store.record(CoreRecord::TurnClaim).bytes().unwrap(), claim);
        assert!(store.closed_if_present().unwrap().is_none());

        assert!(repair().unwrap());
        assert_eq!(
            core.params("terminal.kill").len(),
            usize::from(!initially_finished)
        );
        assert_eq!(
            core.params("terminal.rm"),
            [json!({"terminal": "t7"}), json!({"terminal": "t7"})]
        );
        assert!(core.terminal("t7").is_none());
        assert!(console.methods().is_empty());
        assert!(
            store
                .record(CoreRecord::Terminal)
                .bytes()
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .record(CoreRecord::TerminalClosing)
                .bytes()
                .unwrap()
                .is_none()
        );
        assert!(store.record(CoreRecord::TerminalClosed).present().unwrap());
        assert!(
            store
                .record(CoreRecord::TurnClaim)
                .bytes()
                .unwrap()
                .is_none()
        );
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        core.clear_calls();
        assert!(!repair().unwrap());
        assert!(core.methods().is_empty());
    }
}

#[test]
fn a_terminal_that_is_no_longer_the_created_one_is_never_closed() {
    for (field, value, expected) in [
        ("keep", json!(true), "set to keep"),
        (
            "argv",
            json!(["/bin/zsh"]),
            "not the terminal this session created",
        ),
        ("pid", json!(4242), "not the terminal this session created"),
        (
            "created_ms",
            json!(1),
            "not the terminal this session created",
        ),
    ] {
        let core = FakeCore::new();
        let session = created(&core);
        let adapter = Adapter::with_core(core.clone());
        core.set("t7", field, value);
        core.clear_calls();
        let text = error_text(
            &adapter
                .close(Path::new(DIRECTORY), &session, later())
                .unwrap_err(),
        );
        assert!(text.contains(expected), "{field}: {text}");
        assert!(core.params("terminal.kill").is_empty(), "{field}");
        assert!(core.params("terminal.rm").is_empty(), "{field}");
    }
    // A program whose birth changed is another incarnation behind the recorded PID.
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let pid = session.herdr_core().unwrap().host_pid;
    core.0.borrow_mut().births.insert(pid, test_birth(1));
    let text = error_text(&adapter.screen(&session, later()).unwrap_err());
    assert!(text.contains("another process"), "{text}");
    // Another session's directory in the argv is another session's terminal.
    let core = FakeCore::new();
    let mut session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let mut remote = session.clone();
    session.managed_session_id = Some("session-other".into());
    assert!(adapter.screen(&session, later()).is_err());
    remote.id = "vps/t7".into();
    assert!(attach_command(&remote).is_none());
    core.clear_calls();
    assert!(
        adapter
            .close(Path::new(DIRECTORY), &remote, later())
            .is_err()
    );
    assert!(core.methods().is_empty());
}

#[test]
fn recorded_operations_require_their_own_directory_and_process_evidence() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    let directory = Path::new(DIRECTORY);
    let timeout = Duration::from_secs(1);

    // Live operations have the managed Session id. Presence and close have the full path.
    let mut argv = core.terminal("t7").unwrap()["argv"].clone();
    argv[2] = json!("/another-root/session-herdr1");
    core.set("t7", "argv", argv.clone());
    assert!(adapter.verify(&session, None).is_ok());
    assert!(adapter.present(directory, &session, timeout).is_err());
    assert!(adapter.close(directory, &session, later()).is_err());
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.params("terminal.rm").is_empty());

    argv[2] = json!(DIRECTORY);
    core.set("t7", "argv", argv);
    core.0.borrow_mut().births.clear();
    assert!(adapter.present(directory, &session, timeout).unwrap());
    let error = adapter.verify(&session, None).unwrap_err();
    assert!(error_text(&error).contains("program of Herdr terminal t7 has ended"));
    core.set("t7", "state", json!("finished"));
    assert!(adapter.present(directory, &session, timeout).unwrap());
    let error = adapter.verify(&session, None).unwrap_err();
    assert!(error_text(&error).contains("Herdr terminal t7 has finished"));
}

#[test]
fn screen_reads_ask_for_the_visible_screen_only() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.clear_calls();
    assert_eq!(adapter.screen(&session, later()).unwrap(), "fake screen");
    assert_eq!(
        core.params("terminal.read"),
        [json!({"terminal": "t7", "lines": 0})]
    );
    core.set("t7", "state", json!("finished"));
    assert!(adapter.screen(&session, later()).is_err());
}

#[test]
fn sends_classify_refusals_before_input_apart_from_uncertain_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let prompt = directory.path().join("prompt");
    fs::write(&prompt, "line one\nline two").unwrap();
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.clear_calls();
    adapter.send(&session, &prompt, later()).unwrap();
    assert_eq!(
        core.params("terminal.send"),
        [json!({"terminal": "t7", "text": "line one\nline two"})]
    );
    for (injected, uncertain) in [
        (Injected::Instead(refused("blocked")), false),
        (Injected::Instead(refused("status_mismatch")), false),
        (Injected::Instead(refused("finished")), false),
        (
            Injected::Instead(CallError::NotSent(anyhow!("cannot write"))),
            false,
        ),
        (Injected::Instead(refused("unavailable")), true),
        (Injected::Instead(refused("cancelled")), true),
        (
            Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
            true,
        ),
    ] {
        core.inject("terminal.send", injected);
        let failure = adapter.send(&session, &prompt, later()).unwrap_err();
        assert_eq!(
            failure.delivery_may_have_occurred(),
            uncertain,
            "{:#}",
            failure.error()
        );
    }
    // Nothing is sent to a terminal that is no longer the session's, or that has ended.
    core.set("t7", "state", json!("finished"));
    core.clear_calls();
    let failure = adapter.send(&session, &prompt, later()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert!(core.params("terminal.send").is_empty());
    let large = directory.path().join("large");
    fs::write(&large, vec![b'a'; MAX_SEND_TEXT + 1]).unwrap();
    let failure = adapter.send(&session, &large, later()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    let invalid = directory.path().join("invalid");
    fs::write(&invalid, [0xff, 0xfe]).unwrap();
    assert!(
        !adapter
            .send(&session, &invalid, later())
            .unwrap_err()
            .delivery_may_have_occurred()
    );
}

#[test]
fn a_finished_kept_terminal_reports_the_recovery_its_core_accepts() {
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.set("t7", "state", json!("finished"));
    core.set("t7", "keep", json!(true));
    core.clear_calls();
    let error = adapter
        .present(Path::new(DIRECTORY), &session, Duration::from_secs(1))
        .unwrap_err();
    let message = error_text(&error);
    for command in ["get", "rm"] {
        let expected = if cfg!(windows) {
            format!(
                "$env:HC_SOCKET = '{SOCKET}'; try {{ hc {command} t7 }} finally {{ $env:HC_SOCKET = $prior }}"
            )
        } else {
            format!("HC_SOCKET='{SOCKET}' hc {command} t7")
        };
        assert!(message.contains(&expected), "{message}");
    }
    assert!(!message.contains("--off"), "{message}");
    assert!(core.params("terminal.rm").is_empty());
}

#[test]
fn the_launch_host_binds_only_to_its_own_terminal_and_program() {
    let core = FakeCore::new();
    let session = created(&core);
    let pid = session.herdr_core().unwrap().host_pid;
    assert!(binding_names_host(&session, Some("t7"), pid));
    assert!(!binding_names_host(&session, Some("t8"), pid));
    assert!(!binding_names_host(&session, None, pid));
    assert!(!binding_names_host(&session, Some("t7"), pid + 1));
    let mut other = session.clone();
    other.kind = TerminalKind::Ghostty;
    assert!(!binding_names_host(&other, Some("t7"), pid));
    let bare = TerminalSession::new(TerminalKind::Herdr, "t7".into());
    assert!(!binding_names_host(&bare, Some("t7"), pid));
}

#[test]
fn an_automatic_choice_proves_the_caller_runs_inside_its_terminal() {
    let core = FakeCore::new();
    core.0.borrow_mut().terminals.insert(
        "t3".into(),
        json!({"address": "t3", "state": "live", "pid": 300, "argv": ["claude"],
               "created_ms": 1, "labels": {}}),
    );
    core.0.borrow_mut().caller = Some("t3".into());
    // 900 (this process) -> 800 (a tool shell) -> 300 (the terminal's program).
    core.0.borrow_mut().parents = BTreeMap::from([(900, 800), (800, 300), (300, 200)]);
    verify_caller(&core, later()).unwrap();
    // Only identity and the caller's own terminal were read.
    assert_eq!(core.methods(), ["connect", "hello", "terminal.get"]);
    for (caller, parents, expected) in [
        (None, vec![(900, 800)], "HC_TERMINAL_ID is not set"),
        (
            Some("vps/t3"),
            vec![(900, 800)],
            "is not a local Herdr terminal",
        ),
        (Some("t9"), vec![(900, 800)], "does not hold"),
        (
            Some("t3"),
            vec![(900, 800), (800, 1)],
            "does not run inside its program",
        ),
    ] {
        core.0.borrow_mut().caller = caller.map(str::to_owned);
        core.0.borrow_mut().parents = parents.into_iter().collect();
        let text = error_text(&verify_caller(&core, later()).unwrap_err());
        assert!(text.contains(expected), "{caller:?}: {text}");
        assert!(text.contains("no other terminal was opened"), "{text}");
    }
    core.0.borrow_mut().caller = Some("t3".into());
    core.set("t3", "state", json!("finished"));
    assert!(error_text(&verify_caller(&core, later()).unwrap_err()).contains("no longer running"));
    core.0.borrow_mut().running = false;
    assert!(
        error_text(&verify_caller(&core, later()).unwrap_err())
            .contains("no Herdr Core is running")
    );
}

// The ancestry walk compares every process it reaches, the last one within its bound
// included, and tells a bound it reached from a chain that ended: the first is inconclusive,
// the second proves the command is outside the terminal's program. Both refuse the choice.
#[test]
fn the_caller_check_compares_the_last_ancestor_and_reports_its_bound_apart() {
    let core = FakeCore::new();
    core.0.borrow_mut().caller = Some("t3".into());
    // This process is 900; its ancestors are 1000, 1001, ...: `links` parent links up to
    // the terminal's program, which is then the last process the walk reaches.
    let chain = |links: u32| -> BTreeMap<u32, u32> {
        let mut parents = BTreeMap::from([(900, 1000)]);
        for pid in 1000..1000 + links {
            parents.insert(pid, pid + 1);
        }
        parents
    };
    let program_at = |links: u32| 999 + links;
    let limit = u32::try_from(MAX_ANCESTRY).unwrap();
    for links in [1, limit - 1, limit] {
        core.0.borrow_mut().terminals.insert(
            "t3".into(),
            json!({"address": "t3", "state": "live", "pid": program_at(links),
                   "argv": ["sh"], "created_ms": 1, "labels": {}}),
        );
        core.0.borrow_mut().parents = chain(limit + 5);
        verify_caller(&core, later()).unwrap_or_else(|error| {
            panic!("a program {links} links above is the caller's: {error:#}")
        });
    }
    // One link beyond the bound: not found within it, which proves nothing about the chain.
    core.0.borrow_mut().terminals.insert(
        "t3".into(),
        json!({"address": "t3", "state": "live", "pid": program_at(limit + 1),
               "argv": ["sh"], "created_ms": 1, "labels": {}}),
    );
    let text = error_text(&verify_caller(&core, later()).unwrap_err());
    assert!(
        text.contains(&format!("within {MAX_ANCESTRY} parent processes")),
        "{text}"
    );
    assert!(text.contains("could not verify"), "{text}");
    assert!(!text.contains("does not run inside its program"), "{text}");
    assert!(text.contains("no other terminal was opened"), "{text}");
    // A parent that could not be established (on Windows: it ended, its PID was reused, or it
    // may not be inspected) is not the end of the chain: the check is unverified.
    core.0.borrow_mut().unknown_parent = Some(1002);
    let text = error_text(&verify_caller(&core, later()).unwrap_err());
    assert!(text.contains("could not verify"), "{text}");
    assert!(text.contains("the parent of process 1002 ended"), "{text}");
    assert!(!text.contains("does not run inside its program"), "{text}");
    core.0.borrow_mut().unknown_parent = None;
    // A chain that ends at a root before the bound proves the command is outside.
    core.0.borrow_mut().parents = chain(3);
    let text = error_text(&verify_caller(&core, later()).unwrap_err());
    assert!(text.contains("does not run inside its program"), "{text}");
}

#[test]
fn the_attach_command_names_the_recorded_socket() {
    let core = FakeCore::new();
    let session = created(&core);
    let expected = if cfg!(windows) {
        format!(
            "& {{ $prior = $env:HC_SOCKET; $env:HC_SOCKET = '{SOCKET}'; try {{ hc attach t7 }} finally {{ $env:HC_SOCKET = $prior }} }}"
        )
    } else {
        format!("HC_SOCKET='{SOCKET}' hc attach t7")
    };
    assert_eq!(attach_command(&session).unwrap(), expected);
    // Each shell's own quoting: POSIX ends and reopens the quote, PowerShell doubles it.
    assert_eq!(
        hc_command("/state/it's here/x.sock", "attach", "t7", false),
        "HC_SOCKET='/state/it'\"'\"'s here/x.sock' hc attach t7"
    );
    assert_eq!(
        hc_command(
            r"\\.\pipe\herdr-core-o'neil-default.sock",
            "attach",
            "t7",
            true
        ),
        r"& { $prior = $env:HC_SOCKET; $env:HC_SOCKET = '\\.\pipe\herdr-core-o''neil-default.sock'; try { hc attach t7 } finally { $env:HC_SOCKET = $prior } }"
    );
    assert!(attach_command(&TerminalSession::new(TerminalKind::Iterm2, "x".into())).is_none());
    // A handle that records another platform's endpoint names no core here.
    let mut foreign = session.herdr_core().unwrap().clone();
    foreign.socket = if cfg!(windows) {
        "/state/herdr-core/default/default.sock".into()
    } else {
        r"\\.\pipe\herdr-core-tester-default.sock".into()
    };
    let foreign =
        TerminalSession::new(TerminalKind::Herdr, "t7".into()).with_herdr_core(foreign, "unused");
    assert!(attach_command(&foreign).is_none());
}

// A failed binding before the provider starts removes exactly the created terminal, found
// by the session directory in its argv.
#[test]
fn open_removes_its_terminal_when_binding_fails() {
    let core = FakeCore::new();
    let adapter = Adapter::with_core(core.clone());
    let error = adapter
        .open(
            "launch",
            Path::new(DIRECTORY),
            Instant::now() + Duration::from_secs(10),
            |_| bail!("bind failed"),
            || Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "bind failed");
    assert!(core.terminal("t7").is_none());
    let core = FakeCore::new();
    let adapter = Adapter::with_core(core.clone());
    let bound = adapter
        .open(
            "launch",
            Path::new(DIRECTORY),
            Instant::now() + Duration::from_secs(10),
            |session| {
                session.managed_session_id = Some(SESSION.into());
                Ok(())
            },
            || panic!("no unbind on success"),
        )
        .unwrap();
    assert_eq!(bound.id, "t7");
    assert_eq!(core.terminal("t7").unwrap()["state"], "live");
}

#[test]
fn failed_binding_and_removal_preserve_the_herdr_surface_for_close() {
    use crate::native::session::launch;

    let root = tempfile::tempdir().unwrap();
    let (directory, store, _) =
        launch::test_support::pending_launch("session-herdr", root.path(), Duration::from_secs(30));
    let core = FakeCore::new();
    core.inject("terminal.rm", Injected::Instead(refused("internal")));
    let adapter = Adapter::with_core(core.clone());
    let error = adapter
        .open(
            "launch",
            directory.path(),
            later(),
            |_| bail!("bind failed"),
            || panic!("cannot unbind after unconfirmed removal"),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("bind failed"));
    assert_eq!(core.terminal("t7").unwrap()["state"], "finished");
    launch::terminal_failed(&store, &error).unwrap();
    let handle = store
        .terminal()
        .expect("launcher retained the Herdr handle");
    assert_eq!(handle.id, "t7");
    assert_eq!(handle.herdr_core().unwrap().core_id, "core-a");
    assert_eq!(
        adapter.close(directory.path(), &handle, later()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.kill").len(), 1);
    assert!(core.terminal("t7").is_none());
}

// The installed connection over a real socket: bounded reads, one reply line per request,
// and a written request without an answer is never reported as not sent.
#[cfg(unix)]
#[test]
fn the_socket_connection_bounds_every_reply() {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("c.sock");
    let error = super::unix::open(&socket, later()).err().unwrap();
    assert!(error_text(&error).contains("no Herdr Core is running"));
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        for reply in ["answer", "silent", "close"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                request.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["jsonrpc"], "2.0");
            match reply {
                "answer" => {
                    let line =
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": {"ok": true}});
                    stream.write_all(format!("{line}\n").as_bytes()).unwrap();
                }
                "silent" => thread::sleep(Duration::from_millis(600)),
                _ => {}
            }
        }
    });
    let mut connection = super::unix::open(&socket, later()).unwrap();
    // The core's account is the peer's, which open required to be this process's.
    assert!(
        connection
            .core_account()
            .unwrap()
            .same(&HostAccount::current().unwrap())
    );
    assert_eq!(
        connection
            .call("hello", json!({"generation": 1}), later())
            .unwrap(),
        json!({"ok": true})
    );
    let mut connection = super::unix::open(&socket, later()).unwrap();
    let started = Instant::now();
    let error = connection
        .call(
            "terminal.send",
            json!({}),
            Instant::now() + Duration::from_millis(200),
        )
        .unwrap_err();
    assert!(matches!(error, CallError::Unanswered(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_millis(550));
    let mut connection = super::unix::open(&socket, later()).unwrap();
    let error = connection
        .call("terminal.get", json!({}), later())
        .unwrap_err();
    assert!(matches!(error, CallError::Unanswered(_)), "{error:?}");
    server.join().unwrap();
    // A socket in a directory that others can write is not this user's core.
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let error = super::unix::open(&socket, later()).err().unwrap();
    assert!(error_text(&error).contains("is not a protected Herdr Core socket"));
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
}

// A core that answers and then closes its end leaves its reply in the socket. macOS refuses
// a receive timeout (SO_RCVTIMEO) with EINVAL once the peer has closed, so a read that set
// one first lost a reply that had arrived: the test above failed once on the macOS runner
// (2026-11-26) with "lost the Herdr Core connection: Invalid argument (os error 22)" for an
// answered hello. The transport reads the reply, then the end of the stream, and an open core
// that says nothing still ends the read at its deadline.
#[cfg(unix)]
#[test]
fn a_reply_sent_before_the_core_closed_its_end_is_read() {
    use std::io::{ErrorKind, Write};
    use std::os::unix::net::UnixStream;
    let line = [
        br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.as_slice(),
        b"\n",
    ]
    .concat();
    let mut buffer = [0_u8; 256];
    let (mut tab, mut core) = UnixStream::pair().unwrap();
    core.write_all(&line).unwrap();
    drop(core);
    let read = super::Stream::read_before(&mut tab, &mut buffer, later()).unwrap();
    assert_eq!(&buffer[..read], line.as_slice());
    assert_eq!(
        super::Stream::read_before(&mut tab, &mut buffer, later()).unwrap(),
        0
    );
    let (mut tab, _core) = UnixStream::pair().unwrap();
    let started = Instant::now();
    let error =
        super::Stream::read_before(&mut tab, &mut buffer, started + Duration::from_millis(100))
            .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert!(started.elapsed() >= Duration::from_millis(100));
}

// Sends one urgent (out-of-band) byte from the core's end. `false` where the system keeps no
// out-of-band data on Unix stream sockets and refuses the send: macOS, and Linux before 5.15
// or without CONFIG_AF_UNIX_OOB. Any other error fails the test.
#[cfg(unix)]
fn send_urgent_byte(core: &std::os::unix::net::UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let sent = unsafe { libc::send(core.as_raw_fd(), b"!".as_ptr().cast(), 1, libc::MSG_OOB) };
    if sent == 1 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "{error}");
    false
}

// A socket can be readable with nothing to read in order: Linux keeps an urgent byte on a
// Unix stream socket in its receive queue and reports the socket readable, and an ordinary
// blocking read then waits for ordinary data. The read still ends at its deadline. Where the
// system refuses the urgent byte, the same read covers a silent peer.
#[cfg(unix)]
#[test]
fn a_read_ends_at_its_deadline_when_only_an_urgent_byte_is_queued() {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;
    let (mut tab, core) = UnixStream::pair().unwrap();
    let urgent = send_urgent_byte(&core);
    let (sender, outcome) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buffer = [0_u8; 16];
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let result = super::Stream::read_before(&mut tab, &mut buffer, deadline);
        let _ = sender.send((result.map_err(|error| error.kind()), started.elapsed()));
    });
    // The core's end stays open, so a read that outlives its deadline would block for good.
    let (result, elapsed) = outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("the read was still blocked ten seconds after its 100 ms deadline");
    assert_eq!(
        result,
        Err(ErrorKind::TimedOut),
        "urgent byte sent: {urgent}"
    );
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
        "a 100 ms read returned after {elapsed:?} (urgent byte sent: {urgent})"
    );
    reader.join().unwrap();
    drop(core);
}

// After a receive that finds nothing, the read waits again at once for the time left before
// its original deadline, so that a reply the core sends while time is left is read. The clock
// is simulated and the read's two steps are scripted on it; the socket tests above cover what
// the kernels do. The deadline is 10 ms away. The socket becomes readable at 2 ms with nothing
// to receive, as on Linux for a queue that holds only an urgent byte, and the reply arrives at
// 5 ms. A read that pauses through its clock after the empty receive spends time asleep while
// time is left, and is late or misses the reply: 8f316da paused for up to 10 ms of the time
// left, here all 8 ms, and then found its deadline passed. Without a reply, the read ends at
// its deadline without another receive.
#[cfg(unix)]
#[test]
fn after_an_empty_receive_the_read_waits_again_at_once_for_the_time_left() {
    use crate::native::clock::FakeClock;
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    // Each step with its simulated time since the read began.
    #[derive(Debug, PartialEq)]
    enum Step {
        Wait { at: Duration, up_to: Duration },
        Receive { at: Duration },
    }
    let ms = Duration::from_millis;
    let (wait, receive) = (
        |at, up_to| Step::Wait { at, up_to },
        |at| Step::Receive { at },
    );
    let reply = [
        br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.as_slice(),
        b"\n",
    ]
    .concat();
    // One read with a 10 ms deadline. The socket becomes readable at each time in `readable`,
    // and a receive at `arrives` or later returns the reply. Returns the outcome, the steps
    // and the time the read spent asleep on its clock.
    let read = |readable: &[Duration], arrives: Option<Duration>| {
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let steps = RefCell::new(Vec::new());
        let mut readable = readable.iter().copied().collect::<VecDeque<_>>();
        let mut buffer = [0_u8; 256];
        let result = super::unix::wait_and_receive(
            &mut clock,
            &mut buffer,
            start + ms(10),
            |up_to| {
                let at = now.get() - start;
                steps.borrow_mut().push(Step::Wait { at, up_to });
                match readable.front().copied() {
                    Some(ready) if ready <= at + up_to => {
                        readable.pop_front();
                        now.set(start + ready.max(at));
                        Ok(true)
                    }
                    _ => {
                        now.set(start + at + up_to);
                        Ok(false)
                    }
                }
            },
            |buffer| {
                let at = now.get() - start;
                steps.borrow_mut().push(Step::Receive { at });
                match arrives {
                    Some(arrives) if at >= arrives => {
                        buffer[..reply.len()].copy_from_slice(&reply);
                        Ok(reply.len())
                    }
                    _ => Err(ErrorKind::WouldBlock.into()),
                }
            },
        )
        .map(|read| buffer[..read].to_vec())
        .map_err(|error| error.kind());
        (result, steps.into_inner(), clock.slept)
    };
    assert_eq!(
        read(&[ms(2), ms(5)], Some(ms(5))),
        (
            Ok(reply.clone()),
            vec![
                wait(ms(0), ms(10)),
                receive(ms(2)),
                wait(ms(2), ms(8)),
                receive(ms(5)),
            ],
            Duration::ZERO,
        )
    );
    assert_eq!(
        read(&[ms(2)], None),
        (
            Err(ErrorKind::TimedOut),
            vec![wait(ms(0), ms(10)), receive(ms(2)), wait(ms(2), ms(8))],
            Duration::ZERO,
        )
    );
}

// --- Review regressions (2026-11-18): each was written and run before its fix. ---

// herdr-core `terminal_run` creates the terminal, then `flush_saved` can answer
// `unavailable` (persistence timeout) or `internal` (write error) after it exists.
#[test]
fn a_run_error_after_creation_is_reconciled_on_the_pinned_core() {
    for kind in ["internal", "unavailable"] {
        let core = FakeCore::new();
        core.inject("terminal.run", Injected::After(refused(kind)));
        let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
        assert!(
            text.contains("t7 this launch created was removed"),
            "{kind}: {text}"
        );
        assert!(core.terminal("t7").is_none(), "{kind}");
        assert_eq!(core.params("terminal.run").len(), 1, "{kind}: never rerun");
    }
    // A refusal the core gives before creating anything is not reconciled.
    for kind in ["invalid_params", "name_taken", "config", "spawn_failed"] {
        let core = FakeCore::new();
        core.inject("terminal.run", Injected::Instead(refused(kind)));
        let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
        assert!(text.contains("was not created"), "{kind}: {text}");
        assert_eq!(
            core.methods(),
            ["connect", "hello", "terminal.run"],
            "{kind}"
        );
    }
}

// A same-user client sets keep (so a replacement program can run) after close checked the
// terminal and before it acts: the next observation disproves authority, so nothing is
// killed or removed and the handle is kept.
#[test]
fn close_refuses_when_a_later_observation_changes_the_terminal() {
    let directory = Path::new(DIRECTORY);
    for (change, expected) in [
        ("keep", "set to keep"),
        ("replacement", "not the terminal this session created"),
    ] {
        let core = FakeCore::new();
        let session = created(&core);
        let adapter = Adapter::with_core(core.clone());
        core.clear_calls();
        core.after_call("terminal.get", move |terminals| {
            let terminal = terminals.get_mut("t7").unwrap();
            terminal["keep"] = json!(true);
            if change == "replacement" {
                terminal["pid"] = json!(4242);
            }
        });
        let text = error_text(&adapter.close(directory, &session, later()).unwrap_err());
        assert!(text.contains(expected), "{change}: {text}");
        assert!(core.params("terminal.kill").is_empty(), "{change}");
        assert!(core.params("terminal.rm").is_empty(), "{change}");
    }
    // After the kill: a program that replaced the ended one is not removed.
    let core = FakeCore::new();
    let session = created(&core);
    let adapter = Adapter::with_core(core.clone());
    core.clear_calls();
    core.after_call("terminal.kill", |terminals| {
        let terminal = terminals.get_mut("t7").unwrap();
        terminal["keep"] = json!(true);
        terminal["state"] = json!("live");
        terminal["pid"] = json!(4242);
    });
    assert!(adapter.close(directory, &session, later()).is_err());
    assert_eq!(core.params("terminal.kill").len(), 1);
    assert!(core.params("terminal.rm").is_empty());
    assert!(core.terminal("t7").is_some());
}

// The same rule while removing a terminal this launch created but could not use.
#[test]
fn removing_a_partially_created_terminal_stops_when_it_changes() {
    let core = FakeCore::new();
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    // The first read finds the terminal; before the next one it is set to keep.
    core.after_call("terminal.get", |terminals| {
        terminals.get_mut("t7").unwrap()["keep"] = json!(true);
    });
    let error = try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err();
    assert!(error.downcast_ref::<RetainedLaunchSurface>().is_none());
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.terminal("t7").is_some());
}

// hc's `terminal.list` fans out to every configured machine, whose terminals have remote
// addresses. Recovery names its own terminal on the pinned core instead of listing.
#[test]
fn a_lost_run_reply_is_found_by_its_local_name_without_listing() {
    let core = FakeCore::new();
    core.0.borrow_mut().remote.push(json!({
        "address": "vps/t1", "state": "live", "pid": 5, "argv": ["/opt/tabcli"],
        "created_ms": 1, "labels": {LAUNCH_LABEL: "0123456789abcdef0123456789abcdef"},
    }));
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(
        text.contains("t7 this launch created was removed"),
        "{text}"
    );
    assert!(core.terminal("t7").is_none());
    assert!(core.params("terminal.list").is_empty());
    let name = core.params("terminal.run")[0]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        name.len() <= 32 && name.starts_with("tabcli-") && !valid_address(&name),
        "{name}"
    );
    assert!(
        core.params("terminal.get")
            .contains(&json!({"terminal": name}))
    );
}

// The name resolves to a terminal that is not this launch's: nothing is removed, and the
// run is not repeated.
#[test]
fn a_name_that_resolves_to_another_terminal_fails_closed() {
    let core = FakeCore::new();
    core.inject(
        "terminal.run",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    core.after_call("terminal.run", |terminals| {
        terminals.get_mut("t7").unwrap()["labels"][LAUNCH_LABEL] = json!("another launch");
    });
    let text = error_text(&try_open(core.clone(), "launch", Path::new(DIRECTORY)).unwrap_err());
    assert!(text.contains("creation is uncertain"), "{text}");
    assert!(core.params("terminal.kill").is_empty());
    assert_eq!(core.params("terminal.run").len(), 1);
    assert!(core.terminal("t7").is_some());
}

// --- Windows (2026-11-18): endpoint, framing and transport. ---

// hc 0.1.21 on Windows: HC_SOCKET, else `socket` in the config under XDG_CONFIG_HOME,
// APPDATA or the profile's Roaming folder, else `\\.\pipe\herdr-core-<user>-<session>.sock`
// with only the ASCII letters and digits of USERNAME. Pure, so it runs on every platform.
#[test]
fn the_windows_endpoint_follows_hc_and_accepts_only_a_local_pipe() {
    use super::windows_endpoint::{WindowsEndpointInputs, local_pipe_name, resolve_pipe};
    let no_config = |_: &Path| -> Result<Option<String>> { Ok(None) };
    let base = WindowsEndpointInputs {
        appdata: Some(r"C:\Users\Jo Doe\AppData\Roaming".into()),
        userprofile: Some(r"C:\Users\Jo Doe".into()),
        username: Some("Jo Doe.1".into()),
        ..WindowsEndpointInputs::default()
    };
    let pipe = |inputs: &WindowsEndpointInputs| {
        resolve_pipe(inputs, &no_config).map(|path| path.to_str().unwrap().to_owned())
    };
    assert_eq!(
        pipe(&base).unwrap(),
        r"\\.\pipe\herdr-core-JoDoe1-default.sock"
    );
    let mut inputs = base.clone();
    inputs.hc_session = Some(" smoke ".into());
    assert_eq!(
        pipe(&inputs).unwrap(),
        r"\\.\pipe\herdr-core-JoDoe1-smoke.sock"
    );
    // hc keeps the user part empty when USERNAME is unset.
    let mut inputs = base.clone();
    inputs.username = None;
    assert_eq!(pipe(&inputs).unwrap(), r"\\.\pipe\herdr-core--default.sock");
    // The configuration is read where hc reads it, and HC_SOCKET wins over it.
    let seen = std::cell::RefCell::new(Vec::new());
    let configured = |path: &Path| -> Result<Option<String>> {
        seen.borrow_mut().push(path.to_path_buf());
        Ok(Some(r"\\.\pipe\configured.sock".into()))
    };
    assert_eq!(
        resolve_pipe(&base, &configured).unwrap(),
        Path::new(r"\\.\pipe\configured.sock")
    );
    let mut inputs = base.clone();
    inputs.xdg_config_home = Some(r"D:\xdg".into());
    resolve_pipe(&inputs, &configured).unwrap();
    let mut inputs = base.clone();
    inputs.appdata = None;
    resolve_pipe(&inputs, &configured).unwrap();
    let separator = std::path::MAIN_SEPARATOR;
    let expected: Vec<String> = [
        r"C:\Users\Jo Doe\AppData\Roaming",
        r"D:\xdg",
        &format!(r"C:\Users\Jo Doe{separator}AppData{separator}Roaming"),
    ]
    .iter()
    .map(|home| format!("{home}{separator}herdr-core{separator}config.toml"))
    .collect();
    let seen: Vec<String> = seen
        .borrow()
        .iter()
        .map(|path| path.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(seen, expected);
    let mut inputs = base.clone();
    inputs.hc_socket = Some(r"\\.\PIPE\explicit.sock".into());
    assert_eq!(
        resolve_pipe(&inputs, &|_| panic!("HC_SOCKET is read first")).unwrap(),
        Path::new(r"\\.\PIPE\explicit.sock")
    );
    // Refused: what hc refuses, a profile-less configuration, and any endpoint that is not a
    // local named pipe (another machine's pipe, a file, a socket path).
    for (field, value, expected) in [
        ("session", "a:b", "cannot name a Herdr Core"),
        ("session", "..", "cannot name a Herdr Core"),
        (
            "socket",
            r"\\host\pipe\herdr.sock",
            "is not a local named pipe",
        ),
        (
            "socket",
            r"C:\Users\Jo\hc.sock",
            "is not a local named pipe",
        ),
        ("socket", "/state/default.sock", "is not a local named pipe"),
        (
            "socket",
            r"\\?\pipe\herdr.sock",
            "is not a local named pipe",
        ),
        ("socket", r"\\.\pipe\", "is not a local named pipe"),
        (
            "socket",
            r"\\.\pipe\group\name",
            "is not a local named pipe",
        ),
        (
            "socket",
            r"\\.\pipe\group/name",
            "is not a local named pipe",
        ),
        (
            "socket",
            &format!(r"\\.\pipe\{}", "x".repeat(250)),
            "longer than Windows allows",
        ),
    ] {
        let mut inputs = base.clone();
        match field {
            "session" => inputs.hc_session = Some(value.into()),
            _ => inputs.hc_socket = Some(value.into()),
        }
        let error = error_text(&resolve_pipe(&inputs, &no_config).unwrap_err());
        assert!(error.contains(expected), "{value}: {error}");
    }
    let mut inputs = base.clone();
    inputs.hc_socket = None;
    let configured_file = |_: &Path| -> Result<Option<String>> { Ok(Some(r"C:\hc.sock".into())) };
    assert!(
        error_text(&resolve_pipe(&inputs, &configured_file).unwrap_err())
            .contains("is not a local named pipe")
    );
    let broken = |_: &Path| -> Result<Option<String>> { bail!("cannot parse") };
    assert!(resolve_pipe(&base, &broken).is_err());
    let homeless = WindowsEndpointInputs::default();
    assert!(
        error_text(&resolve_pipe(&homeless, &no_config).unwrap_err())
            .contains("neither APPDATA nor USERPROFILE is set")
    );
    assert_eq!(
        local_pipe_name(Path::new(r"\\.\pipe\herdr-core-u-default.sock")).unwrap(),
        "herdr-core-u-default.sock"
    );
}

/// A stream that answers each write and read from a script: the shared framing's view of
/// either installed transport.
#[derive(Default)]
struct ScriptedStream {
    writes: VecDeque<std::io::Result<usize>>,
    reads: VecDeque<std::io::Result<Vec<u8>>>,
    written: Vec<u8>,
}

impl Stream for ScriptedStream {
    fn write_before(&mut self, bytes: &[u8], _: Instant) -> std::io::Result<usize> {
        let count = self.writes.pop_front().unwrap_or(Ok(bytes.len()))?;
        let count = count.min(bytes.len());
        self.written.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn read_before(&mut self, buffer: &mut [u8], _: Instant) -> std::io::Result<usize> {
        let bytes = self.reads.pop_front().unwrap_or(Ok(Vec::new()))?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn peer_account(&self) -> Result<HostAccount> {
        bail!("a scripted stream has no peer process")
    }
}

fn io(kind: std::io::ErrorKind) -> std::io::Error {
    std::io::Error::from(kind)
}

// Both transports share one rule: a request is unsent only while none of its bytes was
// written. A request whose bytes were partly written, or whose reply is missing, late or
// invalid, is unanswered and never sent again.
#[test]
fn the_shared_framing_reports_unsent_only_before_any_byte_was_written() {
    use std::io::ErrorKind::{BrokenPipe, Interrupted, TimedOut, WouldBlock};
    let call = |stream: ScriptedStream| {
        let mut framed = Framed::new(stream);
        let result = framed.call("terminal.send", json!({"text": "x"}), later());
        (result, framed.stream.written)
    };
    let reply =
        |id: u64| format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"turn":1}}}}"#).into_bytes();
    // A reply split across reads, after an interrupted write was repeated.
    let mut first = reply(1);
    let second = first.split_off(10);
    let (result, written) = call(ScriptedStream {
        writes: VecDeque::from([Err(io(Interrupted)), Ok(7)]),
        reads: VecDeque::from([Ok(first), Ok([second, b"\n".to_vec()].concat())]),
        ..ScriptedStream::default()
    });
    assert_eq!(result.unwrap(), json!({"turn": 1}));
    assert!(written.ends_with(b"\n"));
    let request: Value = serde_json::from_slice(&written[..written.len() - 1]).unwrap();
    assert_eq!(request["id"], 1);
    for (writes, unsent) in [
        (vec![Err(io(TimedOut))], true),
        (vec![Err(io(WouldBlock))], true),
        (vec![Ok(0)], true),
        (vec![Err(io(BrokenPipe))], true),
        (vec![Ok(5), Err(io(TimedOut))], false),
        (vec![Ok(5), Ok(0)], false),
        (vec![Ok(5), Err(io(BrokenPipe))], false),
    ] {
        let (result, _) = call(ScriptedStream {
            writes: writes.into(),
            ..ScriptedStream::default()
        });
        match result {
            Err(CallError::NotSent(_)) => assert!(unsent),
            Err(CallError::Unanswered(_)) => assert!(!unsent),
            other => panic!("{other:?}"),
        }
    }
    for (reads, expected) in [
        (vec![Err(io(TimedOut))], "Herdr reply timed out"),
        (vec![Err(io(WouldBlock))], "Herdr reply timed out"),
        (vec![Ok(Vec::new())], "closed the connection"),
        (vec![Err(io(BrokenPipe))], "lost the Herdr Core connection"),
        (
            vec![Ok([reply(2), b"\n".to_vec()].concat())],
            "does not answer request 1",
        ),
        (vec![Ok(b"no json\n".to_vec())], "is not JSON"),
    ] {
        let (result, _) = call(ScriptedStream {
            reads: reads.into(),
            ..ScriptedStream::default()
        });
        match result {
            Err(CallError::Unanswered(error)) => {
                assert!(error_text(&error).contains(expected), "{error:#}")
            }
            other => panic!("{expected}: {other:?}"),
        }
    }
    // A reply that never ends is bounded.
    let (result, _) = call(ScriptedStream {
        reads: std::iter::repeat_with(|| Ok(vec![b'x'; 64 * 1024]))
            .take(MAX_REPLY / (64 * 1024) + 2)
            .collect(),
        ..ScriptedStream::default()
    });
    match result {
        Err(CallError::Unanswered(error)) => {
            assert!(error_text(&error).contains("reply exceeds"), "{error:#}")
        }
        other => panic!("{other:?}"),
    }
}

// The installed pipe transport over a real named pipe of this user: one reply line per
// request, a silent core bounded by the deadline and reported unanswered, a closed one
// unanswered, a missing one not running, and a name that is no local pipe refused before
// anything is opened.
#[cfg(windows)]
#[test]
fn the_pipe_connection_bounds_every_reply_and_checks_its_server() {
    use std::io::{Read, Write};
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_WAIT,
        },
    };
    let name = format!(r"\\.\pipe\tabcli-herdr-test-{}", std::process::id());
    let error = super::pipe::open(Path::new(&name), later()).err().unwrap();
    assert!(
        error_text(&error).contains("no Herdr Core is running"),
        "{error:#}"
    );
    let error = super::pipe::open(Path::new(r"C:\Windows\win.ini"), later())
        .err()
        .unwrap();
    assert!(
        error_text(&error).contains("is not a local named pipe"),
        "{error:#}"
    );
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // One listening instance at a time, so that each connection reaches the instance the
    // server serves next. The next one listens before the last one closes: a client that
    // finds none listening waits for it (busy) instead of finding no core at all.
    fn instance(wide: &[u16]) -> usize {
        let raw = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                4,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            raw,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        raw as usize
    }
    let first = instance(&wide);
    let server = thread::spawn(move || {
        let mut current = first;
        for (index, reply) in ["answer", "silent", "close"].into_iter().enumerate() {
            let raw = current as windows_sys::Win32::Foundation::HANDLE;
            unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
            let mut stream = unsafe { fs::File::from_raw_handle(raw) };
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                request.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["jsonrpc"], "2.0");
            match reply {
                "answer" => {
                    let line =
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": {"ok": true}});
                    stream.write_all(format!("{line}\n").as_bytes()).unwrap();
                    // Until the client has read the reply and gone.
                    let _ = stream.read(&mut byte);
                }
                "silent" => thread::sleep(Duration::from_millis(600)),
                _ => {}
            }
            if index < 2 {
                current = instance(&wide);
            }
            drop(stream);
        }
    });
    let mut connection = super::pipe::open(Path::new(&name), later()).unwrap();
    // This process serves the pipe, so the core's account is this process's own.
    assert!(
        connection
            .core_account()
            .unwrap()
            .same(&HostAccount::current().unwrap())
    );
    assert_eq!(
        connection
            .call("hello", json!({"generation": 1}), later())
            .unwrap(),
        json!({"ok": true})
    );
    drop(connection);
    let mut connection = super::pipe::open(Path::new(&name), later()).unwrap();
    let started = Instant::now();
    let error = connection
        .call(
            "terminal.send",
            json!({}),
            Instant::now() + Duration::from_millis(200),
        )
        .unwrap_err();
    assert!(matches!(error, CallError::Unanswered(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_millis(550));
    drop(connection);
    let mut connection = super::pipe::open(Path::new(&name), later()).unwrap();
    let error = connection
        .call("terminal.get", json!({}), later())
        .unwrap_err();
    assert!(matches!(error, CallError::Unanswered(_)), "{error:?}");
    server.join().unwrap();
    // The server check compares account SIDs, which Windows spells without regard to case.
    assert!(super::pipe::same_user(
        "S-1-5-21-1-2-3-1001",
        "s-1-5-21-1-2-3-1001"
    ));
    assert!(!super::pipe::same_user(
        "S-1-5-21-1-2-3-1001",
        "S-1-5-21-1-2-3-1002"
    ));
}

// The installed Windows reads of this machine's processes: this process's birth is its
// creation time, a missing process has none, its parent was created no later than it, and
// a launch token is 32 hexadecimal digits.
#[cfg(windows)]
#[test]
fn the_windows_core_reads_births_parents_and_random_tokens() {
    let core = super::Installed;
    let pid = std::process::id();
    let created = super::pipe::creation_time(pid).unwrap().unwrap();
    assert_eq!(
        core.process_start(pid).unwrap(),
        Some(HerdrHostBirth::Windows {
            host_creation_time: created
        })
    );
    assert_eq!(core.process_start(u32::MAX - 3).unwrap(), None);
    let mut child = std::process::Command::new("cmd.exe")
        .args(["/C", "ping -n 3 127.0.0.1 >NUL"])
        .spawn()
        .unwrap();
    assert_eq!(core.parent_pid(child.id()).unwrap(), Parent::Process(pid));
    if let Parent::Process(parent) = core.parent_pid(pid).unwrap() {
        assert!(super::pipe::creation_time(parent).unwrap().unwrap() <= created);
    }
    child.kill().unwrap();
    child.wait().unwrap();
    // An ended process's parent is unknown, never a root.
    assert!(matches!(
        core.parent_pid(child.id()).unwrap(),
        Parent::Unknown(_)
    ));
    let token = core.launch_nonce().unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(token, core.launch_nonce().unwrap());
}

// The PowerShell attach line, run by PowerShell 7 with a stand-in `hc`: hc sees the recorded
// endpoint, quoted exactly, and afterwards HC_SOCKET is what it was before (unset, or the
// earlier value), so later commands in the user's shell keep their own core.
#[cfg(windows)]
#[test]
fn the_powershell_attach_line_scopes_hc_socket_to_hc() {
    let pwsh = crate::native::terminal::windows::powershell_executable().unwrap();
    let root = tempfile::tempdir().unwrap();
    let seen = root.path().join("seen.txt");
    let script = root.path().join("attach.ps1");
    for (prior, endpoint) in [
        (None, r"\\.\pipe\herdr-core-tester-default.sock"),
        (
            Some(r"\\.\pipe\earlier core"),
            r"\\.\pipe\o'neil $x `y; hc.sock",
        ),
    ] {
        let _ = fs::remove_file(&seen);
        fs::write(
            &script,
            format!(
                "function hc {{ Set-Content -LiteralPath $env:TABCLI_TEST_SEEN -Value ($env:HC_SOCKET + ' ' + ($args -join ' ')) -NoNewline }}\n{}\nif (Test-Path Env:HC_SOCKET) {{ 'present:' + $env:HC_SOCKET }} else {{ 'absent' }}\n",
                hc_command(endpoint, "attach", "t7", true)
            ),
        )
        .unwrap();
        let mut command = std::process::Command::new(&pwsh);
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
            ])
            .arg("-File")
            .arg(&script)
            .env("TABCLI_TEST_SEEN", &seen);
        match prior {
            Some(prior) => command.env("HC_SOCKET", prior),
            None => command.env_remove("HC_SOCKET"),
        };
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(&seen).unwrap(),
            format!("{endpoint} attach t7")
        );
        let after = String::from_utf8(output.stdout).unwrap();
        let expected = match prior {
            Some(prior) => format!("present:{prior}"),
            None => "absent".to_owned(),
        };
        assert_eq!(after.trim_end(), expected);
    }
}

// #399: the dead-owner close of a terminal that ended outside TAB.
#[cfg(unix)]
mod ended_close;
