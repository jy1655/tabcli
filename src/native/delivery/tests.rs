use super::super::query;
use super::super::session::{CoreRecord, Store, hold, requests, update_status};
use super::test_support::*;
use super::*;
use agent_bridge::FirstPartyCli;
use std::fs;
#[test]
fn initial_sent_settles_claim_and_removes_initial_prompt_record() {
    let f = Fixture::new();
    let claim = f.claim(true);
    let d = deliver_with(
        claim,
        Turn::Initial {
            gate: &mut |_| Ok(()),
        },
        f.context(),
        &f.fake,
        &f.checks,
    )
    .unwrap();
    assert_eq!(d.correlation_id.as_deref(), Some("correlation"));
    assert!(!f.store.record(CoreRecord::InitialPrompt).path().exists());
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
}
#[test]
fn follow_up_sent_keeps_no_initial_prompt_and_status_working() {
    let f = Fixture::new();
    f.follow(f.claim(false)).unwrap();
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
    assert!(!f.store.record(CoreRecord::InitialPrompt).path().exists());
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
}
#[test]
fn not_sent_rolls_back_claim_with_reason_and_cancels_prepared() {
    let mut f = Fixture::new();
    f.fake.result = "not sent";
    let claim = f.claim(false);
    let token = claim.token().to_owned();
    assert!(f.follow(claim).is_err());
    assert_eq!(f.store.status().unwrap().state, SessionState::Ready);
    assert_eq!(
        f.store.status().unwrap().error.as_deref(),
        Some("send cause")
    );
    assert!(turn::current_claim_token(&f.store).unwrap().is_none());
    assert_eq!(f.calls().last(), Some(&"discard some"));
    // Changed deliberately: the adapter's NotSent is settled as the receipt's failed event
    // by shared settlement; the adapter itself still only returns `Outcome::NotSent`.
    assert_refused_request(&f.store, &token, "send cause");
}

/// The refusal is settled in shared delivery settlement, with the refusal text and
/// `delivery_not_sent` provenance; no adapter writes it.
fn assert_refused_request(store: &Store, claim_token: &str, error: &str) {
    let receipt = requests::for_claim(store, claim_token).unwrap().unwrap();
    let event = store.event_strict(&receipt.event_file).unwrap();
    assert_eq!(event.error.as_deref(), Some(error));
    assert_eq!(
        event.provenance.as_deref(),
        Some(turn::DELIVERY_NOT_SENT_PROVENANCE)
    );
    assert_eq!(store.events().unwrap().len(), 1);
    let result = query::request_result(store, &receipt.request_id).unwrap();
    assert_eq!(result["request_state"], "failed");
    assert_eq!(result["error"], error);
}

