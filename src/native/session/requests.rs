//! Immutable Bridge request addresses. Provider-owned correlation still decides completion.
use crate::native::session::primitives::{delegation_source, unix_ms, valid_session_id};
use crate::native::session::turn::valid_turn_claim_token;

use crate::native::session::{CoreRecord, Reader, RecordReader, RecordStore, Store, record};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A recorded result that a request was explicitly derived from, pinned at resolution time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContextSource {
    pub(crate) session: String,
    pub(crate) request_id: Option<String>,
    pub(crate) event_id: String,
    pub(crate) provider: String,
    pub(crate) created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attached_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) omitted_chars: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct Receipt {
    pub(in crate::native) schema: u32,
    pub(in crate::native) request_id: String,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) event_file: String,
    #[serde(default)]
    pub(in crate::native) created_unix_ms: Option<u128>,
    #[serde(default)]
    pub(in crate::native) source: Option<String>,
    // Receipts written before 0.0.7 have no provenance; they still deserialise as empty.
    #[serde(default)]
    pub(in crate::native) context_sources: Vec<ContextSource>,
}

pub(in crate::native) fn valid_id(value: &str) -> bool {
    value.starts_with("request-")
        && value.len() > "request-".len()
        && value.len() <= 160
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(in crate::native) fn validate(receipt: &Receipt) -> Result<()> {
    if receipt.schema != 1
        || !valid_id(&receipt.request_id)
        || !valid_turn_claim_token(&receipt.claim_token)
        || !Reader::valid_event_file_name(&receipt.event_file)
        || receipt
            .context_sources
            .iter()
            .any(|source| validate_context_source(source).is_err())
    {
        bail!("invalid Bridge request receipt")
    }
    Ok(())
}

/// The rules a receipt applies to each recorded source. Resolution applies the same
/// rules before any session state exists, so a receipt never rejects a resolved source.
pub(in crate::native) fn validate_context_source(source: &ContextSource) -> Result<()> {
    if !valid_session_id(&source.session) {
        bail!("invalid source session id {:?}", source.session)
    }
    if !Reader::valid_event_file_name(&source.event_id) {
        bail!("invalid source event id {:?}", source.event_id)
    }
    if let Some(request_id) = source.request_id.as_deref()
        && !valid_id(request_id)
    {
        bail!("invalid source request id {request_id:?}")
    }
    if source.provider.is_empty() {
        bail!("source provider is empty")
    }
    Ok(())
}

// Called while creating the claim under its lifecycle lock, before any dispatch can begin.
// Provenance comes from the caller's pinned resolution; it is never re-read here.
pub(in crate::native) fn create(
    store: &Store,
    claim_token: &str,
    context_sources: &[ContextSource],
) -> Result<Receipt> {
    let directory = store.directory();
    let receipt = Receipt {
        schema: 1,
        request_id: format!(
            "request-{}-{}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ),
        claim_token: claim_token.to_owned(),
        event_file: Store::new_event_file_name()?,
        created_unix_ms: Some(unix_ms()),
        source: Some(delegation_source()),
        context_sources: context_sources.to_vec(),
    };
    validate(&receipt)?;
    let root = Reader::open_unchecked(directory)
        .record(CoreRecord::Requests)
        .path()
        .to_owned();
    fs::create_dir_all(&root)?;
    RecordStore::at(&root).set_directory_private()?;
    // Atomic publication avoids exposing a partial receipt to readers after a crash.
    store
        .record(CoreRecord::Requests)
        .child(&format!("{claim_token}.json"))
        .write_json(&receipt)?;
    Ok(receipt)
}

/// Whether the session's `requests` directory exists as a real directory. A link or a
/// non-directory at that path is refused: `read_dir` and the receipt reads would follow
/// a link out of the state root, and a receipt read from there would supply request
/// identity for the session's events.
pub(in crate::native) fn requests_directory_present(reader: &Reader) -> Result<bool> {
    let directory = reader.directory();
    let root = Reader::open_unchecked(directory)
        .record(CoreRecord::Requests)
        .path()
        .to_owned();
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "failed to read Bridge requests: refusing linked requests directory: {}",
                root.display()
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!(
                "failed to read Bridge requests: refusing non-directory requests path: {}",
                root.display()
            )
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", root.display())),
    }
}

/// The receipt record of one claim. The caller validates the token and the directory.
pub(in crate::native) fn receipt_record(reader: &Reader, claim_token: &str) -> RecordReader {
    reader
        .record(CoreRecord::Requests)
        .child(&format!("{claim_token}.json"))
}

pub(in crate::native) fn for_claim(reader: &Reader, claim_token: &str) -> Result<Option<Receipt>> {
    if !valid_turn_claim_token(claim_token) {
        bail!("invalid request claim token")
    }
    if !requests_directory_present(reader)? {
        return Ok(None);
    }
    let Some(bytes) = receipt_record(reader, claim_token).bytes()? else {
        return Ok(None);
    };
    record::receipt(&bytes, claim_token)
        .map(Some)
        .map_err(|error| error.context("invalid Bridge request receipt"))
}

#[derive(Default)]
pub(in crate::native) struct Index {
    pub(in crate::native) receipts: Vec<Receipt>,
    pub(in crate::native) unreadable: usize,
}

/// The claim tokens named under `requests/`, in directory order: `None` when the
/// directory is absent, an error when it is refused or cannot be listed.
pub(in crate::native) fn tokens(reader: &Reader) -> Result<Option<Vec<String>>> {
    if !requests_directory_present(reader)? {
        return Ok(None);
    }
    let root = reader.record(CoreRecord::Requests).path().to_owned();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read Bridge requests"),
    };
    let mut tokens = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(token) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if valid_turn_claim_token(token) {
            tokens.push(token.to_owned());
        }
    }
    Ok(Some(tokens))
}

