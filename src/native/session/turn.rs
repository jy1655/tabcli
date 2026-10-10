//! Claims, receipts, completion publication and recovery for one session.
use crate::native::*;

#[derive(Debug, Deserialize, Serialize)]
pub(in crate::native) struct PendingTurnCompletion {
    pub(in crate::native) schema: u32,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) event_file: String,
    pub(in crate::native) event: SessionEvent,
    pub(in crate::native) status_error: Option<String>,
    #[serde(default = "default_completion_status_state")]
    pub(in crate::native) status_state: SessionState,
}

impl PendingTurnCompletion {
    #[cfg(test)]
    pub(in crate::native) fn new(
        claim_token: &str,
        event: SessionEvent,
        status_error: Option<String>,
    ) -> Result<Self> {
        Self::new_with_status(claim_token, event, status_error, SessionState::Ready)
    }

    fn new_with_status(
        claim_token: &str,
        event: SessionEvent,
        status_error: Option<String>,
        status_state: SessionState,
    ) -> Result<Self> {
        if !valid_turn_claim_token(claim_token) {
            bail!("invalid native completion claim token")
        }
        if !matches!(status_state, SessionState::Ready | SessionState::Failed) {
            bail!("invalid native completion status state")
        }
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            event_file: Store::new_event_file_name()?,
            event,
            status_error,
            status_state,
        })
    }
}

fn default_completion_status_state() -> SessionState {
    SessionState::Ready
}

#[cfg(test)]
pub(in crate::native) fn record_provider_result(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_result_for_claim(
        directory,
        provider,
        message,
        provider_session_id,
        turn_id,
        None,
    )
}

#[cfg(test)]
pub(in crate::native) fn record_provider_result_for_claim(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
) -> Result<()> {
    record_provider_result_for_claim_condition(
        directory,
        provider,
        message,
        provider_session_id,
        turn_id,
        expected_claim_token,
        false,
    )
}

fn record_provider_result_for_claim_condition(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
    require_no_prior_provider_event: bool,
) -> Result<()> {
    let expected_claim_token = match expected_claim_token {
        Some(token) => token.to_owned(),
        None => match read_claim_token(directory)? {
            Some(token) => token,
            None => return Ok(()),
        },
    };
    let expected_claim_token = Some(expected_claim_token.as_str());
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _claim_lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    recover_pending_completion_locked(directory, &claim_path)?;
    if require_no_prior_provider_event && provider_has_completed_turn(directory, provider)? {
        return Ok(());
    }
    if !provider_completion_is_current(
        directory,
        provider,
        provider_session_id.as_deref(),
        turn_id.as_deref(),
        expected_claim_token,
    )? {
        return Ok(());
    }
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id,
        turn_id,
        created_unix_ms: Some(unix_ms()),
    };
    commit_completion_locked(
        directory,
        &claim_path,
        expected_claim_token.context("provider completion has no claim token")?,
        event,
        None,
    )
}

#[cfg(test)]
pub(in crate::native) fn record_provider_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_failure_for_claim(
        directory,
        provider,
        error,
        provider_session_id,
        turn_id,
        None,
    )
}

#[cfg(test)]
pub(in crate::native) fn record_provider_failure_for_claim(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
) -> Result<()> {
    record_provider_failure_for_claim_condition(
        directory,
        provider,
        error,
        provider_session_id,
        turn_id,
        expected_claim_token,
        false,
    )
}

fn record_provider_failure_for_claim_condition(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
    require_no_prior_provider_event: bool,
) -> Result<()> {
    let expected_claim_token = match expected_claim_token {
        Some(token) => token.to_owned(),
        None => match read_claim_token(directory)? {
            Some(token) => token,
            None => return Ok(()),
        },
    };
    let expected_claim_token = Some(expected_claim_token.as_str());
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _claim_lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    recover_pending_completion_locked(directory, &claim_path)?;
    if require_no_prior_provider_event && provider_has_completed_turn(directory, provider)? {
        return Ok(());
    }
    if !provider_completion_is_current(
        directory,
        provider,
        provider_session_id.as_deref(),
        turn_id.as_deref(),
        expected_claim_token,
    )? {
        return Ok(());
    }
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id,
        turn_id,
        created_unix_ms: Some(unix_ms()),
    };
    commit_completion_locked(
        directory,
        &claim_path,
        expected_claim_token.context("provider completion has no claim token")?,
        event,
        Some(error),
    )
}

fn record_monitor_failure(directory: &Path, provider: FirstPartyCli, error: &str) -> Result<()> {
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _claim_lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if matches!(
        status.state,
        SessionState::Closed | SessionState::Exited | SessionState::Failed
    ) {
        return Ok(());
    }
    let Some(claim_token) = read_claim_token(directory)? else {
        return update_status(
            directory,
            SessionState::Failed,
            None,
            Some(terminal_safe_text(error, true)),
        );
    };
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id: None,
        turn_id: None,
        created_unix_ms: Some(unix_ms()),
    };
    commit_provider_completion_with_status_locked(
        directory,
        &claim_path,
        &claim_token,
        event,
        Some(error),
        SessionState::Failed,
    )
}

