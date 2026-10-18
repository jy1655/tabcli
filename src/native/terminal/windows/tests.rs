use super::security::private_sddl;
use super::{
    build_console_input_records, console_command_line, console_creation_flags,
    open_verified_control_process, query_process_identity, query_process_identity_from_handle,
    resolve_executable_from_path, verify_control_process_identity, verify_process_identity,
};
use crate::native::console_control::ConsoleControlRequest;
use crate::native::session::primitives::write_json_atomic;
use crate::native::session::test_support::{
    SESSION_OWNER_FILE, TERMINAL_CLOSING_FILE, TERMINAL_HANDLE_FILE,
};
#[cfg(windows)]
use crate::native::terminal::ownership::current_native_session_owner;
#[cfg(windows)]
use crate::native::terminal::ownership::verified_windows_native_owner;
use crate::native::terminal::windows_console_handle_path;
use crate::native::{provider_process, terminal};
use std::fs;
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{TRUE, WAIT_OBJECT_0};
use windows_sys::Win32::Globalization::{CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Console::INPUT_RECORD;
use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, OpenProcess, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE;

#[test]
fn dialog_decision_sends_only_when_screen_text_matches() {
    let screen = "Trust this folder?\n> Yes  No\n";
    assert_eq!(
        super::dialog_decision(screen, screen),
        super::DialogDecision::Send
    );
}

#[test]
fn dialog_decision_rejects_a_single_changed_cell() {
    let screen = "Trust this folder?\n> Yes  No\n";
    for (index, byte) in screen.bytes().enumerate() {
        if byte == b'\n' {
            continue;
        }
        let mut changed = screen.as_bytes().to_vec();
        changed[index] = b'X';
        assert_eq!(
            super::dialog_decision(screen, std::str::from_utf8(&changed).unwrap()),
            super::DialogDecision::Changed,
            "cell {index}"
        );
    }
}

#[test]
fn managed_console_removes_inherited_term_before_starting_the_bridge() {
    let command = console_command_line("Write-Output 'bridge'");

    assert!(command.contains("Remove-Item Env:TERM"));
    assert!(command.ends_with("Write-Output 'bridge'"));
}

#[test]
fn powershell_resolution_uses_only_absolute_path_entries() {
    let directory = tempfile::tempdir().unwrap();
    let relative = directory.path().join("relative");
    let trusted = directory.path().join("trusted");
    fs::create_dir_all(&relative).unwrap();
    fs::create_dir_all(&trusted).unwrap();
    fs::write(relative.join("pwsh.exe"), b"planted").unwrap();
    fs::write(trusted.join("pwsh.exe"), b"trusted").unwrap();

    let path = std::env::join_paths([std::path::Path::new("relative"), trusted.as_path()]).unwrap();
    assert_eq!(
        resolve_executable_from_path("pwsh.exe", &path).unwrap(),
        trusted.join("pwsh.exe").canonicalize().unwrap()
    );
}

#[test]
fn console_launch_is_suspended_until_process_identity_is_recorded() {
    assert_ne!(console_creation_flags() & CREATE_SUSPENDED, 0);
}

#[test]
fn a_managed_surface_is_shown_without_being_activated() {
    use windows_sys::Win32::System::Threading::{STARTF_USESHOWWINDOW, STARTUPINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

    let startup = super::console_startup_info();
    assert_eq!(startup.cb as usize, std::mem::size_of::<STARTUPINFOW>());
    assert_eq!(startup.dwFlags, STARTF_USESHOWWINDOW);
    assert_eq!(i32::from(startup.wShowWindow), SW_SHOWNOACTIVATE);
}

#[test]
fn a_screen_row_holds_fewer_characters_than_cells_when_some_are_full_width() {
    // "한글" fills four of six cells; the console returns it as two characters, followed
    // by the two blank cells, and leaves the rest of the buffer untouched.
    let mut cells = "한글  ".encode_utf16().collect::<Vec<_>>();
    cells.resize(6, 0);

    assert_eq!(super::screen_row(&cells, 4).unwrap(), "한글  ");
    assert_eq!(super::screen_row(&cells, 0).unwrap(), "");
    assert!(super::screen_row(&cells, 7).is_err());
}

#[test]
fn startup_cleanup_rejects_unconfirmed_process_termination() {
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, std::process::id()) };
    assert!(!handle.is_null());
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

    let error = super::terminate_process_until(
        handle.as_raw_handle(),
        Instant::now() + Duration::from_millis(50),
    )
    .unwrap_err();

    assert!(error.to_string().contains("managed Windows console"));
    assert!(agent_bridge::process_is_alive(std::process::id()));
}

