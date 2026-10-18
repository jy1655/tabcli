use super::*;
use crate::native::session::{SessionState, Store, update_status};

fn frame(text: &str) -> String {
    format!("› {}\n\n  100% context left\n", text.replace('\n', "\n  "))
}
fn evidence(rendered: &str, expected: &str) -> Observation {
    composer_segments(&frame(rendered)).map_or(Observation::Ambiguous, |s| observe(&s, expected))
}
#[test]
fn parser_ordered_placeholder_inline_and_unicode_fixtures() {
    for n in [1001, 1500] {
        let expected = format!("{}{}", "a".repeat(n), "b".repeat(1000));
        assert_eq!(
            evidence(
                &format!("[Pasted Content {n} chars]{}", "b".repeat(1000)),
                &expected
            ),
            Observation::Complete
        );
    }
    for text in ["a".repeat(2500), "한글😀👩\u{200d}💻e\u{301}全角".into()] {
        assert_eq!(evidence(&text, &text), Observation::Complete);
    }
    assert_eq!(
        evidence("[Pasted Content 1200 chars]", &"a".repeat(1200)),
        Observation::Complete
    );
    assert_eq!(
        evidence(
            "[Pasted Content 1200 chars]\n[Pasted Content 1200 chars] #2",
            &format!("{}\n{}", "a".repeat(1200), "b".repeat(1200))
        ),
        Observation::Complete
    );
    let marker = "<!-- agent-bridge-codex-turn:claim -->";
    assert_eq!(
        evidence(
            &format!("header[Pasted Content 1200 chars]\n{marker}"),
            &format!("header{}\n{marker}", "a".repeat(1200))
        ),
        Observation::Complete
    );
}
#[test]
fn parser_literal_labels_and_clipped_labels_are_ambiguous_and_dialogs_veto() {
    let cases: [(String, String); 4] = [
        (
            "[Pasted Content 1200 chars][Pasted Content 2000 chars]".into(),
            format!("{}[Pasted Content 2000 chars]", "x".repeat(1200)),
        ),
        ("[Pasted Content\n1200 chars]".into(), "x".repeat(1200)),
        ("[Pasted Content 1200 cha".into(), "x".repeat(1200)),
        ("header wr\napped".into(), "header wrapped".into()),
    ];
    for (shown, expected) in cases {
        assert_ne!(
            evidence(&shown, &expected),
            Observation::Complete,
            "{shown}"
        );
    }
    for dialog in [
        "Trust this folder?",
        "Trust and continue",
        "Review hooks",
        "Trust all and continue",
    ] {
        assert!(
            composer_segments(&format!(
                "{}\n{dialog}",
                frame("[Pasted Content 1200 chars]")
            ))
            .is_err()
        );
        assert_eq!(
            classification(Ok(format!(
                "{}\n{dialog}",
                frame("Ask Codex to do anything")
            ))),
            "blocked"
        );
    }
    assert!(composer_segments("").is_err());
    assert_eq!(
        classification(Ok(frame("Ask Codex to do anything"))),
        "positively empty"
    );
    assert!(classification(Err(anyhow::anyhow!("failed read"))).starts_with("unreadable"));
}

