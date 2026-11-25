//! The Windows admission of a bound host's session directory, read from a handle on the
//! directory itself: a private directory is admitted; one that another account can change,
//! a junction to a private directory and a file are refused.
use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SPEC: BoundHostSpec = BoundHostSpec { name: "Herdr" };

fn private_session(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    crate::native::session::primitives::set_private_directory_permissions(&directory).unwrap();
    directory
}

fn icacls(path: &Path, grant: &str) {
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    let status = Command::new(PathBuf::from(system).join(r"System32\icacls.exe"))
        .arg(path)
        .args(["/grant", grant])
        .status()
        .unwrap();
    assert!(status.success(), "icacls /grant {grant}");
}

fn refusal(directory: &Path) -> String {
    format!(
        "{:#}",
        run_bound_host(directory, &SPEC, |id| Ok(id.to_owned())).unwrap_err()
    )
}

#[test]
fn a_private_session_directory_is_admitted_from_its_handle() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-admitted");
    assert_eq!(
        run_bound_host(&directory, &SPEC, |id| Ok(id.to_owned())).unwrap(),
        "session-admitted"
    );
}

#[test]
fn native_herdr_host_dispatch_reaches_the_launch_receipt_check() {
    use super::super::host;
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-herdr-host");
    let command = host::parse(
        host::HERDR_HOST_COMMAND,
        &[
            directory.to_str().unwrap().to_owned(),
            host::LAUNCH_COMMAND_FLAG.to_owned(),
            "must not run".to_owned(),
        ],
    )
    .unwrap();
    let error = host::run(command).unwrap_err();
    assert_eq!(error.to_string(), "missing Herdr launch receipt");
    let log = fs::read_to_string(directory.join(launch::LOG)).unwrap();
    assert!(log.ends_with(" herdr_host_refused: missing Herdr launch receipt\n"));
}

// Everyone (S-1-1-0) gains a right that changes the directory: writing an entry, or only
// deleting entries, which the trust store's own rule does not count for a file.
#[test]
fn a_session_directory_that_another_account_can_change_is_refused() {
    for grant in ["*S-1-1-0:(W)", "*S-1-1-0:(DC)", "*S-1-1-0:(D)"] {
        let root = tempfile::tempdir().unwrap();
        let directory = private_session(root.path(), "session-hostile");
        icacls(&directory, grant);
        let text = refusal(&directory);
        assert!(
            text.starts_with("Herdr launch directory is not private to the current user"),
            "{grant}: {text}"
        );
        assert!(
            text.contains("session directory can be changed by another account (S-1-1-0)"),
            "{grant}: {text}"
        );
    }
}

#[test]
fn a_junction_or_a_file_is_never_a_session_directory() {
    let root = tempfile::tempdir().unwrap();
    let target = private_session(root.path(), "session-target");
    let junction = root.path().join("session-junction");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J");
    let text = refusal(&junction);
    assert!(
        text.contains("refusing non-directory Herdr launch path"),
        "{text}"
    );
    let file = root.path().join("session-file");
    fs::write(&file, b"not a directory").unwrap();
    let text = refusal(&file);
    assert!(
        text.contains("refusing non-directory Herdr launch path"),
        "{text}"
    );
}

// The launcher's check is the host's own admission: a directory that another account can
// change is refused before any surface, for every kind whose host admits it, and the console,
// whose host does not, is left to its own checks.
#[test]
fn the_launcher_refuses_a_directory_the_launch_host_would_refuse() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-launcher");
    refuse_before_launch(&directory, TerminalKind::Herdr).unwrap();
    icacls(&directory, "*S-1-1-0:(W)");
    let text = format!(
        "{:#}",
        refuse_before_launch(&directory, TerminalKind::Herdr).unwrap_err()
    );
    assert!(
        text.starts_with(&format!(
            "the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user",
            directory.display()
        )),
        "{text}"
    );
    assert!(
        text.contains("session directory can be changed by another account (S-1-1-0)"),
        "{text}"
    );
    refuse_before_launch(&directory, TerminalKind::WindowsConsole).unwrap();
}

// The case observed on 2026-11-26: an elevated TAB creates the session directory, so its owner
// is the default owner of TAB's token (the Administrators group), while a core that is not
// elevated accepts only its user as owner. The Windows CI runner is elevated, so there this
// is that case; a TAB that is not elevated owns the directory as its user, which the core's
// launch host admits.
#[test]
fn an_elevated_tab_is_refused_for_a_core_that_is_not_elevated() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-elevation");
    let own = HostAccount::current().unwrap();
    refuse_before_core_launch(&directory, TerminalKind::Herdr, &own).unwrap();
    let user = own.windows.owners[0].clone();
    let core = HostAccount::of_windows(crate::native::consent::WindowsAccount {
        owners: vec![user],
        elevated: Some(false),
    });
    let result = refuse_before_core_launch(&directory, TerminalKind::Herdr, &core);
    if own.windows.owners.len() == 1 {
        result.unwrap();
        return;
    }
    let text = format!("{:#}", result.unwrap_err());
    assert!(
        text.contains("session directory owner differs from current user"),
        "{text}"
    );
    if own.windows.elevated == Some(true) {
        assert!(
            text.contains("TAB is running elevated and the Herdr Core is not"),
            "{text}"
        );
    }
    // A directory TAB's own account would refuse as well is refused for its reason alone.
    icacls(&directory, "*S-1-1-0:(W)");
    let text = format!(
        "{:#}",
        refuse_before_core_launch(&directory, TerminalKind::Herdr, &core).unwrap_err()
    );
    assert!(!text.contains("elevat"), "{text}");
}