#[test]
fn startup_cleanup_waits_for_the_suspended_process_to_exit() {
    let mut command = std::process::Command::new(std::env::var_os("ComSpec").unwrap());
    command
        .args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"])
        .creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn().unwrap();

    super::terminate_process_until(
        child.as_raw_handle(),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();

    assert!(child.try_wait().unwrap().is_some());
}

// A suspended process with the handle that a close retains for every console process.
fn console_process() -> (std::process::Child, super::ConsoleProcess) {
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };
    let child = std::process::Command::new(std::env::var_os("ComSpec").unwrap())
        .args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"])
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .unwrap();
    let handle = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            child.id(),
        )
    };
    assert!(!handle.is_null());
    let process = super::ConsoleProcess {
        pid: child.id(),
        handle: unsafe { OwnedHandle::from_raw_handle(handle) },
    };
    (child, process)
}

#[test]
fn a_close_ends_the_tab_host_last_and_without_a_failure_code() {
    let (mut other, other_process) = console_process();
    let (mut root, root_process) = console_process();
    let (mut host, host_process) = console_process();
    let (root_pid, host_pid) = (root.id(), host.id());
    // In the order a console lists them: the host of a tab comes first.
    let processes = [host_process, root_process, other_process];

    super::terminate_console_processes(&processes, root_pid, Some(host_pid)).unwrap();

    // Windows Terminal closes a tab whose own process ended without a failure code.
    assert_eq!(host.wait().unwrap().code(), Some(0));
    assert_eq!(root.wait().unwrap().code(), Some(1));
    assert_eq!(other.wait().unwrap().code(), Some(1));

    // A console window has no host: every process ends with the failure code.
    let (mut root, root_process) = console_process();
    let (mut other, other_process) = console_process();
    let root_pid = root.id();
    super::terminate_console_processes(&[root_process, other_process], root_pid, None).unwrap();
    assert_eq!(root.wait().unwrap().code(), Some(1));
    assert_eq!(other.wait().unwrap().code(), Some(1));
}

#[test]
fn the_tab_host_is_the_console_process_that_recorded_itself() {
    let (mut host, host_process) = console_process();
    let (mut root, root_process) = console_process();
    let recorded = (host.id(), query_process_identity(host.id()).unwrap());
    let in_tab = [root_process, host_process];

    assert_eq!(
        super::tab_host_among(&in_tab, Some(&recorded)),
        Some(host.id())
    );
    // A console window has no host, and neither has a session whose host did not
    // record itself.
    assert_eq!(super::tab_host_among(&in_tab, None), None);
    // The host has left the console, as it does once the root has attached.
    assert_eq!(super::tab_host_among(&in_tab[..1], Some(&recorded)), None);
    // Another process has the pid that the host once had.
    let mut reused = recorded.clone();
    reused.1.creation_time = reused.1.creation_time.wrapping_add(1);
    assert_eq!(super::tab_host_among(&in_tab, Some(&reused)), None);

    for process in [&mut host, &mut root] {
        process.kill().unwrap();
        process.wait().unwrap();
    }
}

#[test]
fn console_close_uses_the_window_close_message_instead_of_ctrl_break() {
    assert_eq!(super::console_close_message(), WM_CLOSE);
    assert_ne!(
        super::console_close_message(),
        windows_sys::Win32::System::Console::CTRL_BREAK_EVENT
    );
}

#[test]
fn process_identity_rejects_reused_pid_creation_time() {
    let pid = std::process::id();
    let identity = query_process_identity(pid).unwrap();
    verify_process_identity(pid, identity.creation_time, &identity.executable_path).unwrap();
    assert!(
        verify_process_identity(
            pid,
            identity.creation_time.wrapping_add(1),
            &identity.executable_path,
        )
        .is_err()
    );
}

#[test]
fn verified_control_handle_retains_the_exact_process_object_after_exit() {
    let mut child = std::process::Command::new(super::powershell_executable().unwrap())
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ])
        .spawn()
        .unwrap();
    let identity = query_process_identity(child.id()).unwrap();
    let retained = open_verified_control_process(child.id(), &identity).unwrap();
    assert_eq!(
        query_process_identity_from_handle(retained.as_raw_handle()).unwrap(),
        identity
    );

    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(
        unsafe { WaitForSingleObject(retained.as_raw_handle(), 0) },
        WAIT_OBJECT_0
    );
}

#[test]
fn missing_console_process_converges_to_the_standard_missing_result() {
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let error = verify_control_process_identity(
        pid,
        &super::WindowsProcessIdentity {
            creation_time: 0,
            executable_path: String::new(),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("no longer available"));
}

#[test]
fn submission_return_waits_until_the_console_input_buffer_is_drained() {
    let start = Instant::now();
    let mut pending = [3, 2, 0].into_iter();
    let mut calls = 0;
    super::wait_for_console_input_drain_with(
        || {
            calls += 1;
            Ok(pending.next().unwrap())
        },
        start + Duration::from_secs(1),
        Duration::ZERO,
        || start,
    )
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(pending.next(), None);
}

#[test]
fn submission_return_drain_times_out_at_the_deadline_as_uncertain() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(100);
    let mut clock = [start, start, deadline].into_iter();
    let mut calls = 0;
    let error = super::wait_for_console_input_drain_with(
        || {
            calls += 1;
            Ok(3)
        },
        deadline,
        Duration::ZERO,
        || clock.next().unwrap(),
    )
    .unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(
        error.to_string(),
        "Windows console input was not consumed before the submission Return"
    );
    assert!(
        !super::super::windows_budget::windows_console_helper_reports_send_not_started(
            &error.to_string()
        )
    );
}

