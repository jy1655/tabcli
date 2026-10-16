use super::*;
use crate::native::session::launch::launcher::bridge_shell_command;
use crate::native::session::launch::wrapper::record_provider_process;
use crate::native::session::launch::*;
use std::{
    process::{Child, Stdio},
    sync::mpsc,
};

fn fixture() -> (tempfile::TempDir, String) {
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir()
        .unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    begin(
        &Store::open_unchecked(directory.path()),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    (directory, token)
}

fn child_command() -> Command {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd.exe");
        c.args(["/c", "pause"]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "read _"]);
        c
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn record_started(directory: &Path, child: &mut Child) -> Result<()> {
    let id = directory.file_name().unwrap().to_str().unwrap();
    record_provider_process(directory, id, child)?;
    update_status(directory, SessionState::Running, None, None)
}

#[test]
fn timeout_cannot_release_a_spawn_in_progress() {
    let (directory, token) = fixture();
    let path = directory.path().to_path_buf();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        spawn(
            &Store::open_unchecked(&path),
            &mut child_command(),
            |child| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                record_started(&path, child)
            },
        )
        .unwrap()
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(fail(&Store::open_unchecked(directory.path()), "launch timed out").is_err());
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path())).unwrap(),
        Some(token)
    );
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state
            .as_str(),
        "launching"
    );
    release_tx.send(()).unwrap();
    let mut child = worker.join().unwrap();
    assert_eq!(
        read(&Reader::open_unchecked(directory.path()))
            .unwrap()
            .unwrap()
            .phase,
        Phase::Spawned
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn unwritable_auxiliary_log_does_not_block_provider_start() {
    let (directory, _) = fixture();
    fs::remove_file(directory.path().join(LOG)).unwrap();
    fs::create_dir(directory.path().join(LOG)).unwrap();
    let mut child = spawn(
        &Store::open_unchecked(directory.path()),
        &mut child_command(),
        |child| record_started(directory.path(), child),
    )
    .unwrap();
    assert_eq!(
        read(&Reader::open_unchecked(directory.path()))
            .unwrap()
            .unwrap()
            .phase,
        Phase::Spawned
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn failed_spawn_does_not_claim_a_process_was_started() {
    let (directory, _) = fixture();
    let error = spawn(
        &Store::open_unchecked(directory.path()),
        &mut Command::new(directory.path().join("missing-provider")),
        |_| unreachable!(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("spawn"));
    assert_eq!(
        read(&Reader::open_unchecked(directory.path()))
            .unwrap()
            .unwrap()
            .phase,
        Phase::Pending
    );
    launch::finalize(&Store::open_unchecked(directory.path()), &Err(error)).unwrap();
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[cfg(unix)]
#[test]
fn shell_captures_bootstrap_output_and_exit_without_requiring_logging() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-shell");
    fs::create_dir(&directory).unwrap();
    let executable = root.path().join("bridge ' ; stub");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf 'wrapper stdout\\n'\nprintf 'wrapper stderr\\n' >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let command =
        bridge_shell_command(root.path(), root.path(), &executable, "session-shell").unwrap();
    let output = Command::new("/bin/sh")
        .args(["-c", &command])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let log = fs::read_to_string(directory.join(LOG)).unwrap();
    for part in ["wrapper stdout", "wrapper stderr", "wrapper_exit_code=7"] {
        assert!(log.contains(part));
    }
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    fs::remove_file(directory.join(LOG)).unwrap();
    fs::create_dir(directory.join(LOG)).unwrap();
    let output = Command::new("/bin/sh")
        .args(["-c", &command])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&output.stdout).contains("wrapper stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("wrapper stderr"));
}

// Runs the bootstrap through the command line that the console launch itself builds.
// 0.0.8 put a double-quoted string into the bootstrap, which that command line cannot
// carry, so every launch on native Windows was refused before a console existed.
#[cfg(windows)]
#[test]
fn console_launch_carries_the_bootstrap_and_records_the_wrapper_exit_code() {
    use std::os::windows::process::CommandExt;
    // The stub is a batch file, and cmd.exe cannot start in a verbatim (`\\?\`) path, so
    // the temporary directory is used as given.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-shell");
    fs::create_dir(&directory).unwrap();
    let executable = root.path().join("bridge owner's stub.cmd");
    fs::write(&executable, "@exit /b 7\r\n").unwrap();
    let command =
        bridge_shell_command(root.path(), root.path(), &executable, "session-shell").unwrap();
    let powershell = terminal::windows_powershell_executable().unwrap();
    let command_line =
        terminal::windows_console_launch_command_line(&powershell, &command).unwrap();
    let powershell = provider_process::windows_command_path(&powershell);
    let arguments = command_line
        .strip_prefix(&format!("\"{}\" ", powershell.to_string_lossy()))
        .unwrap();
    let output = Command::new(&powershell)
        .raw_arg(arguments)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(directory.join(LOG))
            .unwrap()
            .contains("wrapper_exit_code=7")
    );
}
