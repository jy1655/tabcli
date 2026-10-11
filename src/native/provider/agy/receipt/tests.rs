use super::super::delivery::{glog_timestamp, input_receipt_window_end};
use super::super::readiness::SKILLS_RELOAD_MARKER;
use super::super::test_support::{
    FakeClock, REAL_MACOS_INITIAL_TURN, REAL_STARTUP_RELOAD_AFTER_STARTUP,
    REAL_SUCCESS_AFTER_RECEIPT, REAL_SUCCESS_STARTUP, REAL_TRUST_RELOAD_AFTER_PASTE,
    framed_receipt_line, go_quoted, late_reload_completion, late_reload_startup_log, log_sequence,
    receipt_line, settled_startup_log, some_log, successful_startup_log,
};
use super::super::{INPUT_RECEIPT_WINDOW, PendingAgyTurn, terminal_correlated_prompt};
use super::*;
use anyhow::Result;
use std::time::{Duration, Instant};

// session-udT6uY line 156: the 2,731 byte receipt of the delivered paste, abridged
// here after the protocol prefix, the marker and the start of the JSON request.
// The original line continues with the Go-quoted request and ends with `\""`.
const REAL_SUCCESS_RECEIPT_HEAD: &str = r#"I0924 16:42:50.212542     595 input_loop.go:107] HandleUserInput called with text: "[Agent Bridge Agy Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs. Complete it as one turn. End the complete final response with the exact marker <!-- agent-bridge-agy-turn:28404-1790235743098225800-0 --> on its own final line; do not alter or omit it. Request JSON: \"[Agent Bridge native delegation]\\nSource: external"#;

const REAL_SUCCESS_TOKEN: &str = "28404-1790235743098225800-0";

// Round 8's receipt window, which session-M8QFPp's 18.0 s receipt outlasted.
const ROUND_8_RECEIPT_WINDOW: Duration = Duration::from_secs(15);

fn long_markdown_prompt() -> String {
    let mut prompt = String::from(
        "[Agent Bridge native delegation]\nSource: external\n\nYou are the prose writer for a documentation task. The author has already decided everything about the content. Your job is expression only.\n\n## Input\n\n- `brief.md` in this directory is the Content Brief.\n",
    );
    while prompt.len() < 1600 {
        prompt.push_str("- Keep every claim from the brief; do not add \"facts\" or C:\\paths.\n");
    }
    prompt
}

#[test]
fn input_receipt_is_recognised_after_an_st_terminated_osc_sequence() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let receipt = framed_receipt_line("hello", &pending);
    let decorated = format!("\u{1b}]0;agy\u{1b}\\{receipt}");
    assert_eq!(
        observe_input_receipt(Some(decorated.as_bytes()), 0, &pending),
        ReceiptEvidence::Delivered
    );
    let bel = format!("\u{1b}]0;agy\u{7}{receipt}");
    assert_eq!(
        observe_input_receipt(Some(bel.as_bytes()), 0, &pending),
        ReceiptEvidence::Delivered
    );
}