#[test]
fn codex_paste_confirmation_and_submission_are_delayed_from_the_text_batch() {
    let immediate = super::super::windows_budget::windows_console_immediate_submit_count(2);
    let initial_records =
        build_console_input_records("prompt", immediate, super::ConsoleNewline::ShiftReturn);
    assert_eq!(return_key_down_count(&initial_records), 0);

    let delayed_batches = (immediate..2)
        .map(|_| build_console_input_records("", 1, super::ConsoleNewline::ShiftReturn))
        .collect::<Vec<_>>();
    assert_eq!(delayed_batches.len(), 2);
    assert!(
        delayed_batches
            .iter()
            .all(|records| return_key_down_count(records) == 1)
    );

    let records = &delayed_batches[0];
    let down = unsafe { records[records.len() - 2].Event.KeyEvent };
    let up = unsafe { records[records.len() - 1].Event.KeyEvent };
    assert_eq!(down.bKeyDown, 1);
    assert_eq!(up.bKeyDown, 0);
    assert_eq!(down.wVirtualKeyCode, 0x0d);
    assert_eq!(down.wVirtualScanCode, 0x1c);
    assert_eq!(unsafe { down.uChar.UnicodeChar }, u16::from(b'\r'));
}

#[test]
fn single_submit_provider_gets_one_return_key() {
    let records = build_console_input_records("prompt", 1, super::ConsoleNewline::LineFeed);
    assert_eq!(
        records
            .iter()
            .filter(|record| unsafe {
                record.Event.KeyEvent.bKeyDown == 1 && record.Event.KeyEvent.wVirtualKeyCode == 0x0d
            })
            .count(),
        1
    );
}

fn return_key_down_count(records: &[INPUT_RECORD]) -> usize {
    records
        .iter()
        .filter(|record| unsafe {
            record.Event.KeyEvent.bKeyDown == 1 && record.Event.KeyEvent.wVirtualKeyCode == 0x0d
        })
        .count()
}

#[test]
fn private_acl_is_one_protected_current_user_entry() {
    assert_eq!(
        private_sddl("S-1-5-21-1", true),
        "D:P(A;OICI;FA;;;S-1-5-21-1)"
    );
    assert_eq!(private_sddl("S-1-5-21-1", false), "D:P(A;;FA;;;S-1-5-21-1)");
}

#[test]
fn close_deadline_console_helper_is_bounded() {
    let powershell = super::powershell_executable().unwrap();
    let mut command = std::process::Command::new(&powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 30",
    ]);
    let started = Instant::now();
    let error =
        super::run_console_helper_command(&mut command, started + Duration::from_millis(100))
            .unwrap_err();
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    // The timeout error alone does not prove the bound: a helper that failed to end the
    // command would report it too, after waiting for the 30 s sleep. 25 s proves that the
    // helper did not wait for the sleep, whether it ended the running command or timed out
    // before PowerShell had started, and leaves a loaded runner room to start and end
    // PowerShell; a 5 s bound let a slow runner decide the result, as the 10 s deadline
    // below did (#180).
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(25), "{elapsed:?}");
    let mut command = std::process::Command::new(&powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Error.WriteLine('own close timeout'); exit 1",
    ]);
    // This case checks that the command's own message survives a failure before the
    // deadline, so the deadline must not expire first. PowerShell did not print within
    // 10 s on a loaded `windows-latest` runner (#180). The command exits at once, so the
    // test never waits this long.
    let error =
        super::run_console_helper_command(&mut command, Instant::now() + Duration::from_secs(120))
            .unwrap_err();
    assert_eq!(error.to_string(), "own close timeout");
}

