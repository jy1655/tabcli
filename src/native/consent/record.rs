use super::store::{Environment, Evidence, Trust, read_store, user_home};
use super::worktree;
use super::{Identity, identity};
use crate::native::provider;
use crate::native::session::root_privacy::{self, FailurePolicy};
use crate::native::session::{Reader, RecordReader, RecordStore, Store};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

// `repository_root` is the identity of the main working tree whose exact trust `source` names,
// for a workspace that is a verified linked git worktree of that repository. It is absent for
// exact consent and then not written, so such a record keeps the bytes of a binary before it;
// that binary ignores the field and refuses an inherited source, whose key is not the
// workspace's (see the stored-data note in AGENTS.md, Trust Store Boundary).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Record {
    schema: u32,
    identity: Identity,
    source: Option<Evidence>,
    pub(super) revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository_root: Option<Identity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct Assessment {
    pub(super) schema: u32,
    pub(super) identity: Identity,
    pub(in crate::native) source: Option<Evidence>,
    pub(in crate::native) state: String,
    pub(in crate::native) detail: String,
    pub(in crate::native) applied: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) repository_root: Option<Identity>,
}

const EXACT_DETAIL: &str = "Exact provider trust and canonical directory identity verified. Tool permissions are unchanged.";
const INHERITED_DETAIL: &str = "The workspace is a verified linked git worktree, and its repository's main working tree has exact provider trust; the canonical identities of both are verified. Tool permissions are unchanged.";

pub(super) const SESSION_FILE: &str = "workspace-consent.json";

#[derive(Debug, Default)]
pub(in crate::native) struct SessionConsentObservation {
    value: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum ConsentState {
    Verified,
    Unverified,
    Alias,
    IdentityChanged,
    Revoked,
    SourceReset,
}

impl SessionConsentObservation {
    pub(in crate::native) fn state(&self) -> Option<ConsentState> {
        match self.value.get("state").and_then(|state| state.as_str()) {
            Some("verified") => Some(ConsentState::Verified),
            Some("unverified") => Some(ConsentState::Unverified),
            Some("alias") => Some(ConsentState::Alias),
            Some("identity-changed") => Some(ConsentState::IdentityChanged),
            Some("revoked") => Some(ConsentState::Revoked),
            Some("source-reset") => Some(ConsentState::SourceReset),
            _ => None,
        }
    }

    pub(in crate::native) fn applied_marker(&self) -> Option<&str> {
        self.value.get("applied").and_then(|marker| marker.as_str())
    }

    pub(in crate::native) fn value(&self) -> &serde_json::Value {
        &self.value
    }

    pub(in crate::native) fn into_value(self) -> serde_json::Value {
        self.value
    }
}

fn alias_free(requested: &Path, canonical: &Path) -> Result<bool> {
    let absolute = std::path::absolute(requested)?;
    // Inspect every component: canonicalizing first would erase a symlink alias.
    for ancestor in absolute.ancestors() {
        if worktree::is_link(&fs::symlink_metadata(ancestor)?) {
            return Ok(false);
        }
    }
    Ok(absolute.canonicalize()? == canonical)
}

// The order in which providers' stores are asked.
const SOURCES: [FirstPartyCli; 4] = [
    FirstPartyCli::Codex,
    FirstPartyCli::Claude,
    FirstPartyCli::Agy,
    FirstPartyCli::Pi,
];

// The first provider, in the order of `SOURCES`, whose own store trusts `path` exactly. One
// provider's error does not stop the next from being asked.
fn exact_source(path: &Path, env: &Environment) -> Option<Evidence> {
    SOURCES.into_iter().find_map(
        |cli| match provider::workspace_trust(cli, path, env, None) {
            Ok(Trust::Trusted(source)) => Some(source),
            _ => None,
        },
    )
}

// The worktree screen: whether some provider's own store, asked in the order of `SOURCES`, lists
// exactly `path` as trusted, each answered from the store alone, with no I/O on `path` or its
// ancestors and no `git`. A main working tree that a `.git` file names is therefore never
// touched before a store lists it. The screen is a necessary condition only, never consent: the
// main working tree that passes it is assessed afterwards by the full readers, exactly as an
// exact folder is, and a path a store lists but its reader refuses gives no inheritance. A
// pinned source is screened in the one store its pin names, whatever
// stores the caller's environment names now; that store's read error is the screen's error.
// Without a pin, one provider's error does not stop the next from being asked.
fn listed(path: &Path, env: &Environment, pin: Option<&Evidence>) -> Result<bool> {
    match pin {
        Some(source) => provider::store_lists_trusted(
            FirstPartyCli::from_str(&source.provider).map_err(anyhow::Error::msg)?,
            path,
            env,
            Some(&source.store),
        ),
        None => Ok(SOURCES.into_iter().any(|cli| {
            matches!(
                provider::store_lists_trusted(cli, path, env, None),
                Ok(true)
            )
        })),
    }
}

/// Provider or Git evidence that a recheck could not read: a pinned source's store, the
/// registration and the main working tree that a `.git` file names, the source's reading of the
/// worktree. The authority reports it as a refusal marked unreadable. Any
/// other error, such as TAB's own shared consent record that cannot be read or decoded, or the
/// workspace that cannot be identified, propagates as in 0.4.49.
#[derive(Debug)]
pub(in crate::native) struct EvidenceUnreadable(&'static str);

impl std::fmt::Display for EvidenceUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for EvidenceUnreadable {}

// Whether `error` says that evidence could not be read, rather than that a fact does not hold:
// an I/O error other than a missing file. A missing registration, main working tree or `.git`
// file is a fact (the worktree is no longer registered), and so is content that is not what
// Git writes.
fn unreadable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() != std::io::ErrorKind::NotFound)
    })
}

