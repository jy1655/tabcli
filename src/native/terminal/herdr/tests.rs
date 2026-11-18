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
}

const SOCKET: &str = "/state/herdr-core/default/default.sock";
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
    caller: Option<String>,
    current_pid: u32,
    /// Terminals of configured remote machines, which hc's list also returns.
    remote: Vec<Value>,
    /// Changes the terminals before the n-th later `terminal.get` (1 = the next one).
    on_get: Option<(usize, Mutation)>,
}

type Mutation = Box<dyn FnMut(&mut BTreeMap<String, Value>)>;

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
    /// Another same-user client changes the terminals just before the n-th later read.
    fn on_get(&self, nth: usize, change: impl FnMut(&mut BTreeMap<String, Value>) + 'static) {
        self.0.borrow_mut().on_get = Some((nth, Box::new(change)));
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
                if let Some((nth, _)) = state.on_get.as_mut() {
                    *nth -= 1;
                    if *nth == 0 {
                        let (_, mut change) = state.on_get.take().unwrap();
                        change(&mut state.terminals);
                    }
                }
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
        match injected {
            Some(Injected::Instead(error)) => Err(error),
            Some(Injected::After(error)) => {
                let _ = self.act(method, &params);
                Err(error)
            }
            None => self.act(method, &params),
        }
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
    fn parent_pid(&self, pid: u32) -> Result<Option<u32>> {
        Ok(self.0.borrow().parents.get(&pid).copied())
    }
    fn process_start(&self, pid: u32) -> Result<Option<HerdrHostBirth>> {
        Ok(self.0.borrow().births.get(&pid).copied())
    }
    fn launch_nonce(&self) -> Result<String> {
        Ok("0123456789abcdef0123456789abcdef".into())
    }
    fn pause(&self, _: Instant) {}
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn created(core: &FakeCore) -> TerminalSession {
    let mut session = create(
        core,
        ". '/state/launch.sh'",
        Path::new(DIRECTORY),
        later(),
        later(),
    )
    .unwrap();
    session.managed_session_id = Some(SESSION.into());
    session
}

fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[test]
fn endpoint_follows_hc_and_refuses_what_hc_would_refuse() {
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
fn creation_runs_the_launch_host_detached_from_any_caller_terminal() {
    let core = FakeCore::new();
    core.0.borrow_mut().caller = Some("t3".into());
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
    let error =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
    assert!(error.contains("no Herdr Core is running"), "{error}");
    assert!(
        error.contains("does not start or restart a core"),
        "{error}"
    );
    assert!(core.methods().is_empty());
}

#[test]
fn a_refused_run_creates_nothing_and_is_not_recovered() {
    let core = FakeCore::new();
    core.inject("terminal.run", Injected::Instead(refused("spawn_failed")));
    let error =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
    assert!(error.contains("was not created"), "{error}");
    assert!(error.contains("spawn_failed"), "{error}");
    assert!(!core.methods().contains(&"terminal.list".to_owned()));
}

// The reply to `terminal.run` is lost after the core created the terminal: the exact
// terminal is read by this launch's name on the same core, checked and removed.
#[test]
fn a_lost_run_reply_removes_only_the_exact_labelled_terminal() {
    let core = FakeCore::new();
    // Another terminal of the user and another launch of Bridge stay untouched.
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
    let error = create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err();
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
    let text =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
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
    let text =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
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
    let error = create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err();
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
        close_until(&core, &handle, Some(Path::new(DIRECTORY)), later()).unwrap(),
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
    let error = create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err();
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
        fn parent_pid(&self, _: u32) -> Result<Option<u32>> {
            Ok(None)
        }
        fn process_start(&self, _: u32) -> Result<Option<HerdrHostBirth>> {
            Ok(None)
        }
        fn launch_nonce(&self) -> Result<String> {
            self.0.launch_nonce()
        }
        fn pause(&self, _: Instant) {}
    }
    let error = create(
        &Forgetful(FakeCore(state)),
        "launch",
        Path::new(DIRECTORY),
        later(),
        later(),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<RetainedLaunchSurface>().is_none());
    assert!(error_text(&error).contains("already ended"));
    assert!(core.terminal("t7").is_none());
}

#[test]
fn a_changed_or_unreachable_core_is_unknown_never_absent() {
    let core = FakeCore::new();
    let session = created(&core);
    let directory = Path::new(DIRECTORY);
    core.0.borrow_mut().core_id = "core-b".into();
    core.clear_calls();
    for error in [
        surface_present(&core, &session, directory, Duration::from_secs(1)).unwrap_err(),
        close_until(&core, &session, Some(directory), later()).unwrap_err(),
        verify_surface(&core, &session, None).unwrap_err(),
        read_screen(&core, &session, later()).unwrap_err(),
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
        &surface_present(&core, &session, directory, Duration::from_secs(1)).unwrap_err(),
    );
    assert!(text.contains("no Herdr Core is running"), "{text}");
}

#[test]
fn a_removed_terminal_on_its_own_core_is_absent() {
    let core = FakeCore::new();
    let session = created(&core);
    let directory = Path::new(DIRECTORY);
    core.0.borrow_mut().terminals.remove("t7");
    assert!(!surface_present(&core, &session, directory, Duration::from_secs(1)).unwrap());
    assert_eq!(
        close_until(&core, &session, Some(directory), later()).unwrap(),
        CloseOutcome::Missing
    );
    assert!(verify_surface(&core, &session, None).is_err());
}

#[test]
fn close_kills_then_removes_and_repeats_idempotently() {
    let core = FakeCore::new();
    let session = created(&core);
    let directory = Path::new(DIRECTORY);
    assert!(surface_present(&core, &session, directory, Duration::from_secs(1)).unwrap());
    core.clear_calls();
    assert_eq!(
        close_until(&core, &session, Some(directory), later()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.kill"), [json!({"terminal": "t7"})]);
    assert_eq!(core.params("terminal.rm"), [json!({"terminal": "t7"})]);
    assert!(core.terminal("t7").is_none());
    core.clear_calls();
    assert_eq!(
        close_until(&core, &session, Some(directory), later()).unwrap(),
        CloseOutcome::Missing
    );
    assert!(core.params("terminal.kill").is_empty());
}

// A close that ended the program but failed to remove the terminal is finished by the next
// close, which only removes; one whose kill reply was lost is not killed twice blindly.
#[test]
fn an_interrupted_close_is_finished_by_the_next_one() {
    let core = FakeCore::new();
    let session = created(&core);
    let directory = Path::new(DIRECTORY);
    core.inject("terminal.rm", Injected::Instead(refused("internal")));
    assert!(close_until(&core, &session, Some(directory), later()).is_err());
    assert_eq!(core.terminal("t7").unwrap()["state"], "finished");
    core.clear_calls();
    assert_eq!(
        close_until(&core, &session, Some(directory), later()).unwrap(),
        CloseOutcome::Closed
    );
    assert!(core.params("terminal.kill").is_empty());
    assert!(core.terminal("t7").is_none());

    let core = FakeCore::new();
    let session = created(&core);
    core.inject(
        "terminal.kill",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&close_until(&core, &session, Some(directory), later()).unwrap_err());
    assert!(text.contains("sent but not answered"), "{text}");
    assert_eq!(
        close_until(&core, &session, Some(directory), later()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(core.params("terminal.kill").len(), 1);
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
        core.set("t7", field, value);
        core.clear_calls();
        let text = error_text(
            &close_until(&core, &session, Some(Path::new(DIRECTORY)), later()).unwrap_err(),
        );
        assert!(text.contains(expected), "{field}: {text}");
        assert!(core.params("terminal.kill").is_empty(), "{field}");
        assert!(core.params("terminal.rm").is_empty(), "{field}");
    }
    // A program whose birth changed is another incarnation behind the recorded PID.
    let core = FakeCore::new();
    let session = created(&core);
    let pid = session.herdr_core().unwrap().host_pid;
    core.0.borrow_mut().births.insert(pid, test_birth(1));
    let text = error_text(&read_screen(&core, &session, later()).unwrap_err());
    assert!(text.contains("another process"), "{text}");
    // Another session's directory in the argv is another session's terminal.
    let core = FakeCore::new();
    let mut session = created(&core);
    session.managed_session_id = Some("session-other".into());
    assert!(read_screen(&core, &session, later()).is_err());
    let mut remote = created(&FakeCore::new());
    remote.id = "vps/t7".into();
    assert!(target(&remote).is_err());
}

#[test]
fn screen_reads_ask_for_the_visible_screen_only() {
    let core = FakeCore::new();
    let session = created(&core);
    core.clear_calls();
    assert_eq!(
        read_screen(&core, &session, later()).unwrap(),
        "fake screen"
    );
    assert_eq!(
        core.params("terminal.read"),
        [json!({"terminal": "t7", "lines": 0})]
    );
    core.set("t7", "state", json!("finished"));
    assert!(read_screen(&core, &session, later()).is_err());
}

#[test]
fn sends_classify_refusals_before_input_apart_from_uncertain_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let prompt = directory.path().join("prompt");
    fs::write(&prompt, "line one\nline two").unwrap();
    let core = FakeCore::new();
    let session = created(&core);
    core.clear_calls();
    send_file(&core, &session, &prompt, later()).unwrap();
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
        let failure = send_file(&core, &session, &prompt, later()).unwrap_err();
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
    let failure = send_file(&core, &session, &prompt, later()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert!(core.params("terminal.send").is_empty());
    let large = directory.path().join("large");
    fs::write(&large, vec![b'a'; MAX_SEND_TEXT + 1]).unwrap();
    let failure = send_file(&core, &session, &large, later()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    let invalid = directory.path().join("invalid");
    fs::write(&invalid, [0xff, 0xfe]).unwrap();
    assert!(
        !send_file(&core, &session, &invalid, later())
            .unwrap_err()
            .delivery_may_have_occurred()
    );
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

#[test]
fn the_attach_command_names_the_recorded_socket() {
    let core = FakeCore::new();
    let mut session = created(&core);
    assert_eq!(
        attach_command(&session).unwrap(),
        format!("HC_SOCKET='{SOCKET}' hc attach t7")
    );
    let mut recorded = session.herdr_core().unwrap().clone();
    recorded.socket = "/state/with space/x.sock".into();
    session = TerminalSession::new(TerminalKind::Herdr, "t7".into()).with_herdr_core(recorded);
    assert_eq!(
        attach_command(&session).unwrap(),
        "HC_SOCKET='/state/with space/x.sock' hc attach t7"
    );
    assert!(attach_command(&TerminalSession::new(TerminalKind::Iterm2, "x".into())).is_none());
}

// A failed binding before the provider starts removes exactly the created terminal, found
// by the session directory in its argv.
#[test]
fn open_removes_its_terminal_when_binding_fails() {
    let core = FakeCore::new();
    let adapter = Adapter::with_core(core.clone());
    let error = adapter
        .open_bound_tab(
            #[cfg(target_os = "macos")]
            MacosOpenMode::TabFirst,
            "launch",
            Path::new(DIRECTORY),
            Instant::now() + Duration::from_secs(10),
            Box::new(|_| bail!("bind failed")),
            Box::new(|| Ok(())),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "bind failed");
    assert!(core.terminal("t7").is_none());
    let core = FakeCore::new();
    let adapter = Adapter::with_core(core.clone());
    let bound = adapter
        .open_bound_tab(
            #[cfg(target_os = "macos")]
            MacosOpenMode::NewWindow,
            "launch",
            Path::new(DIRECTORY),
            Instant::now() + Duration::from_secs(10),
            Box::new(|session| {
                session.managed_session_id = Some(SESSION.into());
                Ok(())
            }),
            Box::new(|| panic!("no unbind on success")),
        )
        .unwrap();
    assert_eq!(bound.id, "t7");
    assert_eq!(core.terminal("t7").unwrap()["state"], "live");
}

// The installed connection over a real socket: bounded reads, one reply line per request,
// and a written request without an answer is never reported as not sent.
#[test]
fn the_socket_connection_bounds_every_reply() {
    use std::os::unix::net::UnixListener;
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("c.sock");
    let error = SocketConnection::open(&socket, later()).err().unwrap();
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
    let mut connection = SocketConnection::open(&socket, later()).unwrap();
    assert_eq!(
        connection
            .call("hello", json!({"generation": 1}), later())
            .unwrap(),
        json!({"ok": true})
    );
    let mut connection = SocketConnection::open(&socket, later()).unwrap();
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
    let mut connection = SocketConnection::open(&socket, later()).unwrap();
    let error = connection
        .call("terminal.get", json!({}), later())
        .unwrap_err();
    assert!(matches!(error, CallError::Unanswered(_)), "{error:?}");
    server.join().unwrap();
    // A socket in a directory that others can write is not this user's core.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let error = SocketConnection::open(&socket, later()).err().unwrap();
    assert!(error_text(&error).contains("is not a protected Herdr Core socket"));
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
}

// --- Review regressions (2026-11-18): each was written and run before its fix. ---

// herdr-core `terminal_run` creates the terminal, then `flush_saved` can answer
// `unavailable` (persistence timeout) or `internal` (write error) after it exists.
#[test]
fn a_run_error_after_creation_is_reconciled_on_the_pinned_core() {
    for kind in ["internal", "unavailable"] {
        let core = FakeCore::new();
        core.inject("terminal.run", Injected::After(refused(kind)));
        let text = error_text(
            &create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err(),
        );
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
        let text = error_text(
            &create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err(),
        );
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
        core.clear_calls();
        core.on_get(2, move |terminals| {
            let terminal = terminals.get_mut("t7").unwrap();
            terminal["keep"] = json!(true);
            if change == "replacement" {
                terminal["pid"] = json!(4242);
            }
        });
        let text = error_text(&close_until(&core, &session, Some(directory), later()).unwrap_err());
        assert!(text.contains(expected), "{change}: {text}");
        assert!(core.params("terminal.kill").is_empty(), "{change}");
        assert!(core.params("terminal.rm").is_empty(), "{change}");
    }
    // After the kill: a program that replaced the ended one is not removed.
    let core = FakeCore::new();
    let session = created(&core);
    core.clear_calls();
    core.on_get(3, |terminals| {
        let terminal = terminals.get_mut("t7").unwrap();
        terminal["keep"] = json!(true);
        terminal["state"] = json!("live");
        terminal["pid"] = json!(4242);
    });
    assert!(close_until(&core, &session, Some(directory), later()).is_err());
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
    core.on_get(2, |terminals| {
        terminals.get_mut("t7").unwrap()["keep"] = json!(true);
    });
    let error = create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err();
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
    let text =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
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
    core.on_get(1, |terminals| {
        terminals.get_mut("t7").unwrap()["labels"][LAUNCH_LABEL] = json!("another launch");
    });
    let text =
        error_text(&create(&core, "launch", Path::new(DIRECTORY), later(), later()).unwrap_err());
    assert!(text.contains("creation is uncertain"), "{text}");
    assert!(core.params("terminal.kill").is_empty());
    assert_eq!(core.params("terminal.run").len(), 1);
    assert!(core.terminal("t7").is_some());
}