// Windows keeps `AB_REVIEW_SS` and `AB_REVIEW_ß` apart, and a key built with Rust's
// `to_uppercase` merged them (#148). ASCII case variants stay one variable, the hidden
// `=C:` name stays, and the block is in the order of the system's own comparison.
#[test]
fn environment_block_compares_names_as_windows_does() {
    let variables = [
        ("path", "C:\\lower"),
        ("AB_REVIEW_ß", "sharp-s"),
        ("=C:", "C:\\"),
        ("AB_REVIEW_SS", "double-s"),
        ("ab_review_case", "lower-case"),
        ("PATH", "C:\\upper"),
    ]
    .map(|(name, value)| {
        (
            std::ffi::OsString::from(name),
            std::ffi::OsString::from(value),
        )
    });
    let block = super::tab::environment_block_from(variables, None);

    let entries = block
        .strip_suffix(&[0, 0])
        .expect("the block ends with two NULs")
        .split(|unit| *unit == 0)
        .map(|entry| {
            // A hidden name begins with `=`, so the separator is the first `=` after it.
            let separator = 1 + entry[1..]
                .iter()
                .position(|unit| *unit == u16::from(b'='))
                .expect("every entry has a value");
            (
                String::from_utf16(&entry[..separator]).unwrap(),
                String::from_utf16(&entry[separator + 1..]).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let names = entries
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 5, "{entries:?}");
    assert_eq!(
        names[..4],
        ["=C:", "ab_review_case", "AB_REVIEW_SS", "AB_REVIEW_ß"]
    );
    // The later of two names that Windows treats as one is kept, with its own spelling.
    assert_eq!(
        entries[4],
        ("PATH".to_owned(), "C:\\upper".to_owned()),
        "{entries:?}"
    );
    let value = |name: &str| {
        entries
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(value("AB_REVIEW_SS"), Some("double-s"));
    assert_eq!(value("AB_REVIEW_ß"), Some("sharp-s"));
    assert_eq!(value("=C:"), Some("C:\\"));
    for pair in names.windows(2) {
        let [first, second] =
            [pair[0], pair[1]].map(|name| name.encode_utf16().collect::<Vec<_>>());
        let order = unsafe {
            CompareStringOrdinal(
                first.as_ptr(),
                first.len() as i32,
                second.as_ptr(),
                second.len() as i32,
                TRUE,
            )
        };
        assert_eq!(order, CSTR_LESS_THAN, "{pair:?}");
    }
}

struct PrivateConsoleChild(std::process::Child);
impl std::ops::Deref for PrivateConsoleChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for PrivateConsoleChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for PrivateConsoleChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn private_console_process() -> (PrivateConsoleChild, super::ConsoleProcess) {
    let (child, process) = console_process();
    (PrivateConsoleChild(child), process)
}

type TerminationLog = (usize, Vec<(u32, u32)>);
thread_local! {
    static TERMINATIONS: std::cell::RefCell<Option<TerminationLog>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn after_console_termination(pid: u32, code: u32) -> anyhow::Result<()> {
    TERMINATIONS.with(|slot| {
        if let Some((limit, seen)) = slot.borrow_mut().as_mut() {
            seen.push((pid, code));
            if seen.len() == *limit {
                anyhow::bail!("helper interrupted after process termination");
            }
        }
        Ok(())
    })
}

#[test]
fn close_deadline_partial_helper_keeps_descendants_before_root_and_host_last_zero() {
    for root_already_gone in [false, true] {
        let (mut other, other_process) = private_console_process();
        let (mut root, root_process) = private_console_process();
        let (mut host, host_process) = private_console_process();
        let (mut remaining, remaining_process) = private_console_process();
        let (root_pid, host_pid) = (root.id(), host.id());
        let expected = vec![
            (other.id(), 1),
            (remaining.id(), 1),
            (root_pid, 1),
            (host_pid, 0),
        ];
        let processes = [host_process, root_process, other_process, remaining_process];
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TERMINATIONS.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        let limit = if root_already_gone { 3 } else { 1 };
        TERMINATIONS.with(|slot| *slot.borrow_mut() = Some((limit, Vec::new())));
        let failure =
            super::terminate_console_processes(&processes, root_pid, Some(host_pid)).unwrap_err();
        assert!(failure.to_string().contains("helper interrupted"));
        other.wait().unwrap();
        if root_already_gone {
            remaining.wait().unwrap();
            root.wait().unwrap();
        } else {
            assert!(root.try_wait().unwrap().is_none());
        }
        assert!(host.try_wait().unwrap().is_none());
        // Settle terminated handles before retry, exactly the partial state a new
        // helper observes. The production sort and termination loop run both times.
        super::terminate_console_processes(&processes, root_pid, Some(host_pid)).unwrap();
        assert_eq!(other.wait().unwrap().code(), Some(1));
        assert_eq!(remaining.wait().unwrap().code(), Some(1));
        assert_eq!(root.wait().unwrap().code(), Some(1));
        assert_eq!(host.wait().unwrap().code(), Some(0));
        TERMINATIONS.with(|slot| assert_eq!(slot.borrow().as_ref().unwrap().1, expected));
    }
}

#[test]
fn close_deadline_new_helper_observes_gone_root_and_ends_recorded_live_host() {
    let directory = tempfile::tempdir().unwrap();
    let (mut root, root_process) = private_console_process();
    let (mut host, _host_process) = private_console_process();
    let identity = query_process_identity(root.id()).unwrap();
    crate::native::session::RecordStore::at(&directory.path().join("console-host-process.json"))
        .write_json(&serde_json::json!({ "schema": 1, "pid": host.id(), "identity": query_process_identity(host.id()).unwrap() })).unwrap();
    let session = super::TerminalSession {
        kind: super::TerminalKind::WindowsConsole,
        id: root.id().to_string(),
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        wezterm_mux: None,
        windows_process_identity: Some(identity),
    };
    super::terminate_console_processes(&[root_process], root.id(), Some(host.id())).unwrap();
    root.wait().unwrap();
    assert!(host.try_wait().unwrap().is_none());
    // This returns before FreeConsole/AttachConsole: it cannot touch the test runner's console.
    let error = super::console_control(
        super::ConsoleControlAction::Close,
        &session,
        &super::super::WindowsSessionRecords {
            directory: directory.path(),
            root_never_ran: false,
        },
    )
    .unwrap_err();
    assert!(
        super::super::windows_budget::windows_console_helper_reports_missing(&error.to_string()),
        "{error:#}"
    );
    assert_eq!(host.wait().unwrap().code(), Some(0));
}

#[test]
fn guarded_dialog_file_stays_private_in_the_supplied_helper_directory() {
    use crate::native::{
        consent,
        terminal::{self, DialogKey, GuardedDialogInput, TerminalSession},
    };
    use std::{path::PathBuf, process::Command};
    const CHILD: &str = "AB_WINDOWS_DIALOG_DIRECTORY_TEST";
    const TEST: &str = "native::terminal::windows::tests::guarded_dialog_file_stays_private_in_the_supplied_helper_directory";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("AGENT_BRIDGE_NATIVE_STATE_DIR").unwrap());
    let directory = root.join("session-dialog");
    fs::create_dir(&directory).unwrap();
    let handle: TerminalSession = serde_json::from_value(serde_json::json!({
        "terminal":"windows-console", "session_id":"1", "managed_session_id":"session-dialog"
    }))
    .unwrap();
    let input = GuardedDialogInput {
        screen: "exact dialog".into(),
        key: DialogKey::Enter,
    };
    let file = super::dialog_input_file(&directory, &handle, &input).unwrap();
    assert_eq!(file.path().parent(), Some(directory.as_path()));
    let bytes = consent::read_store(file.path()).unwrap().unwrap(); // checks owner and ACL on the read handle
    assert_eq!(bytes, serde_json::to_string(&input).unwrap());
    drop(file);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    let other_root = tempfile::tempdir().unwrap();
    let other = other_root.path().join("session-dialog");
    fs::create_dir(&other).unwrap();
    let error = terminal::submit_if_unchanged(
        &other,
        &handle,
        &input.screen,
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("helper state root"), "{error:#}");
    for id in [None, Some("session-other".into())] {
        let mut wrong = handle.clone();
        wrong.managed_session_id = id;
        let error = terminal::guarded_dialog_input(
            &directory,
            &wrong,
            &input,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("managed session"), "{error:#}");
    }
    // The existing command boundary refuses to spawn the helper, after the temporary
    // file was created; returning must remove that file again.
    assert_eq!(
        terminal::submit_if_unchanged(&directory, &handle, &input.screen, Instant::now())
            .unwrap_err()
            .to_string(),
        "managed console dialog input timed out before it started"
    );
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
}

#[test]
fn console_newline_modes_preserve_text_and_plain_submission_records() {
    use super::ConsoleNewline;
    use windows_sys::Win32::System::Console::{KEY_EVENT, SHIFT_PRESSED};
    // Include adjacent/edge LF, tabs in the unchanged mode, and a surrogate pair.
    for (mode, input, submits) in [
        (ConsoleNewline::ShiftReturn, "a\nb", 2),
        (ConsoleNewline::ShiftReturn, "\n가😀\n\n", 0),
        (ConsoleNewline::LineFeed, "a\n\tb😀", 1),
    ] {
        let records = build_console_input_records(input, submits, mode);
        // One key-down and one key-up per BMP unit and per submission Return; a surrogate unit
        // is key-down only (#147).
        let mut expected = Vec::new();
        for unit in input.encode_utf16() {
            let soft = unit == u16::from(b'\n') && mode == ConsoleNewline::ShiftReturn;
            expected.push((unit, 1, soft, false));
            if !(0xD800..=0xDFFF).contains(&unit) {
                expected.push((unit, 0, soft, false));
            }
        }
        for _ in 0..submits {
            expected.push((u16::from(b'\r'), 1, false, true));
            expected.push((u16::from(b'\r'), 0, false, true));
        }
        assert_eq!(records.len(), expected.len());
        for (record, (unit, down, soft, submit)) in records.iter().zip(expected) {
            assert_eq!(record.EventType, KEY_EVENT as u16);
            let key = unsafe { record.Event.KeyEvent };
            assert_eq!(key.bKeyDown, down);
            assert_eq!(key.wRepeatCount, 1);
            assert_eq!(key.wVirtualKeyCode, if submit || soft { 0x0d } else { 0 });
            assert_eq!(key.wVirtualScanCode, if submit || soft { 0x1c } else { 0 });
            assert_eq!(
                unsafe { key.uChar.UnicodeChar },
                if soft { u16::from(b'\r') } else { unit }
            );
            assert_eq!(key.dwControlKeyState, if soft { SHIFT_PRESSED } else { 0 });
        }
    }
}

#[test]
fn console_surrogate_pair_is_two_consecutive_key_down_events_without_releases() {
    // Codex reads the console through crossterm (openai-oss-forks/crossterm ed1cdab,
    // src/event/sys/windows/parse.rs): a surrogate unit is buffered regardless of bKeyDown and
    // combined with the next surrogate event, so the high surrogate's release discarded the
    // pair; a completed pair is always reported as a press, so a release pair would repeat the
    // character (#147).
    let records = build_console_input_records("a😀b", 0, super::ConsoleNewline::ShiftReturn);
    let keys = records
        .iter()
        .map(|record| {
            let key = unsafe { record.Event.KeyEvent };
            (unsafe { key.uChar.UnicodeChar }, key.bKeyDown)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            (u16::from(b'a'), 1),
            (u16::from(b'a'), 0),
            (0xD83D, 1),
            (0xDE00, 1),
            (u16::from(b'b'), 1),
            (u16::from(b'b'), 0),
        ]
    );
}

#[test]
fn console_helpers_explicitly_forward_only_the_selected_state_root() {
    use crate::native::session::primitives::STATE_DIR_ENV;
    const CHILD: &str = "ISSUE111_HELPER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let test = "native::terminal::windows::tests::console_helpers_explicitly_forward_only_the_selected_state_root";
        for root in [Some("C:\\selected-private-root"), None] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1");
            for name in crate::native::process_env::non_provider_removals() {
                command.env(name, "caller-sentinel");
            }
            if let Some(root) = root {
                command.env(STATE_DIR_ENV, root);
            } else {
                command.env_remove(STATE_DIR_ENV);
            }
            let output = crate::native::process_env::command_output_until(
                &mut command,
                Instant::now() + Duration::from_secs(30),
                "helper environment test",
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        return;
    }
    let session = super::TerminalSession {
        managed_session_id: Some("session-environment".into()),
        kind: super::TerminalKind::WindowsConsole,
        id: "1".into(),
        tab_id: None,
        window_id: None,
        windows_process_identity: None,
        wezterm_mux: None,
    };
    let root = std::env::var_os(STATE_DIR_ENV);
    for action in ["window", "screen", "dialog", "send", "close"] {
        let command = super::console_helper_command(action, &session, None).unwrap();
        crate::native::process_env::tests::assert_helper_environment(
            &command,
            &[(STATE_DIR_ENV, root.as_deref())],
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_console_launch_uses_non_verbatim_powershell_paths() {
    // The normalized value must reach both the tab request and console-window
    // constructor, not just argv[0]: CreateProcessW has a separate application name.
    let source = include_str!("mod.rs").replace("\r\n", "\n");
    assert!(source.contains(
        "let powershell = PathBuf::from(crate::native::provider_process::windows_command_path(\n        &powershell_executable()?,\n    ));"
    ));
    assert!(source.contains("tab::offer(\n        &powershell,\n        &command_line,"));
    assert!(source.contains("ConsoleWindow::create(&powershell, &command_line, deadline)"));
    for (path, expected) in [
        (
            r"\\?\C:\Program Files (x86)\PowerShell\7\pwsh.exe",
            r"C:\Program Files (x86)\PowerShell\7\pwsh.exe",
        ),
        (
            r"\\?\UNC\server\share\PowerShell\7\pwsh.exe",
            r"\\server\share\PowerShell\7\pwsh.exe",
        ),
    ] {
        let application = PathBuf::from(provider_process::windows_command_path(Path::new(path)));
        assert_eq!(application, Path::new(expected));
        let command_line =
            terminal::windows_console_launch_command_line(Path::new(path), "exit 0").unwrap();
        assert!(command_line.starts_with(&format!("\"{expected}\" ")));
        assert!(!command_line.contains(r"\\?\"));
    }
}

#[cfg(windows)]
#[test]
fn windows_terminal_input_requires_the_live_native_session_owner() {
    let directory = tempfile::tempdir().unwrap();
    let owner = current_native_session_owner("session-owner123").unwrap();
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();

    verified_windows_native_owner(directory.path(), "session-owner123").unwrap();
    assert!(
        verified_windows_native_owner(directory.path(), "session-other123")
            .unwrap_err()
            .to_string()
            .contains("not bound")
    );
}

#[cfg(windows)]
#[test]
fn windows_close_control_uses_the_claimed_terminal_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(TERMINAL_HANDLE_FILE), b"active").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), &ConsoleControlRequest::Close),
        directory.path().join(TERMINAL_HANDLE_FILE)
    );
    fs::write(directory.path().join(TERMINAL_CLOSING_FILE), b"closing").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), &ConsoleControlRequest::Close),
        directory.path().join(TERMINAL_CLOSING_FILE)
    );
}

