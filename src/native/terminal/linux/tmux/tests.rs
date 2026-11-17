use super::*;
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    os::unix::process::ExitStatusExt,
    process::ExitStatus,
    rc::Rc,
};

const SERVER: u32 = 4242;

fn birth(boot: &str, ticks: u64) -> LinuxBirth {
    serde_json::from_value(serde_json::json!({"boot_id": boot, "starttime_ticks": ticks})).unwrap()
}
const BOOT: &str = "00112233-4455-6677-8899-aabbccddeeff";
const OTHER_BOOT: &str = "ffeeddcc-bbaa-9988-7766-554433221100";

enum Reply {
    Out(&'static str),
    Failed(&'static str),
    NotStarted,
}

#[derive(Default)]
struct ServerState {
    exited: Cell<bool>,
    exits_on_term: Cell<bool>,
    log: RefCell<Vec<&'static str>>,
}

struct FakeServer(Rc<ServerState>);
impl Server for FakeServer {
    fn terminate(&self) -> Result<()> {
        self.0.log.borrow_mut().push("terminate");
        if self.0.exits_on_term.get() {
            self.0.exited.set(true);
        }
        Ok(())
    }
    fn exited_by(&self, _deadline: Instant) -> Result<bool> {
        Ok(self.0.exited.get())
    }
}

struct Fake {
    replies: RefCell<VecDeque<(&'static str, Reply)>>,
    calls: RefCell<Vec<String>>,
    inputs: RefCell<Vec<Vec<u8>>>,
    births: RefCell<HashMap<u32, Option<LinuxBirth>>>,
    floor: LinuxBirth,
    exists: Cell<bool>,
    // From this path_exists call on, the socket exists (after the creating call).
    exists_from: Cell<Option<usize>>,
    exists_calls: Cell<usize>,
    server: Rc<ServerState>,
}

impl Fake {
    fn new() -> Self {
        let server = Rc::new(ServerState::default());
        server.exits_on_term.set(true);
        Self {
            replies: RefCell::default(),
            calls: RefCell::default(),
            inputs: RefCell::default(),
            births: RefCell::new(HashMap::from([(SERVER, Some(birth(BOOT, 100)))])),
            floor: birth(BOOT, 90),
            exists: Cell::new(false),
            exists_from: Cell::new(None),
            exists_calls: Cell::new(0),
            server,
        }
    }
    fn reply(self, command: &'static str, reply: Reply) -> Self {
        self.replies.borrow_mut().push_back((command, reply));
        self
    }
    fn commands(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Host for Fake {
    fn tmux(
        &self,
        socket: &Path,
        arguments: &[&OsStr],
        input: Option<&[u8]>,
        _deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure> {
        let text: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        self.calls
            .borrow_mut()
            .push(format!("{} {}", socket.display(), text.join(" ")));
        if let Some(input) = input {
            self.inputs.borrow_mut().push(input.to_vec());
        }
        let command = text
            .iter()
            .find(|argument| !argument.starts_with('-') && argument.as_str() != "/dev/null")
            .cloned()
            .unwrap_or_else(|| text[0].clone());
        let (expected, reply) = self
            .replies
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected tmux call: {text:?}"));
        assert_eq!(command, expected, "{text:?}");
        let output = |code: i32, stdout: &str, stderr: &str| Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        match reply {
            Reply::Out(stdout) => Ok(output(0, stdout, "")),
            Reply::Failed(stderr) => Ok(output(1, "", stderr)),
            Reply::NotStarted => Err(CommandOutputFailure::not_started(anyhow!(
                "tmux could not start"
            ))),
        }
    }
    fn creation_floor(&self) -> Result<LinuxBirth> {
        self.calls.borrow_mut().push("floor".into());
        Ok(self.floor)
    }
    fn birth(&self, pid: u32) -> Result<Option<LinuxBirth>> {
        Ok(self.births.borrow().get(&pid).copied().flatten())
    }
    fn server(&self, pid: u32, birth: LinuxBirth) -> Result<Option<Box<dyn Server>>> {
        if self.birth(pid)? != Some(birth) {
            return Ok(None);
        }
        Ok(Some(Box::new(FakeServer(Rc::clone(&self.server)))))
    }
    fn path_exists(&self, _path: &Path) -> Result<bool> {
        let call = self.exists_calls.get();
        self.exists_calls.set(call + 1);
        Ok(self.exists.get() || self.exists_from.get().is_some_and(|from| call >= from))
    }
    fn bridge_executable(&self) -> Result<PathBuf> {
        Ok(PathBuf::from("/opt/bridge/tabcli"))
    }
}

fn directory() -> PathBuf {
    PathBuf::from("/state/session-tmuxtest")
}

fn socket() -> PathBuf {
    directory().join(SOCKET_FILE)
}

fn handle() -> TerminalSession {
    TerminalSession {
        kind: TerminalKind::Tmux,
        id: "%0".into(),
        tab_id: Some("@0".into()),
        window_id: Some("$0".into()),
        managed_session_id: Some("session-tmuxtest".into()),
        windows_process_identity: None,
        wezterm_mux: None,
        linux_host: Some(LinuxHostIdentity {
            pid: SERVER,
            linux_birth: birth(BOOT, 100),
        }),
        tmux_socket: Some(socket().to_str().unwrap().into()),
    }
}

const VERSION: &str = "tmux 3.6\n";
const CREATED: &str = "4242\t$0\t@0\t%0\t/state/session-tmuxtest/tmux.sock\n";
const ONE_SESSION: &str = "4242\t$0\n";
const ONE_PANE: &str = "4242\t$0\t@0\t%0\t/dev/pts/9\n";

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn created(host: &Fake) -> Result<TerminalSession> {
    create(host, ". 'launch.sh'", &directory(), deadline())
}

#[test]
fn versions_name_their_release_or_none() {
    assert_eq!(parse_version("tmux 3.6\n"), Some((3, 6)));
    assert_eq!(parse_version("tmux 3.2a"), Some((3, 2)));
    assert_eq!(parse_version("tmux next-3.7"), Some((3, 7)));
    assert_eq!(parse_version("tmux master"), None);
    assert_eq!(parse_version("screen 4.9"), None);
    assert!((3, 1) < MINIMUM_VERSION && (3, 2) >= MINIMUM_VERSION);
}

#[test]
fn tmux_resolves_only_from_absolute_path_entries_to_its_canonical_path() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let linked = root.path().join("linked");
    let plain = root.path().join("plain");
    for directory in [&real, &linked, &plain] {
        std::fs::create_dir(directory).unwrap();
    }
    std::fs::write(real.join("tmux"), "").unwrap();
    std::fs::write(plain.join("tmux"), "").unwrap();
    std::fs::set_permissions(real.join("tmux"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(real.join("tmux"), linked.join("tmux")).unwrap();
    let path = std::env::join_paths([Path::new("relative"), &plain, &linked]).unwrap();
    assert_eq!(
        resolve_tmux_from_path(&path).unwrap(),
        real.join("tmux").canonicalize().unwrap()
    );
    let path = std::env::join_paths([Path::new("relative"), &plain]).unwrap();
    assert!(resolve_tmux_from_path(&path).is_err());
}

#[test]
fn creation_starts_one_detached_server_with_a_direct_host_program_and_proves_it() {
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Out(CREATED))
        .reply("list-sessions", Reply::Out(ONE_SESSION))
        .reply("list-panes", Reply::Out(ONE_PANE));
    let session = created(&host).unwrap();
    assert_eq!(
        session,
        TerminalSession {
            managed_session_id: None,
            ..handle()
        }
    );
    let socket = socket().display().to_string();
    assert_eq!(
        host.commands(),
        [
            format!("{socket} -V"),
            "floor".into(),
            format!(
                "{socket} -f /dev/null new-session -d -s agent-bridge -x 200 -y 50 -P -F {CREATED_FORMAT} -- /opt/bridge/tabcli native-tmux-host /state/session-tmuxtest --launch-command . 'launch.sh'"
            ),
            format!("{socket} list-sessions -F {SESSION_FORMAT}"),
            format!("{socket} list-panes -a -F {PANE_FORMAT}"),
        ]
    );
    // Claude lists the tmux session next to the Bridge session name; a session id here
    // would list the recipient twice.
    assert!(!host.commands()[2].contains("-s session-tmuxtest"));
    assert!(host.server.log.borrow().is_empty());
}

#[test]
fn creation_refuses_before_any_tmux_call_for_a_long_or_occupied_socket() {
    let host = Fake::new();
    let long = PathBuf::from(format!("/{}/session-long", "d".repeat(100)));
    let error = create(&host, "cmd", &long, deadline()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("more than the 107 a Unix socket address holds"),
        "{error:#}"
    );
    host.exists.set(true);
    let error = created(&host).unwrap_err();
    assert_eq!(
        error.to_string(),
        "refusing to start tmux: /state/session-tmuxtest/tmux.sock already exists"
    );
    assert!(host.commands().is_empty());
}

#[test]
fn creation_refuses_an_older_tmux_before_starting_a_server() {
    let host = Fake::new().reply("-V", Reply::Out("tmux 3.1c\n"));
    let error = created(&host).unwrap_err();
    assert_eq!(
        error.to_string(),
        "tmux 3.1c is older than tmux 3.2, which managed sessions require"
    );
    assert_eq!(host.commands().len(), 1);
}

#[test]
fn a_server_not_born_after_the_floor_on_this_boot_gets_no_authority() {
    for server_birth in [birth(BOOT, 89), birth(OTHER_BOOT, 100)] {
        let host = Fake::new()
            .reply("-V", Reply::Out(VERSION))
            .reply("new-session", Reply::Out(CREATED));
        host.births.borrow_mut().insert(SERVER, Some(server_birth));
        let error = created(&host).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the tmux server 4242 answering at /state/session-tmuxtest/tmux.sock was not started by this launch"
        );
        assert!(host.server.log.borrow().is_empty());
    }
}

#[test]
fn a_proven_server_that_fails_its_shape_is_ended_or_retained() {
    for (ends, sessions) in [
        (true, "4242\t$0\n4242\t$1\n"),
        (false, "4242\t$0\n4242\t$1\n"),
    ] {
        let host = Fake::new()
            .reply("-V", Reply::Out(VERSION))
            .reply("new-session", Reply::Out(CREATED))
            .reply("list-sessions", Reply::Out(sessions));
        host.server.exits_on_term.set(ends);
        let error = created(&host).unwrap_err();
        assert_eq!(*host.server.log.borrow(), ["terminate"]);
        let retained = error.downcast_ref::<RetainedLaunchSurface>();
        if ends {
            assert!(retained.is_none(), "{error:#}");
            assert_eq!(
                error.to_string(),
                "the new tmux server does not hold exactly its one session"
            );
        } else {
            let retained = retained.expect("an unconfirmed end retains the proven handle");
            assert_eq!(retained.surface.linux_host, handle().linux_host);
        }
    }
}

#[test]
fn a_failed_creating_call_ends_only_a_proven_residual_server() {
    // Nothing at the socket: the failure is all there is.
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Failed("create window failed"));
    let error = created(&host).unwrap_err();
    assert_eq!(
        error.to_string(),
        "tmux new-session failed: create window failed"
    );
    assert!(host.server.log.borrow().is_empty());

    // A server born after the floor answers at the socket: it is this launch's.
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Failed("lost"))
        .reply("display-message", Reply::Out("4242\n"));
    host.exists_from.set(Some(1));
    let error = created(&host).unwrap_err();
    assert_eq!(error.to_string(), "tmux new-session failed: lost");
    assert_eq!(*host.server.log.borrow(), ["terminate"]);

    // An unproven residual stays, without authority.
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Failed("lost"))
        .reply("display-message", Reply::Failed("no server running"));
    host.exists_from.set(Some(1));
    let error = format!("{:#}", created(&host).unwrap_err());
    assert!(
        error.contains(
            "a tmux server may remain at /state/session-tmuxtest/tmux.sock; it was not ended"
        ),
        "{error}"
    );
    assert!(host.server.log.borrow().is_empty());
}

fn prompt() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("prompt.txt");
    std::fs::write(&path, "\u{1b}[200~line one\nline two\u{1b}[201~").unwrap();
    (root, path)
}

#[test]
fn a_prompt_is_one_raw_buffer_pasted_into_exactly_its_pane() {
    let (_root, path) = prompt();
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("load-buffer", Reply::Out(""))
        .reply("paste-buffer", Reply::Out(""));
    send_file(&host, &handle(), &path, deadline()).unwrap();
    assert_eq!(
        *host.inputs.borrow(),
        [b"\x1b[200~line one\nline two\x1b[201~\r".to_vec()]
    );
    let commands = host.commands();
    let buffer = commands[1].split(' ').nth(3).unwrap().to_owned();
    assert!(buffer.starts_with("agent-bridge-"), "{commands:?}");
    let socket = socket().display().to_string();
    assert_eq!(commands[1], format!("{socket} load-buffer -b {buffer} -"));
    assert_eq!(
        commands[2],
        format!("{socket} paste-buffer -d -r -b {buffer} -t %0")
    );
}

#[test]
fn only_a_failure_before_the_paste_started_is_not_sent() {
    let (_root, path) = prompt();
    // The recorded server is gone: no tmux call at all.
    let host = Fake::new();
    host.births
        .borrow_mut()
        .insert(SERVER, Some(birth(BOOT, 101)));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert!(host.commands().is_empty());

    // The pane is gone.
    let host = Fake::new().reply("list-panes", Reply::Out("4242\t$0\t@0\t%1\t/dev/pts/9\n"));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert_eq!(host.commands().len(), 1);

    // The buffer was not loaded: its name is deleted, nothing is pasted.
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("load-buffer", Reply::Failed("no space"))
        .reply("delete-buffer", Reply::Out(""));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert!(host.commands()[2].contains("delete-buffer -b agent-bridge-"));

    // The paste could not start.
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("load-buffer", Reply::Out(""))
        .reply("paste-buffer", Reply::NotStarted)
        .reply("delete-buffer", Reply::Out(""));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());