fn record_path(root: &Path, workspace: &Path) -> PathBuf {
    // Collisions cannot grant consent: every read also checks the full path and identity.
    let hash = workspace
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    root.join("workspace-consent")
        .join(format!("{hash:016x}.json"))
}

pub(super) fn with_record<T>(
    root: &Path,
    workspace: &Path,
    f: impl FnOnce(&Path, Option<Record>) -> Result<T>,
) -> Result<T> {
    with_record_under(root, workspace, FailurePolicy::Refuse, f)
}

fn with_record_under<T>(
    root: &Path,
    workspace: &Path,
    policy: FailurePolicy,
    f: impl FnOnce(&Path, Option<Record>) -> Result<T>,
) -> Result<T> {
    let path = record_path(root, workspace);
    let parent = path.parent().unwrap();
    if fs::symlink_metadata(parent).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent directory");
    }
    use crate::native::session_start::deadline;
    deadline::before("preparing the workspace consent directory")?;
    let create_directory = |directory: &Path, fact| -> std::io::Result<()> {
        match fs::create_dir(directory) {
            Ok(()) => deadline::wrote(fact),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => return Err(error),
        }
        Ok(())
    };
    let created = crate::native::session::primitives::create_root_directories(
        root,
        "the state root was created",
        policy,
    )?;
    root_privacy::prepare(root, created, policy)?;
    create_directory(parent, "the workspace consent directory was created")?;
    deadline::set_permissions(
        parent,
        "workspace consent directory permissions were set",
        || RecordStore::at(parent).set_directory_private(),
    )?;
    let lock_path = path.with_extension("lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent lock");
    }
    deadline::before("opening the workspace consent lock")?;
    let lock = deadline::open_lock(
        &lock_path,
        OpenOptions::new().read(true).write(true),
        "the workspace consent lock file was created",
    )?;
    deadline::set_permissions(
        &lock_path,
        "workspace consent lock permissions were set",
        || RecordStore::set_file_private(&lock),
    )?;
    match deadline::current() {
        Some(end) => deadline::lock(&lock, end, "locking workspace consent")?,
        None => lock.lock()?,
    }
    let record = read_store(&path)?
        .map(|s| serde_json::from_str(&s))
        .transpose()?;
    f(&path, record)
}

// What an assessment may do beyond reading. `Record` is the assessment at a session's start
// (`ask`, a supported `reopen`): it looks for a first source where none is pinned and records
// what it finds, as every earlier release did. `Inherit` is the main working tree's own
// assessment during such a start: it may look for a first source too, but records only a
// verified one, so a refusal leaves no record behind. `Recheck` is the authority's: it never
// looks for a source, requires the pins already recorded, and writes nothing, so a reset
// consent is acquired again only by a new start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Mode {
    Record,
    Inherit,
    Recheck,
}

pub(super) fn assess_with(root: &Path, requested: &Path, env: &Environment) -> Result<Assessment> {
    assess_in(root, requested, env, Mode::Record)
}

// The recheck: the pins as they are now, without the lock or any write.
pub(super) fn assess_current(
    root: &Path,
    requested: &Path,
    env: &Environment,
) -> Result<Assessment> {
    assess_in(root, requested, env, Mode::Recheck)
}

fn assess_in(root: &Path, requested: &Path, env: &Environment, mode: Mode) -> Result<Assessment> {
    let identity = identity(requested)?;
    let alias_free = alias_free(requested, &identity.path)?;
    assess_identified(root, identity, alias_free, env, mode)
}

// The assessment of a workspace already identified, and whether the path it was requested by
// reaches it without an alias.
fn assess_identified(
    root: &Path,
    identity: Identity,
    alias_free: bool,
    env: &Environment,
    mode: Mode,
) -> Result<Assessment> {
    let mut assessment = Assessment {
        schema: 1,
        identity: identity.clone(),
        source: None,
        state: "unverified".into(),
        detail:
            "No exact provider workspace trust was verified; use the provider's own trust prompt."
                .into(),
        applied: None,
        repository_root: None,
    };
    if !alias_free {
        assessment.state = "alias".into();
        assessment.detail =
            "Workspace aliases do not inherit shared consent. Use the canonical path.".into();
        return Ok(assessment);
    }
    match mode {
        Mode::Record | Mode::Inherit => with_record(root, &identity.path, |path, existing| {
            let (assessment, write) = decide(root, existing, assessment, env, mode)?;
            if let Some(record) = write {
                crate::native::session_start::deadline::before("writing workspace consent")?;
                RecordStore::at(path).write_json(&record)?;
                crate::native::session_start::deadline::wrote("workspace consent was recorded");
            }
            Ok(assessment)
        }),
        Mode::Recheck => {
            let existing = read_record(root, &identity.path)?;
            Ok(decide(root, existing, assessment, env, mode)?.0)
        }
    }
}

// The shared record of `workspace` read without its lock: records are replaced atomically,
// so a reader sees one whole record. A missing directory or record is none.
fn read_record(root: &Path, workspace: &Path) -> Result<Option<Record>> {
    let path = record_path(root, workspace);
    let parent = path.parent().context("consent record has no directory")?;
    if fs::symlink_metadata(parent).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent directory");
    }
    Ok(read_store(&path)?
        .map(|s| serde_json::from_str(&s))
        .transpose()?)
}

