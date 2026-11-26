// The linked git worktree rule: a workspace that no provider trusts exactly inherits the
// consent of its repository's main working tree, derived again at every assessment.
use super::super::store::native_key;
use super::super::tests::{Fixture, session, setup, trust_pi};
use super::super::worktree::tests::{Registered, git_path, registered};
use super::tests::{PROVIDERS, replace_store, store_of, trust};
use super::*;
use crate::native::session::primitives::write_json_atomic;

// The evidence that `provider`'s fixture store yields for `path` itself.
fn exact(fixture: &Fixture, provider: FirstPartyCli, path: &Path) -> Evidence {
    match provider::workspace_trust(provider, path, &fixture.env, None).unwrap() {
        Trust::Trusted(evidence) => evidence,
        other => panic!("{provider:?}: {other:?}"),
    }
}

// A state root, a fixture home and a real repository with a linked worktree that Git
// registered, all under one temporary directory.
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

// Pi's store with these decisions, written the way the provider's private store is.
fn pi_decides(fixture: &Fixture, decisions: &[(&Path, bool)]) {
    let decisions: serde_json::Map<_, _> = decisions
        .iter()
        .map(|(path, trusted)| (native_key(path).unwrap(), serde_json::json!(trusted)))
        .collect();
    replace_store(
        &fixture.pi,
        serde_json::Value::Object(decisions).to_string().as_bytes(),
    )
    .unwrap();
}

// `consent revoke` of `path` in the state root `root`, as the command writes it.
fn revoke(root: &Path, path: &Path) {
    with_record(root, path, |record_path, record| {
        write_json_atomic(record_path, &changed(record, identity(path)?, true))
    })
    .unwrap();
}

#[test]
fn a_verified_linked_worktree_inherits_the_consent_of_its_main_working_tree() {
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
        // The main working tree's own consent was assessed, and its source pinned, as an
        // `ask` there would have.
        assert_eq!(
            with_record(temp.path(), &git.repository, |_, record| Ok(record
                .unwrap()
                .source))
            .unwrap(),
            first.source,
            "{provider:?}"
        );
        // The recheck verifies the same, with and without writing, and no store was written.
        for again in [
            assess_with(temp.path(), &git.worktree, &fixture.env).unwrap(),
            assess_current(temp.path(), &git.worktree, &fixture.env).unwrap(),
        ] {
            assert_eq!(again.state, "verified", "{provider:?}");
            assert_eq!(again.source, first.source, "{provider:?}");
            assert_eq!(again.repository_root, first.repository_root, "{provider:?}");
        }
        assert_eq!(stores(&fixture), written, "{provider:?}");
        assert_eq!(
            keys(&record_path(temp.path(), &git.worktree)),
            ["identity", "repository_root", "revoked", "schema", "source"],
            "{provider:?}"
        );
    }
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
    // A directory inside the worktree is no worktree, and a main working tree trusted only
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

// Review finding 1: the main working tree's own consent decides, as TAB assesses that folder,
// not the provider's store read past it. A revoke, a changed identity or a reset source of the
// main working tree stops inheritance, before and after the worktree's first assessment.
#[test]
fn the_main_working_trees_own_consent_decides() {
    // Revoked before the worktree's first assessment.
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.repository, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
    revoke(temp.path(), &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "unverified"
    );

    // Revoked after it.
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
    revoke(temp.path(), &git.repository);
    for state in [
        assess_with(temp.path(), &git.worktree, &fixture.env).unwrap(),
        assess_current(temp.path(), &git.worktree, &fixture.env).unwrap(),
    ]
    .map(|assessment| assessment.state)
    {
        assert_eq!(state, "source-reset");
    }

    // The main working tree's record names another identity.
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    with_record(temp.path(), &git.repository, |path, record| {
        let mut record = record.unwrap();
        record.identity.owner = "another-owner".into();
        write_json_atomic(path, &record)
    })
    .unwrap();
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "source-reset"
    );

    // The main working tree's pinned source no longer trusts it: another provider that does
    // is not substituted for the main working tree, and so not for the worktree.
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    replace_store(&fixture.claude, b"{\"projects\":{}}").unwrap();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.repository, &fixture.env)
            .unwrap()
            .state,
        "source-reset"
    );
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "source-reset"
    );
}