fn provider_has_completed_turn(directory: &Path, provider: FirstPartyCli) -> Result<bool> {
    for path in Reader::open_unchecked(directory).events()? {
        let event: SessionEvent = session::RecordReader::at(&path).json()?;
        if event.provider == provider.as_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_claim_token(directory: &Path) -> Result<Option<String>> {
    match Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .raw_text()
    {
        Ok(token) => Ok(Some(token.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to inspect native turn claim"),
    }
}

fn claim_is_current(directory: &Path, expected_claim_token: Option<&str>) -> Result<bool> {
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if !matches!(
        status.state,
        SessionState::Running | SessionState::Working | SessionState::ResumePending
    ) {
        return Ok(false);
    }
    if let Some(expected_claim_token) = expected_claim_token {
        let current = match Reader::open_unchecked(directory)
            .record(CoreRecord::TurnClaim)
            .raw_text()
        {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect native turn claim"),
        };
        if current.trim() != expected_claim_token {
            return Ok(false);
        }
    }
    Ok(true)
}

// These historical identity checks remain inside the report's lifecycle critical
// section. Moving the scan to an adapter before Report::complete would let another
// hook publish between the scan and the claim check, or skip recovery before the scan.
// An adapter-side pre-check is not equivalent without extending the locked interface.
fn provider_completion_is_current(
    directory: &Path,
    provider: FirstPartyCli,
    provider_session_id: Option<&str>,
    turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
) -> Result<bool> {
    if !claim_is_current(directory, expected_claim_token)? {
        return Ok(false);
    }
    let Some(turn_id) = turn_id else {
        return Ok(true);
    };
    for path in Reader::open_unchecked(directory).events()? {
        let event: SessionEvent = session::RecordReader::at(&path).json()?;
        if event.provider == provider.as_str()
            && event.provider_session_id.as_deref() == provider_session_id
            && event.turn_id.as_deref() == Some(turn_id)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn commit_completion_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
) -> Result<()> {
    commit_provider_completion_with_status_locked(
        directory,
        claim_path,
        claim_token,
        event,
        status_error,
        SessionState::Ready,
    )
}

fn commit_provider_completion_with_status_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
    status_state: SessionState,
) -> Result<()> {
    commit_completion_within_locked(
        directory,
        claim_path,
        claim_token,
        event,
        status_error,
        status_state,
        EVENT_READ_LIMIT,
    )
}

/// [`commit_provider_completion_with_status_locked`] with an explicit event size limit, so
/// tests exercise the size policy without writing 64 MiB records. Production callers pass
/// [`EVENT_READ_LIMIT`]: the journal is created under the same limit the publication
/// predicate reads with, so no interruption can change whether a completion recovers.
fn commit_completion_within_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
    status_state: SessionState,
    event_limit: u64,
) -> Result<()> {
    let mut pending =
        PendingTurnCompletion::new_with_status(claim_token, event, status_error, status_state)?;
    // Request indexing must not prevent a provider-verified completion from publishing.
    // A missing or damaged receipt remains explicitly unresolved in request queries.
    if let Ok(Some(receipt)) = requests::for_claim(&Reader::open_unchecked(directory), claim_token)
    {
        pending.event_file = receipt.event_file;
    }
    let pending = bound_pending_completion(pending, event_limit)?;
    // Every caller recovers under the lifecycle lock first, so a journal that still exists
    // here belongs to a completion that could not be recovered; refuse to replace it.
    let completion_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Completion)
        .path()
        .to_owned();
    if completion_path.exists() {
        bail!("a pending native turn completion is still awaiting recovery")
    }
    // The journal is published by rename so that a partial journal never exists at its
    // final path; the temporary file carries the same private permissions.
    Store::open_unchecked(directory).write_completion(&pending)?;
    recover_pending_completion_locked(directory, claim_path)?;
    Ok(())
}

/// The one size policy, applied where a completion is journaled. An event record larger
/// than `event_limit` (the bytes the journal would write) is never journaled as
/// publishable: the publication predicate reads at most that many bytes, so such a record
/// could be published when the commit stopped before writing it and refused when it
/// stopped after. The completion is journaled instead as a failure whose error names the
/// size, keeping the provider identity, so every interruption settles to the same state.
fn bound_pending_completion(
    mut pending: PendingTurnCompletion,
    event_limit: u64,
) -> Result<PendingTurnCompletion> {
    let size = serde_json::to_vec_pretty(&pending.event)?.len() as u64;
    if size <= event_limit {
        return Ok(pending);
    }
    let error =
        format!("provider result of {size} bytes exceeds the {event_limit} byte event limit");
    pending.event.message = String::new();
    pending.event.error = Some(error.clone());
    pending.status_error = Some(error);
    pending.status_state = SessionState::Failed;
    Ok(pending)
}

/// The final delivery fact after the adapter has decided whether a fallback is safe.
/// Settlement never sends input or publishes a provider result.
pub(in crate::native) enum Delivery<'a> {
    Sent,
    NotSent(&'a anyhow::Error),
    Uncertain(&'a anyhow::Error),
}

pub(in crate::native) struct Claim {
    path: PathBuf,
    token: String,
    receipt: requests::Receipt,
    retained: bool,
    rollback_state: Option<SessionState>,
    // Only claim_ready assigns this; initial claims never acquire a Hold gate.
    follow_up_session: Option<String>,
}

impl Claim {
    pub(in crate::native) fn begin_delivery(&mut self) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("turn claim has no session directory")?;
        let store = Store::open_unchecked(directory);
        let _lock = store.lock()?;
        recover_pending_completion_locked(directory, &self.path)?;
        if read_claim_token(directory)?.as_deref() != Some(self.token.as_str()) {
            bail!("delivery refused: this turn no longer holds the session claim");
        }
        if store.closed_if_present()?.is_some() || store.status()?.state == SessionState::Closed {
            bail!("delivery refused: the session is closed");
        }
        if let Some(id) = &self.follow_up_session {
            session::hold::permit(&store, id)?;
        }
        update_status(directory, SessionState::Working, None, None)
    }

    pub(in crate::native) fn settle_delivery(&mut self, delivery: Delivery<'_>) -> Result<()> {
        // In particular, a failed diagnostic write must never release uncertain input.
        self.retained = true;
        let directory = self
            .path
            .parent()
            .context("turn claim has no session directory")?;
        match delivery {
            Delivery::Sent => Ok(()),
            Delivery::Uncertain(error) => {
                let error = terminal_safe_text(&format!("{error:#}"), true);
                update_status_for_turn(directory, &self.token, SessionState::Working, Some(error))?;
                Ok(())
            }
            Delivery::NotSent(error) => {
                let error = terminal_safe_text(&format!("{error:#}"), true);
                let _lock = Store::open_unchecked(directory).lock()?;
                // A completion journal may have outlived its sender. Publish it before
                // deciding whether this refusal still owns any state to roll back.
                recover_pending_completion_locked(directory, &self.path)?;
                rollback_turn_claim_token_with_error_locked(
                    &self.path,
                    &self.token,
                    self.rollback_state.clone().unwrap_or(SessionState::Failed),
                    Some(error),
                )?;
                Ok(())
            }
        }
    }

    pub(in crate::native) fn complete_initial_delivery(&mut self) -> Result<()> {
        self.settle_delivery(Delivery::Sent)?;
        let directory = self
            .path
            .parent()
            .context("turn claim has no session directory")?;
        Store::open_unchecked(directory)
            .record(CoreRecord::InitialPrompt)
            .remove_raw()
            .context("failed to remove the delivered initial prompt")
    }

    pub(in crate::native) fn rollback_on_drop(&mut self) {
        self.retained = false;
    }

    pub(in crate::native) fn retain_in_place(&mut self) {
        self.retained = true;
    }

    // Releases the claim now and publishes `state` with `reason` in the same status write,
    // under the lifecycle lock and only while the claim file still holds this token. Returns
    // whether that write happened; a claim that another request already owns is left alone
    // together with the status it published. Dropping the claim later does nothing more.
    pub(in crate::native) fn release_now_with_reason(
        &mut self,
        state: SessionState,
        reason: String,
    ) -> Result<bool> {
        if self.retained {
            return Ok(false);
        }
        self.retained = true;
        rollback_turn_claim_token_with_error(&self.path, &self.token, state, Some(reason))
    }

    pub(in crate::native) fn retain(mut self) {
        self.retain_in_place();
    }
}

/// Compare-and-set status write on behalf of one turn: the status changes only while the
/// claim named by `claim_token` is still the installed claim, checked and written under
/// the turn-claim lifecycle lock. A writer whose turn has already been released or
/// replaced is rejected with `Ok(false)` and leaves the status generation untouched.
///
/// Every status writer that reports about a specific turn (delivery failures, delivery
/// uncertainty) goes through this helper; writers that report about the session as a
/// whole (process exit, monitor failure, close) use `update_status` under their own
/// guards.
fn update_status_for_turn(
    directory: &Path,
    claim_token: &str,
    state: SessionState,
    error: Option<String>,
) -> Result<bool> {
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    update_status_for_turn_locked(directory, claim_token, state, error)
}

fn update_status_for_turn_locked(
    directory: &Path,
    claim_token: &str,
    state: SessionState,
    error: Option<String>,
) -> Result<bool> {
    if read_claim_token(directory)?.as_deref() != Some(claim_token) {
        return Ok(false);
    }
    update_status(directory, state, None, error)?;
    Ok(true)
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.retained {
            if let Some(state) = self.rollback_state.clone() {
                let _ = rollback_turn_claim_token(&self.path, &self.token, state);
            } else {
                let _ = release_claim_token(&self.path, &self.token);
            }
        }
    }
}

fn rollback_turn_claim_token(path: &Path, expected_token: &str, state: SessionState) -> Result<()> {
    rollback_turn_claim_token_with_error(path, expected_token, state, None).map(|_| ())
}

// Under the lifecycle lock: the claim is removed and the status is rolled back to `state`
// (carrying `error`) only while the claim file still holds `expected_token`. Returns
// whether that happened. The reliability branch introduces `update_status_for_turn` for
// claim-checked status writes; this helper is the equivalent for the rollback path.
//
// Merge reconciliation note: this must remain one lifecycle critical section that does the
// ownership check, the status publication, and the claim removal under a single hold of the
// lock. It is not a drop-in for `update_status_for_turn` followed by removal: that helper
// takes the same lock (calling it from inside this section would lock recursively), and
// calling the claim-checking helper after the removal would find no claim and report the
// write as not owned. Keep all three steps here, under the one lock.
fn rollback_turn_claim_token_with_error(
    path: &Path,
    expected_token: &str,
    state: SessionState,
    error: Option<String>,
) -> Result<bool> {
    let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
    rollback_turn_claim_token_with_error_locked(path, expected_token, state, error)
}

fn rollback_turn_claim_token_with_error_locked(
    path: &Path,
    expected_token: &str,
    state: SessionState,
    error: Option<String>,
) -> Result<bool> {
    let current = match session::RecordReader::at(path).raw_text() {
        Ok(current) => current,
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(read_error) => return Err(read_error).context("failed to inspect native turn claim"),
    };
    if current.trim() != expected_token {
        return Ok(false);
    }
    remove_turn_claim_locked(path)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    update_status(directory, state, None, error)?;
    Ok(true)
}

#[cfg(test)]
pub(in crate::native) fn acquire_turn_claim(directory: &Path) -> Result<Claim> {
    acquire_turn_claim_with_context(directory, &[])
}

// The receipt records the pinned context sources the caller already resolved.
fn acquire_turn_claim_with_context(
    directory: &Path,
    context_sources: &[requests::ContextSource],
) -> Result<Claim> {
    let path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
    create_turn_claim_locked(path, context_sources)
}

#[cfg(test)]
pub(in crate::native) fn acquire_ready_turn_claim(
    directory: &Path,
    session_id: &str,
) -> Result<(Claim, usize)> {
    acquire_ready_turn_claim_after_claim(directory, session_id, || Ok(()))
}

fn claim_ready_with_context(
    directory: &Path,
    session_id: &str,
    context_sources: &[requests::ContextSource],
) -> Result<(Claim, usize)> {
    claim_ready_with_callbacks(
        directory,
        session_id,
        || {},
        || Ok(()),
        || {},
        context_sources,
    )
}

#[cfg(test)]
pub(in crate::native) fn acquire_ready_turn_claim_after_claim<F>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
) -> Result<(Claim, usize)>
where
    F: FnOnce() -> Result<()>,
{
    claim_ready_with_callbacks(directory, session_id, || {}, after_claim, || {}, &[])
}