// What one assessment concludes from the workspace's shared record and the current stores,
// and the record to write.
fn decide(
    root: &Path,
    existing: Option<Record>,
    mut assessment: Assessment,
    env: &Environment,
    mode: Mode,
) -> Result<(Assessment, Option<Record>)> {
    let identity = assessment.identity.clone();
    let mut record = existing.unwrap_or_else(|| Record {
        schema: 1,
        identity: identity.clone(),
        source: None,
        revoked: false,
        repository_root: None,
    });
    // The main working tree of an inherited source is assessed with the same reach: looking
    // for its first source only while this assessment may.
    let main_mode = match mode {
        Mode::Record | Mode::Inherit => Mode::Inherit,
        Mode::Recheck => Mode::Recheck,
    };
    let mut write = None;
    if record.schema != 1 || record.identity != identity {
        assessment.state = "identity-changed".into();
        assessment.detail =
            "Directory identity or owner changed; shared consent requires an explicit reset."
                .into();
    } else if record.revoked {
        assessment.state = "revoked".into();
        assessment.detail =
            "Shared workspace consent is revoked; provider-owned trust is unchanged.".into();
    } else if let Some(source) = &record.source {
        let cli = FirstPartyCli::from_str(&source.provider).map_err(anyhow::Error::msg)?;
        let holds = match &record.repository_root {
            // The one provider the record names rechecks the store it was read from, never
            // the store its home resolves to now. At a recheck a store that cannot be read is
            // an error, never a reset; a start records a reset, as every
            // earlier release did.
            None => {
                match provider::workspace_trust(cli, &identity.path, env, Some(&source.store)) {
                    Ok(Trust::Trusted(now)) => now == *source,
                    Ok(_) => false,
                    Err(error) if mode == Mode::Recheck => {
                        return Err(error.context(EvidenceUnreadable(
                            "the pinned source's trust cannot be read",
                        )));
                    }
                    Err(_) => false,
                }
            }
            // An inherited source is derived again: the same main working tree, by identity,
            // whose own consent still names this source.
            Some(main) => inherited(root, &identity.path, env, main_mode, Some(source))?
                .is_some_and(|(now, now_main)| now == *source && now_main == *main),
        };
        if holds {
            assessment.source = Some(source.clone());
            assessment.repository_root = record.repository_root.clone();
        } else {
            assessment.state = "source-reset".into();
            assessment.detail = "The original provider trust is absent, declined, or unreadable. No other provider is silently substituted; reset consent after approving again.".into();
        }
    } else if mode == Mode::Recheck {
        // No pin: a recheck acquires nothing.
        assessment.detail = "No source is pinned for this workspace (it was never verified, or its consent was reset); a new ask acquires consent again.".into();
    } else {
        // Exact trust of the workspace first. Only a workspace that no provider trusts
        // exactly inherits the consent of its repository's main working tree.
        let found = match exact_source(&identity.path, env) {
            Some(source) => Some((source, None)),
            None => inherited(root, &identity.path, env, main_mode, None)?
                .map(|(s, main)| (s, Some(main))),
        };
        record.repository_root = None;
        if let Some((source, main)) = found {
            assessment.source = Some(source.clone());
            assessment.repository_root = main.clone();
            record.source = Some(source);
            record.repository_root = main;
        }
        // The main working tree's own assessment records only what it verified.
        if mode == Mode::Record || record.source.is_some() {
            write = Some(record);
        }
    }
    if assessment.source.is_some() {
        assessment.state = "verified".into();
        assessment.detail = if assessment.repository_root.is_some() {
            INHERITED_DETAIL
        } else {
            EXACT_DETAIL
        }
        .into();
    }
    Ok((assessment, write))
}

