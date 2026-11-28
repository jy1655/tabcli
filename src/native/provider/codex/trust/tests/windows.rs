use super::*;
use crate::native::consent::{self, Environment, Trust};
use crate::native::provider::{NativeProviderAdapter, codex::ADAPTER};
use crate::native::session::primitives::write_private;
use std::{fs, io::Write, process::Command};

fn case_sensitive_root() -> Option<tempfile::TempDir> {
    let temp = tempfile::Builder::new()
        .prefix("CaseScope-")
        .tempdir()
        .unwrap();
    let output = Command::new("fsutil.exe")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(temp.path())
        .arg("enable")
        .output();
    if !matches!(&output, Ok(result) if result.status.success()) {
        // Direct stderr remains visible when libtest captures a passing test's output.
        writeln!(
            std::io::stderr().lock(),
            "::warning::SKIP {}: {output:?}",
            std::thread::current()
                .name()
                .unwrap_or("case-sensitive directory test")
        )
        .unwrap();
        return None;
    }
    assert!(super::super::directory_case_sensitive(temp.path()).unwrap());
    Some(temp)
}

fn codex_key(workspace: &Path) -> String {
    consent::native_key(&workspace.canonicalize().unwrap())
        .unwrap()
        .to_ascii_lowercase()
}

fn store_entry(home: &Path, key: &str, level: &str) {
    let store = home.join(".codex/config.toml");
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    write_private(
        &store,
        format!(
            "[projects.{}]\ntrust_level = '{level}'\n",
            serde_json::to_string(key).unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
}

#[test]
fn a_folded_grant_requires_insensitive_parents_and_a_decline_applies_to_both_spellings() {
    let Some(temp) = case_sensitive_root() else {
        return;
    };
    let upper = temp.path().join("A");
    let lower = temp.path().join("a");
    fs::create_dir(&upper).unwrap();
    fs::create_dir(&lower).unwrap();
    let key = codex_key(&lower);
    assert_eq!(codex_key(&upper), key);
    let grants = projects(&key, "trusted");
    let home = temp.path().join("home");
    store_entry(&home, &key, "untrusted");
    let env = Environment::at_home(&home);
    for folder in [upper, lower] {
        let folder = folder.canonicalize().unwrap();
        let native = consent::native_key(&folder).unwrap();
        assert_ne!(native, key);
        let grant = windows_entry(&grants, &native, || {
            insensitive_ancestors(&folder, super::super::directory_case_sensitive)
        })
        .unwrap();
        assert_eq!(grant, None);
        assert_eq!(
            ADAPTER.workspace_trust(&folder, &env, None).unwrap(),
            Trust::Declined
        );
    }
}

#[test]
fn a_folded_main_tree_entry_under_a_case_sensitive_directory_supplies_no_inherited_pin() {
    let Some(temp) = case_sensitive_root() else {
        return;
    };
    let root = temp.path().canonicalize().unwrap();
    let git = consent::registered(&root);
    let home = root.join("home");
    let key = codex_key(&git.repository);
    assert_ne!(consent::native_key(&git.repository).unwrap(), key);
    store_entry(&home, &key, "trusted");
    let env = Environment::at_home(&home);
    assert!(
        ADAPTER
            .store_lists_trusted(&git.repository, &env, None)
            .unwrap()
    );
    assert_eq!(
        ADAPTER
            .workspace_trust(&git.repository, &env, None)
            .unwrap(),
        Trust::Absent
    );
    let directory = root.join("session-test");
    fs::create_dir(&directory).unwrap();
    let inspected = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = inspected.clone();
    let main = git.repository.clone();
    consent::after_main_identified(move |path| {
        assert_eq!(path, main);
        observed.set(true);
    });
    let assessment = consent::prepare_at_home(&directory, &git.worktree, &home).unwrap();
    assert!(inspected.get());
    assert_eq!(assessment.state, "unverified");
    assert_eq!(assessment.source, None);
    assert_eq!(assessment.repository_root, None);
    // A start records the worktree assessment; no shared record may pin a source.
    for entry in fs::read_dir(root.join("workspace-consent")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let record: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert!(record["source"].is_null());
            assert!(record.get("repository_root").is_none());
        }
    }
}

#[test]
fn the_store_screen_accepts_a_folded_key_after_its_parent_disappears() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let parent = home.join("Parent");
    let folder = parent.join("Missing");
    fs::create_dir_all(&folder).unwrap();
    let workspace = folder.canonicalize().unwrap();
    store_entry(&home, &codex_key(&workspace), "trusted");
    fs::remove_dir(&folder).unwrap();
    fs::remove_dir(&parent).unwrap();
    let env = Environment::at_home(&home);
    assert!(ADAPTER.store_lists_trusted(&workspace, &env, None).unwrap());
    assert_eq!(
        ADAPTER.workspace_trust(&workspace, &env, None).unwrap(),
        Trust::Absent
    );
}