// session-M8QFPp: the paste at 20:37:49.15 was accepted, and Agy logged its
// HandleUserInput receipt at 20:38:07.152165, 18.0 s later, under heavy CPU
// load. The former 15 s receipt window had ended, so the adapter reported
// delivery-uncertain for a delivered paste; the 60 s window sees the receipt.
#[test]
fn receipt_watch_outlasts_the_slow_processing_observed_under_load() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let startup = REAL_STARTUP_RELOAD_AFTER_STARTUP.to_owned();
    let receipt_latency = glog_timestamp("I0924 20:38:07.152165     595 input_loop.go:107] x")
        .unwrap()
        - glog_timestamp("I0924 20:37:49.146941       1 x.go:1] x").unwrap();
    assert!(receipt_latency > ROUND_8_RECEIPT_WINDOW);
    assert!(receipt_latency < INPUT_RECEIPT_WINDOW);
    let framed = terminal_correlated_prompt("hello", &pending, true).unwrap();
    let receipt = format!(
        "I0924 20:38:07.152165     595 input_loop.go:107] HandleUserInput called with text: {}\n",
        go_quoted(&framed)
    );
    let pasted_at = Instant::now();
    let mut clock = FakeClock::new(pasted_at);
    let now = clock.shared();
    let mut read_log = {
        let startup = startup.clone();
        move || -> Result<Option<Vec<u8>>> {
            if now.get().saturating_duration_since(pasted_at) < receipt_latency {
                some_log(&startup)
            } else {
                some_log(&(startup.clone() + &receipt))
            }
        }
    };
    confirm_input_receipt_with(
        &mut read_log,
        &pending,
        startup.len(),
        pasted_at,
        pasted_at + Duration::from_secs(300),
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap();
    assert!(clock.slept >= receipt_latency);
    assert!(clock.slept < receipt_latency + Duration::from_millis(100));
    assert!(
        clock.slept > ROUND_8_RECEIPT_WINDOW,
        "the round-8 window had ended before the receipt"
    );
}

// Live observation (session-udT6uY and the round-7 asks): the skills reload that
// Agy logs milliseconds after `HandleUserInput` and the conversation-start lines
// is triggered by the new conversation, not the deferred startup reload. The
// receipt watch returns on the first read that holds the receipt, so the reload
// can neither delay nor revoke the delivered classification.
#[test]
fn receipt_stays_delivered_when_the_conversation_reload_follows_it() {
    let pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
    let startup = successful_startup_log();
    let with_receipt = format!("{startup}{REAL_SUCCESS_RECEIPT_HEAD}\n");
    let with_reload = format!("{with_receipt}{REAL_SUCCESS_AFTER_RECEIPT}");
    assert!(with_reload[with_receipt.len()..].contains(SKILLS_RELOAD_MARKER));
    assert_eq!(
        observe_input_receipt(Some(with_reload.as_bytes()), startup.len(), &pending),
        ReceiptEvidence::Delivered
    );

    let pasted_at = Instant::now();
    let mut clock = FakeClock::new(pasted_at);
    confirm_input_receipt_with(
        &mut log_sequence(vec![some_log(&startup), some_log(&with_reload)]),
        &pending,
        startup.len(),
        pasted_at,
        pasted_at + Duration::from_secs(120),
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, Duration::from_millis(100));
}

#[test]
fn input_receipt_for_a_long_prompt_carries_the_whole_marker_near_the_front() {
    let pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
    let prompt = long_markdown_prompt();
    assert!(prompt.len() >= 1600);
    let framed = terminal_correlated_prompt(&prompt, &pending, true).unwrap();
    let quoted = go_quoted(&framed);
    let line = receipt_line(&quoted);
    // Agy 1.2.10 logged this shape unabridged for a 1.6 KB prompt: a 2.7 KB line
    // ending in the quoted request's closing `\"` plus the Go closing quote.
    assert!(line.len() > framed.len());
    assert!(line.trim_end().ends_with("\\\"\""));
    let marker_at = line.find(&pending.marker).unwrap();
    let prefix_at = line.find(WINDOWS_PROTOCOL_PREFIX).unwrap();
    assert!(prefix_at < marker_at && marker_at < 400);

    let receipts = input_receipts(line.as_bytes());
    assert_eq!(receipts.len(), 1);
    assert!(!receipts[0].truncated);
    assert_eq!(receipts[0].text, quoted[1..quoted.len() - 1]);
    assert!(receipt_matches(&receipts[0], &pending));
    assert_eq!(
        observe_input_receipt(Some(line.as_bytes()), 0, &pending),
        ReceiptEvidence::Delivered
    );

    // The real receipt head starts with the same prefix and marker.
    let real = format!("{REAL_SUCCESS_RECEIPT_HEAD}\n");
    let receipts = input_receipts(real.as_bytes());
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].truncated, "the fixture is abridged");
    assert!(receipt_matches(&receipts[0], &pending));
    let real_marker_at = REAL_SUCCESS_RECEIPT_HEAD.find(&pending.marker).unwrap();
    let real_prefix_at = REAL_SUCCESS_RECEIPT_HEAD
        .find(WINDOWS_PROTOCOL_PREFIX)
        .unwrap();
    assert!(real_prefix_at < real_marker_at && real_marker_at < 400);

    let legacy = format!(
        "ERROR: logging before google.Init: I0827 22:11:28.641211     406 input_loop.go:36] HandleUserInput called with text: {}\n",
        go_quoted(&framed)
    );
    assert_eq!(
        observe_input_receipt(Some(legacy.as_bytes()), 0, &pending),
        ReceiptEvidence::Delivered
    );

    let manual = receipt_line(&go_quoted(&format!("please finish {}", pending.marker)));
    assert_eq!(
        observe_input_receipt(Some(manual.as_bytes()), 0, &pending),
        ReceiptEvidence::NoReceipt {
            appended: manual.len(),
            partial_tail: false
        },
        "a manual turn without the protocol prefix is not this delivery"
    );
}