// The source and the main working tree that `workspace` inherits consent from, derived from
// current truth: the claimed main working tree screened (`listed`, in the pinned store when
// `pin` names one) before any I/O on it, Git's registration of `workspace` there verified by
// identity, the main working tree's own consent exactly as TAB assesses that folder (its pinned
// source, identity and revocation), and a source provider that neither declines nor fails to
// read the worktree itself. Anything that does not hold inherits nothing. Evidence that cannot
// be read inherits nothing while a start looks for inheritance; at a recheck it is an error, so
// the authority reports it as unreadable rather than as missing trust.
// A missing file or folder is a fact, not unreadable evidence: the worktree is no longer
// registered, or its main working tree is gone, as the verifier's own reads find it. A pinned
// source's reader that touches the main working tree itself (Claude's scope probe) and fails is
// the source's evidence that cannot be read, also when the folder went during that probe. TAB's own
// shared record of the main working tree that cannot be read or
// decoded is an error at a recheck, as the workspace's own record is.
fn inherited(
    root: &Path,
    workspace: &Path,
    env: &Environment,
    mode: Mode,
    pin: Option<&Evidence>,
) -> Result<Option<(Evidence, Identity)>> {
    let strict = mode == Mode::Recheck;
    // At a recheck, provider and Git evidence that cannot be read is typed as such, and the main
    // working tree's own assessment passes its errors on as they are.
    let unread = |error: anyhow::Error| {
        if strict || error.is::<crate::native::failure_kind::TimedOut>() {
            Err(error)
        } else {
            Ok(None)
        }
    };
    let evidence = |error: anyhow::Error, what: &'static str| {
        if strict {
            Err(error.context(EvidenceUnreadable(what)))
        } else {
            Ok(None)
        }
    };
    let mut screen_error = None;
    let verified = worktree::main_worktree(workspace, &mut |main| {
        listed(main, env, pin).unwrap_or_else(|error| {
            screen_error = Some(error);
            false
        })
    });
    let main = match verified {
        Ok(Some(main)) => main,
        Ok(None) => return Ok(None),
        Err(error) => {
            return match screen_error {
                Some(error) => evidence(error, "the pinned source's store cannot be read"),
                None if unreadable(&error) => {
                    evidence(error, "the worktree's registration cannot be read")
                }
                None => Ok(None),
            };
        }
    };
    let main_identity = match identity(&main) {
        Ok(identity) => identity,
        Err(error) if unreadable(&error) => {
            return evidence(error, "the main working tree cannot be identified");
        }
        Err(_) => return Ok(None),
    };
    #[cfg(test)]
    if let Some(hook) = AFTER_MAIN_IDENTIFIED.with(|slot| slot.borrow_mut().take()) {
        hook(&main);
    }
    // The main working tree is identified once, and the rest of its assessment uses that
    // identity. A failure is classified where it arises: the folder gone
    // since it was identified withdraws the inheritance, as a registration that is gone does, and
    // a folder that cannot be inspected is unreadable evidence; neither is an error that would stop
    // a plain launch. TAB's own shared record of the main working tree keeps its error below.
    let main_alias_free = match alias_free(&main, &main_identity.path) {
        Ok(alias_free) => alias_free,
        Err(error) if unreadable(&error) => {
            return evidence(error, "the main working tree cannot be inspected");
        }
        Err(_) => return Ok(None),
    };
    // Two records under one lock file would wait on each other.
    if record_path(root, &main) == record_path(root, workspace) {
        return Ok(None);
    }
    let consent = match assess_identified(root, main_identity.clone(), main_alias_free, env, mode) {
        Ok(consent) => consent,
        Err(error) => return unread(error),
    };
    if consent.state != "verified" || consent.repository_root.is_some() {
        return Ok(None);
    }
    let Some(source) = consent.source else {
        return Ok(None);
    };
    let cli = match FirstPartyCli::from_str(&source.provider) {
        Ok(cli) => cli,
        Err(error) => return unread(anyhow::Error::msg(error)),
    };
    match provider::workspace_trust(cli, workspace, env, Some(&source.store)) {
        // An absent Claude entry needs no further scope check because the worktree verifier
        // requires the worktree's .git to be a regular file and rejects links.
        Ok(Trust::Absent | Trust::Trusted(_)) => Ok(Some((source, main_identity))),
        Ok(Trust::Declined) => Ok(None),
        Err(error) => evidence(error, "the source's trust of the worktree cannot be read"),
    }
}

// A test's hook that runs once, right after the main working tree of an inherited source was
// identified and before anything else reads it: a test places a change of that folder there.
#[cfg(test)]
type MainIdentifiedHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static AFTER_MAIN_IDENTIFIED: std::cell::RefCell<Option<MainIdentifiedHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::native) fn after_main_identified(hook: impl FnOnce(&Path) + 'static) {
    AFTER_MAIN_IDENTIFIED.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

// The assessment may update shared consent before Session creation. Reopen claims the
// source only after assessment succeeds and records the assessment with the new Session.
pub(in crate::native) fn assess(root: &Path, requested: &Path) -> Result<Assessment> {
    let env = Environment::current();
    user_home(&env)?;
    assess_with(root, requested, &env)
}

pub(in crate::native) fn record(directory: &Path, assessment: &Assessment) -> Result<()> {
    Store::open_unchecked(directory)
        .private(SESSION_FILE)
        .write_json(assessment)
}

#[cfg(test)]
pub(in crate::native) fn prepare(directory: &Path, requested: &Path) -> Result<()> {
    let root = directory.parent().context("session has no state root")?;
    record(directory, &assess(root, requested)?)
}

// `prepare` for a caller whose only provider home is the fixture `home`, so that no test reads
// the stores of the user running it.
#[cfg(test)]
pub(in crate::native) fn prepare_at_home(
    directory: &Path,
    requested: &Path,
    home: &Path,
) -> Result<Assessment> {
    let root = directory.parent().context("session has no state root")?;
    let assessment = assess_with(root, requested, &Environment::at_home(home))?;
    record(directory, &assessment)?;
    Ok(assessment)
}

// The session's consent record, changed under its own lock: a launch that records the option
// it applied and the dialog step that records its answer are serialized on it.
const SESSION_LOCK: &str = "workspace-consent.lock";

/// The session record's lock was held by another writer for the whole remaining budget, so the
/// record was not changed.
#[derive(Debug)]
pub(in crate::native) struct SessionRecordBusy;

impl std::fmt::Display for SessionRecordBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the session's consent record stayed locked by another writer for the remaining time, so it was not changed",
        )
    }
}

impl std::error::Error for SessionRecordBusy {}

// `deadline` bounds the wait for the lock: an operation with a deadline
// takes the lock with its remaining budget, and at least one attempt, and its expiry changes
// nothing. A launch plan records without one, as before.
fn with_session_record<T>(
    directory: &Path,
    deadline: Option<Instant>,
    f: impl FnOnce(&Path, Assessment) -> Result<T>,
) -> Result<T> {
    let reader = Reader::open_unchecked(directory);
    let lock_path = reader.private(SESSION_LOCK).path().to_owned();
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent lock");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    RecordStore::set_file_private(&lock)?;
    match deadline {
        None => lock.lock()?,
        Some(deadline) => loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(SessionRecordBusy.into());
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        },
    }
    let path = reader.private(SESSION_FILE).path().to_owned();
    let assessment: Assessment = RecordReader::at(&path).json()?;
    f(&path, assessment)
}

