use super::{insensitive_ancestors, windows_entry, windows_override_key};
use std::io;
use std::path::Path;

fn projects(key: &str, level: &str) -> toml::Table {
    let entry = toml::Table::from_iter([("trust_level".into(), level.into())]);
    toml::Table::from_iter([(key.into(), entry.into())])
}

#[test]
fn several_ascii_case_matches_are_ambiguous_even_with_an_exact_key() {
    for level in ["trusted", "untrusted"] {
        let mut entries = projects("Repo", level);
        entries.extend(projects("repo", "untrusted"));
        let result = windows_entry(&entries, "Repo", || panic!("unexpected flag read"));
        assert_eq!(
            result.unwrap_err().to_string(),
            "Codex's store holds several entries for Repo that differ only in letter case"
        );
    }
}

#[test]
fn an_exact_trusted_spelling_decides_without_a_flag_read() {
    let entries = projects("Repo", "trusted");
    assert_eq!(
        windows_entry(&entries, "Repo", || panic!("unexpected flag read")).unwrap(),
        entries.get("Repo")
    );
}

#[test]
fn another_trusted_spelling_requires_readable_insensitive_ancestors() {
    let entries = projects("repo", "trusted");
    let root = Path::new(if cfg!(windows) { r"C:\" } else { "/" });
    let upper = root.join("ROOT");
    let branch = upper.join("Branch");
    let folder = branch.join("Repo");
    for (flag, accepted) in [
        (Ok(false), true),
        (Ok(true), false),
        (Err(io::ErrorKind::PermissionDenied), false),
    ] {
        let mut visited = Vec::new();
        let entry = windows_entry(&entries, "Repo", || {
            insensitive_ancestors(&folder, |ancestor| {
                visited.push(ancestor.to_path_buf());
                if ancestor == upper {
                    flag.map_err(io::Error::from)
                } else {
                    Ok(false)
                }
            })
        })
        .unwrap();
        assert_eq!(entry, accepted.then(|| &entries["repo"]));
        let mut expected = vec![root, upper.as_path()];
        if accepted {
            expected.push(branch.as_path());
        }
        assert_eq!(visited, expected);
    }
}

#[test]
fn a_unique_decline_decides_without_a_flag_read() {
    let entries = projects("repo", "untrusted");
    assert_eq!(
        windows_entry(&entries, "Repo", || panic!("unexpected flag read")).unwrap(),
        entries.get("repo")
    );
}

#[test]
fn non_ascii_case_is_part_of_the_store_key() {
    let entries = projects("r\u{c9}po", "trusted");
    assert_eq!(
        windows_entry(&entries, "r\u{e9}po", || panic!("unexpected flag read")).unwrap(),
        None
    );
}

#[test]
fn unicode_lowercase_matches_supply_only_declines() {
    let stored = "C:\\r\u{e9}po";
    let judged = "C:\\R\u{c9}po";
    for level in ["untrusted", "trusted"] {
        let entries = projects(stored, level);
        let selected = windows_entry(&entries, judged, || panic!("unexpected flag read")).unwrap();
        assert_eq!(
            selected,
            (level == "untrusted").then(|| &entries[stored]),
            "{level}"
        );
    }
}

#[test]
fn any_unicode_lowercase_decline_applies_only_without_an_ascii_match() {
    let judged = "C:\\R\u{c9}po";
    let declined = "c:\\r\u{e9}po";
    let mut entries = projects("C:\\r\u{e9}po", "trusted");
    entries.extend(projects(declined, "untrusted"));
    assert_eq!(
        windows_entry(&entries, judged, || panic!("unexpected flag read")).unwrap(),
        entries.get(declined)
    );
    entries.extend(projects(judged, "trusted"));
    assert_eq!(
        windows_entry(&entries, judged, || panic!("unexpected flag read")).unwrap(),
        entries.get(judged)
    );
}

#[test]
fn windows_override_folds_ascii_letters_only() {
    assert_eq!(windows_override_key(r"C:\Repo"), r"c:\repo");
    assert_eq!(windows_override_key("C:\\R\u{c9}PO"), "c:\\r\u{c9}po");
}

#[cfg(windows)]
mod windows;
