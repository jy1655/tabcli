use super::security::private_sddl;
use super::{
    build_console_input_records, console_command_line, console_creation_flags,
    query_process_identity, resolve_executable_from_path, verify_control_process_identity,
    verify_process_identity,
};
use std::fs;
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::time::{Duration, Instant};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Console::INPUT_RECORD;
use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, OpenProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE;

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