/// Records that a launch plan put `provider`'s trust-bearing option `marker` in its command, and
/// returns the grant the plan carries to the spawn fence: the option and the identity of the
/// workspace the session's assessment consented to. `workspace` is the one the command was built
/// for (the wrapper's snapshot that gives it its working directory and its option): a consent
/// record for another directory, or one whose identity is not that directory's now, grants
/// nothing.
/// `option_file` is a file the option names (Agy's settings) and the bytes the plan wrote to it:
/// the grant names it, and the spawn fence refuses a command whose file is not that one.
/// A write that fails can already have replaced the record (the replacement comes before the
/// syncs that make it durable): the record is then returned to its earlier value under the same
/// lock, so no marker names a command that is never built, and an earlier plan's marker stays. When
/// that return fails too, the record can keep the attempted marker,
/// and the error says that this attempt received no grant and which marker may remain.
pub(in crate::native) fn apply_option(
    directory: &Path,
    provider: FirstPartyCli,
    marker: &'static str,
    workspace: &Path,
    option_file: Option<(&Path, &[u8])>,
) -> Result<super::authority::LaunchGrant> {
    with_session_record(directory, None, |path, mut assessment| {
        command_workspace(&assessment, workspace)?;
        let earlier = assessment.clone();
        assessment.applied = Some(marker.to_owned());
        if let Err(error) = RecordStore::at(path).write_json(&assessment) {
            return Err(match restore_earlier(path, &earlier) {
                Ok(()) => error,
                Err(restore) => error.context(format!(
                    "this attempt received no grant for the launch option {marker}, and the session's consent record may still name it: returning the record to {} failed ({restore:#})",
                    earlier.applied.as_deref().unwrap_or("none")
                )),
            });
        }
        Ok(super::authority::LaunchGrant {
            provider,
            marker,
            workspace: assessment.identity,
            option_file: option_file.map(|(file, bytes)| super::authority::OptionFile {
                path: file.to_owned(),
                sha256: sha2::Sha256::digest(bytes).into(),
            }),
        })
    })
}

// Returns the session record at `path`, whose lock the caller holds, to `earlier` unless it
// already holds `earlier`'s applied value.
fn restore_earlier(path: &Path, earlier: &Assessment) -> Result<()> {
    if RecordReader::at(path)
        .json::<Assessment>()
        .is_ok_and(|now| now.applied == earlier.applied)
    {
        return Ok(());
    }
    RecordStore::at(path).write_json(earlier)
}

/// Checks, before a plan publishes anything for its option, that the session's consent record
/// was assessed for `workspace`, the directory the command was built for.
pub(in crate::native) fn check_command_workspace(directory: &Path, workspace: &Path) -> Result<()> {
    let path = Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .path()
        .to_owned();
    command_workspace(&RecordReader::at(&path).json()?, workspace)
}

fn command_workspace(assessment: &Assessment, workspace: &Path) -> Result<()> {
    if assessment.identity.path != workspace || identity(workspace)? != assessment.identity {
        bail!(
            "the session's consent names another workspace than the command's, so its launch option is not granted"
        );
    }
    Ok(())
}

pub(in crate::native) fn applied(directory: &Path, method: &str) -> Result<()> {
    with_session_record(directory, None, |path, mut assessment| {
        assessment.applied = Some(method.to_owned());
        RecordStore::at(path).write_json(&assessment)
    })
}

/// The step's deadline was reached once the session record's lock was taken, so the record was
/// not changed.
#[derive(Debug)]
pub(in crate::native) struct RecordDeadlinePassed;

impl std::fmt::Display for RecordDeadlinePassed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the deadline was reached when the session's consent record could be changed, so it was not changed")
    }
}

impl std::error::Error for RecordDeadlinePassed {}

/// Changes the session record's applied marker from `from` to `to` only while it still is
/// `from`, under the record's lock taken by `deadline`, and only while `deadline` has not passed
/// once the lock is held; whether it did. A lock that stays held until then is
/// `SessionRecordBusy`, a deadline reached is `RecordDeadlinePassed`, and the record is unchanged.
/// `now` is the caller's clock, and the deadline is reached at it, as in the caller's own check
/// before the call.
pub(in crate::native) fn transition(
    directory: &Path,
    from: Option<&str>,
    to: Option<&str>,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> Result<bool> {
    with_session_record(directory, Some(deadline), |path, mut assessment| {
        if now() >= deadline {
            return Err(RecordDeadlinePassed.into());
        }
        if assessment.applied.as_deref() != from {
            return Ok(false);
        }
        assessment.applied = to.map(str::to_owned);
        RecordStore::at(path).write_json(&assessment)?;
        Ok(true)
    })
}

/// What a restore found in the session record's applied value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Restore {
    /// It held `pending`, and now holds the assessed value.
    Restored,
    /// It already held the assessed value.
    Assessed,
    /// Another writer changed it; its value is kept.
    ChangedBy(Option<String>),
}

/// Returns the applied value from `pending` to `assessed`, under the record's lock taken by
/// `deadline` (at least one attempt, also after the deadline: a cleanup, not a commit), and says
/// what it found.
pub(in crate::native) fn restore(
    directory: &Path,
    pending: &str,
    assessed: Option<&str>,
    deadline: Instant,
) -> Result<Restore> {
    with_session_record(directory, Some(deadline), |path, mut assessment| {
        let found = assessment.applied.clone();
        if found.as_deref() == assessed {
            return Ok(Restore::Assessed);
        }
        if found.as_deref() != Some(pending) {
            return Ok(Restore::ChangedBy(found));
        }
        assessment.applied = assessed.map(str::to_owned);
        RecordStore::at(path).write_json(&assessment)?;
        Ok(Restore::Restored)
    })
}

/// The applied value of a launch that positively could not apply its adapter's launch option,
/// because the provider was identified as older than the option: only such a launch may still
/// take the guarded dialog answer for exact consent.
pub(in crate::native) const LAUNCH_OPTION_UNAVAILABLE: &str = "launch-option-unavailable";

