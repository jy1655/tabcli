//! Process-record tests against real processes of this machine; moved here from
//! `native/tests/macos.rs` beside the process observer.

use crate::native::terminal::macos::apple_terminal::incarnation::terminal_app_ancestor;
use crate::native::terminal::macos::process::{macos_process_info, macos_processes_named};
use crate::native::tests::reaped_child_pid;
use std::path::Path;
// launchd is an ancestor of every process and belongs to root. The record of a process of
// this user carries the same birth and parent as the query that verifies an owner, so an
// incarnation captured here is the one a later close compares.
#[cfg(target_os = "macos")]
#[test]
fn process_records_answer_for_a_root_owned_ancestor_and_agree_with_the_owner_query() {
    let (launchd, parent, path) = terminal_app_ancestor(1).unwrap();
    assert_eq!(
        (launchd.pid, parent, path.as_str()),
        (1, 0, "/sbin/launchd")
    );
    assert!(launchd.start_seconds > 0 && launchd.start_microseconds < 1_000_000);

    let pid = std::process::id();
    let (own, parent, path) = terminal_app_ancestor(pid).unwrap();
    let info = macos_process_info(pid).unwrap().unwrap();
    assert_eq!(
        (own.start_seconds, own.start_microseconds, parent),
        (
            info.process_start_seconds,
            info.process_start_microseconds,
            info.parent_pid
        )
    );
    let executable = std::env::current_exe().unwrap();
    assert_eq!(Path::new(&path).file_name(), executable.file_name());

    // The command name is the executable's, cut to the kernel's 16 bytes.
    let name = executable.file_name().unwrap().as_encoded_bytes();
    let named = macos_processes_named(&name[..name.len().min(16)]).unwrap();
    assert!(named.contains(&own), "{named:?} lacks {own:?}");
    assert!(named.windows(2).all(|pair| pair[0].pid < pair[1].pid));
    assert!(
        macos_processes_named(b"no-such-command")
            .unwrap()
            .is_empty()
    );

    let error = terminal_app_ancestor(reaped_child_pid()).unwrap_err();
    assert!(error.to_string().contains("ended during attestation"));
}