#[test]
fn text_only_writes_scalars_and_soft_returns_then_reports_after_drain() {
    use std::cell::Cell;
    use windows_sys::Win32::System::Console::SHIFT_PRESSED;
    for input in ["ascii", "한글", "😀", "a\n한😀\n", ""] {
        let start = Instant::now();
        let polls = Cell::new(0);
        struct AfterDrain<'a> {
            polls: &'a Cell<usize>,
            bytes: Vec<u8>,
        }
        impl std::io::Write for AfterDrain<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert_eq!(self.polls.get(), 3, "no output before the third poll");
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = AfterDrain {
            polls: &polls,
            bytes: Vec::new(),
        };
        let mut records = Vec::new();
        super::write_text_only_with(
            input,
            super::ConsoleNewline::ShiftReturn,
            start + Duration::from_secs(5),
            |chunk| {
                records.extend_from_slice(chunk);
                Ok(chunk.len() as u32)
            },
            || {
                let n = polls.get();
                polls.set(n + 1);
                Ok([3, 1, 0][n])
            },
            &mut output,
            || start,
        )
        .unwrap();
        assert_eq!(polls.get(), 3);
        assert_eq!(
            super::parse_text_only_output(&output.bytes).unwrap(),
            input.chars().count()
        );
        assert_eq!(output.bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        let keys = records
            .iter()
            .map(|r| unsafe { r.Event.KeyEvent })
            .collect::<Vec<_>>();
        assert!(
            !keys
                .iter()
                .any(|k| k.wVirtualKeyCode == 0x0d && k.dwControlKeyState == 0)
        );
        assert_eq!(
            keys.iter()
                .filter(|k| k.wVirtualKeyCode == 0x0d && k.dwControlKeyState == SHIFT_PRESSED)
                .count(),
            input.matches('\n').count() * 2
        );
        for key in keys {
            if (0xD800..=0xDFFF).contains(&unsafe { key.uChar.UnicodeChar }) {
                assert_eq!(key.bKeyDown, 1);
            }
        }
    }
}

