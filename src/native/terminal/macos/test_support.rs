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
