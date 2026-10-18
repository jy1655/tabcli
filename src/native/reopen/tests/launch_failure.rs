use super::*;
use std::cell::RefCell;
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Stdio};
use std::rc::Rc;

const CHILD: &str = "TABCLI_REOPEN_LAUNCH_FAILURE_CHILD";
const SOURCE: &str = "session-launch-failure-source";
const FAILURE: &str = "injected terminal creation failure for issue 248";

#[derive(Clone, Copy, Debug)]
enum Fault {
    BeforeSpawn,
    ExitedProcess,
    LiveProcess,
    UnknownProcess,
    SpawnAttemptWithoutProcess,
    MissingReceipt,
    CorruptReceipt,
}

impl Fault {
    fn releases_source(self) -> bool {
        matches!(self, Self::BeforeSpawn | Self::ExitedProcess)
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let status = self.0.wait().unwrap();
        println!("fixture process pid={} reaped with {status}", self.0.id());
    }
}

fn deny_process_synchronize(child: &Child) {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SetKernelObjectSecurity};

    // Only this test-owned child's new handles lose SYNCHRONIZE. Its existing Child
    // handle still allows waiting, and EOF on its private stdin ends it without a kill.
    let sddl: Vec<u16> = "D:(A;;0x1000;;;WD)\0".encode_utf16().collect();
    let mut descriptor = std::ptr::null_mut();
    unsafe {
        assert_ne!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            ),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let changed =
            SetKernelObjectSecurity(child.as_raw_handle(), DACL_SECURITY_INFORMATION, descriptor);
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        assert_ne!(changed, 0, "{error}");
    }
}

