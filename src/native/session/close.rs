//! Explicit close, interrupted-close convergence, and dead-owner repair.
use crate::native::terminal::ownership::{
    close_dead_owner_surface, owner_blocks_prune, repair_owner_is_live,
    retained_surface_outlives_owner,
};
use crate::native::*;

#[cfg(target_os = "macos")]
#[derive(Deserialize, Serialize)]
struct TerminalCloseIntent {
    managed_session_id: String,
    terminal: terminal::TerminalSession,
    owner: NativeSessionOwner,
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn record_terminal_close_intent(
    store: &Store,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
) -> Result<()> {
    store
        .record(CoreRecord::TerminalCloseIntent)
        .write_json(&TerminalCloseIntent {
            managed_session_id: expected_session_id.to_owned(),
            terminal: session.clone(),
            owner: owner.clone(),
        })
}

// An explicit close can end the owner before the window is gone, so a
// close that fails after it can never verify that owner again, and the owner's death
// says nothing about the window. The intent that the close recorded first lets only
// that close finish: it names this managed session, this exact handle and this exact
// owner record, with the owner's whole identity. Anything else, unreadable or not,
// grants nothing.
#[cfg(target_os = "macos")]
pub(in crate::native) fn terminal_close_intent_owner(
    reader: &Reader,
    session: &terminal::TerminalSession,
    surface_outlives_owner: bool,
) -> Result<Option<NativeSessionOwner>> {
    if !surface_outlives_owner {
        return Ok(None);
    }
    let (Some(intent), Some(owner)) = (
        reader.record(CoreRecord::TerminalCloseIntent).text()?,
        reader.record(CoreRecord::Owner).text()?,
    ) else {
        return Ok(None);
    };
    let (Ok(intent), Ok(owner)) = (
        serde_json::from_str::<TerminalCloseIntent>(&intent),
        serde_json::from_str::<NativeSessionOwner>(&owner),
    ) else {
        return Ok(None);
    };
    let attested = matches!(
        (
            owner.terminal_tty_device,
            owner.process_start_seconds,
            owner.process_start_microseconds,
            owner.process_group,
            owner.terminal_process_group,
        ),
        (Some(_), Some(_), Some(_), Some(_), Some(_))
    );
    let bound = session.managed_session_id.as_deref() == Some(intent.managed_session_id.as_str())
        && owner.managed_session_id.as_deref() == Some(intent.managed_session_id.as_str());
    let exact = intent.terminal == *session
        && serde_json::to_value(&intent.owner)? == serde_json::to_value(&owner)?;
    Ok((attested && bound && exact).then_some(owner))
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn terminal_close_resumable(
    reader: &Reader,
    session: &terminal::TerminalSession,
    surface_outlives_owner: bool,
) -> Result<bool> {
    Ok(
        terminal_close_intent_owner(reader, session, surface_outlives_owner)?
            .is_some_and(|owner| !process_is_alive(owner.pid)),
    )
}

impl Reader {
    pub(in crate::native) fn has_active_session_capability(&self) -> bool {
        [
            CoreRecord::Terminal.name(),
            CoreRecord::TerminalClosing.name(),
            CoreRecord::TurnClaim.name(),
            CoreRecord::Completion.name(),
            CoreRecord::LegacyResumePending.name(),
            CoreRecord::LegacyResumeRunning.name(),
        ]
        .into_iter()
        .any(|name| fs::symlink_metadata(self.private(name).path()).is_ok())
    }
}

impl Reader {
    pub(in crate::native) fn native_owner_blocks_prune(&self) -> Result<bool> {
        let path = self.record(CoreRecord::Owner).path().to_owned();
        let text = match session::RecordReader::at(&path).text() {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(false),
            Err(_) => return Ok(true),
        };
        let owner = match serde_json::from_str::<NativeSessionOwner>(&text) {
            Ok(owner) => owner,
            Err(_) => return Ok(true),
        };

        owner_blocks_prune(&owner)
    }
}

// Repairs a dead native owner first, then closes. A repair failure is the recorded close
// error; otherwise `reason` (if any) is kept in the closed status.
pub(in crate::native) fn close<F>(
    store: &Store,
    reason: Option<String>,
    mut close_terminal: F,
) -> Result<terminal::CloseOutcome>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let repair_error = repair_dead_owner(store)
        .err()
        .map(|error| format!("pre-close session repair failed: {error:#}"));
    let mut outcome = terminal::CloseOutcome::Missing;
    close_session_state_with_error(store, repair_error.or(reason), |surface| {
        outcome = close_terminal(surface)?;
        Ok(outcome)
    })?;
    Ok(outcome)
}

fn close_session_state_with_error<F>(
    store: &Store,
    close_error: Option<String>,
    mut close_terminal: F,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let _turn_lock = store.lock()?;
    let status: SessionStatus = store.status()?;
    if status.state == SessionState::Closed {
        let consume_result = consume_terminal_handle(store, None);
        let close_result = mark_session_closed_locked(store, &claim_path, close_error);
        consume_result?;
        return close_result;
    }

    let terminal_path = store.record(CoreRecord::Terminal).path().to_owned();
    let closing_path = store.record(CoreRecord::TerminalClosing).path().to_owned();
    if store
        .record(CoreRecord::TerminalClosed)
        .path()
        .to_owned()
        .exists()
    {
        let consume_result = consume_terminal_handle(store, None);
        let close_result = mark_session_closed_locked(store, &claim_path, close_error);
        consume_result?;
        return close_result;
    }
    match store.claim_terminal_handle()? {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !closing_path.exists() {
                return mark_session_closed_locked(store, &claim_path, close_error);
            }
            // A prior closer may have stopped after atomically claiming the handle but before
            // invoking the terminal adapter. The turn-claim lock serializes recovery, so resume
            // that durable close transaction instead of reporting success with a live surface.
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim terminal handle {}",
                    terminal_path.display()
                )
            });
        }
    }

    let terminal =
        match session::RecordReader::at(&closing_path).json::<terminal::TerminalSession>() {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal_handle(&closing_path, &terminal_path)?;
                return Err(error);
            }
        };
    if let Err(error) = close_terminal(&terminal) {
        restore_terminal_handle(&closing_path, &terminal_path)?;
        return Err(error);
    }

    let consume_result = consume_terminal_handle(store, Some(terminal.kind.as_str()));
    let close_result = mark_session_closed_locked(store, &claim_path, close_error);
    consume_result?;
    close_result
}

