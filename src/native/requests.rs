//! Immutable Bridge request addresses. Provider-owned correlation still decides completion.
use super::*;

const REQUESTS_DIRECTORY: &str = "requests";
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Receipt {
    pub(super) schema: u32,
    pub(super) request_id: String,
    pub(super) claim_token: String,
    pub(super) event_file: String,
    pub(super) created_unix_ms: u128,
    #[serde(default)]
    pub(super) source: Option<String>,
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
        || !valid_event_file_name(&receipt.event_file)
    {
        bail!("invalid Bridge request receipt")
    }
    Ok(())
}

// Called while creating the claim under its lifecycle lock, before any dispatch can begin.
pub(super) fn create(directory: &Path, claim_token: &str) -> Result<Receipt> {
    let receipt = Receipt {
        schema: 1,
        request_id: format!(
            "request-{}-{}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ),
        claim_token: claim_token.to_owned(),
        event_file: new_event_file_name()?,
        created_unix_ms: unix_ms(),
        source: Some(delegation_source()),
    };
    validate(&receipt)?;
    let root = directory.join(REQUESTS_DIRECTORY);
    fs::create_dir_all(&root)?;
    set_private_directory_permissions(&root)?;
    let path = root.join(format!("{claim_token}.json"));
    // Atomic publication avoids exposing a partial receipt to readers after a crash.
    write_json_atomic(&path, &receipt)?;
    Ok(receipt)
}

pub(super) fn for_claim(directory: &Path, claim_token: &str) -> Result<Option<Receipt>> {
    if !valid_turn_claim_token(claim_token) {
        bail!("invalid request claim token")
    }
    let path = directory
        .join(REQUESTS_DIRECTORY)
        .join(format!("{claim_token}.json"));
    let Some(text) = read_regular_text_if_present(&path)? else {
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

pub(super) fn list(directory: &Path) -> Result<Index> {
    let root = directory.join(REQUESTS_DIRECTORY);
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
        match for_claim(directory, token) {
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

    #[test]
    fn receipt_is_durable_before_dispatch_and_does_not_replace_provider_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let receipt = for_claim(directory.path(), &claim.token).unwrap().unwrap();
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
            for_claim(directory.path(), &token)
                .unwrap()
                .unwrap()
                .request_id,
            receipt.request_id
        );
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
                created_unix_ms: 1,
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
            let recovered = for_claim(directory.path(), &token).unwrap().unwrap();
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
        assert!(for_claim(directory.path(), &token).is_err());
    }
}
