//! Immutable Bridge request addresses. Provider-owned correlation still decides completion.
use super::*;
use crate::native::session::{CoreRecord, Reader, RecordStore, Store};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A recorded result that a request was explicitly derived from, pinned at resolution time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContextSource {
    pub(crate) session: String,
    pub(crate) request_id: Option<String>,
    pub(crate) event_id: String,
    pub(crate) provider: String,
    pub(crate) created_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Receipt {
    pub(super) schema: u32,
    pub(super) request_id: String,
    pub(super) claim_token: String,
    pub(super) event_file: String,
    #[serde(default)]
    pub(super) created_unix_ms: Option<u128>,
    #[serde(default)]
    pub(super) source: Option<String>,
    // Receipts written before 0.0.7 have no provenance; they still deserialise as empty.
    #[serde(default)]
    pub(super) context_sources: Vec<ContextSource>,
}

pub(super) fn valid_id(value: &str) -> bool {
    value.starts_with("request-")
        && value.len() > "request-".len()
        && value.len() <= 160
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate(receipt: &Receipt) -> Result<()> {
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
pub(super) fn validate_context_source(source: &ContextSource) -> Result<()> {
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
pub(super) fn create(
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
fn requests_directory_present(reader: &Reader) -> Result<bool> {
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

pub(super) fn for_claim(reader: &Reader, claim_token: &str) -> Result<Option<Receipt>> {
    if !valid_turn_claim_token(claim_token) {
        bail!("invalid request claim token")
    }
    if !requests_directory_present(reader)? {
        return Ok(None);
    }
    let record = reader
        .record(CoreRecord::Requests)
        .child(&format!("{claim_token}.json"));
    let Some(text) = record.text()? else {
        return Ok(None);
    };
    let receipt: Receipt = serde_json::from_str(&text).context("invalid Bridge request receipt")?;
    validate(&receipt)?;
    if receipt.claim_token != claim_token {
        bail!("Bridge request receipt belongs to a different claim")
    }
    Ok(Some(receipt))
}

#[derive(Default)]
pub(super) struct Index {
    pub(super) receipts: Vec<Receipt>,
    pub(super) unreadable: usize,
}

pub(super) fn list(reader: &Reader) -> Result<Index> {
    let directory = reader.directory();
    if !requests_directory_present(reader)? {
        return Ok(Index::default());
    }
    let root = Reader::open_unchecked(directory)
        .record(CoreRecord::Requests)
        .path()
        .to_owned();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
        Err(error) => return Err(error).context("failed to read Bridge requests"),
    };
    let mut index = Index::default();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(token) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if !valid_turn_claim_token(token) {
            continue;
        }
        match for_claim(&Reader::open_unchecked(directory), token) {
            Ok(Some(receipt)) => index.receipts.push(receipt),
            Ok(None) | Err(_) => index.unreadable += 1,
        }
    }
    index.receipts.sort_by(|a, b| {
        a.created_unix_ms
            .cmp(&b.created_unix_ms)
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let receipt = for_claim(&Reader::open_unchecked(directory.path()), &claim.token)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.request_id, claim.receipt.request_id);
        assert_ne!(receipt.request_id, claim.token);
        let token = claim.token.clone();
        claim.retain();
        record_provider_result_for_claim(
            directory.path(),
            FirstPartyCli::Claude,
            "done",
            Some("native-owner".to_owned()),
            Some("native-turn".to_owned()),
            Some(&token),
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
    fn receipt_failure_releases_the_claim_before_dispatch_and_preserves_ready_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "ready", None, None).unwrap();
        fs::write(directory.path().join(REQUESTS_DIRECTORY), "not a directory").unwrap();
        assert!(acquire_ready_turn_claim(directory.path(), "session-test").is_err());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
        assert!(event_paths(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn immutable_request_mapping_survives_each_partial_completion_recovery() {
        for completed_mutations in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join("events")).unwrap();
            update_status(directory.path(), "working", None, None).unwrap();
            let claim = acquire_turn_claim(directory.path()).unwrap();
            let receipt = claim.receipt.clone();
            let token = claim.token.clone();
            claim.retain();
            let event = SessionEvent {
                provider: "codex".to_owned(),
                message: "same text".to_owned(),
                error: None,
                provider_session_id: Some("thread".to_owned()),
                turn_id: Some("turn".to_owned()),
                created_unix_ms: Some(1),
            };
            let mut pending = PendingTurnCompletion::new(&token, event, None).unwrap();
            pending.event_file = receipt.event_file.clone();
            write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
            if completed_mutations >= 1 {
                write_pending_completion_event(directory.path(), &pending).unwrap();
            }
            if completed_mutations >= 2 {
                update_status(directory.path(), "ready", None, None).unwrap();
            }
            if completed_mutations >= 3 {
                release_turn_claim_token(&directory.path().join(TURN_CLAIM_FILE), &token).unwrap();
            }
            recover_pending_completion(directory.path()).unwrap();
            let recovered = for_claim(&Reader::open_unchecked(directory.path()), &token)
                .unwrap()
                .unwrap();
            assert_eq!(recovered.request_id, receipt.request_id);
            assert_eq!(recovered.event_file, receipt.event_file);
            let stored: SessionEvent =
                read_json(&directory.path().join("events").join(recovered.event_file)).unwrap();
            assert_eq!(stored, pending.event);
        }
    }

    #[test]
    fn damaged_optional_request_index_cannot_block_verified_completion() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token.clone();
        claim.retain();
        fs::write(
            directory
                .path()
                .join(REQUESTS_DIRECTORY)
                .join(format!("{token}.json")),
            "invalid",
        )
        .unwrap();
        record_provider_result_for_claim(
            directory.path(),
            FirstPartyCli::Codex,
            "verified result",
            Some("thread".to_owned()),
            Some("turn".to_owned()),
            Some(&token),
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