fn claim_ready_with_callbacks<B, F, G>(
    directory: &Path,
    session_id: &str,
    before_claim: B,
    after_claim: F,
    before_publish: G,
    context_sources: &[requests::ContextSource],
) -> Result<(Claim, usize)>
where
    B: FnOnce(),
    F: FnOnce() -> Result<()>,
    G: FnOnce(),
{
    let path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    session::hold::follow_up_admission(&Reader::open_unchecked(directory), session_id)?;
    before_claim();
    let mut claim = {
        let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
        session::hold::follow_up_admission(&Reader::open_unchecked(directory), session_id)?;
        create_turn_claim_locked(path.clone(), context_sources)?
    };
    if let Err(error) = after_claim() {
        let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
        let _ = release_turn_claim_token_locked(&path, &claim.token);
        claim.retain();
        return Err(error);
    }
    let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
    let current = session::RecordReader::at(&path)
        .raw_text()
        .with_context(|| "native turn claim disappeared before it could start")?;
    if current.trim() != claim.token {
        bail!("native turn claim changed before it could start");
    }
    session::hold::follow_up_admission(&Reader::open_unchecked(directory), session_id)?;
    let baseline = Reader::open_unchecked(directory).events()?.len();
    before_publish();
    if let Err(error) = update_status(directory, SessionState::Claimed, None, None) {
        let _ = release_turn_claim_token_locked(&path, &claim.token);
        claim.retain();
        return Err(error);
    }
    claim.rollback_state = Some(SessionState::Ready);
    claim.follow_up_session = Some(session_id.to_owned());
    Ok((claim, baseline))
}

