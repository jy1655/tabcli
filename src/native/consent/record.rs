use super::store::{Environment, Evidence, Trust, read_store, user_home};
use super::{Identity, identity};
use crate::native::provider;
use crate::native::session::{Reader, RecordReader, RecordStore, Store};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Record {
    schema: u32,
    identity: Identity,
    source: Option<Evidence>,
    pub(super) revoked: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct Assessment {
    schema: u32,
    identity: Identity,
    pub(in crate::native) source: Option<Evidence>,
    pub(in crate::native) state: String,
    pub(in crate::native) detail: String,
    pub(in crate::native) applied: Option<String>,
}

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
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Ok(false);
            }
        }
    }
    Ok(absolute.canonicalize()? == canonical)
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
    let path = record_path(root, workspace);
    let parent = path.parent().unwrap();
    if fs::symlink_metadata(parent).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent directory");
    }
    fs::create_dir_all(parent)?;
    RecordStore::at(parent).set_directory_private()?;
    let lock_path = path.with_extension("lock");
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
    lock.lock()?;
    let record = read_store(&path)?
        .map(|s| serde_json::from_str(&s))
        .transpose()?;
    f(&path, record)
}

pub(super) fn assess_with(root: &Path, requested: &Path, env: &Environment) -> Result<Assessment> {
    let identity = identity(requested)?;
    let mut assessment = Assessment {
        schema: 1,
        identity: identity.clone(),
        source: None,
        state: "unverified".into(),
        detail:
            "No exact provider workspace trust was verified; use the provider's own trust prompt."
                .into(),
        applied: None,
    };
    if !alias_free(requested, &identity.path)? {
        assessment.state = "alias".into();
        assessment.detail =
            "Workspace aliases do not inherit shared consent. Use the canonical path.".into();
        return Ok(assessment);
    }
    with_record(root, &identity.path, |path, existing| {
        let mut record = existing.unwrap_or_else(|| Record {
            schema: 1,
            identity: identity.clone(),
            source: None,
            revoked: false,
        });
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
            // The one provider the record names rechecks the store it was read from, never
            // the store its home resolves to now.
            match provider::workspace_trust(cli, &identity.path, env, Some(&source.store)) {
                Ok(Trust::Trusted(now)) if now == *source => {
                    assessment.source = Some(now);
                }
                _ => {
                    assessment.state = "source-reset".into();
                    assessment.detail = "The original provider trust is absent, declined, or unreadable. No other provider is silently substituted; reset consent after approving again.".into();
                }
            }
        } else {
            for cli in [
                FirstPartyCli::Codex,
                FirstPartyCli::Claude,
                FirstPartyCli::Agy,
                FirstPartyCli::Pi,
            ] {
                if let Ok(Trust::Trusted(source)) =
                    provider::workspace_trust(cli, &identity.path, env, None)
                {
                    assessment.source = Some(source.clone());
                    record.source = Some(source);
                    break;
                }
            }
            RecordStore::at(path).write_json(&record)?;
        }
        if assessment.source.is_some() {
            assessment.state = "verified".into();
            assessment.detail = "Exact provider trust and canonical directory identity verified. Tool permissions are unchanged.".into();
        }
        Ok(assessment)
    })
}

// Assess before anything is created: the assessment reads the shared record and the provider
// stores, and a failure there must consume nothing (a reopen's source marker is claimed by
// create). The assessment is recorded with the session once that exists.
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

pub(in crate::native) fn authorized(directory: &Path, target: FirstPartyCli) -> Result<bool> {
    authorized_with(directory, target, &Environment::current())
}

pub(super) fn authorized_with(
    directory: &Path,
    target: FirstPartyCli,
    env: &Environment,
) -> Result<bool> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .text()?
    else {
        return Ok(false);
    };
    let assessment: Assessment = serde_json::from_str(&text)?;
    if assessment.schema != 1
        || assessment.state != "verified"
        || identity(&assessment.identity.path)? != assessment.identity
    {
        return Ok(false);
    }
    user_home(env)?;
    if matches!(
        provider::workspace_trust(target, &assessment.identity.path, env, None),
        Err(_) | Ok(Trust::Declined)
    ) {
        return Ok(false);
    }
    // Re-read under the consent lock immediately before applying trust. A session file
    // is diagnostic history, never a capability that survives revoke or source reset.
    let now = assess_with(
        directory.parent().context("session has no state root")?,
        &assessment.identity.path,
        env,
    )?;
    Ok(now.state == "verified"
        && now.identity == assessment.identity
        && now.source == assessment.source)
}

pub(in crate::native) fn applied(directory: &Path, method: &str) -> Result<()> {
    let path = Reader::open_unchecked(directory)
        .private(SESSION_FILE)
        .path()
        .to_owned();
    let mut assessment: Assessment = RecordReader::at(&path).json()?;
    assessment.applied = Some(method.to_owned());
    RecordStore::at(&path).write_json(&assessment)
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
    with_record(&root, &identity.path.clone(), |path, existing| {
        let source = existing
            .filter(|r| r.identity == identity)
            .and_then(|r| r.source);
        let record = Record {
            schema: 1,
            identity,
            source: if action == "reset" { None } else { source },
            revoked: action == "revoke",
        };
        RecordStore::at(path).write_json(&record)?;
        println!("{}", serde_json::to_string_pretty(&record)?);
        Ok(())
    })
}

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
    fn trust(fixture: &Fixture, provider: FirstPartyCli, workspace: &Path) {
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
    fn store_of(fixture: &Fixture, provider: FirstPartyCli) -> &Path {
        match provider {
            FirstPartyCli::Codex => &fixture.codex,
            FirstPartyCli::Claude => &fixture.claude,
            FirstPartyCli::Agy => &fixture.agy,
            FirstPartyCli::Pi => &fixture.pi,
        }
    }
    const PROVIDERS: [FirstPartyCli; 4] = [
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

            // Unreadable, declined or absent in the original store: source-reset.
            fs::write(store_of(&fixture, provider), b"= =\n").unwrap();
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
            fs::write(store_of(&fixture, provider), declined).unwrap();
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
                fs::write(store_of(&fixture, *earlier), b"= =\n").unwrap();
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