fn restore_terminal_handle(closing_path: &Path, terminal_path: &Path) -> Result<()> {
    RecordStore::at(closing_path)
        .rename_to(&RecordStore::at(terminal_path))
        .with_context(|| {
            format!(
                "failed to restore terminal handle {} after close failure",
                terminal_path.display()
            )
        })
}

fn consume_terminal_handle(store: &Store, terminal_kind: Option<&str>) -> Result<()> {
    let tombstone_result = store.write_terminal_closed(&serde_json::json!({
        "consumed": true,
        "terminal": terminal_kind,
    }));
    let active_result = RecordStore::at(store.record(CoreRecord::Terminal).path()).remove();
    let closing_result = RecordStore::at(store.record(CoreRecord::TerminalClosing).path()).remove();
    #[cfg(target_os = "macos")]
    let intent_result =
        RecordStore::at(store.record(CoreRecord::TerminalCloseIntent).path()).remove();
    #[cfg(not(target_os = "macos"))]
    let intent_result = Ok(());
    tombstone_result?;
    active_result?;
    closing_result?;
    intent_result
}

// Finishes a close whose tombstone was written but whose later cleanup steps did not run.
// The tombstone is preserved unchanged: status.json is rewritten from it (update_status
// copies the tombstone whenever one exists), and the turn claim, the journal, and the
// legacy resume markers are removed in the same order the uninterrupted close uses. A
// journaled completion is settled exactly as that close settles it: an event it already
// wrote stays published when it matches the journal, is set aside when it does not, and a
// journal without an event is discarded.
pub(in crate::native) fn converge_interrupted(
    store: &Store,
    claim_path: &Path,
    tombstone: &SessionStatus,
) -> Result<bool> {
    let directory = store.directory();
    let mut changed = false;
    let status_matches = store.status_if_present()?.is_some_and(|status| {
        status.state == tombstone.state
            && status.generation == tombstone.generation
            && status.error == tombstone.error
    });
    if !status_matches {
        update_status(
            directory,
            SessionState::Closed,
            None,
            tombstone.error.clone(),
        )?;
        changed = true;
    }
    let completion_path = store.record(CoreRecord::Completion).path().to_owned();
    if completion_path.exists() {
        set_aside_unverified_completion_event_for_close(store)?;
        changed = true;
    }
    if claim_path.exists() {
        remove_turn_claim_locked(claim_path)?;
        changed = true;
    }
    RecordStore::at(&completion_path).remove()?;
    for name in [
        CoreRecord::LegacyResumePending.name(),
        CoreRecord::LegacyResumeRunning.name(),
    ] {
        let path = store.private(name).path().to_owned();
        if path.exists() {
            RecordStore::at(&path).remove()?;
            changed = true;
        }
    }
    Ok(changed)
}