// Review finding 2: a source provider's own decline of the worktree is never turned into an
// approval by that provider's entry for the main working tree, at acquisition or later.
#[test]
fn a_source_that_declines_the_worktree_gives_it_nothing() {
    let (temp, fixture, git) = worktree_setup();
    pi_decides(&fixture, &[(&git.repository, true), (&git.worktree, false)]);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "unverified"
    );

    let (temp, fixture, git) = worktree_setup();
    pi_decides(&fixture, &[(&git.repository, true)]);
    let first = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(first.state, "verified");
    assert_eq!(first.source.unwrap().provider, "pi");
    pi_decides(&fixture, &[(&git.repository, true), (&git.worktree, false)]);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "source-reset"
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

    // The main working tree replaced at the same path, with the same repository in it: a new
    // identity, which neither the worktree's record nor the main working tree's accepts.
    let old = git.repository.with_file_name("repository-old");
    fs::rename(&git.repository, &old).unwrap();
    fs::create_dir(&git.repository).unwrap();
    fs::rename(old.join(".git"), git.repository.join(".git")).unwrap();
    assert_eq!(state(), "source-reset");
    // Reset clears the worktree's source and main working tree; with the main working tree's
    // own record reset too, a later assessment takes the current one.
    for path in [&git.worktree, &git.repository] {
        with_record(temp.path(), path, |record_path, record| {
            write_json_atomic(record_path, &changed(record, identity(path)?, false))
        })
        .unwrap();
    }
    let fresh = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(fresh.state, "verified");
    assert_eq!(
        fresh.repository_root,
        Some(identity(&git.repository).unwrap())
    );
    assert_eq!(fresh.source, verified.source);
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

// Review finding 6: the main working tree's own assessment during a worktree's start records
// only a verified pin; refusing, it leaves no record behind, while the workspace's own start
// records what it found as every earlier release did.
#[test]
fn the_main_working_tree_is_recorded_only_when_its_consent_is_verified() {
    let (temp, fixture, git) = worktree_setup();
    let main_record = record_path(temp.path(), &git.repository);
    let inherited = assess_in(temp.path(), &git.repository, &fixture.env, Mode::Inherit).unwrap();
    assert_eq!(inherited.state, "unverified");
    assert!(!main_record.exists());
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "unverified"
    );
    assert!(!main_record.exists());
    assert!(record_path(temp.path(), &git.worktree).exists());
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(
        assess_in(temp.path(), &git.repository, &fixture.env, Mode::Inherit)
            .unwrap()
            .state,
        "verified"
    );
    assert!(main_record.exists());
}

// Review finding 5: a recheck requires the pins recorded at the start; a reset of the
// worktree's record or of its main working tree's leaves nothing for it to recheck, and only a
// new start acquires consent again.
#[test]
fn a_recheck_requires_the_pins_and_acquires_nothing() {
    for reset in ["worktree", "main"] {
        let (temp, fixture, git) = worktree_setup();
        trust(&fixture, FirstPartyCli::Codex, &git.repository);
        assert_eq!(
            assess_with(temp.path(), &git.worktree, &fixture.env)
                .unwrap()
                .state,
            "verified"
        );
        let path = if reset == "worktree" {
            &git.worktree
        } else {
            &git.repository
        };
        with_record(temp.path(), path, |record_path, record| {
            write_json_atomic(record_path, &changed(record, identity(path)?, false))
        })
        .unwrap();
        let recheck = assess_current(temp.path(), &git.worktree, &fixture.env).unwrap();
        assert_ne!(recheck.state, "verified", "{reset}");
        assert_eq!(
            assess_with(temp.path(), &git.worktree, &fixture.env)
                .unwrap()
                .state,
            "verified",
            "{reset}: a new start acquires it again"
        );
    }
}

// A FIFO at `path`, so that opening it is observable.
#[cfg(unix)]
fn fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

// Whether anything opened the FIFO at `path` for reading while it was watched: a writer opened
// without blocking succeeds only while a reader has the FIFO open, and then lets that reader
// go on as a real `HEAD` would.
#[cfg(unix)]
struct Opened {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    watcher: std::thread::JoinHandle<bool>,
}

#[cfg(unix)]
impl Opened {
    fn watch(path: &Path) -> Self {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (stop, path) = (stop.clone(), path.to_owned());
            std::thread::spawn(move || {
                let mut opened = false;
                while !stop.load(Ordering::SeqCst) {
                    match fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&path)
                    {
                        Ok(mut reader_present) => {
                            opened = true;
                            let _ = reader_present.write_all(b"ref: refs/heads/main\n");
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
                    }
                }
                opened
            })
        };
        Self { stop, watcher }
    }

    fn stop(self) -> bool {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.watcher.join().unwrap()
    }
}