/// Records `LAUNCH_OPTION_UNAVAILABLE` for a session that has a consent record.
pub(in crate::native) fn option_unavailable(directory: &Path) -> Result<()> {
    let path = Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .path()
        .to_owned();
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => applied(directory, LAUNCH_OPTION_UNAVAILABLE),
    }
}

/// The applied value in the session's consent record, read as its typed record: none without a
/// record, and an error for a record that cannot be read or is not a whole assessment. Agy's
/// setting gate reads it here; the Codex wait keeps its existing `observe` reading, which only
/// selects whether to ask the typed authority, so its independent routes stay as they were.
pub(in crate::native) fn session_applied(directory: &Path) -> Result<Option<String>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .text()?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<Assessment>(&text)?.applied)
}

pub(in crate::native) fn observe(directory: &Path) -> SessionConsentObservation {
    let value = match Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .text()
    {
        Ok(Some(text)) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| serde_json::json!({"state":"unreadable","detail":e.to_string()})),
        Ok(None) => serde_json::Value::Null,
        Err(e) => serde_json::json!({"state":"unreadable","detail":e.to_string()}),
    };
    SessionConsentObservation { value }
}

pub(in crate::native) fn run(args: &[String]) -> Result<()> {
    let [action, workspace, rest @ ..] = args else {
        bail!("consent requires inspect|revoke|reset PATH [--json]");
    };
    if !matches!(action.as_str(), "inspect" | "revoke" | "reset")
        || !matches!(rest, [] | [_] if rest.first().is_none_or(|s| s == "--json"))
    {
        bail!("consent requires inspect|revoke|reset PATH [--json]");
    }
    let workspace = Path::new(workspace);
    let identity = identity(workspace)?;
    let root = Reader::state_root()?;
    if action == "inspect" {
        let path = record_path(&root, &identity.path);
        let record = read_store(&path)?
            .map(|s| serde_json::from_str::<serde_json::Value>(&s))
            .transpose()?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"workspace":identity.path,"current_identity":identity,"record":record})
            )?
        );
        return Ok(());
    }
    with_record_under(
        &root,
        &identity.path.clone(),
        FailurePolicy::Warn,
        |path, existing| {
            let record = changed(existing, identity, action == "revoke");
            RecordStore::at(path).write_json(&record)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        },
    )
}

// The record that `consent revoke` (`revoke`) or `consent reset` writes over `existing`. A
// revoke keeps the source of a record for this identity, and with an inherited source the main
// working tree it names; a reset clears both.
fn changed(existing: Option<Record>, identity: Identity, revoke: bool) -> Record {
    let (source, repository_root) = existing
        .filter(|record| record.identity == identity && revoke)
        .map(|record| (record.source, record.repository_root))
        .unwrap_or_default();
    Record {
        schema: 1,
        identity,
        source,
        revoked: revoke,
        repository_root,
    }
}

#[cfg(test)]
mod inherited_tests;