fn install_fault(fault: Fault, process: Rc<RefCell<Option<OwnedChild>>>) {
    terminal::launch_failure_test_support::inject(move |directory| {
        let store = Store::open_unchecked(directory);
        let reader = Reader::open_unchecked(directory);
        let id = directory.file_name().unwrap().to_str().unwrap();
        let mut receipt = launch::read(&reader).unwrap().unwrap();
        assert_eq!(receipt.phase, launch::Phase::Pending);
        assert_eq!(
            turn::current_claim_token(&reader).unwrap().as_deref(),
            Some(receipt.claim_token.as_str())
        );
        assert!(reader.manifest().unwrap().yolo);
        match fault {
            Fault::BeforeSpawn => {}
            Fault::ExitedProcess | Fault::LiveProcess | Fault::UnknownProcess => {
                let mut command = Command::new("cmd.exe");
                command
                    .args(["/d", "/c", "pause"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut child = OwnedChild(
                    launch::spawn(&store, &mut command, |child| {
                        record_provider_process(directory, id, child)
                    })
                    .unwrap(),
                );
                assert_eq!(
                    launch::read(&reader).unwrap().unwrap().phase,
                    launch::Phase::Spawned
                );
                let record: ProviderProcessRecord =
                    reader.record(CoreRecord::ProviderProcess).json().unwrap();
                assert!(record.windows_process_identity.is_some());
                println!(
                    "provider process record={}",
                    serde_json::to_string(&record).unwrap()
                );
                match fault {
                    Fault::ExitedProcess => {
                        drop(child.0.stdin.take());
                        child.0.wait().unwrap();
                        assert_eq!(
                            observe_provider_process(&record),
                            ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
                        );
                    }
                    Fault::LiveProcess => assert_eq!(
                        observe_provider_process(&record),
                        ProviderProcessObservation::Alive
                    ),
                    Fault::UnknownProcess => {
                        deny_process_synchronize(&child.0);
                        assert!(matches!(
                            observe_provider_process(&record),
                            ProviderProcessObservation::Unknown(_)
                        ));
                    }
                    _ => unreachable!(),
                }
                println!(
                    "process evidence {:?}: pid={}, observation={:?}",
                    fault,
                    record.pid,
                    observe_provider_process(&record)
                );
                *process.borrow_mut() = Some(child);
            }
            Fault::SpawnAttemptWithoutProcess => {
                // This is the durable crash window between announcing a spawn and
                // recording its process. No process record cannot rule out a child.
                receipt.phase = launch::Phase::Spawning;
                store
                    .record(CoreRecord::Launch)
                    .write_json(&receipt)
                    .unwrap();
            }
            Fault::MissingReceipt => {
                store.record(CoreRecord::Launch).remove().unwrap();
            }
            Fault::CorruptReceipt => {
                fs::write(reader.record(CoreRecord::Launch).path(), b"{broken").unwrap();
            }
        }
        anyhow::anyhow!(FAILURE)
    });
}

fn request() -> ReopenRequest {
    let NativeCommand::Reopen(request) = parse_args([
        "reopen",
        SOURCE,
        "--prompt",
        "fixture only, never delivered",
        "--terminal",
        "windows-console",
        "--yolo",
        "--timeout-secs",
        "30",
        "--json",
    ])
    .unwrap() else {
        panic!("expected reopen");
    };
    request
}

fn reproduce(test_name: &str, fault: Fault) {
    if std::env::var_os(CHILD).is_none() {
        let test_name = test_name.split_once("::").unwrap().1;
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("state");
        let bin = base.path().join("bin");
        let config = base.path().join("claude");
        for path in [&root, &bin, &config.join("sessions")] {
            fs::create_dir_all(path).unwrap();
        }
        write_private(
            &bin.join("claude.cmd"),
            b"@echo off\r\nif \"%~1\"==\"--version\" (\r\n echo 2.1.281\r\n exit /b 0\r\n)\r\nexit /b 99\r\n",
        )
        .unwrap();
        let system_root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let path = std::env::join_paths([bin, system_root.join("System32"), system_root]).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env(STATE_DIR_ENV, &root)
            .env("HOME", base.path())
            .env("USERPROFILE", base.path())
            .env("CODEX_HOME", base.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", config)
            .env("PATH", path)
            .output()
            .unwrap();
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
        assert!(
            output.status.success(),
            "isolated {fault:?}: {}",
            output.status
        );
        return;
    }

    let root = Reader::state_root().unwrap();
    let source = write_closed_reopen_source(
        &root,
        SOURCE,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let original_source = snapshot_directory(&source);
    let process = Rc::new(RefCell::new(None));
    install_fault(fault, Rc::clone(&process));
    let error = run_reopen(request()).unwrap_err();
    assert!(format!("{error:#}").contains(FAILURE), "{error:#}");

    let created: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path.file_name().unwrap() != SOURCE
                && path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("session-")
        })
        .collect();
    assert_eq!(created.len(), 1);
    let directory = &created[0];
    let reader = Reader::open_unchecked(directory);
    let index = requests::list(&reader).unwrap();
    assert_eq!(index.receipts.len(), 1);
    assert_eq!(index.unreadable, 0);
    assert!(
        reader
            .initial_prompt()
            .unwrap()
            .contains("fixture only, never delivered")
    );
    assert!(reader.events().unwrap().is_empty());
    let marker_exists = source.join(REOPEN_MARKER_FILE).exists();
    let refusal = read_reopen_launch_refusal(directory);
    println!("scenario={fault:?}");
    println!("launcher error={error:#}");
    println!("launch receipt={:?}", launch::read(&reader));
    println!("source marker exists={marker_exists}");
    println!("launch refusal={refusal:?}");
    let mut after = snapshot_directory(&source);
    after.remove(REOPEN_MARKER_FILE);
    assert_eq!(
        after, original_source,
        "source history and ownership stay intact"
    );

    install_fault(Fault::BeforeSpawn, Rc::clone(&process));
    if fault.releases_source() {
        let second = run_reopen(request()).unwrap_err();
        println!("next reopen={second:#}");
        assert_ne!(
            reopen_refusal_gate(&second),
            Some("already-reopened"),
            "a failed launcher with safe cleanup must leave the source reopenable"
        );
        assert!(
            !marker_exists,
            "safe launch failure must release the source marker"
        );
    } else {
        assert!(
            marker_exists,
            "unverified cleanup must retain source ownership"
        );
        let before_retry = snapshot_directory(&source);
        let second = run_reopen(request()).unwrap_err();
        println!("next reopen={second:#}");
        assert_eq!(reopen_refusal_gate(&second), Some("already-reopened"));
        assert_eq!(snapshot_directory(&source), before_retry);
        println!("retention guard passed for {fault:?}");
    }
    let refusal = refusal.expect("generic launcher failure must record a launch-phase refusal");
    assert_eq!(refusal.gate, "launch-failed");
    assert_eq!(reader.status().unwrap().state, SessionState::Failed);
    if !fault.releases_source() {
        assert_ne!(refusal.gate, REOPEN_START_FAILED_GATE);
        assert_ne!(refusal.gate, REOPEN_LAUNCH_GATE);
    }
    drop(process);
}

#[test]
fn before_spawn_releases_source() {
    reproduce(
        concat!(module_path!(), "::before_spawn_releases_source"),
        Fault::BeforeSpawn,
    );
}

#[test]
fn after_verified_exit_releases_source() {
    reproduce(
        concat!(module_path!(), "::after_verified_exit_releases_source"),
        Fault::ExitedProcess,
    );
}

#[test]
fn live_process_retains_source_with_refusal() {
    reproduce(
        concat!(module_path!(), "::live_process_retains_source_with_refusal"),
        Fault::LiveProcess,
    );
}

#[test]
fn unknown_process_retains_source_with_refusal() {
    reproduce(
        concat!(
            module_path!(),
            "::unknown_process_retains_source_with_refusal"
        ),
        Fault::UnknownProcess,
    );
}

#[test]
fn spawn_attempt_without_process_retains_source_with_refusal() {
    reproduce(
        concat!(
            module_path!(),
            "::spawn_attempt_without_process_retains_source_with_refusal"
        ),
        Fault::SpawnAttemptWithoutProcess,
    );
}

#[test]
fn missing_receipt_retains_source_with_refusal() {
    reproduce(
        concat!(
            module_path!(),
            "::missing_receipt_retains_source_with_refusal"
        ),
        Fault::MissingReceipt,
    );
}

#[test]
fn corrupt_receipt_retains_source_with_refusal() {
    reproduce(
        concat!(
            module_path!(),
            "::corrupt_receipt_retains_source_with_refusal"
        ),
        Fault::CorruptReceipt,
    );
}