#[test]
fn initial_not_sent_fails_the_session_and_records_the_refusal() {
    let mut f = Fixture::new();
    f.fake.result = "not sent";
    let claim = f.claim(true);
    let token = claim.token().to_owned();
    let error = deliver_with(
        claim,
        Turn::Initial {
            gate: &mut |_| Ok(()),
        },
        f.context(),
        &f.fake,
        &f.checks,
    )
    .unwrap_err();
    assert_eq!(format!("{error:#}"), "send cause");
    assert_eq!(f.store.status().unwrap().state, SessionState::Failed);
    assert_eq!(
        f.store.status().unwrap().error.as_deref(),
        Some("send cause")
    );
    assert!(turn::current_claim_token(&f.store).unwrap().is_none());
    assert_refused_request(&f.store, &token, "send cause");
}
#[test]
fn uncertain_keeps_claim_working_with_error_and_never_cancels() {
    let mut f = Fixture::new();
    f.fake.result = "uncertain";
    assert!(f.follow(f.claim(false)).is_err());
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
    assert_eq!(
        f.store.status().unwrap().error.as_deref(),
        Some("send cause")
    );
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
    assert!(!f.calls().iter().any(|s| s.starts_with("discard")));
}
#[test]
fn prepare_failure_is_not_sent_without_working_write() {
    let mut f = Fixture::new();
    f.fake.prepare_fails = true;
    let claim = f.claim(false);
    let token = claim.token().to_owned();
    let generation = f.store.status().unwrap().generation;
    assert!(f.follow(claim).is_err());
    assert_eq!(f.store.status().unwrap().generation, generation + 1);
    assert_eq!(f.calls(), ["prepare", "discard none"]);
    // Changed deliberately: a refusal before `begin_delivery` is recorded as well, with
    // the one status write that publishes the rollback.
    assert_refused_request(&f.store, &token, "prepare cause");
}
#[test]
fn ownership_failure_precedes_begin_delivery_for_surface_transport() {
    let mut f = Fixture::new();
    f.checks.fail = "ownership";
    let claim = f.claim(false);
    let generation = f.store.status().unwrap().generation;
    assert!(f.follow(claim).is_err());
    assert_eq!(f.store.status().unwrap().generation, generation + 1);
    assert_eq!(f.calls(), ["prepare", "ownership", "discard some"]);
}
#[test]
fn budget_failure_names_transport_and_is_not_sent() {
    let mut f = Fixture::new();
    f.checks.fail = "budget";
    let e = f.follow(f.claim(false)).unwrap_err();
    assert_eq!(
        format!("{e:#}"),
        "provider follow-up transport fake cannot start inside its remaining total timeout: budget cause"
    );
    assert!(turn::current_claim_token(&f.store).unwrap().is_none());
    // The recorded error is the bare cause: context is added only after settlement.
    let receipt = &requests::list(&f.store).unwrap().receipts[0];
    assert_eq!(
        f.store
            .event_strict(&receipt.event_file)
            .unwrap()
            .error
            .as_deref(),
        Some("budget cause")
    );
}
#[test]
fn begin_delivery_refused_when_claim_replaced_cancels_prepared() {
    let f = Fixture::new();
    let claim = f.claim(false);
    let dir = f.store.directory().to_owned();
    *f.checks.before_begin.borrow_mut() = Some(Box::new(move || {
        let record = Store::open_unchecked(&dir).record(CoreRecord::TurnClaim);
        record.remove().unwrap();
        record.write_private(b"replacement").unwrap();
    }));
    let e = f.follow(claim).unwrap_err();
    assert_eq!(
        format!("{e:#}"),
        "delivery refused: this turn no longer holds the session claim"
    );
    assert_eq!(
        turn::current_claim_token(&f.store).unwrap().as_deref(),
        Some("replacement")
    );
    assert_eq!(f.calls().last(), Some(&"discard some"));
}
#[test]
fn begin_delivery_refused_when_session_closed() {
    let f = Fixture::new();
    let claim = f.claim(false);
    let dir = f.store.directory().to_owned();
    *f.checks.before_begin.borrow_mut() = Some(Box::new(move || {
        update_status(&dir, SessionState::Closed, None, None).unwrap();
    }));
    let e = f.follow(claim).unwrap_err();
    assert!(e.to_string().starts_with("delivery refused:"));
    assert_eq!(f.store.status().unwrap().state, SessionState::Closed);
}
#[test]
fn follow_up_begin_delivery_refused_under_hold() {
    let f = Fixture::new();
    let claim = f.claim(false);
    let dir = f.store.directory().to_owned();
    *f.checks.before_begin.borrow_mut() = Some(Box::new(move || {
        hold::change(&Store::open_unchecked(&dir), false).unwrap();
    }));
    let e = f.follow(claim).unwrap_err();
    assert!(e.to_string().contains("is held"));
    assert!(!f.calls().contains(&"send"));
}
#[test]
fn late_not_sent_after_next_claim_leaves_next_turn_untouched() {
    let mut f = Fixture::new();
    f.fake.result = "not sent";
    let claim = f.claim(false);
    let token = claim.token().to_owned();
    let dir = f.store.directory().to_owned();
    *f.fake.on_send.borrow_mut() = Some(Box::new(move || {
        let store = Store::open_unchecked(&dir);
        turn::Report::for_claim(&store, FirstPartyCli::Pi, Some(&token))
            .complete("done", None, None)
            .unwrap();
        turn::claim_ready(&store, "session-test", &[])
            .unwrap()
            .0
            .retain();
    }));
    assert!(f.follow(claim).is_err());
    assert_eq!(f.store.status().unwrap().state, SessionState::Claimed);
    assert!(f.store.status().unwrap().error.is_none());
}
#[test]
fn gates_run_after_launch_then_before_delivery_in_canonical_order() {
    let f = Fixture::new();
    let claim = f.claim(true);
    let calls = f.fake.calls.clone();
    let mut gate = |g| {
        calls.borrow_mut().push(match g {
            Gate::AfterLaunch => "after launch",
            Gate::BeforeDelivery => "before delivery",
        });
        Ok(())
    };
    deliver_with(
        claim,
        Turn::Initial { gate: &mut gate },
        f.context(),
        &f.fake,
        &f.checks,
    )
    .unwrap();
    assert_eq!(
        f.calls(),
        [
            "ready",
            "after launch",
            "prepare",
            "ownership",
            "remaining",
            "budget",
            "before delivery",
            "send"
        ]
    );
}
#[test]
fn launch_argument_settles_sent_without_waiting_or_sending() {
    let f = Fixture::new();
    let claim = f.claim(true);
    f.store
        .record(CoreRecord::InitialPrompt)
        .remove_raw()
        .unwrap();
    deliver_with(
        claim,
        Turn::Initial {
            gate: &mut |_| panic!("gate"),
        },
        f.context(),
        &LAUNCH_ARGUMENT,
        &f.checks,
    )
    .unwrap();
    assert!(f.calls().is_empty());
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
}
#[test]
fn surface_steps_skipped_for_non_surface_transport() {
    let mut f = Fixture::new();
    f.fake.surface = false;
    f.follow(f.claim(false)).unwrap();
    assert_eq!(f.calls(), ["prepare", "remaining", "send"]);
}
#[test]
fn follow_up_wording_is_byte_identical_to_0_4_3() {
    for surface in [false, true] {
        for result in ["not sent", "uncertain"] {
            let mut f = Fixture::new();
            f.fake.surface = surface;
            f.fake.result = result;
            let e = f.follow(f.claim(false)).unwrap_err();
            let expected = if surface {
                "provider follow-up transport fake failed: failed to type into visible Windows Console session session-test: send cause"
            } else if result == "uncertain" {
                "provider follow-up transport fake could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed: send cause"
            } else {
                "provider follow-up transport fake failed: send cause"
            };
            assert_eq!(format!("{e:#}"), expected);
            assert_eq!(
                f.store.status().unwrap().error.as_deref(),
                Some("send cause")
            );
        }
    }
}

