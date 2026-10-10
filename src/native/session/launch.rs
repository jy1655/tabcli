//! Provider-neutral startup receipt and fencing. A terminal accepting a command is not
//! evidence that its wrapper ran. The lifecycle lock serializes cancellation with spawn.
use crate::native::session::{CoreRecord, Reader, RecordReader, RecordStore};
use crate::native::*;
use std::process::Child;

#[cfg(test)]
// macOS and Windows tests address the launch record by this name; production code uses
// the store.
#[cfg(all(test, any(target_os = "macos", windows)))]
pub(in crate::native) use super::LAUNCH_FILE as FILE;
pub(in crate::native) const LOG: &str = "launch.log";
pub(in crate::native) const STDERR_ENV: &str = "AGENT_BRIDGE_LAUNCH_STDERR_FD";
pub(in crate::native) const STDOUT_ENV: &str = "AGENT_BRIDGE_LAUNCH_STDOUT_FD";
// Retained for older readers and for reading records written before residual_surface.
pub(in crate::native) const RESIDUAL_SURFACE_MARKER: &str =
    "the surface may remain and is not closed by Bridge";
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether cleanup of a surface left by a failed launch has been confirmed. This
/// observation grants no close authority and does not bind a terminal handle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum ResidualSurface {
    Unverified,
    Cleared,
}