    // The paste started and failed: the pane may have the input; nothing is replayed, and
    // only the buffer is deleted.
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("load-buffer", Reply::Out(""))
        .reply("paste-buffer", Reply::Failed("can't find pane: %0"))
        .reply("delete-buffer", Reply::Out(""));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(failure.delivery_may_have_occurred());
    assert_eq!(host.commands().len(), 4);
}

#[test]
fn presence_is_absence_only_when_the_recorded_server_incarnation_is_gone() {
    // Exited, reused, or another boot: absent without asking the socket.
    for now in [None, Some(birth(BOOT, 101)), Some(birth(OTHER_BOOT, 100))] {
        let host = Fake::new();
        host.births.borrow_mut().insert(SERVER, now);
        assert!(!surface_present(&host, &handle(), Duration::from_secs(1)).unwrap());
        assert!(host.commands().is_empty());
    }
    let host = Fake::new().reply("list-panes", Reply::Out(ONE_PANE));
    assert!(surface_present(&host, &handle(), Duration::from_secs(1)).unwrap());
    // The user's own windows keep the server; this pane is gone.
    let host = Fake::new().reply("list-panes", Reply::Out("4242\t$0\t@1\t%1\t/dev/pts/9\n"));
    assert!(!surface_present(&host, &handle(), Duration::from_secs(1)).unwrap());
    // A live recorded server whose socket refuses is an error, never absence.
    let host = Fake::new().reply("list-panes", Reply::Failed("no server running on /state"));
    assert!(surface_present(&host, &handle(), Duration::from_secs(1)).is_err());
    // Another server answering at the socket, or the pane moved: errors.
    for listing in [
        "4343\t$0\t@0\t%0\t/dev/pts/9\n",
        "4242\t$0\t@1\t%0\t/dev/pts/9\n",
    ] {
        let host = Fake::new().reply("list-panes", Reply::Out(listing));
        assert!(surface_present(&host, &handle(), Duration::from_secs(1)).is_err());
    }
}