#[test]
fn input_receipt_requires_the_complete_marker_and_never_a_colliding_prefix() {
    let old = PendingAgyTurn::new("28404-1790235743098225800-1").unwrap();
    let new = PendingAgyTurn::new("28404-1790235743098225800-10").unwrap();
    assert!(new.marker.starts_with(old.marker.trim_end_matches(" -->")));

    // Turn 1 was delivered and its receipt is complete.
    let old_receipt = framed_receipt_line("first", &old);
    let log = successful_startup_log() + &old_receipt;
    assert_eq!(
        observe_input_receipt(Some(log.as_bytes()), 0, &old),
        ReceiptEvidence::Delivered
    );
    assert_eq!(
        observe_input_receipt(Some(log.as_bytes()), 0, &new),
        ReceiptEvidence::NoReceipt {
            appended: log.len(),
            partial_tail: false
        },
        "the complete old marker never confirms the new turn"
    );

    // Turn 10 is pasted after the offset; the old receipt is before it.
    let pre_paste_len = log.len();
    assert_eq!(
        observe_input_receipt(Some(log.as_bytes()), pre_paste_len, &new),
        ReceiptEvidence::NoReceipt {
            appended: 0,
            partial_tail: false
        }
    );
    // A receipt that Agy cut right after the shared token prefix confirms neither
    // turn: the complete marker is not visible.
    let old_framed = terminal_correlated_prompt("first", &old, true).unwrap();
    let old_quoted = go_quoted(&old_framed);
    let cut = old_quoted.find(&old.marker).unwrap() + old.marker.len() - " -->".len();
    let truncated_old = receipt_line(&format!("{}...\"", &old_quoted[..cut]));
    assert!(truncated_old.contains("-1...\""));
    let receipts = input_receipts(truncated_old.as_bytes());
    assert!(receipts[0].truncated);
    assert!(!receipt_matches(&receipts[0], &old));
    assert!(!receipt_matches(&receipts[0], &new));
    let with_truncated = log.clone() + &truncated_old;
    assert_eq!(
        observe_input_receipt(Some(with_truncated.as_bytes()), pre_paste_len, &new),
        ReceiptEvidence::NoReceipt {
            appended: truncated_old.len(),
            partial_tail: false
        }
    );
    assert_eq!(
        observe_input_receipt(Some(with_truncated.as_bytes()), pre_paste_len, &old),
        ReceiptEvidence::NoReceipt {
            appended: truncated_old.len(),
            partial_tail: false
        }
    );

    // Only the complete new marker after the offset is the new turn's receipt.
    let new_receipt = framed_receipt_line("second", &new);
    let delivered = log.clone() + &new_receipt;
    assert_eq!(
        observe_input_receipt(Some(delivered.as_bytes()), pre_paste_len, &new),
        ReceiptEvidence::Delivered
    );
    // The same marker before the offset is not evidence for this submission.
    assert_eq!(
        observe_input_receipt(Some(delivered.as_bytes()), delivered.len(), &new),
        ReceiptEvidence::NoReceipt {
            appended: 0,
            partial_tail: false
        }
    );
    let different = new_receipt.replace("-10 -->", "-17 -->");
    let other = log + &different;
    assert_eq!(
        observe_input_receipt(Some(other.as_bytes()), pre_paste_len, &new),
        ReceiptEvidence::NoReceipt {
            appended: different.len(),
            partial_tail: false
        }
    );
}

