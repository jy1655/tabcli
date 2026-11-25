// The linked git worktree rule: a workspace that no provider trusts exactly inherits the exact
// trust of its repository's main working tree, verified at every assessment and recheck.
use super::super::store::native_key;
use super::super::tests::{Fixture, session, setup, trust_pi};
use super::super::worktree::tests::{Registered, git_path, git_worktree, registered};
use super::tests::{PROVIDERS, replace_store, store_of, trust};
use super::*;
use crate::native::session::primitives::{write_json_atomic, write_private};

// The evidence that `provider`'s fixture store yields for `path` itself.
fn exact(fixture: &Fixture, provider: FirstPartyCli, path: &Path) -> Evidence {
    match provider::workspace_trust(provider, path, &fixture.env, None).unwrap() {
        Trust::Trusted(evidence) => evidence,
        other => panic!("{provider:?}: {other:?}"),
    }
}

// A state root, a fixture home and a linked worktree registered as Git does, all under one
// temporary directory.
fn worktree_setup() -> (tempfile::TempDir, Fixture, Registered) {
    let (temp, _workspace, fixture) = setup();
    let git = registered(&temp.path().canonicalize().unwrap());
    (temp, fixture, git)
}

fn stores(fixture: &Fixture) -> Vec<Option<Vec<u8>>> {
    PROVIDERS
        .iter()
        .map(|provider| fs::read(store_of(fixture, *provider)).ok())
        .collect()
}

fn keys(path: &Path) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn a_verified_linked_worktree_inherits_the_exact_trust_of_its_main_working_tree() {
    for provider in PROVIDERS {
        let (temp, fixture, git) = worktree_setup();
        let before = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
        assert_eq!(before.state, "unverified", "{provider:?}");
        assert_eq!(before.repository_root, None, "{provider:?}");

        trust(&fixture, provider, &git.repository);
        let written = stores(&fixture);
        let first = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
        assert_eq!(first.state, "verified", "{provider:?}");
        assert_eq!(first.detail, INHERITED_DETAIL, "{provider:?}");
        assert_eq!(
            first.source,
            Some(exact(&fixture, provider, &git.repository)),
            "{provider:?}"
        );
        assert_eq!(
            first.repository_root,
            Some(identity(&git.repository).unwrap()),
            "{provider:?}"
        );
        // The recheck of the recorded source verifies the same, and no store was written.
        let again = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
        assert_eq!(again.state, "verified", "{provider:?}");
        assert_eq!(again.source, first.source, "{provider:?}");
        assert_eq!(again.repository_root, first.repository_root, "{provider:?}");
        assert_eq!(stores(&fixture), written, "{provider:?}");
        let shared = record_path(temp.path(), &git.worktree);
        assert_eq!(
            keys(&shared),
            ["identity", "repository_root", "revoked", "schema", "source"],
            "{provider:?}"
        );
    }
}

// Git's own worktrees, as `git worktree add` writes them on each platform.
#[test]
fn a_worktree_that_git_added_inherits_like_the_one_written_by_hand() {
    let (temp, _workspace, fixture) = setup();
    let (repository, worktree) = git_worktree(&temp.path().canonicalize().unwrap(), "added");
    trust(&fixture, FirstPartyCli::Agy, &repository);
    let assessment = assess_with(temp.path(), &worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "verified");
    assert_eq!(
        assessment.repository_root.map(|root| root.path),
        Some(repository)
    );
}