// Router's answer to the open question of round 3: the screen asks each provider's own store
// alone, never a reader that touches the path. A `.git` file that names a main working tree no
// store lists makes TAB open nothing and run nothing there. A reader that probes the claimed
// path's ancestors would find the `.git` above this destination and run `git` in it, and Git
// opens the `HEAD` it finds there: here a FIFO, whose opening is observable. A stat leaves no
// trace a test can observe; each adapter's own test shows that its store-only answer needs
// none.
#[cfg(unix)]
#[test]
fn a_destination_that_no_store_lists_is_never_opened_and_runs_no_git() {
    let (temp, fixture, git) = worktree_setup();
    let root = temp.path().canonicalize().unwrap();
    let share = root.join("share");
    let destination = share.join("repository");
    let gitdir = destination.join(".git");
    fs::create_dir_all(share.join(".git")).unwrap();
    for directory in ["objects", "refs", "worktrees/worktree"] {
        fs::create_dir_all(gitdir.join(directory)).unwrap();
    }
    let head = gitdir.join("HEAD");
    fifo(&head);
    fs::write(
        git.worktree.join(".git"),
        format!("gitdir: {}\n", git_path(&gitdir.join("worktrees/worktree"))),
    )
    .unwrap();
    // The stores list other folders only: the worktree's real main working tree, the
    // destination's parent, and (for Pi, positively) an ancestor of it.
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    trust(&fixture, FirstPartyCli::Claude, &share);
    trust_pi(&fixture, &root);
    let opened = Opened::watch(&head);
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert!(!opened.stop(), "the destination's HEAD was opened");
    assert_eq!(assessment.state, "unverified");
    assert_eq!(assessment.repository_root, None);
    assert!(!record_path(temp.path(), &destination).exists());
}

// On Windows a destination on a network share that no store lists is refused by the composed
// screen before any I/O on it: the assessment does not wait on the network. Runs on the Windows
// CI runner only.
#[cfg(windows)]
#[test]
fn a_share_that_no_store_lists_is_never_reached() {
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    trust(
        &fixture,
        FirstPartyCli::Claude,
        Path::new(r"\\tab-test.invalid\share"),
    );
    fs::write(
        git.worktree.join(".git"),
        "gitdir: //tab-test.invalid/share/repository/.git/worktrees/worktree\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "unverified");
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

// A main working tree that only Claude's store lists passes the screen, and Claude's own reader
// then decides its consent as for any exact folder: the worktree inherits it.
#[test]
fn a_main_working_tree_that_only_claude_trusts_still_gives_inheritance() {
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    trust_pi(
        &fixture,
        &temp.path().canonicalize().unwrap().join("elsewhere"),
    );
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "verified");
    assert_eq!(assessment.detail, INHERITED_DETAIL);
    assert_eq!(
        assessment.source,
        Some(exact(&fixture, FirstPartyCli::Claude, &git.repository))
    );
    assert_eq!(
        assessment.repository_root,
        Some(identity(&git.repository).unwrap())
    );
}

// The screen is a necessary condition only, never consent. Claude's store lists the main working
// tree, but Claude's own reader refuses it as an exact folder: a repository above it makes the
// reader ask Git, which cannot read this repository's format. The worktree inherits nothing and
// nothing is recorded for the main working tree; once the reader accepts it, the same store
// gives inheritance.
#[test]
fn a_main_working_tree_that_a_store_lists_but_its_reader_refuses_gives_nothing() {
    let (temp, fixture, git) = worktree_setup();
    let root = temp.path().canonicalize().unwrap();
    trust(&fixture, FirstPartyCli::Claude, &git.repository);
    let config = git.repository.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("[core]\n\trepositoryformatversion = 99\n");
    fs::write(&config, text).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    assert!(
        provider::workspace_trust(FirstPartyCli::Claude, &git.repository, &fixture.env, None)
            .is_err()
    );
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "unverified");
    assert_eq!(assessment.repository_root, None);
    assert!(!record_path(temp.path(), &git.repository).exists());
    fs::remove_dir(root.join(".git")).unwrap();
    let assessment = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(assessment.state, "verified");
    assert_eq!(assessment.detail, INHERITED_DETAIL);
    assert!(record_path(temp.path(), &git.repository).exists());
}