#[test]
fn input_receipt_evidence_only_counts_lines_that_start_after_the_offset() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let receipt = framed_receipt_line("hello", &pending);

    // The pre-paste snapshot ended inside an unrelated line.
    let snapshot = successful_startup_log()
        + "I0924 16:42:49.000000     590 quota_manager.go:45] doRefreshQuota";
    let log = snapshot.clone() + ": starting reload (force=false)\n" + &receipt;
    assert_eq!(
        evidence_start(log.as_bytes(), snapshot.len()),
        Some(snapshot.len() + ": starting reload (force=false)\n".len())
    );
    assert_eq!(
        observe_input_receipt(Some(log.as_bytes()), snapshot.len(), &pending),
        ReceiptEvidence::Delivered
    );
    let unfinished = snapshot.clone() + ": starting";
    assert_eq!(evidence_start(unfinished.as_bytes(), snapshot.len()), None);
    assert_eq!(
        observe_input_receipt(Some(unfinished.as_bytes()), snapshot.len(), &pending),
        ReceiptEvidence::NoReceipt {
            appended: ": starting".len(),
            partial_tail: true
        }
    );

    // A line that straddles the offset started before the paste and is never
    // evidence, even when its tail carries the marker.
    let (head, tail) =
        receipt.split_at(receipt.find(INPUT_RECEIPT_MARKER).unwrap() + INPUT_RECEIPT_MARKER.len());
    let snapshot = successful_startup_log() + head;
    let straddling = snapshot.clone() + tail;
    assert_eq!(
        observe_input_receipt(Some(straddling.as_bytes()), 0, &pending),
        ReceiptEvidence::Delivered
    );
    assert_eq!(
        observe_input_receipt(Some(straddling.as_bytes()), snapshot.len(), &pending),
        ReceiptEvidence::NoReceipt {
            appended: tail.len(),
            partial_tail: false
        }
    );

    // Real sequence: the readiness startup, then the delivered receipt.
    let real =
        format!("{REAL_SUCCESS_STARTUP}{REAL_SUCCESS_RECEIPT_HEAD}\n{REAL_SUCCESS_AFTER_RECEIPT}");
    let real_pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
    assert_eq!(
        observe_input_receipt(
            Some(real.as_bytes()),
            REAL_SUCCESS_STARTUP.len(),
            &real_pending
        ),
        ReceiptEvidence::Delivered
    );
    assert_eq!(
        observe_input_receipt(Some(real.as_bytes()), real.len(), &real_pending),
        ReceiptEvidence::NoReceipt {
            appended: 0,
            partial_tail: false
        }
    );
}

#[test]
fn receipt_watch_confirms_only_a_complete_receipt_after_the_offset() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let startup = successful_startup_log();
    let receipt = framed_receipt_line("hello", &pending);
    let pasted_at = Instant::now();
    let deadline = pasted_at + Duration::from_secs(120);
    let poll = Duration::from_millis(100);

    let mut clock = FakeClock::new(pasted_at);
    confirm_input_receipt_with(
        &mut log_sequence(vec![
            some_log(&startup),
            some_log(&(startup.clone() + &receipt)),
        ]),
        &pending,
        startup.len(),
        pasted_at,
        deadline,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, poll);

    let now = Instant::now();
    assert_eq!(
        input_receipt_window_end(now, now + Duration::from_secs(60)),
        now + INPUT_RECEIPT_WINDOW
    );
    assert_eq!(
        input_receipt_window_end(now, now + Duration::from_secs(3)),
        now + Duration::from_secs(3)
    );
}

