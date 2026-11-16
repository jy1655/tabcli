//! Command-created surface host: capture without a TTY, observe binding concurrently,
//! then replace this process with the non-startup shell on the already bound terminal.
//! `run` performs the OS acts (identity, the capture thread, the input flush and the exec);
//! `await_handoff` decides, on the clock and two ports (the capture result and the binding
//! observation), whether the host keeps waiting, refuses or hands off.
use super::bound_host::{BoundHostSpec, run_bound_host};
use super::shell_environment::{self, CaptureFailure, Captured};
use crate::native::clock::{Clock, SystemClock};
use crate::native::session::Store;
use crate::native::session::launch::{self, host};
use crate::native::terminal::shell_environment_budget::capture_budget;
use crate::native::terminal::{TerminalKind, TerminalSession};
use anyhow::{Context, Result, bail};
use std::{
    os::unix::process::CommandExt,
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

/// The host's wait between two observations of the capture and the binding.
const POLL: Duration = Duration::from_millis(10);

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
        if !kind.uses_launch_host() {
            bail!("unsupported launch host");
        }
        let prefix = kind.as_str();
        let mut clock = SystemClock;
        let start = clock.now();
        let receipt = launch::read(&store)?.context("missing host launch receipt")?;
        let token = &receipt.claim_token;
        // `start` was read before the wall clock, as the host always has.
        let deadline = launch::saturating_deadline(start, receipt.remaining(Duration::ZERO));
        let identity = super::process::live_native_process_identity(std::process::id())
            .ok()
            .and_then(|i| {
                Some(host::HostIdentity {
                    pid: i.pid,
                    start_seconds: i.process_start_seconds,
                    start_microseconds: i.process_start_microseconds.try_into().ok()?,
                })
            });
        let recorder = Recorder {
            store: &store,
            token,
            kind,
            identity,
            start,
        };
        let identity = identity.ok_or_else(|| {
            recorder.refuse(
                Refusal::host(host::FailureReason::IdentityUnavailable),
                clock.now(),
            )
        })?;
        match host::observe(&store, Some(&receipt)) {
            host::HostObservation::Recorded(record) if record.terminal == kind => {}
            _ => {
                return Err(recorder.refuse(
                    Refusal::host(host::FailureReason::BindingMismatch),
                    clock.now(),
                ));
            }
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
        // The host's estimate, which the preparing record and the launch log carry. The capture
        // decides the budget it enforces when it starts, a little later, and the next record
        // carries that one.
        let budget_ms = capture_budget(clock.now(), deadline).as_millis() as u64;
        host::advance(
            &store,
            token,
            identity,
            host::Stage::EnvironmentPreparing,
            host::Progress {
                elapsed_ms: recorder.elapsed(clock.now()).as_millis() as u64,
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
                recorder.refuse(
                    Refusal {
                        reason: host::FailureReason::CaptureSpawnFailed,
                        exit_code: None,
                        signal: None,
                        capture_elapsed: Some(Duration::ZERO),
                        capture_budget: None,
                    },
                    clock.now(),
                )
            })?;
        let captured = await_handoff(
            &mut clock,
            start,
            deadline,
            &mut ChannelCapture(receive),
            &mut LockedBinding {
                store: &store,
                token,
                identity,
                session_id,
                kind,
            },
            &mut |captured, now| recorder.ready(identity, captured, now),
        )?
        .map_err(|refusal| recorder.refuse(refusal, clock.now()))?;
        let flushed = {
            let _lock = store.lock()?;
            host::eligible_incarnation(&store, token, identity)?;
            unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) == 0 }
        };
        if !flushed {
            return Err(recorder.refuse(
                Refusal::host(host::FailureReason::InputFlushFailed),
                clock.now(),
            ));
        }
        host::advance(
            &store,
            token,
            identity,
            host::Stage::LaunchShellStarting,
            progress(recorder.elapsed(clock.now()), &captured),
        )?;
        launch::log(&store, &format!("{prefix}_host_launch_shell"));
        let error = {
            let _lock = store.lock()?;
            host::eligible_incarnation(&store, token, identity)?;
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
        Err(recorder.refuse(
            Refusal::host(host::FailureReason::LaunchShellExecFailed),
            clock.now(),
        ))
    })
}