pub(in crate::native) fn list(reader: &Reader) -> Result<Index> {
    let Some(tokens) = tokens(reader)? else {
        return Ok(Index::default());
    };
    Ok(record::receipt_index(
        &tokens,
        |token| match receipt_record(reader, token).bytes() {
            Ok(Some(bytes)) => record::Read::Present(bytes),
            Ok(None) => record::Read::Absent,
            Err(error) => record::Read::Fault(format!("{error:#}")),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::names::*;
    use crate::native::session::primitives::{event_paths, read_json};
    use crate::native::session::schema::SessionEvent;
    use crate::native::session::turn;
    use crate::native::session::{SessionState, SessionStatus, update_status};
    use agent_bridge::FirstPartyCli;

    // A `requests` path that is a link (or a file) is refused by both the index and the
    // per-claim read, so no receipt outside the state root can name a request.
    #[cfg(unix)]
    #[test]
    fn linked_requests_directory_is_refused_by_the_index_and_the_claim_read() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-linked");
        fs::create_dir_all(directory.join("events")).unwrap();
        fs::write(
            outside.path().join("1-2-3.json"),
            r#"{"schema":1,"request_id":"request-external","claim_token":"1-2-3","event_file":"event-1.json","created_unix_ms":5,"source":"external","context_sources":[]}"#,
        )
        .unwrap();
        symlink(outside.path(), directory.join(REQUESTS_DIRECTORY)).unwrap();

        let error = list(&Reader::open_unchecked(&directory))
            .err()
            .expect("refused")
            .to_string();
        assert!(
            error.contains("refusing linked requests directory"),
            "{error}"
        );
        let error = for_claim(&Reader::open_unchecked(&directory), "1-2-3")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refusing linked requests directory"),
            "{error}"
        );

        // A regular file at the path is refused too; a missing path is an empty index.
        fs::remove_file(directory.join(REQUESTS_DIRECTORY)).unwrap();
        fs::write(directory.join(REQUESTS_DIRECTORY), b"").unwrap();
        let error = list(&Reader::open_unchecked(&directory))
            .err()
            .expect("refused")
            .to_string();
        assert!(
            error.contains("refusing non-directory requests path"),
            "{error}"
        );
        fs::remove_file(directory.join(REQUESTS_DIRECTORY)).unwrap();
        assert!(
            list(&Reader::open_unchecked(&directory))
                .unwrap()
                .receipts
                .is_empty()
        );
        assert!(
            for_claim(&Reader::open_unchecked(&directory), "1-2-3")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn receipt_is_durable_before_dispatch_and_does_not_replace_provider_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        let receipt = for_claim(&Reader::open_unchecked(directory.path()), claim.token())
            .unwrap()
            .unwrap();
        assert_eq!(receipt.request_id, claim.receipt().request_id);
        assert_ne!(receipt.request_id, claim.token());
        let token = claim.token().to_owned();
        claim.retain();
        turn::Report::for_claim(
            &Store::open_unchecked(directory.path()),
            FirstPartyCli::Claude,
            Some(&token),
        )
        .complete(
            "done",
            Some("native-owner".to_owned()),
            Some("native-turn".to_owned()),
        )
        .unwrap();
        let event: SessionEvent =
            read_json(&directory.path().join("events").join(&receipt.event_file)).unwrap();
        assert_eq!(event.provider_session_id.as_deref(), Some("native-owner"));
        assert_eq!(event.turn_id.as_deref(), Some("native-turn"));
        assert_eq!(
            for_claim(&Reader::open_unchecked(directory.path()), &token)
                .unwrap()
                .unwrap()
                .request_id,
            receipt.request_id
        );
    }

    #[test]
    fn receipts_round_trip_context_sources_and_old_receipts_still_parse() {
        let legacy: Receipt = serde_json::from_str(
            r#"{"schema":1,"request_id":"request-old","claim_token":"1-2-3",
            "event_file":"event-1.json","created_unix_ms":7}"#,
        )
        .unwrap();
        validate(&legacy).unwrap();
        assert!(legacy.context_sources.is_empty());
        assert_eq!(legacy.source, None);

        let source = ContextSource {
            session: "session-parent".to_owned(),
            request_id: Some("request-parent-1".to_owned()),
            event_id: "event-9.json".to_owned(),
            provider: "codex".to_owned(),
            created_unix_ms: 42,
            mode: None,
            attached_chars: None,
            omitted_chars: None,
        };
        let legacy_event = ContextSource {
            request_id: None,
            ..source.clone()
        };
        let directory = tempfile::tempdir().unwrap();
        let receipt = create(
            &Store::open_unchecked(directory.path()),
            "1-2-3",
            &[source.clone(), legacy_event.clone()],
        )
        .unwrap();
        let stored = for_claim(&Reader::open_unchecked(directory.path()), "1-2-3")
            .unwrap()
            .unwrap();
        assert_eq!(stored.request_id, receipt.request_id);
        assert_eq!(stored.context_sources, vec![source, legacy_event]);
        let text = fs::read_to_string(directory.path().join("requests/1-2-3.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["context_sources"][1]["request_id"],
            serde_json::Value::Null
        );

        let mut broken = stored.clone();
        broken.context_sources[0].session = "../escape".to_owned();
        assert!(validate(&broken).is_err());
    }

    #[test]
    fn context_source_optional_fields_preserve_old_json_and_round_trip_both_modes() {
        let old = serde_json::json!({"session":"session-old","request_id":"request-old",
            "event_id":"event-1.json","provider":"codex","created_unix_ms":1});
        let source: ContextSource = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(source.mode, None);
        assert_eq!(source.attached_chars, None);
        assert_eq!(source.omitted_chars, None);
        assert_eq!(serde_json::to_value(&source).unwrap(), old);
        for (mode, attached, omitted) in [("full", 6000, 0), ("compact", 5120, 880)] {
            let mut source = source.clone();
            source.mode = Some(mode.into());
            source.attached_chars = Some(attached);
            source.omitted_chars = Some(omitted);
            let directory = tempfile::tempdir().unwrap();
            create(
                &Store::open_unchecked(directory.path()),
                "1-2-3",
                &[source.clone()],
            )
            .unwrap();
            let stored = for_claim(&Reader::open_unchecked(directory.path()), "1-2-3")
                .unwrap()
                .unwrap();
            assert_eq!(stored.context_sources, vec![source]);
        }
    }

    #[test]
    fn receipt_failure_releases_the_claim_before_dispatch_and_preserves_ready_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        fs::write(directory.path().join(REQUESTS_DIRECTORY), "not a directory").unwrap();
        assert!(
            turn::claim_ready(
                &Store::open_unchecked(directory.path()),
                "session-test",
                &[]
            )
            .is_err()
        );
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        assert!(event_paths(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn damaged_optional_request_index_cannot_block_verified_completion() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        fs::write(
            directory
                .path()
                .join(REQUESTS_DIRECTORY)
                .join(format!("{token}.json")),
            "invalid",
        )
        .unwrap();
        turn::Report::for_claim(
            &Store::open_unchecked(directory.path()),
            FirstPartyCli::Codex,
            Some(&token),
        )
        .complete(
            "verified result",
            Some("thread".to_owned()),
            Some("turn".to_owned()),
        )
        .unwrap();
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let paths = event_paths(directory.path()).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            read_json::<SessionEvent>(&paths[0]).unwrap().message,
            "verified result"
        );
        assert!(for_claim(&Reader::open_unchecked(directory.path()), &token).is_err());
    }
}