#[test]
fn receipt_watch_never_reports_not_sent_after_the_paste() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let startup = successful_startup_log();
    let receipt = framed_receipt_line("hello", &pending);
    let pasted_at = Instant::now();
    let deadline = pasted_at + Duration::from_secs(120);
    let poll = Duration::from_secs(5);
    let uncertain = |logs: Vec<Result<Option<Vec<u8>>>>,
                     pre_paste_len: usize,
                     deadline: Instant|
     -> (String, FakeClock) {
        let mut clock = FakeClock::new(pasted_at);
        let failure = confirm_input_receipt_with(
            &mut log_sequence(logs),
            &pending,
            pre_paste_len,
            pasted_at,
            deadline,
            poll,
            &mut clock,
        )
        .unwrap_err();
        assert!(
            failure.delivery_may_have_occurred(),
            "a paste without a receipt is never not_sent"
        );
        let message = format!("{:#}", failure.error());
        assert!(message.contains(&pending.marker));
        assert!(message.contains("may have been accepted and is not repeated"));
        (message, clock)
    };

    // Continuous, complete log with no receipt for the whole window: no marker
    // proves the input was drained, so non-delivery is not proven.
    let (message, clock) = uncertain(vec![some_log(&startup)], startup.len(), deadline);
    assert!(message.contains("no HandleUserInput receipt in the 0 bytes appended"));
    assert!(message.contains("within 60 seconds"));
    assert!(message.contains("non-delivery cannot be proven"));
    assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);

    // The real failure log: the late reload after the discarded paste is the only
    // thing Agy wrote, and it is not a receipt.
    let failure_log = late_reload_startup_log();
    let (message, _) = uncertain(
        vec![some_log(&(failure_log.clone() + &late_reload_completion()))],
        failure_log.len(),
        deadline,
    );
    assert!(message.contains(&format!(
        "no HandleUserInput receipt in the {} bytes appended",
        late_reload_completion().len()
    )));
    assert!(message.contains("non-delivery cannot be proven"));

    // A receipt appended between the last read (the 13th, at 60 s with a 5 s poll)
    // and the deadline check is not
    // seen; the outcome is still uncertain, never not_sent.
    let mut reads = 0;
    let receipt_after_last_read = {
        let startup = startup.clone();
        let receipt = receipt.clone();
        move || {
            reads += 1;
            if reads <= 13 {
                some_log(&startup)
            } else {
                some_log(&(startup.clone() + &receipt))
            }
        }
    };
    let mut clock = FakeClock::new(pasted_at);
    let mut read_log = receipt_after_last_read;
    let failure = confirm_input_receipt_with(
        &mut read_log,
        &pending,
        startup.len(),
        pasted_at,
        deadline,
        poll,
        &mut clock,
    )
    .unwrap_err();
    assert!(failure.delivery_may_have_occurred());
    assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);
    assert_eq!(
        observe_input_receipt(read_log().unwrap().as_deref(), startup.len(), &pending),
        ReceiptEvidence::Delivered,
        "the receipt landed right after the last read"
    );

    // A deadline-capped window that ends before 60 s elapsed.
    let (message, clock) = uncertain(
        vec![some_log(&startup)],
        startup.len(),
        pasted_at + Duration::from_secs(3),
    );
    assert!(
        message.contains("the deadline ended the receipt window after 3 of the 60 second window")
    );
    assert_eq!(clock.slept, Duration::from_secs(3));

    // Rotation or truncation below the pre-paste offset: uncertain at once.
    let (message, clock) = uncertain(
        vec![some_log(&startup[..startup.len() / 2])],
        startup.len(),
        deadline,
    );
    assert!(message.contains("rotated or truncated"));
    assert!(message.contains(&format!("below the pre-paste offset {}", startup.len())));
    assert_eq!(clock.slept, Duration::ZERO);

    // A partial trailing line may still become the receipt.
    let torn = startup.clone() + receipt.trim_end_matches('\n');
    let (message, _) = uncertain(vec![some_log(&torn)], startup.len(), deadline);
    assert!(message.contains("ends with a partial line"));

    // Missing and unreadable logs.
    let (message, clock) = uncertain(vec![Ok(None)], startup.len(), deadline);
    assert!(message.contains("agy.log is missing after the paste"));
    assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);
    let (message, clock) = uncertain(
        vec![Err(anyhow::anyhow!("refusing non-regular session file"))],
        startup.len(),
        deadline,
    );
    assert!(message.contains("agy.log is unreadable"));
    assert_eq!(clock.slept, Duration::ZERO);
}