#[test]
fn verification_returns_the_tty_of_exactly_its_pane() {
    let host = Fake::new().reply("list-panes", Reply::Out(ONE_PANE));
    assert_eq!(
        verify_session(&host, &handle(), Some(Duration::from_secs(1))).unwrap(),
        "/dev/pts/9"
    );
    let host = Fake::new().reply("list-panes", Reply::Out("4242\t$0\t@0\t%0\t\n"));
    assert!(verify_session(&host, &handle(), Some(Duration::from_secs(1))).is_err());
    for broken in [
        TerminalSession {
            linux_host: None,
            ..handle()
        },
        TerminalSession {
            tmux_socket: Some("relative/tmux.sock".into()),
            ..handle()
        },
        TerminalSession {
            tmux_socket: Some("/state/other.sock".into()),
            ..handle()
        },
        TerminalSession {
            tab_id: None,
            ..handle()
        },
        TerminalSession {
            kind: TerminalKind::WezTerm,
            ..handle()
        },
    ] {
        let host = Fake::new();
        assert!(verify_session(&host, &broken, Some(Duration::from_secs(1))).is_err());
        assert!(host.commands().is_empty());
    }
}

#[test]
fn the_screen_is_the_visible_pane_text() {
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("capture-pane", Reply::Out("> ready\n"));
    assert_eq!(
        read_screen(&host, &handle(), deadline()).unwrap(),
        "> ready\n"
    );
    assert!(host.commands()[1].ends_with("capture-pane -p -t %0"));
}

