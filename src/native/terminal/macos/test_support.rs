use crate::native::{
    launch,
    session::{SessionState, Store, turn},
    set_private_directory_permissions, update_status,
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(super) fn launch_fixture(prefix: &str, timeout: Duration, parent: &Path) -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .unwrap();
    // On macOS this is exactly set_permissions(Permissions::from_mode(0o700)).
    set_private_directory_permissions(directory.path()).unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let store = Store::open_unchecked(directory.path());
    let claim = turn::claim(&store, &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launch::begin(&store, &token, Instant::now() + timeout).unwrap();
    directory
}

// Let zsh decode the actual builder's quoting, without running the host or wrapper.
pub(super) fn host_argv(invocation: &str) -> Vec<String> {
    let mut args = shell_argv(invocation);
    assert_eq!(
        args.remove(0),
        std::env::current_exe().unwrap().to_str().unwrap()
    );
    args
}

pub(super) fn shell_argv(invocation: &str) -> Vec<String> {
    let output = std::process::Command::new("/bin/zsh")
        .args([
            "-f",
            "-c",
            &format!("set -- {invocation}; printf '%s\\0' \"$@\""),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .split_terminator('\0')
        .map(str::to_owned)
        .collect()
}

pub(super) fn host_log_messages(directory: &Path, prefix: &str) -> Vec<String> {
    let text = std::fs::read_to_string(directory.join(launch::LOG)).unwrap_or_default();
    text.split_inclusive('\n')
        .filter_map(|line| {
            let (timestamp, message) = line.split_once(' ')?;
            if !message.starts_with(prefix) {
                return None;
            }
            timestamp.parse::<u128>().unwrap();
            Some(message.to_owned())
        })
        .collect()
}