fn create_turn_claim_locked(
    path: PathBuf,
    context_sources: &[requests::ContextSource],
) -> Result<Claim> {
    let store = Store::open_unchecked(path.with_file_name(""));
    let mut file = store.create_claim_file()?;
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        TURN_CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    store.write_claim_token(&mut file, &token)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    let receipt = match requests::create(&Store::open_unchecked(directory), &token, context_sources)
    {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = remove_turn_claim_locked(&path);
            return Err(error).context("failed to persist request receipt before dispatch");
        }
    };
    Ok(Claim {
        path,
        token,
        receipt,
        retained: false,
        rollback_state: None,
        follow_up_session: None,
    })
}

fn release_claim_token(path: &Path, expected_token: &str) -> Result<()> {
    let _lock = Store::open_unchecked((path).with_file_name("")).lock()?;
    release_turn_claim_token_locked(path, expected_token)
}

fn release_turn_claim_token_locked(path: &Path, expected_token: &str) -> Result<()> {
    let token = match session::RecordReader::at(path).raw_text() {
        Ok(token) => token,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect native turn claim"),
    };
    if token.trim() != expected_token {
        return Ok(());
    }
    RecordStore::at(path)
        .remove()
        .context("failed to release native turn claim")
}

#[cfg(test)]
pub(in crate::native) fn release_turn_claim(directory: &Path) -> Result<()> {
    let path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&path)?;
    remove_turn_claim_locked(&path)
}