#[test]
fn text_only_timeout_partial_write_and_pending_failure_emit_no_count() {
    use std::cell::Cell;
    let start = Instant::now();
    // Stop within a normal key pair or the surrogate pair at the end of a
    // 1024-record chunk; neither partial write can yield a scalar count.
    let input = format!("{}😀tail", "a".repeat(511));
    for mode in 0..7 {
        let mut output = Vec::new();
        let clock = Cell::new(start);
        let calls = Cell::new(0);
        let result = super::write_text_only_with(
            &input,
            super::ConsoleNewline::ShiftReturn,
            start + Duration::from_secs(1),
            |chunk| {
                calls.set(calls.get() + 1);
                assert!(chunk.len() <= 1024);
                if mode == 5 {
                    anyhow::bail!("write failed");
                }
                if mode == 0 {
                    return Ok(1);
                }
                if mode == 1 {
                    return Ok(1023);
                }
                if mode == 4 {
                    clock.set(start + Duration::from_secs(2));
                }
                Ok(chunk.len() as u32)
            },
            || {
                if mode == 3 {
                    anyhow::bail!("count unavailable");
                }
                clock.set(start + Duration::from_secs(2));
                Ok(if mode == 6 { 0 } else { 1 })
            },
            &mut output,
            || clock.get(),
        );
        assert!(result.is_err(), "mode {mode}");
        assert!(output.is_empty());
        if mode == 4 {
            assert_eq!(calls.get(), 1);
        }
    }
}

