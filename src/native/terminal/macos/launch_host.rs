//! Command-created surface host: capture without a TTY, observe binding concurrently,
//! then replace this process with the non-startup shell on the already bound terminal.
use super::bound_host::{BoundHostSpec, run_bound_host};
use super::shell_environment::{self, Captured};
use crate::native::session::launch::{self, host};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{CoreRecord, Store, turn};
use crate::native::terminal::{TerminalKind, TerminalSession};
use anyhow::{Context, Result, bail};
use std::{
    os::unix::process::CommandExt,
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

pub(super) fn word(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("enum serialization")
        .as_str()
        .expect("string enum")
        .into()
}

pub(in crate::native::terminal) fn run(
    directory: &Path,
    command: &str,
    kind: TerminalKind,
) -> Result<()> {
    let name = kind.display_name();
    run_bound_host(directory, &BoundHostSpec { name }, |session_id| {
        let store = Store::open_unchecked(directory);
        let terminal = match kind {
            TerminalKind::Iterm2 => host::Terminal::Iterm2,
            TerminalKind::WezTerm => host::Terminal::Wezterm,
            _ => bail!("unsupported launch host"),
        };
        let prefix = word(terminal);
        let start = Instant::now();
        let receipt = launch::read(&store)?.context("missing host launch receipt")?;
        let token = &receipt.claim_token;
        let remaining = Duration::from_millis(
            receipt
                .deadline_unix_ms
                .saturating_sub(unix_ms())
                .min(u64::MAX as u128) as u64,
        );
        let deadline = start + remaining;
        let identity = super::process::live_native_process_identity(std::process::id())
            .ok()
            .and_then(|i| {
                Some(host::HostIdentity {
                    pid: i.pid,
                    start_seconds: i.process_start_seconds,
                    start_microseconds: i.process_start_microseconds.try_into().ok()?,
                })
            });
        let fail =
            |reason, exit_code, signal, capture_elapsed: Option<Duration>| -> anyhow::Error {
                let elapsed = start.elapsed().as_millis() as u64;
                let _ = host::fail(
                    &store,
                    token,
                    identity,
                    host::Failure {
                        reason,
                        exit_code,
                        signal,
                    },
                    elapsed,
                    capture_elapsed.map(|d| d.as_millis() as u64),
                );
                if let Some(elapsed) = capture_elapsed {
                    launch::log(
                        &store,
                        &format!(
                            "shell_environment_failed reason={} elapsed_ms={}",
                            word(reason),
                            elapsed.as_millis()
                        ),
                    );
                }
                launch::log(
                    &store,
                    &format!("{prefix}_host_refused reason={}", word(reason)),
                );
                anyhow::anyhow!("{name} launch host refused: {}", word(reason))
            };
        let identity = identity
            .ok_or_else(|| fail(host::FailureReason::IdentityUnavailable, None, None, None))?;
        match host::observe(&store, Some(&receipt)) {
            host::HostObservation::Recorded(record) if record.terminal == terminal => {}
            _ => return Err(fail(host::FailureReason::BindingMismatch, None, None, None)),
        }
        if host::admit(&store, token, identity).is_err() {
            // A refused duplicate must not publish failure for the admitted incarnation.
            launch::log(
                &store,
                &format!("{prefix}_host_refused reason=binding_mismatch"),
            );
            bail!("{name} launch host admission refused");
        }
        launch::log(&store, &format!("{prefix}_host_started"));
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_millis(2500))
            .min(Duration::from_secs(10));
        let budget_ms = budget.as_millis() as u64;
        host::advance(
            &store,
            token,
            identity,
            host::Stage::EnvironmentPreparing,
            host::Progress {
                elapsed_ms: start.elapsed().as_millis() as u64,
                capture_budget_ms: budget_ms,
                ..Default::default()
            },
        )?;
        launch::log(
            &store,
            &format!("shell_environment_started budget_ms={budget_ms}"),
        );
        let base: Vec<_> = std::env::vars_os().collect();
        let (send, receive) = mpsc::sync_channel(1);
        // A failed binding returns promptly. The host exits on that failure; the
        // capture supervisor's lifetime pipe then owns cleanup, without joining here.
        std::thread::Builder::new()
            .name("shell-environment".into())
            .spawn(move || {
                let _ = send.send(shell_environment::capture(base, deadline));
            })
            .map_err(|_| {
                fail(
                    host::FailureReason::CaptureSpawnFailed,
                    None,
                    None,
                    Some(Duration::ZERO),
                )
            })?;
        let mut captured: Option<Captured> = None;
        loop {
            if captured.is_none() {
                match receive.try_recv() {
                    Ok(Ok(result)) => {
                        host::advance(
                            &store,
                            token,
                            identity,
                            host::Stage::EnvironmentReady,
                            progress(start, budget_ms, &result),
                        )?;
                        launch::log(
                            &store,
                            &format!(
                                "shell_environment_ready elapsed_ms={} variables={} background_children_stopped={}",
                                result.elapsed.as_millis(),
                                result.variables.len(),
                                result.background_children_stopped
                            ),
                        );
                        captured = Some(result);
                    }
                    Ok(Err(error)) => {
                        return Err(fail(
                            error.reason,
                            error.exit_code,
                            error.signal,
                            Some(error.elapsed),
                        ));
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(fail(
                            host::FailureReason::CaptureSpawnFailed,
                            None,
                            None,
                            Some(start.elapsed()),
                        ));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(fail(host::FailureReason::BindingTimeout, None, None, None));
            }
            let binding = (|| -> Result<Option<bool>> {
                let _lock = store.lock()?;
                eligible(&store, token, identity)?;
                Ok(match store.record(CoreRecord::Terminal).text()? {
                    None => None,
                    Some(text) => {
                        let surface = crate::native::session::record::terminal(&text)
                            .map_err(|_| anyhow::anyhow!("invalid host surface binding"))?;
                        Some(matches_surface(&surface, session_id, kind))
                    }
                })
            })()
            .map_err(|_| fail(host::FailureReason::BindingMismatch, None, None, None))?;
            // A present, mismatched handle is final; absence is still waiting.
            if binding == Some(false) {
                return Err(fail(host::FailureReason::BindingMismatch, None, None, None));
            }
            if binding == Some(true) && captured.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let captured = captured.expect("capture completed before handoff");
        let flushed = {
            let _lock = store.lock()?;
            eligible(&store, token, identity)?;
            unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) == 0 }
        };
        if !flushed {
            return Err(fail(
                host::FailureReason::InputFlushFailed,
                None,
                None,
                None,
            ));
        }
        host::advance(
            &store,
            token,
            identity,
            host::Stage::LaunchShellStarting,
            progress(start, budget_ms, &captured),
        )?;
        launch::log(&store, &format!("{prefix}_host_launch_shell"));
        let error = {
            let _lock = store.lock()?;
            eligible(&store, token, identity)?;
            let mut shell = crate::native::process_env::helper_command("/bin/zsh");
            shell
                .args(["-f", "-i", "-c", command])
                .env_clear()
                .envs(captured.variables);
            for variable in crate::native::process_env::non_provider_removals() {
                shell.env_remove(variable);
            }
            // All nonstandard descriptors close on successful exec, including the
            // lifecycle lock. If exec fails, Rust still owns valid descriptors.
            unsafe {
                shell.pre_exec(|| {
                    for fd in 3..libc::getdtablesize() {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags >= 0
                            && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            shell.exec()
        };
        let _ = error; // Never log free-form child/startup errors.
        Err(fail(
            host::FailureReason::LaunchShellExecFailed,
            None,
            None,
            None,
        ))
    })
}

fn progress(start: Instant, budget: u64, capture: &Captured) -> host::Progress {
    host::Progress {
        elapsed_ms: start.elapsed().as_millis() as u64,
        capture_budget_ms: budget,
        capture_elapsed_ms: Some(capture.elapsed.as_millis() as u64),
        variables: Some(capture.variables.len() as u32),
        background_children_stopped: Some(capture.background_children_stopped),
    }
}

fn eligible(store: &Store, token: &str, identity: host::HostIdentity) -> Result<()> {
    let receipt = launch::read(store)?.context("missing launch receipt")?;
    if receipt.claim_token != token
        || receipt.phase != launch::Phase::Pending
        || unix_ms() >= receipt.deadline_unix_ms
        || store.status()?.state != crate::native::session::SessionState::Launching
        || store.closed_if_present()?.is_some()
        || turn::current_claim_token(store)?.as_deref() != Some(token)
        || !matches!(host::observe(store, Some(&receipt)), host::HostObservation::Recorded(r) if r.host == Some(identity) && r.failure.is_none())
    {
        bail!("launch host is no longer eligible");
    }
    Ok(())
}

fn matches_surface(surface: &TerminalSession, id: &str, kind: TerminalKind) -> bool {
    if surface.verify_managed_session(id).is_err() || surface.kind != kind {
        return false;
    }
    match kind {
        TerminalKind::Iterm2 => std::env::var("ITERM_SESSION_ID")
            .ok()
            .and_then(|s| s.rsplit(':').next().map(str::to_owned))
            .is_some_and(|s| !s.is_empty() && s == surface.id),
        TerminalKind::WezTerm => {
            std::env::var("WEZTERM_PANE").is_ok_and(|s| s == surface.id)
                && std::env::var_os("WEZTERM_UNIX_SOCKET").is_some_and(|s| {
                    surface
                        .wezterm_mux
                        .as_ref()
                        .is_some_and(|m| Path::new(&m.socket) == Path::new(&s))
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