// Review r3, finding 9: a pinned recheck screens the main working tree against the store its pin
// names, not against the stores the caller's environment names now. A session that inherited
// Pi's trust from one Pi home keeps it when a later command runs with another home whose stores
// list nothing; acquisition (no pin yet) keeps discovering in the stores the environment names.
#[test]
fn a_pinned_recheck_screens_the_main_working_tree_against_the_pinned_store() {
    let (temp, fixture, git) = worktree_setup();
    trust_pi(&fixture, &git.repository);
    let first = assess_with(temp.path(), &git.worktree, &fixture.env).unwrap();
    assert_eq!(first.state, "verified");
    assert_eq!(first.detail, INHERITED_DETAIL);
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = Environment::at_home(&elsewhere.path().canonicalize().unwrap());
    for again in [
        assess_current(temp.path(), &git.worktree, &moved).unwrap(),
        assess_with(temp.path(), &git.worktree, &moved).unwrap(),
    ] {
        assert_eq!(again.state, "verified");
        assert_eq!(again.source, first.source);
        assert_eq!(again.repository_root, first.repository_root);
    }
    // Without a pin, the stores the environment names decide: none lists the main working tree.
    let (other_temp, other_fixture, other_git) = worktree_setup();
    trust_pi(&other_fixture, &other_git.repository);
    assert_eq!(
        assess_with(other_temp.path(), &other_git.worktree, &moved)
            .unwrap()
            .state,
        "unverified"
    );
    // The pinned store that no longer lists the main working tree refuses.
    trust_pi(
        &fixture,
        &temp.path().canonicalize().unwrap().join("elsewhere"),
    );
    assert_eq!(
        assess_current(temp.path(), &git.worktree, &moved)
            .unwrap()
            .state,
        "source-reset"
    );
}

// Review r3, finding 12: at a recheck, a registration that cannot be read is an error, never a
// refusal that reads as missing trust; a registration that is gone is a refusal.
#[cfg(unix)]
#[test]
fn an_unreadable_registration_is_an_error_at_a_recheck() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, fixture, git) = worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
    let gitdir = git.registration.join("gitdir");
    fs::set_permissions(&gitdir, fs::Permissions::from_mode(0o000)).unwrap();
    // A user that may read anything (root) cannot observe an unreadable file.
    if fs::read(&gitdir).is_err() {
        let error = assess_current(temp.path(), &git.worktree, &fixture.env).unwrap_err();
        assert!(format!("{error:#}").contains("gitdir"), "{error:#}");
    }
    fs::set_permissions(&gitdir, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&gitdir).unwrap();
    assert_eq!(
        assess_current(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "source-reset"
    );
}

// A repository and its linked worktree inside `root/nest`, whose permissions a test may change
// without touching the state root or the fixture stores at `root`.
fn nested_worktree_setup() -> (tempfile::TempDir, Fixture, Registered, PathBuf) {
    let (temp, _workspace, fixture) = setup();
    let nest = temp.path().canonicalize().unwrap().join("nest");
    fs::create_dir(&nest).unwrap();
    let git = registered(&nest);
    (temp, fixture, git, nest)
}

// Review r6, finding 1: the main working tree lost between its identification and the rest of its
// assessment during a recheck withdraws the inheritance, as a registration that is gone does; it
// is no error that would stop a plain launch's planning.
#[test]
fn a_main_working_tree_lost_during_a_recheck_withdraws_inheritance() {
    let (temp, fixture, git, nest) = nested_worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
    let (from, to) = (git.repository.clone(), nest.join("moved"));
    after_main_identified(move |_| fs::rename(&from, &to).unwrap());
    let recheck = assess_current(temp.path(), &git.worktree, &fixture.env)
        .map(|assessment| assessment.state)
        .map_err(|error| format!("{error:#}"));
    assert_eq!(recheck, Ok("source-reset".to_owned()));
    assert!(!git.repository.exists());
}

// Review r6, finding 1: evidence of the main working tree that cannot be read between its
// identification and the rest of its assessment is typed as unreadable at a recheck, never a raw
// error; TAB's own shared record of the main working tree that cannot be decoded keeps its error.
#[cfg(unix)]
#[test]
fn an_unreadable_main_working_tree_is_typed_and_a_damaged_main_record_keeps_its_error() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, fixture, git, nest) = nested_worktree_setup();
    trust(&fixture, FirstPartyCli::Codex, &git.repository);
    assert_eq!(
        assess_with(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
    let locked = nest.clone();
    after_main_identified(move |_| {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap()
    });
    let recheck = assess_current(temp.path(), &git.worktree, &fixture.env);
    fs::set_permissions(&nest, fs::Permissions::from_mode(0o755)).unwrap();
    // A user that may read anything (root) cannot observe an unreadable directory.
    if unsafe { libc::geteuid() } != 0 {
        let error = recheck.unwrap_err();
        assert!(error.is::<EvidenceUnreadable>(), "{error:#}");
    }

    let main_record = record_path(temp.path(), &git.repository);
    let saved = fs::read(&main_record).unwrap();
    replace_store(&main_record, b"{").unwrap();
    let error = assess_current(temp.path(), &git.worktree, &fixture.env).unwrap_err();
    assert!(!error.is::<EvidenceUnreadable>(), "{error:#}");
    replace_store(&main_record, &saved).unwrap();
    assert_eq!(
        assess_current(temp.path(), &git.worktree, &fixture.env)
            .unwrap()
            .state,
        "verified"
    );
}
