//! A running Herdr Core at its own endpoint, served by this process, for tests that go through
//! the installed Herdr adapter: a Unix socket on macOS and Linux, a named pipe on Windows. It
//! answers `hello`, `terminal.get` for the terminals it holds and `terminal.rm` for a finished
//! one, and refuses every `terminal.run` before creating anything, counting those requests.
use super::record::HerdrHostBirth;
use super::{HerdrSurface, TerminalKind, TerminalSession};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

const CORE_ID: &str = "core-scripted";
const METHODS: [&str; 6] = [
    "terminal.run",
    "terminal.get",
    "terminal.kill",
    "terminal.rm",
    "terminal.send",
    "terminal.read",
];
const ADDRESS: &str = "t7";
const CREATED_MS: u64 = 1_794_000_000_007;
// Above Linux's largest PID and macOS's, and no multiple of four as every Windows PID is: no
// process holds it, so the adapter compares no birth for it.
const HOST_PID: u32 = 4_194_305;
const NONCE: &str = "0123456789abcdef0123456789abcdef";

pub(super) fn test_birth(start: u64) -> HerdrHostBirth {
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

pub(in crate::native) struct ScriptedHerdrCore {
    endpoint: PathBuf,
    runs: Arc<AtomicUsize>,
    terminals: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    #[cfg(unix)]
    _directory: tempfile::TempDir,
}

impl ScriptedHerdrCore {
    pub(in crate::native) fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    /// Runs `call` with `HC_SOCKET` naming this core and without a caller terminal, and
    /// restores both variables, also when `call` panics.
    pub(in crate::native) fn reached_by<T>(&self, call: impl FnOnce() -> T) -> T {
        let _restore = CoreEnvironment::point_at(&self.endpoint);
        call()
    }

    /// The terminal that a launch for `directory` created on this core, in `state` (`live` or
    /// `finished`) with `extra` facts, and the handle that launch recorded for it.
    pub(in crate::native) fn hold_launch(
        &self,
        directory: &Path,
        state: &str,
        extra: Value,
    ) -> TerminalSession {
        let id = directory.file_name().unwrap().to_str().unwrap();
        let mut terminal = json!({
            "address": ADDRESS, "state": state, "pid": HOST_PID,
            "argv": ["/opt/tabcli", TerminalKind::Herdr.registration().host_command,
                     directory.to_str().unwrap(), "--launch-command", "launch"],
            "keep": false, "rm": false, "created_ms": CREATED_MS,
            "name": "tabcli-0123456789abcdef01234567",
            "labels": {"tabcli-session": id, "tabcli-launch": NONCE},
        });
        if let (Some(terminal), Some(extra)) = (terminal.as_object_mut(), extra.as_object()) {
            terminal.extend(extra.clone());
        }
        self.terminals.lock().unwrap().push(terminal);
        let mut surface = TerminalSession::new(TerminalKind::Herdr, ADDRESS.into())
            .with_herdr_core(
                HerdrSurface {
                    socket: self.endpoint.to_str().unwrap().into(),
                    core_id: CORE_ID.into(),
                    created_ms: CREATED_MS,
                    host_pid: HOST_PID,
                    birth: test_birth(1),
                },
                NONCE,
            );
        surface.managed_session_id = Some(id.into());
        surface
    }

    /// Whether this core still holds the launch's terminal.
    pub(in crate::native) fn holds_launch(&self) -> bool {
        self.terminals
            .lock()
            .unwrap()
            .iter()
            .any(|terminal| terminal["address"] == ADDRESS)
    }
}

/// `HC_SOCKET` naming a core and no caller terminal, until dropped.
pub(in crate::native) struct CoreEnvironment {
    saved: [(&'static str, Option<std::ffi::OsString>); 2],
}

impl CoreEnvironment {
    pub(in crate::native) fn point_at(endpoint: &Path) -> Self {
        let saved = ["HC_SOCKET", "HC_TERMINAL_ID"].map(|name| (name, std::env::var_os(name)));
        // SAFETY: the test harness runs serially (`--test-threads=1`); the values are restored
        // when this guard is dropped, before the test goes on.
        unsafe {
            std::env::set_var("HC_SOCKET", endpoint);
            std::env::remove_var("HC_TERMINAL_ID");
        }
        Self { saved }
    }
}

impl Drop for CoreEnvironment {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

fn error(id: Value, kind: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {
        "code": -32000,
        "message": format!("{kind} by the scripted core"),
        "data": {"kind": kind},
    }})
}

// One request line, or `None` at the end of the connection.
fn request(stream: &mut impl Read) -> Option<Value> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    serde_json::from_slice(&line).ok()
}

fn serve(mut stream: impl Read + Write, runs: &AtomicUsize, terminals: &Mutex<Vec<Value>>) {
    while let Some(request) = request(&mut stream) {
        let id = request["id"].clone();
        let address = request["params"]["terminal"].clone();
        let mut terminals = terminals.lock().unwrap();
        let held = terminals
            .iter()
            .position(|terminal| terminal["address"] == address);
        let reply = match (request["method"].as_str(), held) {
            (Some("hello"), _) => json!({"jsonrpc": "2.0", "id": id, "result": {
                "generation": 1, "version": "0.1.21", "methods": METHODS,
                "core": {"id": CORE_ID, "name": "scripted"},
            }}),
            (Some("terminal.run"), _) => {
                runs.fetch_add(1, Ordering::SeqCst);
                error(id, "spawn_failed")
            }
            (Some("terminal.get"), Some(index)) => {
                json!({"jsonrpc": "2.0", "id": id, "result": terminals[index]})
            }
            (Some("terminal.rm"), Some(index)) if terminals[index]["state"] == "finished" => {
                terminals.remove(index);
                json!({"jsonrpc": "2.0", "id": id, "result": {"removed": address}})
            }
            (Some("terminal.rm"), Some(_)) => error(id, "status_mismatch"),
            _ => error(id, "not_found"),
        };
        drop(terminals);
        if stream
            .write_all(format!("{reply}\n").as_bytes())
            .and_then(|()| stream.flush())
            .is_err()
        {
            return;
        }
    }
}

#[cfg(unix)]
impl ScriptedHerdrCore {
    pub(in crate::native) fn start() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let private = std::fs::Permissions::from_mode(0o700);
        let directory = tempfile::Builder::new()
            .permissions(private.clone())
            .tempdir()
            .unwrap();
        // Restore owner access if the umask suppressed it at creation.
        std::fs::set_permissions(directory.path(), private).unwrap();
        let endpoint = directory.path().join("core.sock");
        let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let terminals = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server = thread::spawn({
            let (runs, terminals, stop) =
                (Arc::clone(&runs), Arc::clone(&terminals), Arc::clone(&stop));
            move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            serve(stream, &runs, &terminals);
                        }
                        Err(_) => thread::sleep(std::time::Duration::from_millis(5)),
                    }
                }
            }
        });
        Self {
            endpoint,
            runs,
            terminals,
            stop,
            server: Some(server),
            _directory: directory,
        }
    }

    fn wake(_: &Path) {}
}

