use super::authority::{Authority, authority};
use super::record::observe;
use super::store::{Environment, Trust, user_home};
use crate::native::clock::{Clock, SystemClock};
use crate::native::provider;
use crate::native::session::Reader;
use agent_bridge::FirstPartyCli;
use anyhow::{Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

// What one poll of a trust wait's readiness callback found. A fatal error is the
// callback's `Err` and ends the wait at once; a retryable observation is `Pending`
// with the reason the timeout message reports when it is the last one.
pub(in crate::native) enum TrustReadiness<R> {
    Ready,
    Pending(R),
}

// How the wait bounds its sleep by the deadline: from the instant it compared with
// the deadline, or from the clock sampled again after the comparison.
#[derive(Clone, Copy)]
pub(in crate::native) enum SleepBound {
    CapturedNow,
    Resampled,
}

// A caller's poll cadence: how long one sleep is at most, and which instant bounds it.
#[derive(Clone, Copy)]
pub(in crate::native) struct TrustPoll {
    pub(in crate::native) interval: Duration,
    pub(in crate::native) sleep_bound: SleepBound,
}

// The one trust wait of the paste gates (AGENTS.md, Trust Store Boundary). Every poll
// looks at the session before it looks at the evidence: a session that has ended has
// no terminal left to send to, whatever becomes of its workspace's trust, and without
// this the launcher of a closed session went on waiting until its deadline (#60). A
// failed status read is ignored. Then the caller's readiness: `Ready` ends the wait,
// and positive evidence wins over an already-expired deadline; a fatal error
// propagates; `Pending` keeps waiting until the deadline, whose message formats the
// current poll's reason. The caller reads the manifest once, outside this loop.
pub(in crate::native) fn wait_until_trusted<R, C: Clock>(
    directory: &Path,
    deadline: Instant,
    poll: TrustPoll,
    clock: &mut C,
    ended_message: &dyn Fn(&str) -> String,
    ready: &mut dyn FnMut() -> Result<TrustReadiness<R>>,
    timeout_message: &dyn Fn(R) -> String,
) -> Result<()> {
    loop {
        if let Ok(status) = Reader::open_unchecked(directory).status()
            && status.state.ended()
        {
            bail!("{}", ended_message(&status.state.to_string()));
        }
        let reason = match ready()? {
            TrustReadiness::Ready => return Ok(()),
            TrustReadiness::Pending(reason) => reason,
        };
        let now = clock.now();
        if now >= deadline {
            bail!("{}", timeout_message(reason));
        }
        let remaining = match poll.sleep_bound {
            SleepBound::CapturedNow => deadline.saturating_duration_since(now),
            SleepBound::Resampled => deadline.saturating_duration_since(clock.now()),
        };
        clock.sleep(remaining.min(poll.interval));
    }
}

const NATIVE_TRUST_TIMEOUT: &str = "workspace trust is not verified; no initial console input was sent. Approve the exact workspace in the managed provider and start a new request after a timeout";

// A provider's positive input-readiness evidence may open the paste gate without exact stored trust.
// It never widens shared consent; see AGENTS.md, Trust Store Boundary.
pub(in crate::native) fn wait_for_native_trust(
    directory: &Path,
    target: FirstPartyCli,
    deadline: Instant,
    env: &Environment,
    ready: &mut dyn FnMut() -> bool,
) -> Result<()> {
    user_home(env)?;
    wait_for_native_trust_with(directory, target, deadline, env, ready)
}

fn wait_for_native_trust_with(
    directory: &Path,
    target: FirstPartyCli,
    deadline: Instant,
    env: &Environment,
    ready: &mut dyn FnMut() -> bool,
) -> Result<()> {
    wait_for_native_trust_authorized_with(directory, target, deadline, env, ready, &mut || {
        authority(directory, target, env)
    })
}

// Codex's readiness, in order and short-circuiting: the target's own applied marker, read
// again in the one typed authority result together with its yes (an authority error is
// fatal), then the adapter's exact stored trust (a lookup error is not trust), then the
// adapter's own readiness evidence. Another adapter's marker is not the target's option.
fn wait_for_native_trust_authorized_with(
    directory: &Path,
    target: FirstPartyCli,
    deadline: Instant,
    env: &Environment,
    ready: &mut dyn FnMut() -> bool,
    authority: &mut dyn FnMut() -> Result<Authority>,
) -> Result<()> {
    let manifest = Reader::open_unchecked(directory).manifest()?;
    wait_until_trusted(
        directory,
        deadline,
        TrustPoll {
            interval: Duration::from_millis(100),
            sleep_bound: SleepBound::Resampled,
        },
        &mut SystemClock,
        &|state| {
            format!(
                "the session is {state} and no longer waits for workspace trust; no initial console input was sent"
            )
        },
        &mut || {
            let own = provider::applied_marker(target);
            let applied_here = own.is_some() && observe(directory).applied_marker() == own;
            let applied_with_consent = applied_here && {
                let found = authority()?;
                found.applied.as_deref() == own && found.verdict.is_ok()
            };
            Ok(
                if applied_with_consent
                    || matches!(
                        provider::workspace_trust(target, &manifest.workspace, env, None),
                        Ok(Trust::Trusted(_))
                    )
                    || ready()
                {
                    TrustReadiness::Ready
                } else {
                    TrustReadiness::Pending(())
                },
            )
        },
        &|()| NATIVE_TRUST_TIMEOUT.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::authority::{Consented, Refusal};
    use super::super::record::SESSION_FILE;
    use super::super::tests::{session, setup, trust_pi};
    use super::*;

    // An authority result with `applied` recorded and a yes or a refusal.
    fn found(applied: &str, consented: bool) -> Authority {
        Authority {
            applied: Some(applied.to_owned()),
            verdict: if consented {
                Ok(Consented::Exact)
            } else {
                Err(Refusal {
                    reason: "refused".into(),
                    unreadable: false,
                })
            },
        }
    }
    use crate::native::clock::FakeClock;
    use crate::native::session::primitives::write_json_atomic;
    use std::fs;

    #[test]
    fn native_trust_rejects_absent_and_unregistered_markers() {
        let (_temp, workspace, fixture) = setup();
        let (_root, directory) = session(&workspace, "awaiting-initial-input");
        for applied in [
            serde_json::Value::Null,
            serde_json::json!("unregistered"),
            serde_json::json!(42),
        ] {
            write_json_atomic(
                &directory.join(SESSION_FILE),
                &serde_json::json!({"applied": applied}),
            )
            .unwrap();
            for target in [
                FirstPartyCli::Codex,
                FirstPartyCli::Claude,
                FirstPartyCli::Agy,
                FirstPartyCli::Pi,
            ] {
                let mut asked = false;
                let error = wait_for_native_trust_authorized_with(
                    &directory,
                    target,
                    Instant::now(),
                    &fixture.env,
                    &mut || {
                        asked = true;
                        false
                    },
                    &mut || panic!("unregistered marker cannot authorize"),
                )
                .unwrap_err();
                assert!(asked);
                assert_eq!(error.to_string(), NATIVE_TRUST_TIMEOUT);
            }
        }
    }
    // Review round 3, finding 11: only the target's own marker, confirmed in the one typed
    // authority result together with its yes, opens the gate; another adapter's marker is
    // not asked about and opens nothing.
    #[test]
    fn native_trust_accepts_only_the_targets_own_marker_with_its_authority() {
        let (_temp, workspace, fixture) = setup();
        let (_root, directory) = session(&workspace, "awaiting-initial-input");
        for marker in [
            "codex-config-override",
            "pi-approve-once",
            "agy-settings-override",
        ] {
            write_json_atomic(
                &directory.join(SESSION_FILE),
                &serde_json::json!({"applied": marker}),
            )
            .unwrap();
            for target in [
                FirstPartyCli::Codex,
                FirstPartyCli::Claude,
                FirstPartyCli::Agy,
                FirstPartyCli::Pi,
            ] {
                let label = format!("{marker} {target:?}");
                let wait =
                    |ready: &mut dyn FnMut() -> bool,
                     authority: &mut dyn FnMut() -> Result<Authority>| {
                        wait_for_native_trust_authorized_with(
                            &directory,
                            target,
                            Instant::now(),
                            &fixture.env,
                            ready,
                            authority,
                        )
                    };
                if provider::applied_marker(target) != Some(marker) {
                    let mut asked = false;
                    let error = wait(
                        &mut || {
                            asked = true;
                            false
                        },
                        &mut || panic!("{label}: another adapter's marker is not asked about"),
                    )
                    .unwrap_err();
                    assert!(asked, "{label}");
                    assert_eq!(error.to_string(), NATIVE_TRUST_TIMEOUT, "{label}");
                    continue;
                }
                let mut authorizations = 0;
                wait(
                    &mut || panic!("{label}: the authorized marker short circuits"),
                    &mut || {
                        authorizations += 1;
                        Ok(found(marker, true))
                    },
                )
                .unwrap();
                assert_eq!(authorizations, 1, "{label}");
                for refused in [found(marker, false), found("another-marker", true)] {
                    let error = wait(&mut || false, &mut || Ok(refused.clone())).unwrap_err();
                    assert_eq!(error.to_string(), NATIVE_TRUST_TIMEOUT, "{label}");
                    // A refusal still permits the adapter's independent readiness evidence.
                    wait(&mut || true, &mut || Ok(refused.clone())).unwrap();
                }
                let error = wait(
                    &mut || panic!("{label}: an authority error precedes ready"),
                    &mut || bail!("authority error"),
                )
                .unwrap_err();
                assert_eq!(error.to_string(), "authority error", "{label}");
            }
        }
    }
    #[test]
    fn native_trust_ended_status_precedes_marker_store_and_ready() {
        let (_temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        for state in ["closed", "failed", "exited"] {
            let (_root, directory) = session(&workspace, state);
            write_json_atomic(
                &directory.join(SESSION_FILE),
                &serde_json::json!({"applied": "pi-approve-once"}),
            )
            .unwrap();
            let error = wait_for_native_trust_authorized_with(
                &directory,
                FirstPartyCli::Pi,
                Instant::now(),
                &fixture.env,
                &mut || panic!("ready after ended"),
                &mut || panic!("authorization after ended"),
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "the session is {state} and no longer waits for workspace trust; no initial console input was sent"
                )
            );
        }
    }
    #[test]
    fn native_trust_checks_store_then_ready_before_expired_deadline() {
        let (_temp, workspace, fixture) = setup();
        let (_root, directory) = session(&workspace, "awaiting-initial-input");
        let mut ready_calls = 0;
        wait_for_native_trust_authorized_with(
            &directory,
            FirstPartyCli::Pi,
            Instant::now(),
            &fixture.env,
            &mut || {
                ready_calls += 1;
                true
            },
            &mut || panic!("no marker"),
        )
        .unwrap();
        assert_eq!(ready_calls, 1);
        trust_pi(&fixture, &workspace);
        wait_for_native_trust_authorized_with(
            &directory,
            FirstPartyCli::Pi,
            Instant::now(),
            &fixture.env,
            &mut || panic!("trusted store precedes ready and deadline"),
            &mut || panic!("no marker"),
        )
        .unwrap();
        // The target's own marker still requires the authority before the already trusted
        // store.
        write_json_atomic(
            &directory.join(SESSION_FILE),
            &serde_json::json!({"applied": "pi-approve-once"}),
        )
        .unwrap();
        let error = wait_for_native_trust_authorized_with(
            &directory,
            FirstPartyCli::Pi,
            Instant::now(),
            &fixture.env,
            &mut || panic!("authorization error precedes ready"),
            &mut || bail!("authorization before trusted store"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "authorization before trusted store");
        wait_for_native_trust_authorized_with(
            &directory,
            FirstPartyCli::Pi,
            Instant::now(),
            &fixture.env,
            &mut || panic!("trusted store precedes ready"),
            &mut || Ok(found("pi-approve-once", false)),
        )
        .unwrap();
        write_json_atomic(&directory.join(SESSION_FILE), &serde_json::json!({})).unwrap();
        write_json_atomic(&fixture.pi, &serde_json::json!({})).unwrap();
        let error = wait_for_native_trust_authorized_with(
            &directory,
            FirstPartyCli::Pi,
            Instant::now(),
            &fixture.env,
            &mut || {
                ready_calls += 1;
                false
            },
            &mut || panic!("no marker"),
        )
        .unwrap_err();
        assert_eq!(ready_calls, 2);
        assert_eq!(error.to_string(), NATIVE_TRUST_TIMEOUT);
    }
    // Issue #60. The store never lists a workspace whose trust the provider takes from
    // elsewhere, and a launcher went on waiting for a session that had been closed.
    #[test]
    fn the_wait_for_native_trust_ends_with_the_adapters_evidence_or_with_the_session() {
        let (_temp, workspace, fixture) = setup();
        let far = || Instant::now() + Duration::from_secs(600);
        let wait = |directory: &Path, deadline: Instant, ready: &mut dyn FnMut() -> bool| {
            wait_for_native_trust_with(
                directory,
                FirstPartyCli::Codex,
                deadline,
                &fixture.env,
                ready,
            )
            .map_err(|error| error.to_string())
        };

        // The adapter is asked on every poll, and the wait ends with its first yes.
        let (_root, waiting) = session(&workspace, "awaiting-initial-input");
        let mut asked = 0;
        wait(&waiting, far(), &mut || {
            asked += 1;
            asked == 3
        })
        .unwrap();
        assert_eq!(asked, 3);

        // A session that has ended is not sent to, even when the evidence holds in the
        // same poll: the adapter is not even asked.
        for state in ["closed", "failed", "exited"] {
            let (_root, ended) = session(&workspace, state);
            let mut asked = false;
            assert_eq!(
                wait(&ended, far(), &mut || {
                    asked = true;
                    true
                }),
                Err(format!(
                    "the session is {state} and no longer waits for workspace trust; no initial console input was sent"
                ))
            );
            assert!(!asked, "{state}");
        }

        // A session that ends while its input waits ends the wait at the next poll.
        let (_root, closing) = session(&workspace, "awaiting-initial-input");
        let mut polls = 0;
        assert_eq!(
            wait(&closing, far(), &mut || {
                polls += 1;
                if polls == 2 {
                    fs::write(
                        closing.join("status.json"),
                        r#"{"state":"closed","generation":2,"updated_unix_ms":2,"exit_code":null,"error":null}"#,
                    )
                    .unwrap();
                }
                false
            }),
            Err(
                "the session is closed and no longer waits for workspace trust; no initial console input was sent"
                    .to_owned()
            )
        );
        assert_eq!(polls, 2);

        let error = wait(
            &waiting,
            Instant::now() + Duration::from_millis(250),
            &mut || false,
        )
        .unwrap_err();
        assert_eq!(error, NATIVE_TRUST_TIMEOUT);
    }
    // The shared loop looks at the session before it asks either caller's readiness,
    // with each caller's own message, and sleeps nothing for an ended session.
    #[test]
    fn the_shared_trust_wait_checks_the_session_before_any_readiness_for_both_callers() {
        let (_temp, workspace, _fixture) = setup();
        let start = Instant::now();
        let poll = TrustPoll {
            interval: Duration::from_millis(100),
            sleep_bound: SleepBound::CapturedNow,
        };
        fn codex_ended(state: &str) -> String {
            format!(
                "the session is {state} and no longer waits for workspace trust; no initial console input was sent"
            )
        }
        fn agy_ended(state: &str) -> String {
            format!(
                "the session is {state} and no longer waits for Agy workspace trust, so the prompt was not pasted"
            )
        }
        type EndedMessage = fn(&str) -> String;
        let messages: [(&str, EndedMessage); 2] = [("codex", codex_ended), ("agy", agy_ended)];
        for state in ["closed", "failed", "exited"] {
            let (_root, ended) = session(&workspace, state);
            for (caller, ended_message) in messages {
                let mut clock = FakeClock::new(start);
                let error = wait_until_trusted(
                    &ended,
                    start + Duration::from_secs(60),
                    poll,
                    &mut clock,
                    &ended_message,
                    &mut || -> Result<TrustReadiness<()>> { panic!("{caller}: ready after ended") },
                    &|()| panic!("{caller}: timeout after ended"),
                )
                .unwrap_err();
                assert_eq!(error.to_string(), ended_message(state), "{caller}");
                assert_eq!(clock.slept, Duration::ZERO, "{caller}");
            }
        }

        // A session that is still waiting is asked, a fatal error ends the wait at
        // once, and the deadline formats the reason of its own poll.
        let (_root, waiting) = session(&workspace, "awaiting-initial-input");
        let mut clock = FakeClock::new(start);
        let error = wait_until_trusted(
            &waiting,
            start + Duration::from_secs(60),
            poll,
            &mut clock,
            &|_| panic!("not ended"),
            &mut || -> Result<TrustReadiness<()>> { bail!("fatal") },
            &|()| panic!("fatal precedes the deadline"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "fatal");
        assert_eq!(clock.slept, Duration::ZERO);
        let mut reasons = ["first", "second", "third", "fourth"].into_iter();
        let mut clock = FakeClock::new(start);
        let error = wait_until_trusted(
            &waiting,
            start + Duration::from_millis(250),
            TrustPoll {
                interval: Duration::from_millis(100),
                sleep_bound: SleepBound::Resampled,
            },
            &mut clock,
            &|_| panic!("not ended"),
            &mut || Ok(TrustReadiness::Pending(reasons.next().unwrap())),
            &|reason| format!("timed out: {reason}"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "timed out: fourth");
        assert_eq!(clock.slept, Duration::from_millis(250));
    }

    // Review r4, finding 1: a source store that becomes unreadable after the session's
    // assessment is a refusal for the Codex wait, as a missing consent was in 0.4.49: the
    // marker route closes and the independent evidence is still checked.
    #[test]
    fn an_unreadable_source_store_leaves_the_independent_evidence_to_the_wait() {
        use super::super::record::{applied, assess_with, record};
        let (_temp, workspace, fixture) = setup();
        let (root, directory) = session(&workspace, "awaiting-initial-input");
        trust_pi(&fixture, &workspace);
        let assessment = assess_with(root.path(), &workspace, &fixture.env).unwrap();
        assert_eq!(assessment.state, "verified");
        record(&directory, &assessment).unwrap();
        applied(&directory, "codex-config-override").unwrap();
        fs::remove_file(&fixture.pi).unwrap();
        crate::native::session::primitives::write_private(&fixture.pi, b"{").unwrap();
        let mut asked = 0;
        wait_for_native_trust_with(
            &directory,
            FirstPartyCli::Codex,
            Instant::now() + Duration::from_secs(5),
            &fixture.env,
            &mut || {
                asked += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(asked, 1);
    }
}