pub(in crate::native) fn remove_turn_claim_locked(path: &Path) -> Result<()> {
    RecordStore::at(path)
        .remove()
        .context("failed to release native turn claim")
}

pub(in crate::native) fn valid_turn_claim_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 160
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}

pub(in crate::native) fn recover_pending_completion(directory: &Path) -> Result<bool> {
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    recover_pending_completion_locked(directory, &claim_path)
}

/// Converges every partial lifecycle transition that a stopped process can leave behind,
/// under the turn-claim lifecycle lock. Returns whether any record changed.
///
/// Two transitions are journaled and therefore recoverable: a provider completion (journal
/// -> event -> status -> claim release -> journal removal) and an explicit or repair close
/// (tombstone -> status -> set aside an unverified event -> claim release -> journal
/// removal). The `closed.json` tombstone is the durable commit point of a close: once it
/// exists the session is closed even when the later cleanup steps never ran, so recovery
/// finishes those steps instead of publishing. Both sequences release the claim before they
/// remove the journal: while the claim is installed the journal is the only evidence that
/// the event at its path is the provider's committed result, so no interruption may leave
/// the claim without the journal. For the same reason both sequences sync `events/` before
/// they discard the journal of an event that already matches it: the completion that wrote
/// the event may have stopped between its rename and that sync.
pub(in crate::native) fn recover_pending_completion_locked(
    directory: &Path,
    claim_path: &Path,
) -> Result<bool> {
    let completion_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Completion)
        .path()
        .to_owned();
    if let Some(tombstone) = Reader::open_unchecked(directory).closed_if_present()? {
        return super::close::converge_interrupted(
            &Store::open_unchecked(directory),
            claim_path,
            &tombstone,
        );
    }
    let Some(text) = session::RecordReader::at(&completion_path).text()? else {
        return Ok(false);
    };
    let pending: PendingTurnCompletion =
        serde_json::from_str(&text).context("invalid pending native turn completion")?;
    validate_pending_completion(&pending)?;

    match session::RecordReader::at(claim_path).raw_text() {
        Ok(current) if current.trim() == pending.claim_token => {
            write_completion_event(directory, &pending)?;
            update_status(
                directory,
                pending.status_state.clone(),
                None,
                pending.status_error.clone(),
            )?;
            release_turn_claim_token_locked(claim_path, &pending.claim_token)?;
            RecordStore::at(&completion_path).remove()?;
            Ok(true)
        }
        Ok(_) => bail!("pending native completion belongs to a different turn claim"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The same byte-match predicate every other lifecycle path uses: a semantically
            // equal event stored in another encoding is not the journal's committed write.
            match journaled_event_state(directory, &pending)? {
                JournaledEventState::Committed => (),
                JournaledEventState::Absent => {
                    bail!("claim-free pending completion has no committed event")
                }
                JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => {
                    bail!("claim-free pending completion event does not match its journal")
                }
            }
            let status: SessionStatus = Reader::open_unchecked(directory).status()?;
            if status.state != pending.status_state || status.error != pending.status_error {
                bail!("claim-free pending completion has no matching terminal status")
            }
            // The claim is released only after the event's directory was synced, but the
            // journal is the last evidence of the result, so its removal is preceded by
            // the same barrier regardless of which run released the claim.
            sync_committed_event_directory(directory)?;
            RecordStore::at(&completion_path).remove()?;
            Ok(true)
        }
        Err(error) => Err(error).context("failed to inspect pending completion turn claim"),
    }
}