/// The first settlement step of a close that finds a completion journal in place. The
/// tombstone is the close's commit point, but an event the interrupted completion already
/// wrote is the provider's authoritative result: when it matches the journal byte for byte
/// it stays published (the receipt already maps the request to it), and when it does not
/// match, or is too large to compare, it is moved aside under an `unpublished-` name that
/// no query reads. A journal whose event was never written needs no step here and is
/// discarded when the close removes the journal; a session whose `events` directory is
/// missing altogether holds no event to verify and settles the same way, so a close is
/// never left permanently unsettled by that damage. A link or a non-directory at `events`
/// is still rejected, and the journal then stays in place with the claim. The move is
/// idempotent, so an interrupted close converges on the next run.
///
/// The close removes the journal only after this step and after the turn claim is
/// released: while the claim is installed, the journal is the evidence that the event at
/// its path is the committed result, so an interruption before claim release would
/// otherwise hide a published result until the next recovery. The journal is also the
/// only evidence that a set-aside event was unverified, so the move is made durable
/// before the journal can be discarded: the rename syncs `events/` itself, and a run that
/// finds the event already moved aside by an interrupted close, which may have stopped
/// between the rename and that sync, syncs `events/` again before it returns. A committed
/// event gets the same barrier: the completion that wrote it may have stopped between its
/// rename and the sync of `events/`, so the close syncs the directory before the journal,
/// the only proof that the entry is the result, is removed.
fn set_aside_unverified_completion_event_for_close(store: &Store) -> Result<()> {
    let directory = store.directory();
    let completion_path = store.record(CoreRecord::Completion).path().to_owned();
    let Some(text) = session::RecordReader::at(&completion_path).text()? else {
        return Ok(());
    };
    if store.events_directory_state()? == EventsDirectory::Missing {
        return Ok(());
    }
    let Ok(pending) = serde_json::from_str::<PendingTurnCompletion>(&text) else {
        return Ok(());
    };
    if validate_pending_completion(&pending).is_err() {
        return Ok(());
    }
    let events = store.record(CoreRecord::Events).path().to_owned();
    let set_aside = store
        .unpublished_event(&pending.event_file)
        .path()
        .to_owned();
    match journaled_event_state(directory, &pending)? {
        JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => {
            RecordStore::at(&events.join(&pending.event_file))
                .rename_to(&RecordStore::at(&set_aside))
                .context(
                    "failed to set aside a completion event that disagrees with its journal",
                )?;
        }
        JournaledEventState::Absent => {
            if fs::symlink_metadata(&set_aside).is_ok() {
                store.sync_set_aside_event_directory()?;
            }
        }
        JournaledEventState::Committed => store.sync_committed_event_directory()?,
    }
    Ok(())
}

fn mark_session_closed(store: &Store, error: Option<String>) -> Result<()> {
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let _lock = store.lock()?;
    mark_session_closed_locked(store, &claim_path, error)
}