#[test]
fn text_only_launcher_rejects_malformed_success_as_uncertain() {
    assert_eq!(
        super::parse_text_only_output(b"{\"schema\":1,\"written_chars\":3}\n").unwrap(),
        3
    );
    for output in [
        "",
        "{}",
        "{\"schema\":1,\"written_chars\":3,\"unknown\":true}",
        "{\"schema\":1,\"schema\":1,\"written_chars\":3}",
        "{\"schema\":1,\"written_chars\":184467440737095516160}",
        "{\"schema\":2,\"written_chars\":3}",
        "{\"schema\":1,\"written_chars\":1.5}",
        "{\"schema\":1,\"written_chars\":-1}",
        "{\"schema\":1,\"written_chars\":3} garbage",
        "{\"schema\":1,\"written_chars\":3}\n{}",
    ] {
        let failure = super::parse_text_only_output(output.as_bytes()).unwrap_err();
        assert!(failure.delivery_may_have_occurred());
    }
    assert!(
        super::parse_text_only_output(b"\xff")
            .unwrap_err()
            .delivery_may_have_occurred()
    );
    assert_eq!(
        super::parse_text_only_output(b" \n {\"schema\":1,\"written_chars\":3}\n ").unwrap(),
        3
    );
}

#[test]
fn text_only_launcher_classifies_start_and_exit_failures() {
    let root = tempfile::tempdir().unwrap();
    let mut missing = std::process::Command::new(root.path().join("missing.exe"));
    let deadline = Instant::now() + Duration::from_secs(120);
    assert!(
        !super::run_console_send_command(&mut missing, deadline)
            .unwrap_err()
            .delivery_may_have_occurred()
    );
    let mut failed =
        crate::native::process_env::helper_command(std::env::var_os("ComSpec").unwrap());
    failed.args(["/d", "/c", "exit 1"]);
    assert!(
        super::run_console_send_command(&mut failed, deadline)
            .unwrap_err()
            .delivery_may_have_occurred()
    );
}

