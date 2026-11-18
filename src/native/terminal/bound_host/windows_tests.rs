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
