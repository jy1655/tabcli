//! Three generations inherit the test's anonymous pipes. Each reports its own PID before
//! starting its child and stays live until the parent releases stdin, including on unwind.
use super::*;
use crate::native::session::owner::attest_owner;
use crate::native::terminal::{self, close_authority, windows_dispatch};
use close_authority::windows_herdr::{PinnedProcess, ProcessPin};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    time::Duration,
};

const ROLE: &str = "TABCLI_TEST_HERDR_TREE_ROLE";
const CHILD_TEST: &str = concat!(module_path!(), "::tree_child");

fn child(role: &str) -> Command {
    // libtest omits the crate name from the fully qualified filter.
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            CHILD_TEST.split_once("::").unwrap().1,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, role);
    command
}

#[test]
fn tree_child() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    println!("\nTABCLI_T421 {role} {}", std::process::id());
    std::io::stdout().flush().unwrap();
    let mut descendant = match role.as_str() {
        "wrapper" => Some(child("provider").spawn().unwrap()),
        "provider" => Some(child("grandchild").spawn().unwrap()),
        "grandchild" => None,
        _ => panic!("unknown test role {role}"),
    };
    let _ = std::io::stdin().read(&mut [0]);
    if let Some(child) = descendant.as_mut() {
        child.wait().unwrap();
    }
}

#[test]
fn absent_herdr_stops_the_recorded_provider_and_wrapper_leaving_the_grandchild() {
    let mut wrapper = child("wrapper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = wrapper.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(fields) = line.strip_prefix("TABCLI_T421 ") {
                let (role, pid) = fields.split_once(' ').unwrap();
                if tx
                    .send((role.to_owned(), pid.parse::<u32>().unwrap()))
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    let mut tree = std::collections::HashMap::new();
    for _ in 0..3 {
        let (role, pid) = rx.recv_timeout(Duration::from_secs(120)).unwrap();
        tree.insert(role, pid);
    }
    let wrapper_pid = tree["wrapper"];
    let provider_pid = tree["provider"];
    assert_eq!(wrapper.id(), wrapper_pid);
    let provider_identity = terminal::windows_process_identity(provider_pid).unwrap();
    let mut stale = provider_identity.clone();
    stale.creation_time ^= 1;
    assert!(matches!(
        terminal::windows::pin_teardown_process(provider_pid, &stale).unwrap(),
        ProcessPin::Reused
    ));
    stale = provider_identity.clone();
    stale.executable_path.push_str(".other");
    assert_eq!(
        terminal::windows::pin_teardown_process(provider_pid, &stale)
            .unwrap_err()
            .to_string(),
        format!(
            "recorded Windows process {provider_pid} has a different executable at the same creation time"
        )
    );
    let ProcessPin::Live(provider) =
        terminal::windows::pin_teardown_process(provider_pid, &provider_identity).unwrap()
    else {
        panic!("the fixture provider must be live");
    };
    let ProcessPin::Live(grandchild) = terminal::windows::pin_teardown_process(
        tree["grandchild"],
        &terminal::windows_process_identity(tree["grandchild"]).unwrap(),
    )
    .unwrap() else {
        panic!("the fixture grandchild must be live");
    };
    let owner = NativeSessionOwner {
        pid: wrapper_pid,
        windows_process_identity: Some(terminal::windows_process_identity(wrapper_pid).unwrap()),
        ..complete_owner()
    };
    let fixture = Fixture::new("ready", &owner);
    fixture.remove();
    fixture
        .store()
        .write_provider_process(&session::schema::ProviderProcessRecord {
            schema: 1,
            managed_session_id: SESSION.into(),
            pid: provider_pid,
            windows_process_identity: Some(provider_identity),
            spawned_unix_ms: 1,
        })
        .unwrap();
    let dispatch = windows_dispatch::Dispatch::new(
        Adapter::with_core(fixture.console.clone()),
        Adapter::with_core(fixture.core.clone()),
    );
    let deadline = session::close::budget::close_deadline();
    session::close::close_with_attestation(
        &fixture.store(),
        None,
        deadline,
        attest_owner,
        |surface| {
            let observation = close_authority::test_support::TestObservation::on_records(
                &fixture.directory,
                SESSION,
                surface,
                deadline,
            )
            .with_proof(|| windows_dispatch::verify_surface(&dispatch, surface, None))
            .with_presence(|| {
                windows_dispatch::surface_present(
                    &dispatch,
                    &fixture.directory,
                    surface,
                    Duration::from_secs(60),
                )
            });
            close_authority::close_windows_owned_surface_with(
                || close_authority::decide(&observation, SESSION, surface, deadline),
                &dispatch,
                &fixture.directory,
                surface,
                deadline,
            )
        },
    )
    .unwrap();
    fixture.assert_closed("live owner, absent terminal");
    assert_eq!(fixture.store().closed_if_present().unwrap().unwrap().error.as_deref(), Some(format!("Herdr terminal absent; recorded provider PID {provider_pid} was already ended by TAB; recorded wrapper PID {wrapper_pid} was ended by TAB").as_str()));
    assert!(provider.exited().unwrap());
    assert!(wrapper.try_wait().unwrap().is_some());
    assert!(!grandchild.exited().unwrap());
    drop(wrapper.stdin.take());
    assert_eq!(fixture.mutations(), (0, 0));
}
