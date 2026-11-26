use super::super::record::{SESSION_FILE, applied, assess_with, record};
use super::super::store::native_key;
use super::super::tests::{Fixture, session, setup, trust_pi};
use super::super::worktree::tests::{Registered, registered};
use super::*;
use crate::native::session::primitives::{write_json_atomic, write_private};
use std::fs;
use std::path::PathBuf;

// A launching session in its own state root whose recorded consent is the assessment of
// `workspace` there, with the fixture stores.
struct Session {
    root: tempfile::TempDir,
    directory: PathBuf,
}

fn launched(workspace: &Path, fixture: &Fixture) -> Session {
    let (root, directory) = session(workspace, "launching");
    record(
        &directory,
        &assess_with(root.path(), workspace, &fixture.env).unwrap(),
    )
    .unwrap();
    Session { root, directory }
}

fn worktree() -> (tempfile::TempDir, Fixture, Registered) {
    let (temp, _workspace, fixture) = setup();
    let git = registered(&temp.path().canonicalize().unwrap());
    (temp, fixture, git)
}

// Pi's store with these decisions.
fn pi_decides(fixture: &Fixture, decisions: &[(&Path, bool)]) {
    let decisions: serde_json::Map<_, _> = decisions
        .iter()
        .map(|(path, trusted)| (native_key(path).unwrap(), serde_json::json!(trusted)))
        .collect();
    if fixture.pi.exists() {
        fs::remove_file(&fixture.pi).unwrap();
    }
    write_private(
        &fixture.pi,
        serde_json::Value::Object(decisions).to_string().as_bytes(),
    )
    .unwrap();
}

fn verdict(
    session: &Session,
    target: FirstPartyCli,
    fixture: &Fixture,
) -> Result<Consented, String> {
    authority(&session.directory, target, &fixture.env)
        .unwrap()
        .verdict
}

#[test]
fn exact_and_inherited_consent_are_told_apart_for_every_target() {
    let (_temp, workspace, fixture) = setup();
    trust_pi(&fixture, &workspace);
    let exact = launched(&workspace, &fixture);
    let (_temp, worktree_fixture, git) = worktree();
    trust_pi(&worktree_fixture, &git.repository);
    let inherited = launched(&git.worktree, &worktree_fixture);
    for target in [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ] {
        assert_eq!(verdict(&exact, target, &fixture), Ok(Consented::Exact));
        assert_eq!(
            verdict(&inherited, target, &worktree_fixture),
            Ok(Consented::Inherited)
        );
    }
    // No record, and a record of unverified consent, are refusals, not errors.
    let (_temp, untrusted, fixture) = setup();
    let unverified = launched(&untrusted, &fixture);
    assert!(verdict(&unverified, FirstPartyCli::Agy, &fixture).is_err());
    fs::remove_file(unverified.directory.join(SESSION_FILE)).unwrap();
    assert_eq!(
        verdict(&unverified, FirstPartyCli::Agy, &fixture),
        Err("the session has no workspace consent record".to_owned())
    );
}