fn progress(elapsed: Duration, capture: &Captured) -> host::Progress {
    host::Progress {
        elapsed_ms: elapsed.as_millis() as u64,
        capture_budget_ms: capture.budget.as_millis() as u64,
        capture_elapsed_ms: Some(capture.elapsed.as_millis() as u64),
        variables: Some(capture.variables.len() as u32),
        background_children_stopped: Some(capture.background_children_stopped),
    }
}

/// Why the host refuses, as its failure record keeps it.
#[derive(Debug, Eq, PartialEq)]
struct Refusal {
    reason: host::FailureReason,
    exit_code: Option<i32>,
    signal: Option<i32>,
    /// How long the capture ran when the capture ends the launch; the launch log names it then.
    capture_elapsed: Option<Duration>,
    /// The budget the capture enforced, when it answered with its failure.
    capture_budget: Option<Duration>,
}

impl Refusal {
    /// A refusal of the host's own step, which carries no capture facts.
    fn host(reason: host::FailureReason) -> Self {
        Self {
            reason,
            exit_code: None,
            signal: None,
            capture_elapsed: None,
            capture_budget: None,
        }
    }
}

/// The host's writes for its launch: the host record through `session::launch::host`, then
/// the launch log. Durations are measured from `start` on the host's clock.
struct Recorder<'a> {
    store: &'a Store,
    token: &'a str,
    kind: TerminalKind,
    /// `None` only while the host has not identified itself.
    identity: Option<host::HostIdentity>,
    start: Instant,
}

impl Recorder<'_> {
    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.start)
    }

    /// Records `refusal` as the host's failure, best effort, and returns the error the host
    /// exits with.
    fn refuse(&self, refusal: Refusal, now: Instant) -> anyhow::Error {
        let Refusal {
            reason,
            exit_code,
            signal,
            capture_elapsed,
            capture_budget,
        } = refusal;
        let elapsed = self.elapsed(now).as_millis() as u64;
        let _ = host::fail(
            self.store,
            self.token,
            self.identity,
            host::Failure {
                reason,
                exit_code,
                signal,
            },
            elapsed,
            capture_elapsed.map(|d| d.as_millis() as u64),
            capture_budget.map(|d| d.as_millis() as u64),
        );
        if let Some(elapsed) = capture_elapsed {
            launch::log(
                self.store,
                &format!(
                    "shell_environment_failed reason={} elapsed_ms={}",
                    word(reason),
                    elapsed.as_millis()
                ),
            );
        }
        launch::log(
            self.store,
            &format!(
                "{}_host_refused reason={}",
                self.kind.as_str(),
                word(reason)
            ),
        );
        anyhow::anyhow!(
            "{} launch host refused: {}",
            self.kind.display_name(),
            word(reason)
        )
    }

    /// Records the prepared environment, with the budget the capture enforced: the next stage,
    /// then the launch log line. A failed record write ends the host without a failure record.
    fn ready(&self, identity: host::HostIdentity, captured: &Captured, now: Instant) -> Result<()> {
        host::advance(
            self.store,
            self.token,
            identity,
            host::Stage::EnvironmentReady,
            progress(self.elapsed(now), captured),
        )?;
        launch::log(
            self.store,
            &format!(
                "shell_environment_ready elapsed_ms={} variables={} background_children_stopped={}",
                captured.elapsed.as_millis(),
                captured.variables.len(),
                captured.background_children_stopped
            ),
        );
        Ok(())
    }
}

/// What the capture has answered so far.
enum CapturePoll {
    Running,
    Finished(Result<Captured, CaptureFailure>),
    /// The capture thread ended without an answer.
    Gone,
}

/// The capture result port.
trait CaptureResult {
    fn poll(&mut self) -> CapturePoll;
}

/// Production: the capture thread's channel, read without blocking.
struct ChannelCapture(mpsc::Receiver<Result<Captured, CaptureFailure>>);

impl CaptureResult for ChannelCapture {
    fn poll(&mut self) -> CapturePoll {
        match self.0.try_recv() {
            Ok(result) => CapturePoll::Finished(result),
            Err(mpsc::TryRecvError::Empty) => CapturePoll::Running,
            Err(mpsc::TryRecvError::Disconnected) => CapturePoll::Gone,
        }
    }
}

/// The host's observation of its Session's bound surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Binding {
    /// No surface is bound yet.
    Absent,
    /// The bound surface names this host's own terminal surface.
    Host,
    /// The bound surface names another surface.
    Other,
    /// This incarnation may no longer act for the launch, or the binding cannot be read.
    Unavailable,
}

