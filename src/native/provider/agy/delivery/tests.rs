use super::super::readiness::wait_for_startup_readiness_with;
use super::super::receipt::confirm_input_receipt_with;
use super::super::test_support::{
    FakeClock, REAL_MACOS_INITIAL_TURN, late_reload_completion, late_reload_startup_log,
    log_sequence, some_log, successful_startup_log,
};
use super::super::{
    MACOS_FOLLOW_UP_READINESS_TIMING, PendingAgyTurn, STARTUP_QUIET_PERIOD,
    WINDOWS_STARTUP_READINESS_TIMING, install_pending_turn, read_pending_turn,
};
use super::*;
use crate::native::session::Store;
use crate::native::session::primitives::read_json;
use crate::native::session::{SessionState, turn};
use crate::native::session::{SessionStatus, update_status};
use crate::native::terminal;
use anyhow::bail;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn paste_preparation_requires_trust_correlation_and_readiness_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let readiness_reads = std::cell::Cell::new(0);
    let readiness = |_: &mut ()| {
        readiness_reads.set(readiness_reads.get() + 1);
        Ok(26060)
    };
    assert!(
        PreparedTerminalTurn::prepare_with(
            tmp.path(),
            &mut (),
            |_| bail!("session ended during trust wait"),
            readiness
        )
        .is_err()
    );
    assert_eq!(readiness_reads.get(), 0);
    assert!(
        PreparedTerminalTurn::prepare_with(tmp.path(), &mut (), |_| Ok(()), readiness).is_err()
    );
    assert_eq!(
        readiness_reads.get(),
        0,
        "missing correlation must prevent a paste"
    );
    install_pending_turn(tmp.path(), "1-2-3").unwrap();
    assert!(
        PreparedTerminalTurn::prepare_with(tmp.path(), &mut (), |_| Ok(()), |_| bail!("not ready"))
            .is_err()
    );
    assert!(!tmp.path().join("agy-input-1-2-3.json").exists());
}

#[test]
fn prepared_paste_uses_the_ready_read_offset_once_and_never_retries() {
    let tmp = tempfile::tempdir().unwrap();
    install_pending_turn(tmp.path(), "1-2-3").unwrap();
    for failure in ["none", "not-sent", "send-uncertain", "receipt-uncertain"] {
        let order = std::cell::RefCell::new(Vec::new());
        let prepared = PreparedTerminalTurn::prepare_with(
            tmp.path(),
            &mut (),
            |_| {
                order.borrow_mut().push("trust");
                Ok(())
            },
            |_| {
                order.borrow_mut().push("ready-read");
                Ok(26060)
            },
        )
        .unwrap();
        let result = prepared.deliver(
            "iterm2",
            &mut (),
            |_| {
                order.borrow_mut().push("paste");
                match failure {
                    "not-sent" => Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                        "not sent"
                    ))),
                    "send-uncertain" => Err(terminal::TerminalSendFailure::delivery_uncertain(
                        anyhow::anyhow!("uncertain"),
                    )),
                    _ => Ok(()),
                }
            },
            |_, pending, offset| {
                order.borrow_mut().push("receipt");
                assert_eq!(offset, 26060);
                assert_eq!(pending.claim_token, "1-2-3");
                if failure == "receipt-uncertain" {
                    Err(terminal::TerminalSendFailure::delivery_uncertain(
                        anyhow::anyhow!("no receipt"),
                    ))
                } else {
                    Ok(())
                }
            },
        );
        let expected = if matches!(failure, "not-sent" | "send-uncertain") {
            vec!["trust", "ready-read", "paste"]
        } else {
            vec!["trust", "ready-read", "paste", "receipt"]
        };
        assert_eq!(*order.borrow(), expected);
        match failure {
            "none" => result.unwrap(),
            "not-sent" => assert!(!result.unwrap_err().delivery_may_have_occurred()),
            _ => assert!(result.unwrap_err().delivery_may_have_occurred()),
        }
    }
}

#[test]
fn missing_receipt_is_traced_once_and_trace_failure_does_not_cancel_delivery() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = PendingAgyTurn::new("48-1-0").unwrap();
    let sends = std::cell::Cell::new(0);
    let result = trace_terminal_delivery(
        tmp.path(),
        &pending,
        26060,
        "apple-terminal",
        &mut (),
        |_| {
            sends.set(sends.get() + 1);
            Ok(())
        },
        |_| {
            Err(terminal::TerminalSendFailure::delivery_uncertain(
                anyhow::anyhow!("no HandleUserInput receipt"),
            ))
        },
    );
    assert!(result.unwrap_err().delivery_may_have_occurred());
    assert_eq!(sends.get(), 1);
    let trace: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("agy-input-48-1-0.json")).unwrap())
            .unwrap();
    assert_eq!(trace["pre_paste_offset"], 26060);
    assert_eq!(trace["outcome"], "delivery-uncertain");
    assert!(
        trace["paste_started_unix_ms"].as_u64().unwrap()
            <= trace["paste_returned_unix_ms"].as_u64().unwrap()
    );
    let blocked = tmp.path().join("not-a-directory");
    std::fs::write(&blocked, b"preserve").unwrap();
    trace_terminal_delivery(
        &blocked,
        &pending,
        10,
        "iterm2",
        &mut (),
        |_| {
            sends.set(sends.get() + 1);
            Ok(())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(sends.get(), 2);
    assert_eq!(std::fs::read(&blocked).unwrap(), b"preserve");
    let confirms = std::cell::Cell::new(0);
    let failure = trace_terminal_delivery(
        tmp.path(),
        &pending,
        10,
        "iterm2",
        &mut (),
        |_| {
            Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "deadline"
            )))
        },
        |_| {
            confirms.set(confirms.get() + 1);
            Ok(())
        },
    )
    .unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    assert_eq!(confirms.get(), 0);
}