// The target provider's decline of the worktree or of its main working tree refuses; a read
// failure refuses too.
#[test]
fn a_target_that_declines_the_worktree_or_its_main_working_tree_refuses() {
    let (_temp, fixture, git) = worktree();
    crate::native::session::primitives::write_private(
        &fixture.codex,
        format!(
            "[projects.{}]\ntrust_level = \"trusted\"\n",
            serde_json::to_string(&if cfg!(windows) {
                native_key(&git.repository).unwrap().to_lowercase()
            } else {
                native_key(&git.repository).unwrap()
            })
            .unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    let session = launched(&git.worktree, &fixture);
    assert_eq!(
        verdict(&session, FirstPartyCli::Pi, &fixture),
        Ok(Consented::Inherited)
    );
    for declined in [&git.repository, &git.worktree] {
        pi_decides(&fixture, &[(declined, false)]);
        assert!(
            verdict(&session, FirstPartyCli::Pi, &fixture)
                .unwrap_err()
                .starts_with("pi declines"),
            "{}",
            declined.display()
        );
        assert_eq!(
            verdict(&session, FirstPartyCli::Codex, &fixture),
            Ok(Consented::Inherited)
        );
    }
    fs::remove_file(&fixture.pi).unwrap();
    write_private(&fixture.pi, b"= =").unwrap();
    assert!(
        verdict(&session, FirstPartyCli::Pi, &fixture)
            .unwrap_err()
            .contains("cannot be read")
    );
}

// Review finding 5: the consent is bound to the directory the provider runs in, the
// manifest's workspace, by path and by identity.
#[test]
fn consent_for_another_directory_than_the_manifests_workspace_refuses() {
    let (temp, workspace, fixture) = setup();
    trust_pi(&fixture, &workspace);
    let other = temp.path().canonicalize().unwrap().join("other");
    fs::create_dir(&other).unwrap();
    let session = launched(&workspace, &fixture);
    assert_eq!(
        verdict(&session, FirstPartyCli::Agy, &fixture),
        Ok(Consented::Exact)
    );
    // The manifest names another directory than the assessment.
    let manifest = session.directory.join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    value["workspace"] = serde_json::json!(other);
    write_json_atomic(&manifest, &value).unwrap();
    assert_eq!(
        verdict(&session, FirstPartyCli::Agy, &fixture),
        Err("the session's workspace consent names another directory than its workspace".into())
    );
    // The same path, but another directory now stands there.
    value["workspace"] = serde_json::json!(workspace);
    write_json_atomic(&manifest, &value).unwrap();
    fs::rename(&workspace, temp.path().join("moved")).unwrap();
    fs::create_dir(&workspace).unwrap();
    assert!(verdict(&session, FirstPartyCli::Agy, &fixture).is_err());
}

// Review findings 1, 6 and 8: what was recorded at launch is history. A revoke of the
// worktree or of its main working tree, or a withdrawn source, refuses at the next use, and
// the applied marker is reported but grants nothing.
#[test]
fn the_launch_record_is_history_and_never_authority() {
    let (_temp, fixture, git) = worktree();
    trust_pi(&fixture, &git.repository);
    let session = launched(&git.worktree, &fixture);
    applied(&session.directory, "agy-settings-override").unwrap();
    let found = authority(&session.directory, FirstPartyCli::Agy, &fixture.env).unwrap();
    assert_eq!(found.applied.as_deref(), Some("agy-settings-override"));
    assert_eq!(found.verdict, Ok(Consented::Inherited));

    // The main working tree's consent revoked in the session's own state root.
    super::super::record::with_record(session.root.path(), &git.repository, |path, record| {
        let mut value = serde_json::to_value(record.unwrap()).unwrap();
        value["revoked"] = serde_json::json!(true);
        write_json_atomic(path, &value)
    })
    .unwrap();
    let found = authority(&session.directory, FirstPartyCli::Agy, &fixture.env).unwrap();
    assert_eq!(found.applied.as_deref(), Some("agy-settings-override"));
    assert!(found.verdict.unwrap_err().contains("source-reset"));

    // A withdrawn source, on a fresh session.
    let (_temp, fixture, git) = worktree();
    trust_pi(&fixture, &git.repository);
    let session = launched(&git.worktree, &fixture);
    fs::remove_file(&fixture.pi).unwrap();
    assert!(verdict(&session, FirstPartyCli::Agy, &fixture).is_err());

    // A record that is not a whole assessment is an error, whatever marker it carries.
    write_json_atomic(
        &session.directory.join(SESSION_FILE),
        &serde_json::json!({"state":"verified","applied":"agy-settings-override"}),
    )
    .unwrap();
    assert!(authority(&session.directory, FirstPartyCli::Agy, &fixture.env).is_err());
}

// The authority writes nothing, so doctor and a paste gate stay observations: with the shared
// records gone, it finds the same consent again and creates no record or directory.
#[test]
fn the_authority_writes_nothing() {
    let (_temp, fixture, git) = worktree();
    trust_pi(&fixture, &git.repository);
    let session = launched(&git.worktree, &fixture);
    let shared = session.root.path().join("workspace-consent");
    fs::remove_dir_all(&shared).unwrap();
    let before = fs::read(session.directory.join(SESSION_FILE)).unwrap();
    assert_eq!(
        verdict(&session, FirstPartyCli::Agy, &fixture),
        Ok(Consented::Inherited)
    );
    assert!(!shared.exists());
    assert_eq!(
        fs::read(session.directory.join(SESSION_FILE)).unwrap(),
        before
    );
}
