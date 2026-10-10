use super::security::private_sddl;
use super::{
    build_console_input_records, console_command_line, console_creation_flags,
    open_verified_control_process, query_process_identity, query_process_identity_from_handle,
    resolve_executable_from_path, verify_control_process_identity, verify_process_identity,
};
use std::fs;
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
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
    let mut child = std::process::Command::new("powershell.exe")
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
fn codex_paste_confirmation_and_submission_are_delayed_from_the_text_batch() {
    let immediate = super::super::windows_console_immediate_submit_count(2);
    let initial_records = build_console_input_records("prompt", immediate);
    assert_eq!(return_key_down_count(&initial_records), 0);

    let delayed_batches = (immediate..2)
        .map(|_| build_console_input_records("", 1))
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
    let records = build_console_input_records("prompt", 1);
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