#[test]
fn initial_prompt_delay_must_fit_inside_the_original_ask_budget() {
    // Reach the delay-fit check with budget remaining; the 12-second delay
    // still cannot fit, and this check does not sleep.
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = initial_prompt_delay_within_budget(
        deadline,
        Duration::from_secs(12),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("initial prompt readiness delay"));

    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        initial_prompt_delay_within_budget(
            deadline,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .unwrap(),
        Duration::from_millis(10)
    );
}
#[test]
fn preparation_and_preflight_errors_preserve_chains_reasons_and_generations() {
    for initial in [false, true] {
        for surface in [false, true] {
            for failure in ["prepare", "ownership", "timeout", "budget"] {
                if !surface && matches!(failure, "ownership" | "budget") {
                    continue;
                }
                let mut f = Fixture::new();
                f.fake.surface = surface;
                f.fake.prepare_fails = failure == "prepare";
                f.checks.fail = failure;
                let claim = f.claim(initial);
                let generation = f.store.status().unwrap().generation;
                let result = if initial {
                    deliver_with(
                        claim,
                        Turn::Initial {
                            gate: &mut |_| Ok(()),
                        },
                        f.context(),
                        &f.fake,
                        &f.checks,
                    )
                } else {
                    f.follow(claim)
                };
                let cause = match failure {
                    "prepare" => "prepare cause",
                    "ownership" => "ownership cause",
                    "timeout" => "timed out after 30 seconds",
                    _ => "budget cause",
                };
                let expected = if initial || !surface {
                    cause.to_owned()
                } else {
                    match failure {
                        "prepare" | "ownership" => format!(
                            "provider follow-up transport fake failed: failed to prepare input for visible Windows Console session session-test: {cause}"
                        ),
                        "timeout" => format!(
                            "provider follow-up transport fake exhausted its total timeout before delivery: {cause}"
                        ),
                        _ => format!(
                            "provider follow-up transport fake cannot start inside its remaining total timeout: {cause}"
                        ),
                    }
                };
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    expected,
                    "{initial} {surface} {failure}"
                );
                let status = f.store.status().unwrap();
                assert_eq!(status.error.as_deref(), Some(cause));
                assert_eq!(status.generation, generation + 1);
                assert_eq!(
                    status.state,
                    if initial {
                        SessionState::Failed
                    } else {
                        SessionState::Ready
                    }
                );
                assert!(turn::current_claim_token(&f.store).unwrap().is_none());
                assert_eq!(
                    f.store.record(CoreRecord::InitialPrompt).path().exists(),
                    initial
                );
                assert!(!f.calls().contains(&"send"));
                assert_eq!(
                    f.calls().last(),
                    Some(&if failure == "prepare" {
                        "discard none"
                    } else {
                        "discard some"
                    })
                );
            }
        }
    }
}