#[test]
fn exact_trust_comes_first_and_consent_extends_to_no_child_or_parent() {
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    trust_pi(&fixture, &git.worktree);
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "verified");
    assert_eq!(assessment.detail, EXACT_DETAIL);
    assert_eq!(
        assessment.source,
        Some(exact(&fixture, FirstPartyCli::Pi, &git.worktree))
    );
    assert_eq!(assessment.repository_root, None);
    assert_eq!(
        keys(&record_path(temp.path(), &git.worktree)),
        ["identity", "revoked", "schema", "source"]
    );
    // A directory inside the worktree is no worktree, and the main working tree trusted only
    // as a parent gives a worktree nothing.
    let child = git.worktree.join("child");
    fs::create_dir(&child).unwrap();
    assert_eq!(
        assess_with(temp.path(), &child, &fixture.env)
            .unwrap()
            .state,
        "unverified"
    );
    let (temp, fixture, git) = worktree_setup();
    trust(
        &fixture,
        FirstPartyCli::Codex,
        git.repository.parent().unwrap(),
    );
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "unverified"
    );
}

#[test]
fn inherited_consent_is_rechecked_against_the_registration_and_the_main_working_tree() {
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    let verified = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(verified.state, "verified");
    let state = || {
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state
    };

    // The registration no longer names this worktree back.
    let backlink = git.registration.join("gitdir");
    let saved = fs::read(&backlink).unwrap();
    fs::write(
        &backlink,
        format!("{}\n", git_path(&temp.path().join("x/.git"))),
    )
    .unwrap();
    assert_eq!(state(), "source-reset");
    fs::write(&backlink, &saved).unwrap();
    assert_eq!(state(), "verified");

    // The source's store no longer trusts the main working tree: no other provider is
    // substituted, not even one that trusts the worktree exactly, until it trusts it again.
    replace_store(&fixture.claude, b"{\"projects\":{}}").unwrap();
    assert_eq!(state(), "source-reset");
    trust_pi(&fixture, &git.worktree);
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(state(), "source-reset");
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    let again = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(again.state, "verified");
    assert_eq!(again.source, verified.source);

    // The main working tree replaced at the same path, with the same registration in it.
    let old = git.repository.with_file_name("repository-old");
    fs::rename(&git.repository, &old).unwrap();
    fs::create_dir(&git.repository).unwrap();
    fs::rename(old.join(".git"), git.repository.join(".git")).unwrap();
    assert_eq!(state(), "source-reset");
    // Reset clears the source and the main working tree; a later assessment takes the
    // current one, and exact trust of the worktree comes first again.
    with_record(temp.path(), &git.worktree, |path, record| {
        write_json_atomic(path, &changed(record, identity(&git.worktree)?, false))
    })
    .unwrap();
    let fresh = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(fresh.state, "verified");
    assert_eq!(fresh.repository_root, None);
    assert_eq!(
        fresh.source,
        Some(exact(&fixture, FirstPartyCli::Pi, &git.worktree))
    );
}

#[test]
fn revoke_keeps_the_main_working_tree_of_an_inherited_source_and_reset_clears_it() {
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    let identity = identity(&git.worktree).unwrap();
    with_record(temp.path(), &git.worktree, |path, record| {
        let record = record.unwrap();
        let revoked = changed(Some(record.clone()), identity.clone(), true);
        assert!(revoked.revoked);
        assert_eq!(revoked.source, record.source);
        assert_eq!(revoked.repository_root, record.repository_root);
        assert!(revoked.repository_root.is_some());
        let reset = changed(Some(record.clone()), identity.clone(), false);
        assert!(!reset.revoked);
        assert_eq!(reset.source, None);
        assert_eq!(reset.repository_root, None);
        // A record of another identity is not carried over.
        let mut other = identity.clone();
        other.owner = "another-owner".into();
        let replaced = changed(Some(record), other, true);
        assert_eq!(replaced.source, None);
        assert_eq!(replaced.repository_root, None);
        write_json_atomic(path, &revoked)
    })
    .unwrap();
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "revoked"
    );
}