/// How the file at a journal's event path relates to the journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum JournaledEventState {
    /// No event file exists at the journal's event path: nothing was published.
    Absent,
    /// The event file holds exactly the bytes the journal would write, so it is the
    /// provider result under its receipt's immutable event name. Byte equality alone does
    /// not make it durable: the completion that wrote it may have stopped between the
    /// rename and the sync of `events/`, so every lifecycle path syncs `events/` before it
    /// discards the journal of a committed event (`sync_committed_event_directory`).
    Committed,
    /// A different record occupies the journal's event path.
    Mismatched,
    /// The file at the journal's event path is larger than the caller's read limit (its
    /// size in bytes), so it was not compared and is never treated as published.
    Oversized(u64),
}

/// The publication predicate's verdict together with what it cost: the bytes it read, and
/// the stored text when they are the journal's, so a budgeted caller can charge the read
/// once and search the record without reading it again.
pub(in crate::native) struct JournaledEventRead {
    pub(in crate::native) state: JournaledEventState,
    pub(in crate::native) bytes_read: u64,
    pub(in crate::native) committed_text: Option<String>,
}

/// Compare bytes already read by a bounded reader. Timeline supplies its retained
/// read here; publication readers supply their single bounded file read.
pub(in crate::native) fn journaled_event_state_of(
    pending: &PendingTurnCompletion,
    bytes: &[u8],
    limit: u64,
) -> Result<JournaledEventState> {
    if bytes.len() as u64 > limit {
        return Ok(JournaledEventState::Oversized(bytes.len() as u64));
    }
    Ok(if bytes == serde_json::to_vec_pretty(&pending.event)? {
        JournaledEventState::Committed
    } else {
        JournaledEventState::Mismatched
    })
}

/// The single byte-match predicate: a journaled event is committed exactly when its file
/// holds the bytes the journal would write. Every lifecycle path (completion recovery,
/// claim-free recovery, close, interrupted close) and every read-only query decide
/// publication with this comparison.
pub(in crate::native) fn journaled_event_state(
    directory: &Path,
    pending: &PendingTurnCompletion,
) -> Result<JournaledEventState> {
    Ok(Reader::open_unchecked(directory)
        .journaled_event_state_within(pending, EVENT_READ_LIMIT)?
        .state)
}

pub(in crate::native) fn validate_pending_completion(
    pending: &PendingTurnCompletion,
) -> Result<()> {
    if pending.event.created_unix_ms.is_none() {
        bail!("pending native completion event is missing created_unix_ms")
    }
    if pending.schema != 1
        || !valid_turn_claim_token(&pending.claim_token)
        || !matches!(
            pending.status_state,
            SessionState::Ready | SessionState::Failed
        )
    {
        bail!("invalid pending native completion identity")
    }
    if !Reader::valid_event_file_name(&pending.event_file) {
        bail!("invalid pending native completion event file")
    }
    if pending.event.error != pending.status_error {
        bail!("pending native completion status does not match its event")
    }
    Ok(())
}

fn write_completion_event(directory: &Path, pending: &PendingTurnCompletion) -> Result<()> {
    validate_pending_completion(pending)?;
    match journaled_event_state(directory, pending)? {
        // The completion that wrote this event may have stopped between its rename and
        // the sync of `events/`; the journal is discarded once this returns, so the
        // entry is made durable here.
        JournaledEventState::Committed => sync_committed_event_directory(directory),
        JournaledEventState::Mismatched => {
            bail!("pending native completion event file contains different data")
        }
        JournaledEventState::Oversized(size) => {
            bail!(
                "pending native completion event file is {size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
            )
        }
        JournaledEventState::Absent => {
            // The same size policy the journal was created under, re-checked for a
            // journal an earlier version wrote: an event the predicate could never compare
            // is not written, so the absent and present orders settle the same way.
            let size = serde_json::to_vec_pretty(&pending.event)?.len() as u64;
            if size > EVENT_READ_LIMIT {
                bail!(
                    "pending native completion event is {size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
                )
            }
            RecordStore::at(
                &Reader::open_unchecked(directory)
                    .record(CoreRecord::Events)
                    .path()
                    .to_owned()
                    .join(&pending.event_file),
            )
            .write_json(&pending.event)
        }
    }
}

#[cfg(target_os = "macos")]
fn running_wait_owner(directory: &Path) -> Result<()> {
    if let Some(text) = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .text()?
    {
        let owner: NativeSessionOwner =
            serde_json::from_str(&text).context("invalid native-session owner record")?;
        if !process_is_alive(owner.pid) {
            bail!(
                "native session process {} is no longer running; terminal cleanup remains unverified",
                owner.pid
            );
        }
    }
    Ok(())
}