#[test]
fn initial_send_failures_keep_prompt_and_preserve_the_transport_reason() {
    for surface in [false, true] {
        for result in ["not sent", "uncertain"] {
            let mut f = Fixture::new();
            f.fake.surface = surface;
            f.fake.result = result;
            let claim = f.claim(true);
            let generation = f.store.status().unwrap().generation;
            let error = deliver_with(
                claim,
                Turn::Initial {
                    gate: &mut |_| Ok(()),
                },
                f.context(),
                &f.fake,
                &f.checks,
            )
            .unwrap_err();
            assert_eq!(format!("{error:#}"), "send cause");
            assert_eq!(
                f.store.status().unwrap().error.as_deref(),
                Some("send cause")
            );
            assert_eq!(f.store.status().unwrap().generation, generation + 2);
            assert!(f.store.record(CoreRecord::InitialPrompt).path().is_file());
            assert_eq!(
                turn::current_claim_token(&f.store).unwrap().is_some(),
                result == "uncertain"
            );
        }
    }
}

#[test]
fn sent_initial_cleanup_failure_is_diagnostic_and_never_rolls_back() {
    let f = Fixture::new();
    let claim = f.claim(true);
    let dir = f.store.directory().to_owned();
    *f.fake.on_send.borrow_mut() = Some(Box::new(move || {
        Store::open_unchecked(&dir)
            .record(CoreRecord::InitialPrompt)
            .remove_raw()
            .unwrap();
    }));
    let error = deliver_with(
        claim,
        Turn::Initial {
            gate: &mut |_| Ok(()),
        },
        f.context(),
        &f.fake,
        &f.checks,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to remove the delivered initial prompt"
    );
    assert_eq!(
        f.store.status().unwrap().error.as_deref(),
        Some(format!("{error:#}").as_str())
    );
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
    assert!(turn::current_claim_token(&f.store).unwrap().is_some());
    assert!(!f.calls().iter().any(|call| call.starts_with("discard")));
}

#[test]
fn deadline_and_budget_refusals_precede_a_concurrent_hold() {
    for failure in ["timeout", "budget"] {
        let mut f = Fixture::new();
        f.checks.fail = failure;
        let claim = f.claim(false);
        let dir = f.store.directory().to_owned();
        *f.checks.before_begin.borrow_mut() = Some(Box::new(move || {
            hold::change(&Store::open_unchecked(&dir), false).unwrap();
        }));
        let error = f.follow(claim).unwrap_err();
        assert!(!format!("{error:#}").contains("is held"));
        assert!(hold::read(&f.store).unwrap());
        assert!(!f.calls().contains(&"send"));
    }
}