// What a binary before this rule reads (0.4.49's record and assessment have no
// `repository_root`): unverified and exact records keep their keys, an inherited one parses
// there, and its exact recheck (the only rule that binary has) never verifies it, because the
// source's key is the main working tree's and not the workspace's.
#[test]
fn records_keep_the_shape_an_earlier_binary_reads_and_an_inherited_one_never_verifies_there() {
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct EarlierRecord {
        schema: u32,
        identity: Identity,
        source: Option<Evidence>,
        revoked: bool,
    }
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct EarlierAssessment {
        schema: u32,
        identity: Identity,
        source: Option<Evidence>,
        state: String,
        detail: String,
        applied: Option<String>,
    }
    const EARLIER_RECORD: [&str; 4] = ["identity", "revoked", "schema", "source"];
    const EARLIER_ASSESSMENT: [&str; 6] =
        ["applied", "detail", "identity", "schema", "source", "state"];
    for provider in PROVIDERS {
        let (temp, fixture, git) = worktree_setup();
        let (_root, directory) = session(&git.worktree, "launching");
        let shared = record_path(temp.path(), &git.worktree);
        let session_file = directory.join(SESSION_FILE);
        record(
            &directory,
            &assess_with(temp.path(), &git.worktree, &fixture.env).unwrap(),
        )
        .unwrap();
        assert_eq!(keys(&shared), EARLIER_RECORD);
        assert_eq!(keys(&session_file), EARLIER_ASSESSMENT);

        trust(&fixture, provider, &git.repository);
        record(
            &directory,
            &assess_with(temp.path(), &git.worktree, &fixture.env).unwrap(),
        )
        .unwrap();
        let earlier: EarlierRecord = serde_json::from_slice(&fs::read(&shared).unwrap()).unwrap();
        let _: EarlierAssessment =
            serde_json::from_slice(&fs::read(&session_file).unwrap()).unwrap();
        let source = earlier.source.unwrap();
        let cli = FirstPartyCli::from_str(&source.provider).unwrap();
        assert_ne!(
            provider::workspace_trust(
                cli,
                &earlier.identity.path,
                &fixture.env,
                Some(&source.store)
            )
            .ok(),
            Some(Trust::Trusted(source.clone())),
            "{provider:?}"
        );
        assert_ne!(
            source.key,
            native_key(&git.worktree).unwrap(),
            "{provider:?}"
        );
    }
    // An exact source written now is the record 0.4.49 writes.
    let (temp, fixture, git) = worktree_setup();
    trust_pi(&fixture, &git.worktree);
    let exact = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(exact.state, "verified");
    assert_eq!(
        keys(&record_path(temp.path(), &git.worktree)),
        EARLIER_RECORD
    );
    assert!(
        serde_json::to_value(&exact)
            .unwrap()
            .get("repository_root")
            .is_none()
    );
}

#[test]
fn a_target_that_declines_the_worktree_or_its_main_working_tree_takes_no_inherited_consent() {
    let (_temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    let (root, directory) = session(&git.worktree, "launching");
    let assessment = assess_with(root.path(), &git.worktree, &fixture.env).unwrap();
    assert!(assessment.repository_root.is_some());
    record(&directory, &assessment).unwrap();
    for target in PROVIDERS {
        assert!(
            authorized_with(&directory, target, &fixture.env).unwrap(),
            "{target:?}"
        );
    }
    for declined in [&git.repository, &git.worktree] {
        if fixture.pi.exists() {
            fs::remove_file(&fixture.pi).unwrap();
        }
        write_private(
            &fixture.pi,
            serde_json::json!({ native_key(declined).unwrap(): false })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(
            !authorized_with(&directory, FirstPartyCli::Pi, &fixture.env).unwrap(),
            "{}",
            declined.display()
        );
        assert!(authorized_with(&directory, FirstPartyCli::Codex, &fixture.env).unwrap());
    }
    // A session record whose main working tree differs from the current assessment's is not
    // authorized.
    let mut forged = assessment.clone();
    forged.repository_root.as_mut().unwrap().owner = "another-owner".into();
    record(&directory, &forged).unwrap();
    assert!(!authorized_with(&directory, FirstPartyCli::Codex, &fixture.env).unwrap());
}
