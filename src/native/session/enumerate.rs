//! Lazy state-root traversal. Callers own error policy, sorting and mutations.
use super::primitives::valid_session_id;
use super::schema::SessionManifest;
use agent_bridge::FirstPartyCli;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
pub(in crate::native) enum CheckOrder {
    FileTypeFirst,
    IdFirst,
}

#[derive(Debug)]
pub(in crate::native) enum Fault {
    Entry(io::Error),
    FileType { id: String, error: io::Error },
}
impl Fault {
    pub(in crate::native) fn into_io(self) -> io::Error {
        match self {
            Self::Entry(error) | Self::FileType { error, .. } => error,
        }
    }
}

#[derive(Debug)]
pub(in crate::native) struct Entry {
    pub(in crate::native) id: String,
    pub(in crate::native) directory: PathBuf,
}

pub(in crate::native) struct Enumeration {
    entries: fs::ReadDir,
    order: CheckOrder,
    #[cfg(test)]
    injected: Option<std::collections::VecDeque<io::Result<TestEntry>>>,
}

/// Root-open errors are distinct from the typed iterator errors; no NotFound policy here.
pub(in crate::native) fn enumerate(root: &Path, order: CheckOrder) -> io::Result<Enumeration> {
    let entries = fs::read_dir(root)?;
    Ok(Enumeration {
        entries,
        order,
        #[cfg(test)]
        injected: INJECTED.with(|cell| cell.borrow_mut().take()),
    })
}

fn admit(
    id: String,
    directory: PathBuf,
    order: CheckOrder,
    kind: impl FnOnce() -> io::Result<bool>,
) -> Option<Result<Entry, Fault>> {
    if matches!(order, CheckOrder::IdFirst) && !valid_session_id(&id) {
        return None;
    }
    match kind() {
        Err(error) => Some(Err(Fault::FileType { id, error })),
        Ok(false) => None,
        Ok(true) if !valid_session_id(&id) => None,
        Ok(true) => Some(Ok(Entry { id, directory })),
    }
}

impl Iterator for Enumeration {
    type Item = Result<Entry, Fault>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            #[cfg(test)]
            if let Some(entries) = &mut self.injected {
                let entry = match entries.pop_front()? {
                    Ok(entry) => entry,
                    Err(error) => return Some(Err(Fault::Entry(error))),
                };
                if let Some(outcome) = admit(entry.id, entry.directory, self.order, || {
                    entry.directory_kind
                }) {
                    return Some(outcome);
                }
                continue;
            }
            let entry = match self.entries.next()? {
                Ok(entry) => entry,
                Err(error) => return Some(Err(Fault::Entry(error))),
            };
            if let Some(outcome) = admit(
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
                self.order,
                || entry.file_type().map(|kind| kind.is_dir()),
            ) {
                return Some(outcome);
            }
        }
    }
}

pub(in crate::native) struct Scope<'a> {
    pub(in crate::native) workspace: Option<&'a PathBuf>,
    pub(in crate::native) provider: Option<FirstPartyCli>,
}
impl Scope<'_> {
    pub(in crate::native) fn admits(&self, manifest: &SessionManifest) -> bool {
        !self
            .workspace
            .is_some_and(|workspace| workspace != &manifest.workspace)
            && !self
                .provider
                .is_some_and(|provider| provider.as_str() != manifest.provider)
    }
}

#[cfg(test)]
pub(in crate::native) struct TestEntry {
    pub(in crate::native) id: String,
    pub(in crate::native) directory: PathBuf,
    pub(in crate::native) directory_kind: io::Result<bool>,
}
#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::RefCell<Option<std::collections::VecDeque<io::Result<TestEntry>>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(in crate::native) fn with_entries<T>(
    entries: Vec<io::Result<TestEntry>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            INJECTED.with(|cell| *cell.borrow_mut() = None);
        }
    }
    INJECTED.with(|cell| *cell.borrow_mut() = Some(entries.into()));
    let _reset = Reset;
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_errors_and_entries_keep_filesystem_order() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            enumerate(&root.path().join("missing"), CheckOrder::IdFirst)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            enumerate(root.path(), CheckOrder::IdFirst).unwrap().count(),
            0
        );
        fs::write(root.path().join("file"), "").unwrap();
        assert!(enumerate(&root.path().join("file"), CheckOrder::IdFirst).is_err());
        for name in ["session-z", "session-a", "invalid"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::write(root.path().join("session-file"), "").unwrap();
        let expected: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|id| valid_session_id(id))
            .collect();
        let got: Vec<_> = enumerate(root.path(), CheckOrder::IdFirst)
            .unwrap()
            .map(|entry| entry.unwrap().id)
            .collect();
        assert_eq!(got, expected);
    }
    #[test]
    fn typed_faults_preserve_validation_order_and_lazy_encounter_order() {
        let root = tempfile::tempdir().unwrap();
        for order in [CheckOrder::FileTypeFirst, CheckOrder::IdFirst] {
            let entries = vec![
                Ok(TestEntry {
                    id: "invalid".into(),
                    directory: root.path().into(),
                    directory_kind: Err(io::Error::other("kind")),
                }),
                Ok(TestEntry {
                    id: "session-z".into(),
                    directory: root.path().into(),
                    directory_kind: Ok(true),
                }),
                Err(io::Error::other("entry")),
                Ok(TestEntry {
                    id: "session-a".into(),
                    directory: root.path().into(),
                    directory_kind: Err(io::Error::other("kind")),
                }),
            ];
            with_entries(entries, || {
                let mut entries = enumerate(root.path(), order).unwrap();
                if matches!(order, CheckOrder::FileTypeFirst) {
                    assert!(
                        matches!(entries.next().unwrap(), Err(Fault::FileType { id, error }) if id == "invalid" && error.to_string() == "kind")
                    );
                }
                assert_eq!(entries.next().unwrap().unwrap().id, "session-z");
                assert!(
                    matches!(entries.next().unwrap(), Err(Fault::Entry(error)) if error.to_string() == "entry")
                );
                assert!(
                    matches!(entries.next().unwrap(), Err(Fault::FileType { id, .. }) if id == "session-a")
                );
                assert!(entries.next().is_none());
            });
        }
    }
    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_not_sessions() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("session-link")).unwrap();
        assert_eq!(
            enumerate(root.path(), CheckOrder::IdFirst).unwrap().count(),
            0
        );
    }
}