#[test]
fn guarded_submission_writes_one_return_only_for_unchanged_screen() {
    let input = terminal::GuardedDialogInput {
        screen: "composer text".into(),
        key: terminal::DialogKey::Enter,
    };
    for (screen, expected, count) in [("composer text", "sent", 2), ("changed text", "changed", 0)]
    {
        let mut records = Vec::new();
        let result = super::guarded_key_with(&input, screen, |key| {
            records = super::build_dialog_records(key);
            Ok(())
        })
        .unwrap();
        assert_eq!(result, expected);
        assert_eq!(records.len(), count);
        for (index, record) in records.iter().enumerate() {
            let key = unsafe { record.Event.KeyEvent };
            assert_eq!(key.wVirtualKeyCode, 0x0d);
            assert_eq!(key.dwControlKeyState, 0);
            assert_eq!(key.bKeyDown, if index == 0 { 1 } else { 0 });
        }
    }
}

#[test]
fn text_only_multichunk_unicode_count_and_second_chunk_failure() {
    let start = Instant::now();
    // Every scalar has two records. The emoji occupies records 1022/1023,
    // immediately before the next chunk; full chunks cannot bisect this pair.
    let input = format!(
        "{}😀한글e\u{301}👩\u{200d}💻{}",
        "a".repeat(511),
        "끝".repeat(600)
    );
    for fail in [false, true] {
        let mut output = Vec::new();
        let mut records = Vec::new();
        let mut calls = 0;
        let mut drained = false;
        let result = super::write_text_only_with(
            &input,
            super::ConsoleNewline::ShiftReturn,
            start + Duration::from_secs(5),
            |chunk| {
                calls += 1;
                if fail && calls == 2 {
                    anyhow::bail!("second chunk failed");
                }
                records.extend_from_slice(chunk);
                Ok(chunk.len() as u32)
            },
            || {
                drained = true;
                Ok(0)
            },
            &mut output,
            || start,
        );
        if fail {
            assert!(result.is_err());
            assert_eq!(calls, 2);
            assert_eq!(records.len(), 1024);
            assert!(!drained);
            assert!(output.is_empty());
        } else {
            result.unwrap();
            assert!(calls >= 3);
            assert!(drained);
            assert_eq!(
                super::parse_text_only_output(&output).unwrap(),
                input.chars().count()
            );
            let units: Vec<u16> = records
                .iter()
                .filter_map(|r| {
                    let key = unsafe { r.Event.KeyEvent };
                    (key.bKeyDown != 0).then_some(unsafe { key.uChar.UnicodeChar })
                })
                .collect();
            assert_eq!(String::from_utf16(&units).unwrap(), input);
        }
        for index in [1022, 1023] {
            let key = unsafe { records[index].Event.KeyEvent };
            assert_eq!(key.bKeyDown, 1);
            let unit = unsafe { key.uChar.UnicodeChar };
            assert!((0xD800..=0xDFFF).contains(&unit));
        }
    }
}

#[test]
fn guarded_down_enter_sequence_and_writer_error() {
    let input = terminal::GuardedDialogInput {
        screen: "dialog".into(),
        key: terminal::DialogKey::DownEnter,
    };
    let result = super::guarded_key_with(&input, "dialog", |key| {
        let records = super::build_dialog_records(key);
        assert_eq!(records.len(), 4);
        for (record, (code, down)) in
            records
                .iter()
                .zip([(0x28, 1), (0x28, 0), (0x0d, 1), (0x0d, 0)])
        {
            let key = unsafe { record.Event.KeyEvent };
            assert_eq!(key.wVirtualKeyCode, code);
            assert_eq!(key.bKeyDown, down);
            assert_eq!(key.dwControlKeyState, 0);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(result, "sent");
    let error = super::guarded_key_with(&input, "dialog", |_| {
        anyhow::bail!("writer failed after key down")
    })
    .unwrap_err();
    assert!(error.to_string().contains("writer failed"));
}