#[test]
fn follow_up_pre_paste_offset_is_the_log_length_and_never_zero_for_a_missing_log() {
    let startup = successful_startup_log();
    assert_eq!(
        follow_up_pre_paste_offset(Some(startup.as_bytes())).unwrap(),
        startup.len()
    );
    assert_eq!(follow_up_pre_paste_offset(Some(b"")).unwrap(), 0);
    let error = follow_up_pre_paste_offset(None).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("agy.log is missing before the console paste"));
    assert!(message.contains("the follow-up was not pasted"));
}

// The launcher path for a lost initial paste: the gate passes (here the startup
// log of session-fMqSQc stays static, so its quiet period ends), the paste is
// followed by the trust reload and no receipt, as it was when it met the trust
// dialog, and the launcher records the delivery-uncertain reason without touching
// the claim or the composer.
#[test]
fn lost_initial_paste_ends_delivery_uncertain_with_the_reason_in_status_json() {
    use crate::native::session::test_support::TURN_CLAIM_FILE;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-fMqSQc");
    fs::create_dir_all(directory.join("events")).unwrap();
    update_status(&directory, SessionState::AwaitingInitialInput, None, None).unwrap();
    let mut claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let pending = install_pending_turn(&directory, claim.token()).unwrap();

    let startup = late_reload_startup_log();
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let pre_paste_len = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup)]),
        start + Duration::from_secs(300),
        WINDOWS_STARTUP_READINESS_TIMING,
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
    // The read that passed the gate is the pre-paste offset: the whole startup
    // log. The launcher marks the session working before the paste.
    assert_eq!(pre_paste_len, startup.len());
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let pasted_at = clock.now();
    let after_paste = startup.clone() + &late_reload_completion();
    let failure = confirm_input_receipt_with(
        &mut log_sequence(vec![some_log(&startup), some_log(&after_paste)]),
        &pending,
        pre_paste_len,
        pasted_at,
        start + Duration::from_secs(300),
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap_err();
    assert!(failure.delivery_may_have_occurred());

    {
        let delivery_error = failure.error();
        let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
            turn::Delivery::Uncertain(delivery_error)
        } else {
            turn::Delivery::NotSent(delivery_error)
        });
    };
    drop(claim);

    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
    let error = status
        .error
        .expect("the reason is recorded in status.error");
    assert!(error.contains(&format!(
        "Agy input receipt for turn marker {} was not confirmed",
        pending.marker
    )));
    assert!(error.contains("no HandleUserInput receipt in the"));
    assert!(error.contains("non-delivery cannot be proven"));
    assert!(error.contains("the console paste may have been accepted and is not repeated"));
    assert!(
        directory.join(TURN_CLAIM_FILE).exists(),
        "the turn claim stays until the caller closes or the target completes"
    );
    assert!(read_pending_turn(&directory).unwrap().is_some());
}

// The launcher path for the smoke's lost macOS follow-up, which had no trust
// evidence: with the log as it stood before the paste the log-only gate is ready
// after one quiet period, the paste lands on the trust dialog, Agy logs the trust
// reload and no receipt, and the follow-up is recorded delivery-uncertain with the
// claim retained.
#[test]
fn lost_macos_follow_up_paste_ends_delivery_uncertain_with_the_reason_in_status_json() {
    use crate::native::session::test_support::TURN_CLAIM_FILE;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-QMFk6F");
    fs::create_dir_all(directory.join("events")).unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    // A follow-up claims the ready session before it prepares the paste.
    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    let mut claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let pending = install_pending_turn(&directory, claim.token()).unwrap();

    let before_reload = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
        .rfind("I0924 21:32:19.637013")
        .unwrap()];
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let pre_paste_len = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(before_reload)]),
        start + Duration::from_secs(300),
        MACOS_FOLLOW_UP_READINESS_TIMING,
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap();
    assert_eq!(
        clock.slept, STARTUP_QUIET_PERIOD,
        "a static macOS log waits one quiet period; it cannot show the trust dialog"
    );
    assert_eq!(pre_paste_len, before_reload.len());
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let pasted_at = clock.now();
    let failure = confirm_input_receipt_with(
        &mut log_sequence(vec![
            some_log(before_reload),
            some_log(REAL_MACOS_INITIAL_TURN),
        ]),
        &pending,
        pre_paste_len,
        pasted_at,
        start + Duration::from_secs(300),
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap_err();
    assert!(failure.delivery_may_have_occurred());

    {
        let delivery_error = failure.error();
        let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
            turn::Delivery::Uncertain(delivery_error)
        } else {
            turn::Delivery::NotSent(delivery_error)
        });
    };
    drop(claim);

    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
    let error = status
        .error
        .expect("the reason is recorded in status.error");
    assert!(error.contains(&format!(
        "Agy input receipt for turn marker {} was not confirmed",
        pending.marker
    )));
    assert!(error.contains("no HandleUserInput receipt in the"));
    assert!(
        directory.join(TURN_CLAIM_FILE).exists(),
        "the turn claim stays until the caller closes or the target completes"
    );
    assert!(read_pending_turn(&directory).unwrap().is_some());
}