pub(in crate::native) fn wait_for_status(
    directory: &Path,
    expected_state: SessionState,
    deadline: Instant,
    requested: Duration,
) -> Result<SessionStatus> {
    loop {
        Store::open_unchecked(directory).converge()?;
        if let Ok(status) = Reader::open_unchecked(directory).status() {
            if matches!(
                status.state,
                SessionState::Failed | SessionState::Exited | SessionState::Closed
            ) {
                let reason = status
                    .error
                    .unwrap_or_else(|| format!("session entered state {}", status.state));
                bail!("{reason}");
            }
            #[cfg(target_os = "macos")]
            running_wait_owner(directory)?;
            if status.state == expected_state {
                return Ok(status);
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .with_context(|| {
                format!(
                    "timed out after {} seconds waiting for session state {expected_state}",
                    requested.as_secs()
                )
            })?;
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
pub(in crate::native) fn wait_for_event(
    directory: &Path,
    baseline: usize,
    timeout: Duration,
) -> Result<SessionEvent> {
    wait_for_event_for_turn(directory, baseline, None, None, timeout)
}

#[cfg(test)]
pub(in crate::native) fn wait_for_event_for_turn(
    directory: &Path,
    baseline: usize,
    expected_turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
    timeout: Duration,
) -> Result<SessionEvent> {
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    wait_for_event_for_turn_until(
        directory,
        baseline,
        expected_turn_id,
        expected_claim_token,
        deadline,
        timeout,
    )
}

fn wait_for_event_for_turn_until(
    directory: &Path,
    baseline: usize,
    expected_turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
    deadline: Instant,
    requested: Duration,
) -> Result<SessionEvent> {
    loop {
        Store::open_unchecked(directory).converge()?;
        let paths = Reader::open_unchecked(directory).events()?;
        if paths.len() > baseline {
            let candidates = &paths[baseline..];
            let event = if let Some(expected_turn_id) = expected_turn_id {
                let mut matched = None;
                for path in candidates {
                    let event: SessionEvent = session::RecordReader::at(path).json()?;
                    if event.turn_id.as_deref() == Some(expected_turn_id) {
                        matched = Some(event);
                        break;
                    }
                }
                matched
            } else {
                Some(
                    RecordReader::at(candidates.first().context("event path disappeared")?)
                        .json()?,
                )
            };
            if let Some(event) = event
                && published(&Reader::open_unchecked(directory), expected_claim_token)?
            {
                if let Some(error) = event.error.as_deref() {
                    bail!("{error}");
                }
                return Ok(event);
            }
        }
        if let Ok(status) = Reader::open_unchecked(directory).status()
            && matches!(
                status.state,
                SessionState::Failed | SessionState::Exited | SessionState::Closed
            )
        {
            let reason = status
                .error
                .unwrap_or_else(|| format!("session entered state {}", status.state));
            bail!("{reason}");
        }
        #[cfg(target_os = "macos")]
        running_wait_owner(directory)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .with_context(|| format!("timed out after {} seconds", requested.as_secs()))?;
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

/// Command wait barrier, addressed by the claim rather than a receipt: a missing or
/// damaged receipt must not prevent a verified provider completion from reaching its
/// waiting caller. Read-only queries instead use `event_published` on their snapshot.
pub(in crate::native) fn published(
    reader: &Reader,
    expected_claim_token: Option<&str>,
) -> Result<bool> {
    if let Some(expected_token) = expected_claim_token {
        return match reader.record(CoreRecord::TurnClaim).raw_text() {
            Ok(current_token) => Ok(current_token.trim() != expected_token),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error).context("failed to inspect native turn claim"),
        };
    }
    Ok(reader
        .status()
        .is_ok_and(|status| status.state == SessionState::Ready))
}

/// Acquire the initial turn, binding its receipt before dispatch.
pub(in crate::native) fn claim(
    store: &Store,
    context: &[requests::ContextSource],
) -> Result<Claim> {
    acquire_turn_claim_with_context(store.directory(), context)
}

/// Acquire a follow-up and its event baseline under the existing lifecycle protocol.
pub(in crate::native) fn claim_ready(
    store: &Store,
    session_id: &str,
    context: &[requests::ContextSource],
) -> Result<(Claim, usize)> {
    claim_ready_with_context(store.directory(), session_id, context)
}

pub(in crate::native) fn current_claim_token(reader: &Reader) -> Result<Option<String>> {
    read_claim_token(reader.directory())
}

impl Claim {
    pub(in crate::native) fn token(&self) -> &str {
        &self.token
    }
    pub(in crate::native) fn receipt(&self) -> &requests::Receipt {
        &self.receipt
    }
}

/// An adapter's completion handle. Provider identity is explicit: constructing a report
/// must not introduce a manifest read into a provider-verified completion path.
pub(in crate::native) struct Report<'a> {
    store: &'a Store,
    provider: FirstPartyCli,
    claim_token: Option<&'a str>,
    initial: bool,
}

impl<'a> Report<'a> {
    pub(in crate::native) fn for_claim(
        store: &'a Store,
        provider: FirstPartyCli,
        claim_token: Option<&'a str>,
    ) -> Self {
        Self {
            store,
            provider,
            claim_token,
            initial: false,
        }
    }

    /// Claude's uncorrelated initial hooks are accepted only before its first event.
    pub(in crate::native) fn initial(store: &'a Store, provider: FirstPartyCli) -> Self {
        Self {
            store,
            provider,
            claim_token: None,
            initial: true,
        }
    }

    pub(in crate::native) fn monitor_failure(
        store: &Store,
        provider: FirstPartyCli,
        error: &str,
    ) -> Result<()> {
        record_monitor_failure(store.directory(), provider, error)
    }

    pub(in crate::native) fn complete(
        &self,
        message: &str,
        provider_session_id: Option<String>,
        turn_id: Option<String>,
    ) -> Result<()> {
        record_provider_result_for_claim_condition(
            self.store.directory(),
            self.provider,
            message,
            provider_session_id,
            turn_id,
            self.claim_token,
            self.initial,
        )
    }

    pub(in crate::native) fn fail(
        &self,
        error: &str,
        provider_session_id: Option<String>,
        turn_id: Option<String>,
    ) -> Result<()> {
        record_provider_failure_for_claim_condition(
            self.store.directory(),
            self.provider,
            error,
            provider_session_id,
            turn_id,
            self.claim_token,
            self.initial,
        )
    }
}

#[cfg(test)]
pub(in crate::native) fn commit_provider_completion_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
) -> Result<()> {
    commit_completion_locked(directory, claim_path, claim_token, event, status_error)
}

#[cfg(test)]
pub(in crate::native) fn commit_provider_completion_within_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
    status_state: SessionState,
    event_limit: u64,
) -> Result<()> {
    commit_completion_within_locked(
        directory,
        claim_path,
        claim_token,
        event,
        status_error,
        status_state,
        event_limit,
    )
}

#[cfg(test)]
pub(in crate::native) fn acquire_ready_turn_claim_with_context(
    directory: &Path,
    session_id: &str,
    context_sources: &[requests::ContextSource],
) -> Result<(Claim, usize)> {
    claim_ready_with_context(directory, session_id, context_sources)
}

#[cfg(test)]
pub(in crate::native) fn acquire_ready_turn_claim_with_callbacks<F, G>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
    before_publish: G,
    context_sources: &[requests::ContextSource],
) -> Result<(Claim, usize)>
where
    F: FnOnce() -> Result<()>,
    G: FnOnce(),
{
    claim_ready_with_callbacks(
        directory,
        session_id,
        || {},
        after_claim,
        before_publish,
        context_sources,
    )
}

#[cfg(test)]
pub(in crate::native) fn release_turn_claim_token(path: &Path, expected_token: &str) -> Result<()> {
    release_claim_token(path, expected_token)
}

#[cfg(test)]
pub(in crate::native) fn write_pending_completion_event(
    directory: &Path,
    pending: &PendingTurnCompletion,
) -> Result<()> {
    write_completion_event(directory, pending)
}

#[cfg(test)]
pub(in crate::native) fn current_turn_claim_token(directory: &Path) -> Result<Option<String>> {
    current_claim_token(&Reader::open_unchecked(directory))
}

#[cfg(test)]
mod tests;

/// Makes a committed event's directory entry durable before the journal that proves the
/// event is the provider's result can be discarded. A completion that stopped between the
/// event's rename and the sync of `events/` leaves the entry unsynced while the journal
/// still exists; every lifecycle path that discards the journal of a committed event
/// (completion recovery, claim-free recovery, close, interrupted close) syncs `events/`
/// first, so a later crash cannot lose the event together with its evidence. The sync is
/// idempotent when the completion already made the entry durable, and it is a fault
/// boundary like every other sync that follows a rename.
pub(in crate::native) fn sync_committed_event_directory(directory: &Path) -> Result<()> {
    Store::open_unchecked(directory).sync_committed_event_directory()
}

/// Publication over records already read by a query. In particular, a journal can
/// establish publication before its claim is released. Do not replace this with the
/// command wait's claim-release barrier, or re-read records from a retained snapshot.
pub(in crate::native) fn event_published(
    journaled: bool,
    pending_event: Option<&JournaledEventRead>,
    claim_matches_receipt: bool,
) -> bool {
    if journaled {
        return pending_event.is_some_and(|read| read.state == JournaledEventState::Committed);
    }
    !claim_matches_receipt
}

#[cfg(test)]
pub(in crate::native) fn record_provider_monitor_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
) -> Result<()> {
    record_monitor_failure(directory, provider, error)
}

#[cfg(all(test, target_os = "macos"))]
pub(in crate::native) fn require_running_wait_owner(directory: &Path) -> Result<()> {
    running_wait_owner(directory)
}

/// Wait for a delivered turn, converging its journal before observing its result.
pub(in crate::native) fn wait(
    store: &Store,
    baseline: usize,
    expected_turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
    deadline: Instant,
    requested: Duration,
) -> Result<SessionEvent> {
    wait_for_event_for_turn_until(
        store.directory(),
        baseline,
        expected_turn_id,
        expected_claim_token,
        deadline,
        requested,
    )
}
