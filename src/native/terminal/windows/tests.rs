use super::security::private_sddl;
use super::{
    build_console_input_records, console_command_line, console_creation_flags,
    query_process_identity, resolve_executable_from_path, verify_control_process_identity,
    verify_process_identity,
};
use std::fs;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
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
fn codex_paste_detection_and_submission_use_two_real_return_key_events() {
    let records = build_console_input_records("prompt", 2);
    let down = unsafe { records[records.len() - 2].Event.KeyEvent };
    let up = unsafe { records[records.len() - 1].Event.KeyEvent };
    assert_eq!(down.bKeyDown, 1);
    assert_eq!(up.bKeyDown, 0);
    assert_eq!(down.wVirtualKeyCode, 0x0d);
    assert_eq!(down.wVirtualScanCode, 0x1c);
    assert_eq!(unsafe { down.uChar.UnicodeChar }, u16::from(b'\r'));
    assert_eq!(
        records
            .iter()
            .filter(|record| unsafe {
                record.Event.KeyEvent.bKeyDown == 1 && record.Event.KeyEvent.wVirtualKeyCode == 0x0d
            })
            .count(),
        2,
        "Codex first confirms the synthetic paste batch and then submits it"
    );
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

#[test]
fn private_acl_is_one_protected_current_user_entry() {
    assert_eq!(
        private_sddl("S-1-5-21-1", true),
        "D:P(A;OICI;FA;;;S-1-5-21-1)"
    );
    assert_eq!(private_sddl("S-1-5-21-1", false), "D:P(A;;FA;;;S-1-5-21-1)");
}