#[test]
fn ending_the_server_signals_its_incarnation_once_then_forgets_the_socket() {
    let host = Fake::new();
    assert_eq!(
        end_server(&host, &handle(), deadline()).unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(*host.server.log.borrow(), ["terminate"]);
    // Repeated close, or a server that already ended: never signalled again.
    assert_eq!(
        end_server(&host, &handle(), deadline()).unwrap(),
        CloseOutcome::Missing
    );
    assert_eq!(*host.server.log.borrow(), ["terminate"]);
    host.births.borrow_mut().insert(SERVER, None);
    assert_eq!(
        end_server(&host, &handle(), deadline()).unwrap(),
        CloseOutcome::Missing
    );
    assert_eq!(*host.server.log.borrow(), ["terminate"]);
    // A reused PID is not the recorded server: nothing is signalled.
    let host = Fake::new();
    host.births
        .borrow_mut()
        .insert(SERVER, Some(birth(BOOT, 101)));
    assert_eq!(
        end_server(&host, &handle(), deadline()).unwrap(),
        CloseOutcome::Missing
    );
    assert!(host.server.log.borrow().is_empty());
    // A server that does not exit keeps its socket and reports the failure.
    let host = Fake::new();
    host.server.exits_on_term.set(false);
    let error = end_server(&host, &handle(), deadline()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "tmux server 4242 of this session did not exit before the close deadline"
    );
}

#[test]
fn a_binding_names_this_host_only_through_its_server_and_pane() {
    let socket = "/state/a,b/session-tmuxtest/tmux.sock";
    let directory = Path::new("/state/a,b/session-tmuxtest");
    let surface = TerminalSession {
        tmux_socket: Some(socket.into()),
        ..handle()
    };
    let names = |surface: &TerminalSession, tmux: &str, pane: &str| {
        binding_names_host(
            surface,
            directory,
            Some(OsStr::new(tmux)),
            Some(OsStr::new(pane)),
        )
    };
    assert!(names(&surface, &format!("{socket},4242,0"), "%0"));
    assert!(!names(&surface, &format!("{socket},4343,0"), "%0"));
    assert!(!names(&surface, &format!("{socket},4242,0"), "%1"));
    assert!(!names(&surface, "/other/tmux.sock,4242,0", "%0"));
    assert!(!names(&surface, "4242,0", "%0"));
    assert!(!names(
        &handle(),
        "/state/session-tmuxtest/tmux.sock,4242,0",
        "%0"
    ));
    assert!(!binding_names_host(
        &surface,
        directory,
        None,
        Some(OsStr::new("%0"))
    ));
}

#[test]
fn the_attach_command_quotes_its_socket() {
    let surface = TerminalSession {
        tmux_socket: Some("/state/it's/session-x/tmux.sock".into()),
        ..handle()
    };
    assert_eq!(
        attach_command(&surface).unwrap(),
        "tmux -S '/state/it'\"'\"'s/session-x/tmux.sock' attach"
    );
    assert_eq!(
        attach_command(&TerminalSession {
            kind: TerminalKind::WezTerm,
            ..surface
        }),
        None
    );
}

#[test]
fn the_pane_program_runs_the_host_without_a_shell() {
    let program = host_program(Path::new("/b/tabcli"), Path::new("/s/session-x"), "a 'b'");
    assert_eq!(
        program,
        [
            "/b/tabcli",
            "native-tmux-host",
            "/s/session-x",
            "--launch-command",
            "a 'b'"
        ]
    );
    assert!(matches!(
        crate::native::terminal::host::parse(
            &program[1].to_string_lossy(),
            &program[2..]
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        )
        .unwrap(),
        crate::native::terminal::host::HostCommand::Tmux { command, .. } if command == "a 'b'"
    ));
}

// The production close observation of a tmux handle reads the recorded server's procfs
// birth: live grants a surface-only close, exited/reused/another boot proves absence.
#[test]
fn production_close_evidence_classifies_the_recorded_server_incarnation() {
    use crate::native::terminal::close_authority::{CloseObservation, Production};
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let live = process_birth(child.id()).unwrap().unwrap();
    let with = |pid: u32, linux_birth: LinuxBirth| TerminalSession {
        linux_host: Some(LinuxHostIdentity { pid, linux_birth }),
        ..handle()
    };
    let observe = |surface: &TerminalSession| {
        Production::new(root.path(), "session-tmuxtest", surface, deadline())
            .created_host_incarnation()
    };
    assert_eq!(
        observe(&with(child.id(), live)).unwrap(),
        Some(ProcessObservation::Live(Attested::Incarnation))
    );
    let mut later = live;
    later.starttime_ticks += 1;
    assert!(matches!(
        observe(&with(child.id(), later)).unwrap(),
        Some(ProcessObservation::Gone(Gone::Reused(_)))
    ));
    let other_boot = LinuxBirth {
        boot_id: OTHER_BOOT.parse().unwrap(),
        ..live
    };
    assert!(matches!(
        observe(&with(child.id(), other_boot)).unwrap(),
        Some(ProcessObservation::Gone(Gone::Reused(_)))
    ));
    assert!(
        observe(&TerminalSession {
            linux_host: None,
            ..handle()
        })
        .is_err()
    );
    assert!(
        observe(&TerminalSession {
            managed_session_id: Some("session-other".into()),
            ..with(child.id(), live)
        })
        .is_err()
    );
    assert_eq!(
        observe(&TerminalSession {
            kind: TerminalKind::Iterm2,
            ..handle()
        })
        .unwrap(),
        None
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        observe(&with(child.id(), live)).unwrap(),
        Some(ProcessObservation::Gone(Gone::Exited))
    );
}

#[test]
fn the_adapter_creates_through_the_shared_bind_and_cleans_up_a_failed_bind() {
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Out(CREATED))
        .reply("list-sessions", Reply::Out(ONE_SESSION))
        .reply("list-panes", Reply::Out(ONE_PANE));
    let adapter = Adapter::with_host(host);
    let unbound = Cell::new(false);
    let error = adapter
        .open_bound_tab(
            "cmd",
            &directory(),
            deadline(),
            Box::new(|session: &mut TerminalSession| {
                session.managed_session_id = Some("session-tmuxtest".into());
                bail!("bind failed")
            }),
            Box::new(|| {
                unbound.set(true);
                Ok(())
            }),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "bind failed");
    assert!(unbound.get());
    assert_eq!(*adapter.host.server.log.borrow(), ["terminate"]);
}