struct Fake {
    now: Instant,
    calls: Vec<&'static str>,
    screens: std::collections::VecDeque<String>,
    fallback: String,
    last_screen: String,
    bound: bool,
    changed: bool,
    fail_text: Option<usize>,
    texts: usize,
    writes: Vec<String>,
    fault: Option<&'static str>,
    invalidate_at: Option<usize>,
    checks: usize,
    expire_read: bool,
    count_mismatch: bool,
    root: tempfile::TempDir,
    token: String,
    ended: bool,
}
impl Fake {
    fn new() -> (Self, turn::Claim, String) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("events")).unwrap();
        update_status(root.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(root.path()), &[]).unwrap();
        let token = claim.token().to_owned();
        let prompt = format!("body\n<!-- agent-bridge-codex-turn:{token} -->");
        (
            Self {
                now: Instant::now(),
                calls: vec![],
                screens: [frame(&prompt)].into(),
                fallback: frame("Ask Codex to do anything"),
                last_screen: String::new(),
                bound: true,
                changed: false,
                fail_text: None,
                texts: 0,
                writes: vec![],
                fault: None,
                invalidate_at: None,
                checks: 0,
                expire_read: false,
                count_mismatch: false,
                root,
                token,
                ended: false,
            },
            claim,
            prompt,
        )
    }
    fn execute(&mut self, prompt: &str) -> TerminalSendResult {
        let token = self.token.clone();
        run(self, prompt, &token, self.now + Duration::from_secs(21))
    }
}
impl Input for Fake {
    fn now(&mut self) -> Instant {
        self.now
    }
    fn sleep(&mut self, duration: Duration) {
        self.now += duration;
    }
    fn current(&mut self) -> Result<()> {
        self.checks += 1;
        if self.invalidate_at == Some(self.checks) {
            if self.ended {
                update_status(self.root.path(), SessionState::Closed, None, None)?;
            } else {
                std::fs::remove_file(self.root.path().join("turn.claim"))?;
                Store::open_unchecked(self.root.path())
                    .private("turn.claim")
                    .write_private(b"1-2-3")?;
            }
        }
        let reader = Reader::open_unchecked(self.root.path());
        if turn::current_claim_token(&reader)?.as_deref() != Some(&self.token) {
            bail!("claim changed");
        }
        if reader.status()?.state.ended() {
            bail!("session ended");
        }
        Ok(())
    }
    fn screen(&mut self) -> Result<String> {
        self.calls.push("screen");
        if self.fault == Some("read")
            || (self.fault == Some("final_read") && self.screens.is_empty())
        {
            bail!("screen read refused");
        }
        if self.expire_read {
            self.now += Duration::from_secs(30);
        }
        self.last_screen = self
            .screens
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        Ok(self.last_screen.clone())
    }
    fn text(&mut self, text: &str) -> std::result::Result<usize, Failure> {
        self.calls.push("text");
        self.texts += 1;
        self.writes.push(text.to_owned());
        if self.fault == Some("body_started") {
            return Err(Failure::delivery_uncertain(anyhow::anyhow!(
                "helper started then failed"
            )));
        }
        if self.fault == Some("tail_count") && self.texts == 2 {
            return Ok(0);
        }
        if self.fail_text == Some(self.texts) {
            return Err(Failure::not_sent(anyhow::anyhow!(
                "helper refused before write"
            )));
        }
        Ok(text.chars().count() + usize::from(self.count_mismatch))
    }
    fn submit(&mut self, screen: &str) -> Result<bool> {
        self.calls.push("submit");
        if self.fault == Some("submit") {
            bail!("submit helper could not start");
        }
        assert_eq!(screen, self.last_screen);
        Ok(!self.changed)
    }
    fn bound(&mut self) -> Result<bool> {
        self.calls.push("binding");
        if self.fault == Some("binding_deadline") {
            self.now += Duration::from_secs(30);
        }
        if self.bound {
            Store::open_unchecked(self.root.path())
                .private(&format!("codex-turn-{}.json", self.token))
                .write_json(&serde_json::json!({
                    "schema":1, "claim_token":self.token,
                    "thread_id":"01234567-89ab-cdef-0123-456789abcdef",
                    "turn_id":"bound", "transcript_path":"rollout.jsonl", "bound_unix_ms":1
                }))?;
        }
        Ok(super::super::super::lifecycle::read_binding(self.root.path(), &self.token)?.is_some())
    }
}
fn settle(mut claim: turn::Claim, result: TerminalSendResult) {
    match result {
        Ok(()) => claim.settle_delivery(turn::Delivery::Sent).unwrap(),
        Err(e) if e.delivery_may_have_occurred() => claim
            .settle_delivery(turn::Delivery::Uncertain(e.error()))
            .unwrap(),
        Err(e) => claim
            .settle_delivery(turn::Delivery::NotSent(e.error()))
            .unwrap(),
    }
}
#[test]
fn protocol_success_orders_body_tail_guard_and_binding() {
    let (mut io, claim, prompt) = Fake::new();
    let result = io.execute(&prompt);
    assert!(result.is_ok());
    assert_eq!(io.calls, ["text", "text", "screen", "submit", "binding"]);
    assert_eq!(
        io.writes,
        [
            "body".to_owned(),
            format!("\n{}", prompt.rsplit_once('\n').unwrap().1)
        ]
    );
    settle(claim, result);
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(io.root.path()))
            .unwrap()
            .as_deref(),
        Some(io.token.as_str())
    );
}
#[test]
fn protocol_failures_are_sticky_and_never_replay() {
    for case in [
        "short", "dialog", "tail", "changed", "binding", "blocking", "count",
    ] {
        let (mut io, claim, prompt) = Fake::new();
        match case {
            "short" => {
                io.screens.clear();
                io.fallback = frame("bo");
            }
            "dialog" => {
                io.screens = [format!("{}\nReview hooks", frame("body"))].into();
            }
            "tail" => io.fail_text = Some(2),
            "changed" => io.changed = true,
            "binding" => io.bound = false,
            "blocking" => io.expire_read = true,
            "count" => io.count_mismatch = true,
            _ => unreachable!(),
        }
        let error = io.execute(&prompt).unwrap_err();
        assert!(error.delivery_may_have_occurred(), "{case}");
        if case == "binding" {
            assert!(error.error().to_string().contains("positively empty"));
        }
        if ["short", "dialog", "tail", "blocking", "count"].contains(&case) {
            assert!(!io.calls.contains(&"submit"));
        }
        if case == "dialog" {
            assert_eq!(io.texts, 2);
        }
        settle(claim, Err(error));
        let reader = Reader::open_unchecked(io.root.path());
        assert_eq!(
            turn::current_claim_token(&reader).unwrap().as_deref(),
            Some(io.token.as_str())
        );
        assert!(reader.status().unwrap().error.is_some(), "{case}");
        // Late evidence is a record change, not a retry of the protocol.
        let before = io.calls.clone();
        io.bound = true;
        Store::open_unchecked(io.root.path()).private(&format!("codex-turn-{}.json", io.token)).write_json(&serde_json::json!({
            "schema":1, "claim_token":io.token, "thread_id":"01234567-89ab-cdef-0123-456789abcdef", "turn_id":"late", "transcript_path":"rollout.jsonl", "bound_unix_ms":1
        })).unwrap();
        assert!(
            super::super::super::lifecycle::read_binding(io.root.path(), &io.token)
                .unwrap()
                .is_some()
        );
        assert_eq!(io.calls, before);
    }
}
#[test]
fn protocol_claim_replaced_or_session_ended_at_every_check() {
    let (mut success, claim, prompt) = Fake::new();
    success.execute(&prompt).unwrap();
    let checks = success.checks;
    drop(claim);
    for ended in [false, true] {
        for step in 1..=checks {
            let (mut io, claim, prompt) = Fake::new();
            io.invalidate_at = Some(step);
            io.ended = ended;
            let error = io.execute(&prompt).unwrap_err();
            assert_eq!(error.delivery_may_have_occurred(), step > 1, "step {step}");
            if step == 1 {
                assert_eq!(io.texts, 0);
            }
            assert!(io.calls.iter().filter(|c| **c == "submit").count() <= 1);
            settle(claim, Err(error));
            let reader = Reader::open_unchecked(io.root.path());
            if !ended {
                assert_eq!(
                    turn::current_claim_token(&reader).unwrap().as_deref(),
                    Some("1-2-3")
                );
            } else {
                assert_eq!(reader.status().unwrap().state, SessionState::Closed);
            }
        }
    }
}
#[test]
fn protocol_reserve_and_proven_prewrite_refusal_release_claim() {
    for reserve in [true, false] {
        let (mut io, claim, prompt) = Fake::new();
        io.fail_text = Some(1);
        let token = io.token.clone();
        let deadline = io.now + Duration::from_secs(if reserve { 19 } else { 21 });
        let error = run(&mut io, &prompt, &token, deadline).unwrap_err();
        assert!(!error.delivery_may_have_occurred());
        assert_eq!(io.texts, usize::from(!reserve));
        settle(claim, Err(error));
        assert!(
            turn::current_claim_token(&Reader::open_unchecked(io.root.path()))
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn unrelated_provider_routes_keep_send_file_and_single_return_facts() {
    // Pin the dispatch seam as well as the record-level Windows tests from R1.
    use agent_bridge::FirstPartyCli;
    for provider in [FirstPartyCli::Agy, FirstPartyCli::Pi] {
        let follow_up = crate::native::provider::follow_up_transport(provider);
        let facts = follow_up.facts().surface.unwrap();
        assert_eq!(facts.submit_count, 1);
        assert_eq!(facts.newline, terminal::ConsoleNewline::LineFeed);
        #[cfg(windows)]
        assert_eq!(
            crate::native::provider::initial_transport(provider)
                .facts()
                .surface
                .unwrap()
                .submit_count,
            1
        );
    }
    for source in [
        include_str!("../../../agy/mod.rs"),
        include_str!("../../../pi/mod.rs"),
    ] {
        assert!(source.contains("terminal::send_file(session, prompt_path, deadline)"));
        assert!(source.contains("submit_count: 1"));
        assert!(!source.contains("send_file_text_only"));
    }
}

#[test]
fn parser_padded_console_rows_preserve_full_width_scalar_counts() {
    let text = "한글全角😀";
    let screen = format!(
        "› {text}                 \n                         \n  ? for shortcuts        \n                         \n"
    );
    assert_eq!(
        observe(&composer_segments(&screen).unwrap(), text),
        Observation::Complete
    );
    assert_eq!(
        evidence("[Pasted Content 5 chars]", text),
        Observation::Complete
    );
    // Character-only capture cannot distinguish payload spaces from console padding.
    assert_ne!(evidence("text  ", "text  "), Observation::Complete);
}
#[test]
fn protocol_collapsed_tail_and_disabled_burst_inline_input() {
    let (mut io, claim, _) = Fake::new();
    let prompt = format!(
        "{}\n<!-- agent-bridge-codex-turn:{} -->",
        "x".repeat(2001),
        io.token
    );
    io.screens = [
        frame(&"x".repeat(2001)),
        frame(&format!(
            "[Pasted Content {} chars]",
            prompt.chars().count()
        )),
    ]
    .into();
    assert!(io.execute(&prompt).is_ok());
    assert_eq!(io.texts, 2);
    claim.retain();
}

#[test]
fn no_binding_diagnostic_distinguishes_all_final_screen_classes() {
    for (screen, expected) in [
        (frame("draft"), "populated"),
        (frame("Ask Codex to do anything"), "positively empty"),
        ("Review hooks".into(), "blocked"),
        (String::new(), "ambiguous"),
    ] {
        let (mut io, claim, prompt) = Fake::new();
        io.bound = false;
        io.fallback = screen;
        let error = io.execute(&prompt).unwrap_err();
        assert!(error.delivery_may_have_occurred());
        assert!(
            error
                .error()
                .to_string()
                .contains(&format!("final composer: {expected}"))
        );
        assert_eq!(io.calls.iter().filter(|c| **c == "submit").count(), 1);
        settle(claim, Err(error));
    }
    assert!(
        include_str!("../../../agy/delivery.rs")
            .contains("|| terminal::send_file(session, prompt_path, deadline)")
    );
    assert!(
        include_str!("../../../pi/startup.rs")
            .contains("terminal::send_file(session, prompt_path, deadline)")
    );
}

#[test]
fn protocol_helper_and_read_failures_keep_post_text_uncertainty() {
    for fault in [
        "body_started",
        "tail_count",
        "read",
        "submit",
        "final_read",
        "binding_deadline",
    ] {
        let (mut io, claim, prompt) = Fake::new();
        io.fault = Some(fault);
        io.bound = false;
        let error = io.execute(&prompt).unwrap_err();
        assert!(error.delivery_may_have_occurred(), "{fault}");
        if fault == "final_read" {
            assert!(
                error
                    .error()
                    .to_string()
                    .contains("final composer: unreadable")
            );
        }
        if fault == "binding_deadline" {
            assert!(error.error().to_string().contains("final composer:"));
        }
        assert!(io.calls.iter().filter(|c| **c == "submit").count() <= 1);
        settle(claim, Err(error));
        assert!(
            Reader::open_unchecked(io.root.path())
                .status()
                .unwrap()
                .error
                .is_some()
        );
        assert_eq!(
            turn::current_claim_token(&Reader::open_unchecked(io.root.path()))
                .unwrap()
                .as_deref(),
            Some(io.token.as_str())
        );
    }
}

#[test]
fn marker_tail_accepts_wrapped_and_scrolled_inline_body() {
    for body_view in ["bo\ndy", "dy", ""] {
        let (mut io, claim, prompt) = Fake::new();
        let tail = prompt.rsplit_once('\n').unwrap().1;
        io.screens = [frame(&if body_view.is_empty() {
            tail.to_owned()
        } else {
            format!("{body_view}\n{tail}")
        })]
        .into();
        let result = io.execute(&prompt);
        assert!(result.is_ok());
        assert_eq!(&io.calls[..3], ["text", "text", "screen"]);
        settle(claim, result);
    }
}

#[test]
fn body_prefix_without_marker_never_authorizes_submission() {
    let (mut io, claim, prompt) = Fake::new();
    io.screens.clear();
    io.fallback = frame("body");
    let error = io.execute(&prompt).unwrap_err();
    assert!(error.delivery_may_have_occurred());
    assert!(error.error().to_string().contains("Inline(\"body\")"));
    assert_eq!(io.texts, 2);
    assert!(!io.calls.contains(&"submit"));
    settle(claim, Err(error));
}

#[test]
fn windows_transport_bytes_split_exact_framing_and_only_one_terminal_lf() {
    let (mut io, claim, _) = Fake::new();
    let pending = super::super::super::install_pending_turn(io.root.path(), &io.token).unwrap();
    let framed = super::super::super::correlation::correlated_prompt(
        "[Agent Bridge native delegation]\n한글😀",
        &pending,
    );
    let bytes = crate::native::delivery::terminal_input_bytes(
        terminal::TerminalKind::WindowsConsole,
        &framed,
    );
    assert_eq!(bytes, framed.as_bytes());
    let file_prompt = String::from_utf8(bytes).unwrap();
    for suffix in ["", "\n"] {
        io.screens = [frame(&pending.marker)].into();
        io.execute(&format!("{file_prompt}{suffix}")).unwrap();
        assert_eq!(io.writes[io.writes.len() - 2..].concat(), file_prompt);
    }
    let before = io.texts;
    assert!(
        !io.execute(&format!("{file_prompt}\n\n"))
            .unwrap_err()
            .delivery_may_have_occurred()
    );
    assert_eq!(io.texts, before);
    claim.retain();
}