// The macOS paste is the verbatim `correlated_prompt`; iTerm2 pastes it as one
// multi-line record and Agy logs it Go-quoted with `\n` escapes and the complete
// marker (session-QMFk6F, 21:42:19). A composer line that merely quotes the marker
// carries neither framing and never confirms delivery.
#[test]
fn macos_receipt_matches_the_verbatim_framed_follow_up() {
    let pending = PendingAgyTurn::new("20087-1790253138785843000-0").unwrap();
    let framed = terminal_correlated_prompt(
        "Reply with exactly this marker and nothing else: AGENT_BRIDGE_NATIVE_AGY_RESULT_OK_DETACHED",
        &pending,
        false,
    )
    .unwrap();
    assert!(framed.contains(TURN_PROTOCOL_HEADER));
    assert!(!framed.contains(WINDOWS_PROTOCOL_PREFIX));
    let quoted = go_quoted(&framed);
    assert!(quoted.contains("\\n[Agent Bridge Agy turn protocol]\\n"));
    let pre_paste_len = REAL_MACOS_INITIAL_TURN.len();
    let delivered = REAL_MACOS_INITIAL_TURN.to_owned() + &receipt_line(&quoted);
    assert_eq!(
        observe_input_receipt(Some(delivered.as_bytes()), pre_paste_len, &pending),
        ReceiptEvidence::Delivered
    );
    let typed = REAL_MACOS_INITIAL_TURN.to_owned()
        + &receipt_line(&go_quoted(&format!("please echo {}", pending.marker)));
    assert!(matches!(
        observe_input_receipt(Some(typed.as_bytes()), pre_paste_len, &pending),
        ReceiptEvidence::NoReceipt { .. }
    ));
    // The Windows framing still matches, so a Windows session's receipt is judged
    // by the same rule.
    let windows = REAL_MACOS_INITIAL_TURN.to_owned() + &framed_receipt_line("x", &pending);
    assert_eq!(
        observe_input_receipt(Some(windows.as_bytes()), pre_paste_len, &pending),
        ReceiptEvidence::Delivered
    );
}

#[test]
fn a_paste_onto_the_trust_dialog_leaves_only_the_trust_reload_and_no_receipt() {
    let pending = PendingAgyTurn::new("21521-1790839736415840000-1").unwrap();
    let before = settled_startup_log();
    let after = before.clone() + REAL_TRUST_RELOAD_AFTER_PASTE;
    let evidence = observe_input_receipt(Some(after.as_bytes()), before.len(), &pending);
    assert_eq!(
        evidence,
        ReceiptEvidence::NoReceipt {
            appended: 356,
            partial_tail: false
        }
    );
    let message = unconfirmed_receipt_error(
        &evidence,
        &pending,
        before.len(),
        INPUT_RECEIPT_WINDOW,
        true,
    )
    .to_string();
    assert!(
        message.contains("no HandleUserInput receipt in the 356 bytes appended"),
        "{message}"
    );
}
