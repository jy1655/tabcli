use std::{
    fs,
    path::{Path, PathBuf},
};

/// Every path under the state root: directories carry no content, files carry their bytes,
/// so an added directory is as visible as an added file.
pub(super) type Entry = (PathBuf, Option<Vec<u8>>);

pub(super) fn entries(directory: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            entries.push((entry.path(), None));
            entries.extend(self::entries(&entry.path()));
        } else {
            entries.push((entry.path(), Some(fs::read(entry.path()).unwrap())));
        }
    }
    entries.sort();
    entries
}