// The installed tmux, with a pane program that echoes its input instead of this Bridge's
// host: creation, proof, paste, screen and end run against a real server.
struct RealTmux(PathBuf);
impl Host for RealTmux {
    fn tmux(
        &self,
        socket: &Path,
        arguments: &[&OsStr],
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> std::result::Result<Output, CommandOutputFailure> {
        Installed.tmux(socket, arguments, input, deadline)
    }
    fn creation_floor(&self) -> Result<LinuxBirth> {
        Installed.creation_floor()
    }
    fn birth(&self, pid: u32) -> Result<Option<LinuxBirth>> {
        Installed.birth(pid)
    }
    fn server(&self, pid: u32, birth: LinuxBirth) -> Result<Option<Box<dyn Server>>> {
        Installed.server(pid, birth)
    }
    fn path_exists(&self, path: &Path) -> Result<bool> {
        Installed.path_exists(path)
    }
    fn bridge_executable(&self) -> Result<PathBuf> {
        Ok(self.0.clone())
    }
}

#[test]
fn a_real_tmux_server_is_created_proven_written_read_and_ended() {
    use std::os::unix::fs::PermissionsExt;
    if resolve_tmux().is_err() {
        eprintln!("tmux is not installed; the real tmux check is skipped");
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("tmx")
        .tempdir_in("/tmp")
        .unwrap();
    let directory = root.path().join("session-realtmux");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = root.path().join("pane.sh");
    std::fs::write(&program, "#!/bin/sh\nexec cat\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = RealTmux(program);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut session = create(&host, "unused", &directory, deadline).unwrap();
    session.managed_session_id = Some("session-realtmux".into());
    let ended = || end_server(&host, &session, deadline);
    let checks = (|| -> Result<()> {
        let tty = verify_session(&host, &session, Some(Duration::from_secs(5)))?;
        anyhow::ensure!(Path::new(&tty).exists(), "{tty}");
        anyhow::ensure!(surface_present(&host, &session, Duration::from_secs(5))?);
        let prompt = root.path().join("prompt.txt");
        std::fs::write(&prompt, "hello from the bridge")?;
        send_file(&host, &session, &prompt, deadline).map_err(|failure| failure.into_error())?;
        let screen_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = read_screen(&host, &session, deadline)?;
            if screen.matches("hello from the bridge").count() >= 2 {
                break;
            }
            anyhow::ensure!(Instant::now() < screen_deadline, "{screen:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();
    assert_eq!(ended().unwrap(), CloseOutcome::Closed);
    checks.unwrap();
    assert!(!surface_present(&host, &session, Duration::from_secs(5)).unwrap());
    assert_eq!(ended().unwrap(), CloseOutcome::Missing);
}

#[test]
fn the_launch_bash_runs_no_startup_file_but_keeps_bash_env_for_the_provider() {
    let root = tempfile::tempdir().unwrap();
    let startup = root.path().join("startup.sh");
    let ran = root.path().join("ran");
    let seen = root.path().join("seen");
    std::fs::write(&startup, format!("touch '{}'\n", ran.display())).unwrap();
    let command = format!("printf '%s' \"$BASH_ENV\" > '{}'", seen.display());
    let script = bash_script(&command, Some(startup.as_os_str()));
    let BashScript::WithoutStartupFile(script) = script else {
        panic!("{script:?}")
    };
    let status = Command::new("/bin/bash")
        .env("BASH_ENV", &startup)
        .env_remove("BASH_ENV")
        .args(["--noprofile", "--norc", "-m", "-c", &script])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!ran.exists(), "BASH_ENV ran before the launch command");
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        startup.to_str().unwrap()
    );
    assert_eq!(bash_script("cmd", None), BashScript::Plain("cmd".into()));
    use std::os::unix::ffi::OsStrExt;
    assert_eq!(
        bash_script("cmd", Some(OsStr::from_bytes(b"/x\xff"))),
        BashScript::WithoutStartupFile("cmd".into())
    );
}

#[test]
fn a_delivery_keeps_time_to_delete_its_buffer_after_any_failed_paste() {
    let (_root, path) = prompt();
    // Too short to keep the reserve: nothing is loaded.
    let host = Fake::new();
    let failure = send_file(
        &host,
        &handle(),
        &path,
        Instant::now() + Duration::from_millis(500),
    )
    .unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert!(host.commands().is_empty());
    // A started paste that failed (the pane went away) still deletes the request's buffer.
    let host = Fake::new()
        .reply("list-panes", Reply::Out(ONE_PANE))
        .reply("load-buffer", Reply::Out(""))
        .reply("paste-buffer", Reply::Failed("can't find pane: %0"))
        .reply("delete-buffer", Reply::Out(""));
    let failure = send_file(&host, &handle(), &path, deadline()).unwrap_err();
    assert!(failure.delivery_may_have_occurred());
    let commands = host.commands();
    let buffer = commands[1].split(' ').nth(3).unwrap();
    assert_eq!(
        commands[3],
        format!("{} delete-buffer -b {buffer}", socket().display())
    );
}

#[test]
fn a_server_is_not_signalled_once_the_close_deadline_has_passed() {
    let host = Fake::new();
    let error = end_server(&host, &handle(), Instant::now()).unwrap_err();
    assert!(
        error.to_string().contains("close deadline expired"),
        "{error:#}"
    );
    assert!(host.server.log.borrow().is_empty());
}

#[test]
fn a_failed_bind_whose_server_does_not_end_retains_the_proven_server() {
    let host = Fake::new()
        .reply("-V", Reply::Out(VERSION))
        .reply("new-session", Reply::Out(CREATED))
        .reply("list-sessions", Reply::Out(ONE_SESSION))
        .reply("list-panes", Reply::Out(ONE_PANE));
    host.server.exits_on_term.set(false);
    let adapter = Adapter::with_host(host);
    let error = adapter
        .open_bound_tab(
            "cmd",
            &directory(),
            deadline(),
            Box::new(|_: &mut TerminalSession| bail!("bind failed")),
            Box::new(|| panic!("an unconfirmed cleanup keeps its binding")),
        )
        .unwrap_err();
    let retained = error
        .downcast_ref::<RetainedLaunchSurface>()
        .expect("the proven server is handed back");
    assert_eq!(retained.surface.linux_host, handle().linux_host);
    assert_eq!(retained.surface.tmux_socket, handle().tmux_socket);
    assert!(
        format!("{error:#}").starts_with("bind failed; exact surface cleanup also failed: "),
        "{error:#}"
    );
}