fn mark_session_closed_locked(
    store: &Store,
    claim_path: &Path,
    error: Option<String>,
) -> Result<()> {
    let directory = store.directory();
    let consume_result = if store
        .record(CoreRecord::Terminal)
        .path()
        .to_owned()
        .exists()
        || store
            .record(CoreRecord::TerminalClosing)
            .path()
            .to_owned()
            .exists()
    {
        consume_terminal_handle(store, None)
    } else {
        Ok(())
    };
    let status_result = update_status(directory, SessionState::Closed, None, error);
    let (pending_result, running_result, claim_result) = if status_result.is_ok() {
        // An event the journal disagrees with is set aside first, then the claim is
        // released, and only then is the journal removed: every interruption of this order
        // leaves a state in which a committed event stays published and an unverified one
        // stays hidden. The journal is kept whenever an earlier step failed.
        let claim_result = set_aside_unverified_completion_event_for_close(store)
            .and_then(|()| remove_turn_claim_locked(claim_path));
        let pending_result = if claim_result.is_ok() {
            RecordStore::at(store.record(CoreRecord::Completion).path()).remove()
        } else {
            Ok(())
        }
        .and_then(|()| {
            RecordStore::at(store.record(CoreRecord::LegacyResumePending).path()).remove()
        });
        (
            pending_result,
            RecordStore::at(store.record(CoreRecord::LegacyResumeRunning).path()).remove(),
            claim_result,
        )
    } else {
        (Ok(()), Ok(()), Ok(()))
    };
    consume_result?;
    status_result?;
    pending_result?;
    running_result?;
    claim_result
}

pub(in crate::native) fn repair_dead_owner(store: &Store) -> Result<bool> {
    repair_dead_owner_with(
        store,
        repair_owner_is_live,
        retained_surface_outlives_owner,
        close_dead_owner_surface,
    )
}

fn repair_dead_owner_with<F>(
    store: &Store,
    owner_is_live: impl FnOnce(&NativeSessionOwner) -> Result<bool>,
    retain_surface: impl Fn(&terminal::TerminalSession) -> bool,
    close_terminal: F,
) -> Result<bool>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let directory = store.directory();
    #[cfg(not(windows))]
    let _ = &close_terminal;
    #[cfg(not(target_os = "macos"))]
    let _ = &retain_surface;
    // Completion recovery runs first so a live owner's finished turn is published before
    // anything else is decided. Its failure is damage (a missing `events/`, a journal
    // whose event cannot be compared), not a reason to leave a dead owner's session
    // installed forever: the owner check still runs, a dead owner's session is closed as
    // it would be without the damage (the close settles the journal without publishing),
    // and the damage is reported in the close error. Under a live owner, or when the
    // owner cannot be shown dead, the damage is the result.
    let recovery_damage = recover_pending_completion(directory).err();
    let untouched = |damage: Option<anyhow::Error>| match damage {
        Some(error) => Err(error),
        None => Ok(false),
    };
    if launch::repair(store)? {
        return untouched(recovery_damage);
    }
    let status: SessionStatus = store.status()?;
    if !matches!(
        status.state,
        SessionState::Launching
            | SessionState::AwaitingInitialInput
            | SessionState::Running
            | SessionState::Ready
            | SessionState::Claimed
            | SessionState::ResumePending
            | SessionState::Working
            | SessionState::Exited
            | SessionState::Failed
    ) {
        return untouched(recovery_damage);
    }
    let owner_path = store.record(CoreRecord::Owner).path().to_owned();
    let owner = match session::RecordReader::at(&owner_path).raw_text() {
        Ok(text) => serde_json::from_str::<NativeSessionOwner>(&text)
            .with_context(|| format!("invalid JSON in {}", owner_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return untouched(recovery_damage);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", owner_path.display()));
        }
    };
    if owner_is_live(&owner)? {
        return untouched(recovery_damage);
    }
    let mut repair_error = status
        .error
        .clone()
        .unwrap_or_else(|| format!("native session process {} is no longer running", owner.pid));
    if let Some(damage) = &recovery_damage {
        repair_error = format!("{repair_error}; completion recovery failed: {damage:#}");
    }
    let repair_error = Some(repair_error);
    #[cfg(windows)]
    {
        if matches!(status.state, SessionState::Exited | SessionState::Failed) {
            mark_session_closed(store, repair_error)?;
            return Ok(true);
        }
        // The visible console root can outlive a failed native-session owner. Reuse the same
        // atomic terminal-handle claim as explicit close so concurrent repair callers cannot
        // perform the external close side effect twice.
        close_session_state_with_error(store, repair_error, close_terminal)
            .context("failed to close a Windows console whose native owner exited")?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        // An explicit close that began its teardown still has to close the surface or
        // prove it absent. Even a damaged/mismatched intent must preserve that pending
        // cleanup: it grants no authority, but is not evidence of a vanished surface.
        // The explicit close validates the full intent before using it as authority.
        #[cfg(target_os = "macos")]
        if store
            .record(CoreRecord::TerminalCloseIntent)
            .bytes()?
            .is_some()
        {
            return untouched(recovery_damage);
        }
        // A native owner ending on its own does not prove that a terminal window
        // disappeared. Keep the exact handle for the affected macOS adapters even
        // before a first explicit close. This gives no authority to send a close:
        // the normal live-owner/previous-intent checks must still pass.
        #[cfg(target_os = "macos")]
        for name in [
            CoreRecord::Terminal.name(),
            CoreRecord::TerminalClosing.name(),
        ] {
            if let Some(bytes) = store.private(name).bytes()? {
                let surface: terminal::TerminalSession = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid retained terminal handle {name}"))?;
                if retain_surface(&surface) {
                    return untouched(recovery_damage);
                }
            }
        }
        mark_session_closed(store, repair_error)?;
        Ok(true)
    }
}