impl SessionStatus {
    pub(in crate::native) fn residual_surface(&self) -> Option<ResidualSurface> {
        if self.residual_surface == Some(ResidualSurface::Cleared) {
            return None;
        }
        self.residual_surface.or_else(|| {
            self.error
                .as_deref()
                .is_some_and(|error| error.contains(RESIDUAL_SURFACE_MARKER))
                .then_some(ResidualSurface::Unverified)
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "launch/binding_tests.rs"]
pub(in crate::native) mod binding_tests;

#[cfg(target_os = "macos")]
fn binding_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = binding_tests::now() {
        return now;
    }
    Instant::now()
}

#[cfg(target_os = "macos")]
fn binding_sleep() {
    let delay = Duration::from_millis(10);
    #[cfg(test)]
    if binding_tests::retry(delay) {
        return;
    }
    thread::sleep(delay);
}

pub(in crate::native) fn install_script(store: &Store, contents: &str) -> Result<String> {
    let directory = store.directory();
    // A new terminal can still be in canonical input mode while its shell starts. A
    // long write-text command was truncated there in macOS LIVE (#50). Send only a
    // quoted script path; source it in the original shell to preserve owner/TTY identity.
    #[cfg(unix)]
    {
        let path = directory.join("launch.sh");
        RecordStore::at(&path).write_private(contents.as_bytes())?;
        Ok(format!(". {}", shell_quote(path.as_os_str())))
    }
    #[cfg(not(unix))]
    {
        // Windows passes -Command directly to its console process, not through a TTY
        // input buffer. Keep it native; sourcing a .ps1 would add an execution-policy gate.
        let _ = directory;
        Ok(contents.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum Phase {
    Pending,
    // Durable before spawn: a crash here cannot prove that no child exists.
    Spawning,
    Spawned,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct Record {
    pub(in crate::native) schema: u32,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) deadline_unix_ms: u128,
    pub(in crate::native) phase: Phase,
}

pub(in crate::native) fn read(reader: &Reader) -> Result<Option<Record>> {
    let record = reader
        .record(CoreRecord::Launch)
        .text()?
        .map(|text| serde_json::from_str::<Record>(&text))
        .transpose()
        .context("invalid launch receipt")?;
    if let Some(record) = &record
        && (record.schema != 1 || record.claim_token.is_empty())
    {
        bail!("invalid launch receipt identity");
    }
    Ok(record)
}

/// The existing macOS launch hosts have distinct diagnostics and two wait budgets.
/// Keeping their selection here leaves adapters responsible only for surface identity.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
pub(in crate::native) enum BindingHost {
    Iterm2,
    AppleTerminal,
    WezTerm,
}

/// Observe a pending launch until its managed surface is bound. This is an ordered,
/// unlocked observation, not authorization to spawn; `spawn` owns that final fence.
#[cfg(target_os = "macos")]
pub(in crate::native) fn wait_for_binding(
    reader: &Reader,
    session_id: &str,
    host: BindingHost,
) -> Result<terminal::TerminalSession> {
    let (name, cancelled) = match host {
        BindingHost::Iterm2 => (
            "iTerm2",
            "iTerm2 launch was cancelled or timed out before surface binding",
        ),
        BindingHost::AppleTerminal => (
            "Terminal.app",
            "Terminal.app launch was cancelled or timed out before its surface was bound",
        ),
        BindingHost::WezTerm => (
            "WezTerm",
            "WezTerm launch was cancelled or timed out before surface binding",
        ),
    };
    let initial = read(reader)?.with_context(|| format!("missing {name} launch receipt"))?;
    #[cfg(test)]
    binding_tests::checkpoint(binding_tests::Point::InitialReceipt);
    // Preserve both budgets: after wall-clock rollback or a receipt extension,
    // Terminal.app may continue beyond the initial receipt's remaining duration.
    let budget = match host {
        BindingHost::AppleTerminal => START_TIMEOUT,
        BindingHost::Iterm2 | BindingHost::WezTerm => Duration::from_millis(
            initial
                .deadline_unix_ms
                .saturating_sub(unix_ms())
                .min(30_000) as u64,
        ),
    };
    let deadline = binding_now() + budget;
    #[cfg(test)]
    binding_tests::checkpoint(binding_tests::Point::BudgetCaptured);
    loop {
        let record = read(reader)?.with_context(|| format!("missing {name} launch receipt"))?;
        // Status is read even after expiry. Keep claim and binding reads lazy: these
        // short circuits also determine which damaged-record error the host reports.
        let status: SessionStatus = reader.status()?;
        if binding_now() >= deadline
            || unix_ms() >= record.deadline_unix_ms
            || record.phase != Phase::Pending
            || record.claim_token != initial.claim_token
            || status.state != SessionState::Launching
            || turn::current_claim_token(reader)?.as_deref() != Some(initial.claim_token.as_str())
        {
            bail!("{cancelled}");
        }
        #[cfg(test)]
        binding_tests::checkpoint(binding_tests::Point::BeforeBinding);
        if let Some(text) = reader.record(CoreRecord::Terminal).text()? {
            let surface: terminal::TerminalSession = serde_json::from_str(&text)
                .with_context(|| format!("invalid {name} surface binding"))?;
            surface.verify_managed_session(session_id)?;
            return Ok(surface);
        }
        binding_sleep();
    }
}

pub(in crate::native) fn log(store: &Store, message: &str) {
    let directory = store.directory();
    // Diagnostics are auxiliary. A missing/unwritable log must never stop valid input.
    let path = Reader::open_unchecked(directory)
        .private(LOG)
        .path()
        .to_owned();
    let _ = (|| -> Result<()> {
        if !path.exists() {
            let _ = RecordStore::at(&path).write_private(b"");
        }
        if !RecordReader::at(&path).is_regular_file()? {
            return Ok(());
        }
        // One write per line: the launcher and the wrapper append at the same time.
        RecordStore::at(&path).append(|| format!("{} {message}\n", unix_ms()).into_bytes())?;
        Ok(())
    })();
}

pub(in crate::native) fn begin(
    store: &Store,
    token: &str,
    request_deadline: Instant,
) -> Result<Instant> {
    let directory = store.directory();
    let budget = request_deadline
        .saturating_duration_since(Instant::now())
        .min(START_TIMEOUT);
    let deadline = Instant::now() + budget;
    let _lock = Store::open_unchecked(directory).lock()?;
    Store::open_unchecked(directory)
        .record(CoreRecord::Launch)
        .write_json(&Record {
            schema: 1,
            claim_token: token.to_owned(),
            deadline_unix_ms: unix_ms() + budget.as_millis(),
            phase: Phase::Pending,
        })?;
    log(store, "launch_pending; waiting for provider spawn");
    Ok(deadline)
}

fn fail_locked(store: &Store, record: &Record, reason: &str) -> Result<()> {
    fail_locked_with_residual(store, record, reason, None)
}

fn fail_locked_with_residual(
    store: &Store,
    record: &Record,
    reason: &str,
    residual_surface: Option<ResidualSurface>,
) -> Result<()> {
    let directory = store.directory();
    if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
        != Some(&record.claim_token)
    {
        return Ok(());
    }
    let status: SessionStatus = store.status()?;
    if status.state != SessionState::Launching {
        if status.state == SessionState::Failed && record.phase == Phase::Pending {
            remove_turn_claim_locked(
                Reader::open_unchecked(directory)
                    .record(CoreRecord::TurnClaim)
                    .path(),
            )?;
        }
        return Ok(());
    }
    let reason = if record.phase == Phase::Spawning {
        format!("{reason}; provider spawn is uncertain; the claim is retained, do not resend")
    } else {
        reason.to_owned()
    };
    update_status_with_residual(
        directory,
        SessionState::Failed,
        None,
        Some(reason.clone()),
        residual_surface,
    )?;
    if record.phase == Phase::Pending {
        remove_turn_claim_locked(
            Reader::open_unchecked(directory)
                .record(CoreRecord::TurnClaim)
                .path(),
        )?;
    }
    log(store, &reason);
    Ok(())
}

// The plain form, which the tests of this module and of close use.
#[cfg(test)]
pub(in crate::native) fn fail(store: &Store, reason: &str) -> Result<()> {
    fail_deciding(store, reason, |_, reason| Ok(reason.to_owned()))
}

// `fail`, with the recorded wording decided under the lock from the launch record
// and the other records as they are at that moment.
fn fail_deciding(
    store: &Store,
    reason: &str,
    wording: impl FnOnce(&Record, &str) -> Result<String>,
) -> Result<()> {
    let Some(_lock) = store.try_lock()? else {
        bail!("{reason}; provider startup is in progress; the claim is retained");
    };
    if let Some(record) = read(store)?
        && record.phase != Phase::Spawned
    {
        let reason = wording(&record, reason)?;
        fail_locked(store, &record, &reason)?;
    }
    Ok(())
}

// The launcher's terminal-creation failure handoff.
pub(in crate::native) fn terminal_failed(store: &Store, error: &anyhow::Error) -> Result<()> {
    let reason = format!("terminal launch failed: {error:#}");
    let Some(retained) = error.downcast_ref::<terminal::RetainedLaunchSurface>() else {
        // Only a failure before a surface was bound (a close in progress moves the handle
        // to the closing record and may give it back) and before the provider spawn began
        // proves that nothing was sent. A bound surface keeps its handle, and a spawn in
        // progress has its own wording. Decided under the lock, on the records as they are.
        return fail_deciding(store, &reason, |record, reason| {
            let reader = Reader::open_unchecked(store.directory());
            let unbound = reader.record(CoreRecord::Terminal).text()?.is_none()
                && reader.record(CoreRecord::TerminalClosing).text()?.is_none();
            Ok(if unbound && record.phase == Phase::Pending {
                format!("{reason}; no surface handle was recorded and no prompt was sent")
            } else {
                reason.to_owned()
            })
        });
    };
    let _lock = store.lock()?;
    let status = store.status()?;
    let record = read(store)?;
    let claim = turn::current_claim_token(store)?;
    let owns_claim = record
        .as_ref()
        .is_some_and(|record| claim.as_deref() == Some(record.claim_token.as_str()));
    if status.state != SessionState::Launching
        || !owns_claim
        || store.closed_if_present()?.is_some()
    {
        let disposition =
            if status.state == SessionState::Closed || store.closed_if_present()?.is_some() {
                "the session was closed during the launch"
            } else {
                "the session is no longer owned by this launch"
            };
        let message = format!(
            "{disposition}; exact retained surface={:?}; {RESIDUAL_SURFACE_MARKER}; {reason}",
            retained.surface
        );
        log(store, &message);
        // Diagnostic-only amendment: never bind or reopen a cancelled launch. The
        // tombstone must carry the warning too, or later status convergence erases it.
        // Preserve state, generation, timestamps and exit code under the status lock.
        store
            .record_residual_surface(&message)
            .with_context(|| message.clone())?;
        bail!("{message}");
    }
    let mut surface = retained.surface.clone();
    surface.managed_session_id = Some(
        store
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .context("session directory has no session id")?
            .to_owned(),
    );
    let persisted = store.write_terminal(&surface);
    let residual =
        (retained.unverified_cleanup || persisted.is_err()).then_some(ResidualSurface::Unverified);
    let reason = if residual.is_some() && !reason.contains(RESIDUAL_SURFACE_MARKER) {
        format!("{reason}; {RESIDUAL_SURFACE_MARKER}")
    } else {
        reason
    };
    // Even a failed record write must leave the launch failure and exact surface in
    // status.error. Return the write error as well; never claim persistence succeeded.
    if let Some(record) = record {
        fail_locked_with_residual(store, &record, &reason, residual)?;
    }
    persisted.context("failed to persist the retained launch surface")
}

fn confirmed(reader: &Reader, record: &Record, status: &SessionStatus) -> bool {
    record.phase == Phase::Spawned
        || (record.phase == Phase::Spawning
            && matches!(
                status.state,
                SessionState::Running
                    | SessionState::AwaitingInitialInput
                    | SessionState::Working
                    | SessionState::Ready
            )
            && reader.provider_process().is_ok_and(|p| {
                reader.directory().file_name().and_then(|n| n.to_str())
                    == Some(p.managed_session_id.as_str())
            }))
}

/// Called before old dead-owner repair. A new launch with an uncertain spawn must not
/// be silently closed/released by the legacy owner-only repair path.
pub(in crate::native) fn repair(store: &Store) -> Result<bool> {
    let Some(record) = read(store)? else {
        return Ok(false);
    };
    let status: SessionStatus = store.status()?;
    if confirmed(store, &record, &status) {
        return Ok(false);
    }
    let Some(_lock) = store.try_lock()? else {
        return Ok(true);
    };
    let Some(record) = read(store)? else {
        return Ok(false);
    };
    let status: SessionStatus = store.status()?;
    if confirmed(store, &record, &status) {
        return Ok(false);
    }
    if status.state == SessionState::Closed {
        return Ok(false);
    }
    if status.state == SessionState::Launching {
        let owner = super::observe_owner(store);
        if unix_ms() >= record.deadline_unix_ms {
            fail_locked(
                store,
                &record,
                "provider launch timed out before startup was confirmed",
            )?;
        } else if owner.process_alive == Some(false) || owner.identity_matches == Some(false) {
            fail_locked(
                store,
                &record,
                "native launch wrapper exited before startup was confirmed",
            )?;
        }
    } else if status.state == SessionState::Failed {
        fail_locked(
            store,
            &record,
            status.error.as_deref().unwrap_or("provider launch failed"),
        )?;
    }
    Ok(true)
}

pub(in crate::native) fn wait(
    store: &Store,
    surface: &terminal::TerminalSession,
    deadline: Instant,
) -> Result<()> {
    let mut next_probe = Instant::now() + Duration::from_secs(1);
    loop {
        let status: SessionStatus = store.status()?;
        if matches!(
            status.state,
            SessionState::Failed | SessionState::Closed | SessionState::Exited
        ) {
            bail!(
                "{}",
                status
                    .error
                    .unwrap_or_else(|| format!("launch entered {}", status.state))
            );
        }
        if read(store)?.is_some_and(|record| confirmed(store, &record, &status)) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let reason = if remaining.is_zero() {
            Some("provider launch timed out before startup was confirmed")
        } else if Instant::now() >= next_probe {
            next_probe = Instant::now() + Duration::from_secs(1);
            match terminal::surface_present(surface, remaining.min(Duration::from_secs(2))) {
                Ok(false) => {
                    Some("terminal surface disappeared before provider launch was confirmed")
                }
                // Inspection errors (including permissions/timeouts) do not prove absence.
                Ok(true) | Err(_) => None,
            }
        } else {
            None
        };
        if let Some(reason) = reason {
            let Some(_lock) = store.try_lock()? else {
                bail!(
                    "{reason}; startup is still holding the lifecycle lock, execution is uncertain; the claim is retained"
                );
            };
            let record = read(store)?.context("launch receipt disappeared")?;
            let status: SessionStatus = store.status()?;
            if confirmed(store, &record, &status) {
                return Ok(());
            }
            fail_locked(store, &record, reason)?;
            let status: SessionStatus = store.status()?;
            bail!("{}", status.error.as_deref().unwrap_or(reason));
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn check_spawn_locked(store: &Store, record: Option<&Record>) -> Result<()> {
    let directory = store.directory();
    let status: SessionStatus = store.status()?;
    if status.state != SessionState::Launching
        || Reader::open_unchecked(directory)
            .record(CoreRecord::Closed)
            .path()
            .to_owned()
            .exists()
    {
        bail!("provider launch cancelled: session is {}", status.state);
    }
    if let Some(record) = record {
        if record.phase != Phase::Pending {
            bail!("provider launch already attempted; refusing a second spawn");
        }
        if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
            != Some(&record.claim_token)
        {
            bail!("provider launch claim no longer belongs to this wrapper");
        }
        if unix_ms() >= record.deadline_unix_ms {
            bail!("provider launch deadline expired before spawn");
        }
    }
    Ok(())
}

pub(in crate::native) fn spawn<F>(
    store: &Store,
    command: &mut Command,
    record_process: F,
) -> Result<Child>
where
    F: FnOnce(&mut Child) -> Result<()>,
{
    let directory = store.directory();
    let _lock = Store::open_unchecked(directory).lock()?;
    let mut record = read(store)?;
    check_spawn_locked(store, record.as_ref())?;
    if let Some(record) = &mut record {
        record.phase = Phase::Spawning;
        Store::open_unchecked(directory)
            .record(CoreRecord::Launch)
            .write_json(record)?;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // Command::spawn returning Err establishes that it started no provider.
            if let Some(record) = &mut record {
                record.phase = Phase::Pending;
                Store::open_unchecked(directory)
                    .record(CoreRecord::Launch)
                    .write_json(record)?;
            }
            return Err(error).context("failed to spawn provider process");
        }
    };
    if let Err(error) = record_process(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        // The child existed and could already have consumed its argument prompt. Keep
        // Spawning even after cleanup; missing durable evidence is never a retry permit.
        return Err(error);
    }
    if let Some(record) = &mut record {
        record.phase = Phase::Spawned;
        // The authoritative process record is already durable. An auxiliary handshake
        // write must not kill a successfully started provider; readers also see its record.
        if let Err(error) = Store::open_unchecked(directory)
            .record(CoreRecord::Launch)
            .write_json(record)
        {
            log(
                store,
                &format!("launch receipt update failed after spawn: {error:#}"),
            );
        }
    }
    log(store, &format!("provider_spawned pid={}", child.id()));
    Ok(child)
}

pub(in crate::native) fn uncertain(reader: &Reader) -> bool {
    read(reader)
        .ok()
        .flatten()
        .is_some_and(|record| record.phase == Phase::Spawning)
}

/// Called with the read-only snapshot's lifecycle lock held.
pub(in crate::native) fn diagnostic(
    record: Option<&Record>,
    status: &SessionStatus,
    claim: Option<&str>,
) -> Option<(&'static str, String)> {
    let record = record?;
    if record.phase == Phase::Spawned {
        return None;
    }
    if status.state == SessionState::Failed {
        return Some((
            if record.phase == Phase::Spawning {
                "launch_uncertain"
            } else {
                "launch_failed"
            },
            status
                .error
                .clone()
                .unwrap_or_else(|| "provider launch failed".to_owned()),
        ));
    }
    if status.state == SessionState::Launching
        && claim == Some(&record.claim_token)
        && unix_ms() >= record.deadline_unix_ms
    {
        return Some(("launch_timeout", "provider launch deadline expired; run sessions for this workspace to settle startup, then inspect the same request; do not resend".to_owned()));
    }
    None
}

/// The shell redirects only Bridge's stderr. The provider keeps the original terminal
/// stderr, so logging does not turn its interactive surface into a pipe or a file.
pub(in crate::native) fn provider_stderr(command: &mut Command) {
    command.env_remove(STDERR_ENV);
    command.env_remove(STDOUT_ENV);
    #[cfg(unix)]
    if std::env::var(STDERR_ENV).as_deref() == Ok("3") {
        use std::os::fd::FromRawFd;
        // SAFETY: dup produces a new owned fd; a failed dup is not adopted.
        let fd = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
        if fd >= 0 {
            command.stderr(unsafe { File::from_raw_fd(fd) });
        }
    }
}

pub(in crate::native) fn restore_stdout() -> Result<()> {
    #[cfg(unix)]
    if std::env::var(STDOUT_ENV).as_deref() == Ok("4") {
        // SAFETY: the launch shell supplies fd 4 as its original stdout. This runs at
        // entry, before monitors/threads exist; provider stdout must remain the real TTY.
        if unsafe { libc::dup2(4, libc::STDOUT_FILENO) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to restore launch terminal stdout");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn fixture() -> (tempfile::TempDir, String) {
        let directory = tempfile::Builder::new()
            .prefix("session-")
            .tempdir()
            .unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        begin(
            &Store::open_unchecked(directory.path()),
            &token,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        (directory, token)
    }

    fn child_command() -> Command {
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("cmd.exe");
            c.args(["/c", "pause"]);
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "read _"]);
            c
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn record_started(directory: &Path, child: &mut Child) -> Result<()> {
        let id = directory.file_name().unwrap().to_str().unwrap();
        record_provider_process(directory, id, child)?;
        update_status(directory, SessionState::Running, None, None)
    }

    #[test]
    fn unproven_terminal_failure_records_no_handle_and_no_prompt() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        terminal_failed(&store, &anyhow::anyhow!("iTerm2 automation timed out")).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some(
                "terminal launch failed: iTerm2 automation timed out; no surface handle was recorded and no prompt was sent"
            )
        );
        assert_eq!(status.residual_surface(), None);
        assert!(store.terminal().is_err());
    }

    #[test]
    fn bound_surface_or_spawn_in_progress_keeps_its_own_failure_wording() {
        // A surface bound before the failure keeps its handle, so the sentence is absent.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let surface: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
        }))
        .unwrap();
        store.write_terminal(&surface).unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("terminal launch failed: Ghostty start failed")
        );
        assert!(store.terminal().is_ok());

        // A handle that a close moved to the closing record is still a handle.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        store
            .record(CoreRecord::TerminalClosing)
            .write_json(&surface)
            .unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        assert_eq!(
            store.status().unwrap().error.as_deref(),
            Some("terminal launch failed: Ghostty start failed")
        );
        assert!(
            store
                .record(CoreRecord::TerminalClosing)
                .text()
                .unwrap()
                .is_some()
        );

        // A spawn in progress may have carried the prompt: the spawn wording stays alone.
        let (directory, token) = fixture();
        let store = Store::open_unchecked(directory.path());
        let mut record = read(&store).unwrap().unwrap();
        assert_eq!(record.claim_token, token);
        record.phase = Phase::Spawning;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        let error = store.status().unwrap().error.unwrap();
        assert!(error.contains("provider spawn is uncertain"), "{error}");
        assert!(!error.contains("no prompt was sent"), "{error}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn retained_surface_record_failure_is_durable() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let surface = serde_json::from_value(serde_json::json!({
            "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
        }))
        .unwrap();
        let error =
            terminal::RetainedLaunchSurface::new(surface, "exact cleanup failed".to_owned()).into();
        let handle = store.record(CoreRecord::Terminal).path().to_owned();
        fs::create_dir(&handle).unwrap();
        assert!(terminal_failed(&store, &error).is_err());
        fs::remove_dir(&handle).unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert_eq!(
            store.status().unwrap().residual_surface(),
            Some(ResidualSurface::Unverified)
        );
        session::close::close(&store, None, |_| {
            panic!("no persisted handle grants authority")
        })
        .unwrap();
        assert_eq!(
            store.status().unwrap().residual_surface(),
            Some(ResidualSurface::Unverified)
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn unbound_residual_survives_changed_diagnostics_and_repeated_close() {
        for closed_first in [false, true] {
            let (directory, _) = fixture();
            let store = Store::open_unchecked(directory.path());
            fail(&store, "launch cancelled").unwrap();
            if closed_first {
                session::close::close(&store, None, |_| panic!("no surface is bound")).unwrap();
            }
            let surface = serde_json::from_value(serde_json::json!({
                "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
            }))
            .unwrap();
            let error =
                terminal::RetainedLaunchSurface::new(surface, "late failure".to_owned()).into();
            assert!(terminal_failed(&store, &error).is_err());
            let mut status = store.status().unwrap();
            assert_eq!(status.residual_surface, Some(ResidualSurface::Unverified));
            status.error = Some("new diagnostic wording".to_owned());
            store.write_status(&status).unwrap();
            if closed_first {
                store.write_closed(&status).unwrap();
                // Simulate interruption after the tombstone amendment but before the
                // status amendment. Close convergence must restore the typed fact.
                status.residual_surface = None;
                store.write_status(&status).unwrap();
                store.converge().unwrap();
                assert_eq!(
                    store.status().unwrap().residual_surface,
                    Some(ResidualSurface::Unverified)
                );
            }
            for _ in 0..2 {
                session::close::close(&store, None, |_| {
                    panic!("residual evidence grants no authority")
                })
                .unwrap();
                let status = store.status().unwrap();
                assert_eq!(status.state, SessionState::Closed);
                assert_eq!(status.residual_surface(), Some(ResidualSurface::Unverified));
                assert_eq!(status.error.as_deref(), Some("new diagnostic wording"));
                assert_eq!(
                    store.closed_if_present().unwrap().unwrap().residual_surface,
                    Some(ResidualSurface::Unverified)
                );
                assert!(!store.record(CoreRecord::Terminal).path().exists());
            }
        }
    }

    #[test]
    fn cancelled_launch_cannot_spawn_late() {
        let (directory, _) = fixture();
        fail(&Store::open_unchecked(directory.path()), "launch timed out").unwrap();
        let error = spawn(
            &Store::open_unchecked(directory.path()),
            &mut child_command(),
            |_| panic!("cancelled provider spawned"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!directory.path().join(PROVIDER_PROCESS_FILE).exists());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[test]
    fn timeout_cannot_release_a_spawn_in_progress() {
        let (directory, token) = fixture();
        let path = directory.path().to_path_buf();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            spawn(
                &Store::open_unchecked(&path),
                &mut child_command(),
                |child| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    record_started(&path, child)
                },
            )
            .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(fail(&Store::open_unchecked(directory.path()), "launch timed out").is_err());
        assert_eq!(
            current_turn_claim_token(directory.path()).unwrap(),
            Some(token)
        );
        assert_eq!(
            read_json::<SessionStatus>(&directory.path().join("status.json"))
                .unwrap()
                .state
                .as_str(),
            "launching"
        );
        release_tx.send(()).unwrap();
        let mut child = worker.join().unwrap();
        assert_eq!(
            read(&Reader::open_unchecked(directory.path()))
                .unwrap()
                .unwrap()
                .phase,
            Phase::Spawned
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn unwritable_auxiliary_log_does_not_block_provider_start() {
        let (directory, _) = fixture();
        fs::remove_file(directory.path().join(LOG)).unwrap();
        fs::create_dir(directory.path().join(LOG)).unwrap();
        let mut child = spawn(
            &Store::open_unchecked(directory.path()),
            &mut child_command(),
            |child| record_started(directory.path(), child),
        )
        .unwrap();
        assert_eq!(
            read(&Reader::open_unchecked(directory.path()))
                .unwrap()
                .unwrap()
                .phase,
            Phase::Spawned
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn failed_spawn_does_not_claim_a_process_was_started() {
        let (directory, _) = fixture();
        let error = spawn(
            &Store::open_unchecked(directory.path()),
            &mut Command::new(directory.path().join("missing-provider")),
            |_| unreachable!(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("spawn"));
        assert_eq!(
            read(&Reader::open_unchecked(directory.path()))
                .unwrap()
                .unwrap()
                .phase,
            Phase::Pending
        );
        finalize_native_session(directory.path(), &Err(error)).unwrap();
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn shell_captures_bootstrap_output_and_exit_without_requiring_logging() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-shell");
        fs::create_dir(&directory).unwrap();
        let executable = root.path().join("bridge ' ; stub");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf 'wrapper stdout\\n'\nprintf 'wrapper stderr\\n' >&2\nexit 7\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let command =
            bridge_shell_command(root.path(), root.path(), &executable, "session-shell").unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        let log = fs::read_to_string(directory.join(LOG)).unwrap();
        for part in ["wrapper stdout", "wrapper stderr", "wrapper_exit_code=7"] {
            assert!(log.contains(part));
        }
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        fs::remove_file(directory.join(LOG)).unwrap();
        fs::create_dir(directory.join(LOG)).unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&output.stdout).contains("wrapper stdout"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("wrapper stderr"));
    }

    // Runs the bootstrap through the command line that the console launch itself builds.
    // 0.0.8 put a double-quoted string into the bootstrap, which that command line cannot
    // carry, so every launch on native Windows was refused before a console existed.
    #[cfg(windows)]
    #[test]
    fn console_launch_carries_the_bootstrap_and_records_the_wrapper_exit_code() {
        use std::os::windows::process::CommandExt;
        // The stub is a batch file, and cmd.exe cannot start in a verbatim (`\\?\`) path, so
        // the temporary directory is used as given.
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-shell");
        fs::create_dir(&directory).unwrap();
        let executable = root.path().join("bridge owner's stub.cmd");
        fs::write(&executable, "@exit /b 7\r\n").unwrap();
        let command =
            bridge_shell_command(root.path(), root.path(), &executable, "session-shell").unwrap();
        let powershell = terminal::windows_powershell_executable().unwrap();
        let command_line =
            terminal::windows_console_launch_command_line(&powershell, &command).unwrap();
        let arguments = command_line
            .strip_prefix(&format!("\"{}\" ", powershell.to_string_lossy()))
            .unwrap();
        let output = Command::new(&powershell)
            .raw_arg(arguments)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(7),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fs::read_to_string(directory.join(LOG))
                .unwrap()
                .contains("wrapper_exit_code=7")
        );
    }

    #[cfg(unix)]
    #[test]
    fn long_bootstrap_is_sourced_from_a_private_file_through_a_short_command() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let script = format!("# {}\nprintf 'SCRIPT_OK\\n'\nexit 3", "x".repeat(8192));
        let command = install_script(&Store::open_unchecked(directory.path()), &script).unwrap();
        assert!(command.len() < 512);
        assert_eq!(
            fs::read_to_string(directory.path().join("launch.sh")).unwrap(),
            script
        );
        assert_eq!(
            fs::metadata(directory.path().join("launch.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "SCRIPT_OK\n");
    }
}