// One listening instance at a time, created before the last one is served, as hc does: a
// client always finds the pipe.
#[cfg(windows)]
impl ScriptedHerdrCore {
    pub(in crate::native) fn start() -> Self {
        use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\tabcli-scripted-core-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let wide: Vec<u16> = endpoint
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let first = instance(&wide);
        let runs = Arc::new(AtomicUsize::new(0));
        let terminals = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server = thread::spawn({
            let (runs, terminals, stop) =
                (Arc::clone(&runs), Arc::clone(&terminals), Arc::clone(&stop));
            move || {
                let mut current = first;
                loop {
                    let raw = current as windows_sys::Win32::Foundation::HANDLE;
                    // A client that connected before this call is connected as well.
                    unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
                    let stream = unsafe { std::fs::File::from_raw_handle(raw) };
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    current = instance(&wide);
                    serve(stream, &runs, &terminals);
                }
            }
        });
        Self {
            endpoint,
            runs,
            terminals,
            stop,
            server: Some(server),
        }
    }

    // A client connection ends the server's wait for one.
    fn wake(endpoint: &Path) {
        let _ = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint);
    }
}

#[cfg(windows)]
fn instance(wide: &[u16]) -> usize {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_WAIT,
        },
    };
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

impl Drop for ScriptedHerdrCore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        Self::wake(&self.endpoint);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

#[cfg(unix)]
#[test]
fn the_scripted_socket_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let core = ScriptedHerdrCore::start();
    let permissions = core
        .endpoint
        .parent()
        .unwrap()
        .metadata()
        .unwrap()
        .permissions();
    assert_eq!(permissions.mode() & 0o777, 0o700);
}

// A call that panics leaves the variables as they were for the tests that follow.
#[test]
fn reached_by_restores_the_environment_when_its_call_panics() {
    let core = ScriptedHerdrCore::start();
    let names = ["HC_SOCKET", "HC_TERMINAL_ID"];
    let before = names.map(std::env::var_os);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        core.reached_by(|| panic!("the scripted call failed"))
    }));
    assert!(result.is_err());
    assert_eq!(names.map(std::env::var_os), before);
}