#[cfg(test)]
pub(in crate::native) mod compatibility {
    use super::*;
    use crate::native::terminal::ownership::close_dead_owner_surface_with;
    pub(in crate::native) fn mark_session_closed(
        directory: &Path,
        error: Option<String>,
    ) -> Result<()> {
        super::mark_session_closed(&Store::open_unchecked(directory), error)
    }
    pub(in crate::native) fn close_session_state(
        directory: &Path,
        closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    ) -> Result<()> {
        super::close_session_state_with_error(&Store::open_unchecked(directory), None, closer)
    }
    pub(in crate::native) fn close_session_state_with_error(
        directory: &Path,
        error: Option<String>,
        closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    ) -> Result<()> {
        super::close_session_state_with_error(&Store::open_unchecked(directory), error, closer)
    }
    pub(in crate::native) fn close_repaired_session_state(
        directory: &Path,
        closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    ) -> Result<()> {
        super::close(&Store::open_unchecked(directory), None, closer).map(|_| ())
    }
    pub(in crate::native) fn repair_dead_native_owner(directory: &Path) -> Result<bool> {
        repair_dead_owner(&Store::open_unchecked(directory))
    }
    pub(in crate::native) fn repair_dead_native_owner_with_terminal_close(
        directory: &Path,
        mut closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    ) -> Result<bool> {
        repair_dead_owner_with(
            &Store::open_unchecked(directory),
            repair_owner_is_live,
            retained_surface_outlives_owner,
            |session| close_dead_owner_surface_with(session, &mut closer),
        )
    }
    #[cfg(target_os = "macos")]
    pub(in crate::native) fn record_terminal_close_intent(
        directory: &Path,
        id: &str,
        session: &terminal::TerminalSession,
        owner: &NativeSessionOwner,
    ) -> Result<()> {
        super::record_terminal_close_intent(&Store::open_unchecked(directory), id, session, owner)
    }
    #[cfg(target_os = "macos")]
    pub(in crate::native) fn terminal_close_intent_owner(
        directory: &Path,
        session: &terminal::TerminalSession,
    ) -> Result<Option<NativeSessionOwner>> {
        super::terminal_close_intent_owner(
            &Reader::open_unchecked(directory),
            session,
            retained_surface_outlives_owner(session),
        )
    }
    #[cfg(target_os = "macos")]
    pub(in crate::native) fn terminal_close_resumable(
        directory: &Path,
        session: &terminal::TerminalSession,
    ) -> Result<bool> {
        super::terminal_close_resumable(
            &Reader::open_unchecked(directory),
            session,
            retained_surface_outlives_owner(session),
        )
    }
}

#[cfg(test)]
mod tests;