#[test]
fn launch_gates_preserve_typed_refusal_and_never_send() {
    #[derive(Debug)]
    struct Refused;
    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("typed holder refusal")
        }
    }
    impl std::error::Error for Refused {}
    for rejected in [Gate::AfterLaunch, Gate::BeforeDelivery] {
        let f = Fixture::new();
        let claim = f.claim(true);
        let mut gate = |gate| {
            if gate == rejected {
                Err(anyhow::Error::new(Refused))
            } else {
                Ok(())
            }
        };
        let error = deliver_with(
            claim,
            Turn::Initial { gate: &mut gate },
            f.context(),
            &f.fake,
            &f.checks,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Refused>().is_some());
        assert!(turn::current_claim_token(&f.store).unwrap().is_none());
        assert!(!f.calls().contains(&"send"));
        assert_eq!(
            f.calls().contains(&"prepare"),
            rejected == Gate::BeforeDelivery
        );
    }
}

#[test]
fn runtime_readiness_keeps_wait_state_and_delay_error_context() {
    let f = Fixture::new();
    let _claim = f.claim(true);
    let mut context = f.context();
    context.deadline = Instant::now();
    let error = RuntimeChecks.ready(&context, Duration::ZERO).unwrap_err();
    assert!(format!("{error:#}").contains("timed out after 30 seconds"));
    let context = f.context();
    let error = RuntimeChecks
        .ready(&context, Duration::from_secs(40))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "initial prompt readiness delay of 40 seconds does not fit inside the 30 second ask timeout"
    );
}
#[test]
fn deadline_expiring_during_begin_lock_wait_does_not_add_a_second_preflight() {
    struct BlockingChecks;
    impl Checks for BlockingChecks {
        fn ready(&self, _: &DeliveryContext<'_>, _: Duration) -> Result<()> {
            unreachable!()
        }
        fn ownership(&self, _: &DeliveryContext<'_>) -> Result<()> {
            Ok(())
        }
        fn remaining(&self, c: &DeliveryContext<'_>) -> Result<Duration> {
            let remaining = RuntimeChecks.remaining(c)?;
            let directory = c.directory.to_owned();
            let deadline = c.deadline;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let store = Store::open_unchecked(&directory);
                let lock = store.lock().unwrap();
                tx.send(()).unwrap();
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
                );
                drop(lock);
            });
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(remaining)
        }
        fn budget(&self, _: &DeliveryContext<'_>, _: usize, _: Duration) -> Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let claim = f.claim(false);
    let mut context = f.context();
    // Allow preflight to reach RuntimeChecks.remaining on a slow host. The
    // holder above deliberately keeps the begin lock past this one-second budget.
    context.deadline = Instant::now() + Duration::from_secs(1);
    let deadline = context.deadline;
    *f.fake.on_send.borrow_mut() = Some(Box::new(move || assert!(Instant::now() >= deadline)));
    deliver_with(
        claim,
        Turn::FollowUp { prompt: "prompt" },
        context,
        &f.fake,
        &BlockingChecks,
    )
    .unwrap();
    assert!(f.calls().contains(&"send"));
    assert_eq!(f.store.status().unwrap().state, SessionState::Working);
}

#[test]
fn temporary_prompt_lifetime_preserves_initial_and_follow_up_failure_order() {
    for initial in [false, true] {
        for stage in ["ownership", "timeout", "budget", "send"] {
            let mut f = Fixture::new();
            f.checks.fail = stage;
            f.fake.result = "not sent";
            let claim = f.claim(initial);
            let result = if initial {
                deliver_with(
                    claim,
                    Turn::Initial {
                        gate: &mut |_| Ok(()),
                    },
                    f.context(),
                    &f.fake,
                    &f.checks,
                )
            } else {
                f.follow(claim)
            };
            assert!(result.is_err());
            assert_eq!(
                *f.fake.files_at_discard.borrow(),
                vec![!initial && stage != "ownership"]
            );
            assert!(!fs::read_dir(f.store.directory()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pending-prompt-")
            }));
        }
    }
}