/// The surface observation port.
trait BindingObservation {
    fn observe(&mut self) -> Binding;
}

/// Production: under the lifecycle lock, whether this incarnation may still act, then the
/// bound surface and whether it names this host.
struct LockedBinding<'a> {
    store: &'a Store,
    token: &'a str,
    identity: host::HostIdentity,
    session_id: &'a str,
    kind: TerminalKind,
}

impl BindingObservation for LockedBinding<'_> {
    fn observe(&mut self) -> Binding {
        let binding = (|| -> Result<Option<bool>> {
            let _lock = self.store.lock()?;
            host::eligible_incarnation(self.store, self.token, self.identity)?;
            Ok(launch::pending::bound_surface(
                self.store,
                self.session_id,
                || "invalid host surface binding",
            )?
            .map(|surface| matches_surface(&surface, self.kind)))
        })();
        match binding {
            Ok(None) => Binding::Absent,
            Ok(Some(true)) => Binding::Host,
            Ok(Some(false)) => Binding::Other,
            Err(_) => Binding::Unavailable,
        }
    }
}

/// The host's decision at one observation, once the capture has not ended the launch.
#[derive(Debug, Eq, PartialEq)]
enum Step {
    Continue,
    Refuse(Refusal),
    HandOff,
}

/// The step decision from the time, whether the environment is ready, and the binding,
/// which is observed only before the deadline. A present binding that does not name this
/// host is final, absence is still waiting, and the handoff needs both the binding and the
/// environment.
fn decide(now: Instant, deadline: Instant, ready: bool, binding: impl FnOnce() -> Binding) -> Step {
    if now >= deadline {
        return Step::Refuse(Refusal::host(host::FailureReason::BindingTimeout));
    }
    match binding() {
        Binding::Other | Binding::Unavailable => {
            Step::Refuse(Refusal::host(host::FailureReason::BindingMismatch))
        }
        Binding::Host if ready => Step::HandOff,
        Binding::Host | Binding::Absent => Step::Continue,
    }
}

/// The host's wait from the capture's start to the handoff. Every 10 ms it polls the capture
/// until it answers, then decides on the time and the binding: the capture keeps running
/// while the binding is observed, and a capture failure, the deadline or a binding that does
/// not name this host refuses at once, without waiting for the capture. `ready` records the
/// prepared environment as soon as the capture returns it; its error ends the wait without
/// a refusal.
fn await_handoff(
    clock: &mut dyn Clock,
    start: Instant,
    deadline: Instant,
    capture: &mut dyn CaptureResult,
    binding: &mut dyn BindingObservation,
    ready: &mut dyn FnMut(&Captured, Instant) -> Result<()>,
) -> Result<std::result::Result<Captured, Refusal>> {
    let mut captured: Option<Captured> = None;
    loop {
        if captured.is_none() {
            match capture.poll() {
                CapturePoll::Running => {}
                CapturePoll::Finished(Ok(result)) => {
                    ready(&result, clock.now())?;
                    captured = Some(result);
                }
                CapturePoll::Finished(Err(failure)) => {
                    return Ok(Err(Refusal {
                        reason: failure.reason,
                        exit_code: failure.exit_code,
                        signal: failure.signal,
                        capture_elapsed: Some(failure.elapsed),
                        capture_budget: Some(failure.budget),
                    }));
                }
                CapturePoll::Gone => {
                    return Ok(Err(Refusal {
                        reason: host::FailureReason::CaptureSpawnFailed,
                        exit_code: None,
                        signal: None,
                        capture_elapsed: Some(clock.now().saturating_duration_since(start)),
                        capture_budget: None,
                    }));
                }
            }
        }
        match decide(clock.now(), deadline, captured.is_some(), || {
            binding.observe()
        }) {
            Step::Continue => clock.sleep(POLL),
            Step::Refuse(refusal) => return Ok(Err(refusal)),
            Step::HandOff => {
                return Ok(Ok(captured.expect("capture completed before handoff")));
            }
        }
    }
}

// Whether the bound surface of this Session names this host's own terminal surface, by the
// check that the kind registers for its launch host.
fn matches_surface(surface: &TerminalSession, kind: TerminalKind) -> bool {
    surface.kind == kind
        && kind
            .registration()
            .launch_host
            .as_ref()
            .is_some_and(|host| (host.binding_names_host)(surface))
}

#[cfg(test)]
mod tests;