#[cfg(test)]
mod tests {
    use super::super::store::native_key;
    use super::super::tests::{Fixture, fixture, setup, trust_pi};
    use super::*;
    use crate::native::session::primitives::{write_json_atomic, write_private};
    use std::thread;
    #[test]
    fn session_consent_observation_preserves_permissive_json_and_read_failures() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(SESSION_FILE);
        assert_eq!(observe(temp.path()).into_value(), serde_json::Value::Null);
        assert!(!path.exists());
        for value in [
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!("verified"),
            serde_json::json!(["verified"]),
            serde_json::json!({}),
            serde_json::json!({"state":"future", "applied":17, "extra":{"kept":true}}),
            serde_json::json!({"state":"verified", "applied":"future-marker"}),
        ] {
            write_json_atomic(&path, &value).unwrap();
            let before = fs::read(&path).unwrap();
            let observed = observe(temp.path());
            assert_eq!(observed.value(), &value);
            assert_eq!(observed.into_value(), value);
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        fs::write(&path, b"{").unwrap();
        let error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(
            observe(temp.path()).into_value(),
            serde_json::json!({"state":"unreadable", "detail":error.to_string()})
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let observed = observe(temp.path()).into_value();
        assert_eq!(observed["state"], "unreadable");
        assert!(observed["detail"].as_str().is_some_and(|s| !s.is_empty()));
    }
    #[test]
    fn session_consent_accessors_recognize_only_known_states_and_string_markers() {
        for (state, expected) in [
            ("verified", ConsentState::Verified),
            ("unverified", ConsentState::Unverified),
            ("alias", ConsentState::Alias),
            ("identity-changed", ConsentState::IdentityChanged),
            ("revoked", ConsentState::Revoked),
            ("source-reset", ConsentState::SourceReset),
        ] {
            let observed = SessionConsentObservation {
                value: serde_json::json!({"state":state, "applied":"future-marker"}),
            };
            assert_eq!(observed.state(), Some(expected));
            assert_eq!(observed.applied_marker(), Some("future-marker"));
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!("verified"),
            serde_json::json!(["verified"]),
            serde_json::json!({}),
            serde_json::json!({"state":"future", "applied":false}),
            serde_json::json!({"state":42, "applied":null}),
        ] {
            let observed = SessionConsentObservation { value };
            assert_eq!(observed.state(), None);
            assert_eq!(observed.applied_marker(), None);
        }
    }
    // Writes the exact workspace as trusted into one provider's store, as that provider
    // writes it.
    pub(super) fn trust(fixture: &Fixture, provider: FirstPartyCli, workspace: &Path) {
        let key = native_key(workspace).unwrap();
        match provider {
            FirstPartyCli::Codex => {
                let key = if cfg!(windows) {
                    key.to_lowercase()
                } else {
                    key
                };
                if fixture.codex.exists() {
                    fs::remove_file(&fixture.codex).unwrap();
                }
                write_private(
                    &fixture.codex,
                    format!(
                        "[projects.{}]\ntrust_level = \"trusted\"\n",
                        serde_json::to_string(&key).unwrap()
                    )
                    .as_bytes(),
                )
                .unwrap();
            }
            FirstPartyCli::Claude => {
                let key = if cfg!(windows) {
                    key.replace('\\', "/")
                } else {
                    key
                };
                write_json_atomic(
                    &fixture.claude,
                    &serde_json::json!({"projects":{key:{"hasTrustDialogAccepted":true}}}),
                )
                .unwrap();
            }
            FirstPartyCli::Agy => write_json_atomic(
                &fixture.agy,
                &serde_json::json!({"trustedWorkspaces":[key]}),
            )
            .unwrap(),
            FirstPartyCli::Pi => trust_pi(fixture, workspace),
        }
    }
    // Fixtures replace their own store, using the same private creation policy each time.
    pub(super) fn replace_store(path: &Path, bytes: &[u8]) -> Result<()> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        write_private(path, bytes)
    }
    pub(super) fn store_of(fixture: &Fixture, provider: FirstPartyCli) -> &Path {
        match provider {
            FirstPartyCli::Codex => &fixture.codex,
            FirstPartyCli::Claude => &fixture.claude,
            FirstPartyCli::Agy => &fixture.agy,
            FirstPartyCli::Pi => &fixture.pi,
        }
    }
    pub(super) const PROVIDERS: [FirstPartyCli; 4] = [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ];
    // The recorded source is rechecked in the store it was read from, whatever the
    // caller's environment resolves to now (the former clone of `Homes` with that one
    // field swapped), and no other provider is substituted when it no longer holds.
    #[test]
    fn a_recorded_source_is_rechecked_in_its_own_store_for_every_provider() {
        for provider in PROVIDERS {
            let (temp, workspace, fixture) = setup();
            trust(&fixture, provider, &workspace);
            let first = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
            assert_eq!(first.state, "verified", "{provider:?}");
            let source = first.source.clone().unwrap();
            assert_eq!(source.provider, provider.as_str());
            assert_eq!(source.store, store_of(&fixture, provider));

            // The environment moves to a home without stores: the original store is
            // still the one rechecked.
            let other = tempfile::tempdir().unwrap();
            let other_root = other.path().canonicalize().unwrap();
            let moved = self::fixture(&other_root);
            let again = assess_with(temp.path(), &workspace, &moved.env).unwrap();
            assert_eq!(again.state, "verified", "{provider:?}");
            assert_eq!(again.source, Some(source.clone()));

            // A store at the new home that trusts the workspace is not the source, and
            // is not substituted once the original store is gone.
            trust(&moved, provider, &workspace);
            assert_eq!(
                assess_with(temp.path(), &workspace, &moved.env)
                    .unwrap()
                    .source,
                Some(source.clone()),
                "{provider:?}"
            );
            fs::remove_file(store_of(&fixture, provider)).unwrap();
            let reset = assess_with(temp.path(), &workspace, &moved.env).unwrap();
            assert_eq!(reset.state, "source-reset", "{provider:?}");
            assert!(reset.source.is_none());
            // The record keeps the source: a later store that lists the workspace again
            // verifies it again.
            trust(&fixture, provider, &workspace);
            assert_eq!(
                assess_with(temp.path(), &workspace, &moved.env)
                    .unwrap()
                    .state,
                "verified",
                "{provider:?}"
            );

            // Unreadable, declined or absent in the original store: source-reset at a start, as
            // every earlier release recorded it. The recheck reports an unreadable source as an
            // error, never as a reset.
            replace_store(store_of(&fixture, provider), b"= =\n").unwrap();
            assert!(
                assess_current(temp.path(), &workspace, &fixture.env).is_err(),
                "{provider:?}: unreadable at a recheck"
            );
            assert_eq!(
                assess_with(temp.path(), &workspace, &fixture.env)
                    .unwrap()
                    .state,
                "source-reset",
                "{provider:?}: unreadable"
            );
            let declined: &[u8] = match provider {
                FirstPartyCli::Codex => b"[projects]\n",
                FirstPartyCli::Claude => b"{\"projects\":{}}",
                FirstPartyCli::Agy => b"{\"trustedWorkspaces\":[]}",
                FirstPartyCli::Pi => b"{}",
            };
            replace_store(store_of(&fixture, provider), declined).unwrap();
            assert_eq!(
                assess_with(temp.path(), &workspace, &fixture.env)
                    .unwrap()
                    .state,
                "source-reset",
                "{provider:?}: absent"
            );

            // A recorded key that the store no longer yields: source-reset.
            trust(&fixture, provider, &workspace);
            with_record(temp.path(), &workspace, |path, record| {
                let mut record = record.unwrap();
                record.source.as_mut().unwrap().key = "another key".into();
                write_json_atomic(path, &record)
            })
            .unwrap();
            assert_eq!(
                assess_with(temp.path(), &workspace, &fixture.env)
                    .unwrap()
                    .state,
                "source-reset",
                "{provider:?}: key"
            );
        }
    }
    // Without a record the providers are asked in the order Codex, Claude, Agy, Pi, and
    // the first that trusts the workspace is the source; one provider's error does not
    // stop the next from being asked.
    #[test]
    fn the_first_trusting_provider_in_order_is_the_source_and_an_error_is_skipped() {
        for (index, provider) in PROVIDERS.into_iter().enumerate() {
            let (temp, workspace, fixture) = setup();
            for later in &PROVIDERS[index..] {
                trust(&fixture, *later, &workspace);
            }
            for earlier in &PROVIDERS[..index] {
                replace_store(store_of(&fixture, *earlier), b"= =\n").unwrap();
                assert!(
                    provider::workspace_trust(*earlier, &workspace, &fixture.env, None).is_err()
                );
            }
            let assessment = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
            assert_eq!(assessment.state, "verified", "{provider:?}");
            let source = assessment.source.unwrap();
            assert_eq!(source.provider, provider.as_str());
            assert_eq!(source.store, store_of(&fixture, provider));
        }
    }
    #[test]
    fn unknown_workspace_is_not_approved_and_exact_native_trust_can_be_imported() {
        let (temp, workspace, fixture) = setup();
        let first = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        assert_eq!(first.state, "unverified");
        assert!(first.source.is_none());
        trust_pi(&fixture, &workspace);
        let next = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        assert_eq!(next.state, "verified");
        assert_eq!(next.source.unwrap().provider, "pi");
        let child = workspace.join("child");
        fs::create_dir(&child).unwrap();
        assert!(
            assess_with(temp.path(), &child, &fixture.env)
                .unwrap()
                .source
                .is_none()
        );
        let sibling = temp.path().join("workspace2");
        fs::create_dir(&sibling).unwrap();
        assert!(
            assess_with(temp.path(), &sibling, &fixture.env)
                .unwrap()
                .source
                .is_none()
        );
    }
    #[test]
    fn replaced_directory_or_owner_does_not_reimport_old_path_trust() {
        let (temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        assert_eq!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .state,
            "verified"
        );
        fs::rename(&workspace, temp.path().join("old-workspace")).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .state,
            "identity-changed"
        );
        with_record(temp.path(), &workspace, |path, record| {
            let mut record = record.unwrap();
            record.identity = identity(&workspace)?;
            record.identity.owner = "another-owner".into();
            write_json_atomic(path, &record)
        })
        .unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .state,
            "identity-changed"
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_alias_and_unsafe_store_do_not_supply_consent() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        let alias = temp.path().join("alias");
        symlink(&workspace, &alias).unwrap();
        assert_eq!(
            assess_with(temp.path(), &alias, &fixture.env)
                .unwrap()
                .state,
            "alias"
        );
        fs::set_permissions(&fixture.pi, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            provider::workspace_trust(FirstPartyCli::Pi, &workspace, &fixture.env, None).is_err()
        );
        assert!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .source
                .is_none()
        );
    }
    #[test]
    fn source_reset_does_not_reimport_an_automatically_trusted_target() {
        let (temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        write_json_atomic(
            &fixture.agy,
            &serde_json::json!({"trustedWorkspaces":[native_key(&workspace).unwrap()]}),
        )
        .unwrap();
        fs::remove_file(&fixture.pi).unwrap();
        let assessment = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        assert_eq!(assessment.state, "source-reset");
        assert!(assessment.source.is_none());
    }
    #[test]
    fn revocation_is_serialized_with_import_and_reset_requires_fresh_evidence() {
        let (temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        // Deterministic lock contention: the importer must see the revoke written
        // while it waits, rather than replace it using a pre-lock snapshot.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let root = temp.path().to_owned();
        let child_workspace = workspace.clone();
        let child_env = fixture.env.clone();
        let child_barrier = barrier.clone();
        let worker = with_record(temp.path(), &workspace, |path, record| {
            let worker = thread::spawn(move || {
                child_barrier.wait();
                assess_with(&root, &child_workspace, &child_env).unwrap()
            });
            barrier.wait();
            let mut record = record.unwrap();
            record.revoked = true;
            write_json_atomic(path, &record)?;
            Ok(worker)
        })
        .unwrap();
        assert_eq!(worker.join().unwrap().state, "revoked");
        with_record(temp.path(), &workspace, |path, record| {
            let mut record = record.unwrap();
            record.revoked = false;
            record.source = None;
            write_json_atomic(path, &record)
        })
        .unwrap();
        fs::remove_file(&fixture.pi).unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .state,
            "unverified"
        );
        trust_pi(&fixture, &workspace);
        assert_eq!(
            assess_with(temp.path(), &workspace, &fixture.env)
                .unwrap()
                .state,
            "verified"
        );
    }
}

#[cfg(test)]
#[test]
fn request_deadline_consent_record_is_absent() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let canonical = workspace.path().canonicalize().unwrap();
    let env = Environment::at_home(root.path());
    let result =
        crate::native::session_start::deadline::expire_at("writing workspace consent", || {
            assess_with(root.path(), &canonical, &env)
        });
    assert!(
        !record_path(root.path(), &canonical).exists(),
        "consent record was written"
    );
    assert!(result.is_err());
    let error =
        crate::native::session_start::deadline::expire_at("writing workspace consent", || {
            assess_with(root.path(), &canonical, &env)
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the request's deadline passed before writing workspace consent; no Session was created"
    );
}

#[cfg(test)]
#[test]
fn request_deadline_reports_a_state_root_created_during_consent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing").join("state");
    let workspace = temp.path().canonicalize().unwrap();
    for created in [true, false] {
        assert_eq!(root.exists(), !created);
        let error = crate::native::session_start::deadline::expire_at(
            "opening the workspace consent lock",
            || with_record(&root, &workspace, |_, _| Ok(())),
        )
        .unwrap_err();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
                0o700
            );
        }
        assert!(!root.join("state-root.durable").exists());
        assert!(root.is_dir());
        assert_eq!(
            error.to_string().contains("the state root was created"),
            created,
            "{error:#}"
        );
    }
}
