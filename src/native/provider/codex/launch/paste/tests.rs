use super::*;
use crate::native::session::{SessionState, Store, update_status};

const WORKSPACE: &str = "D:/Dev/agent-bridge";
fn composer_segments(screen: &str) -> std::result::Result<Vec<Segment>, Ambiguous> {
    super::composer_segments(screen, "", Path::new(WORKSPACE))
}
fn classification(screen: Result<String>) -> String {
    super::classification(screen, "", Path::new(WORKSPACE))
}
fn run(io: &mut impl Input, prompt: &str, claim: &str, deadline: Instant) -> TerminalSendResult {
    super::run(io, prompt, claim, deadline, Path::new(WORKSPACE))
}

fn frame(text: &str) -> String {
    format!("› {}\n\n  100% context left\n", text.replace('\n', "\n  "))
}
fn evidence(rendered: &str, expected: &str) -> Observation {
    composer_segments(&frame(rendered)).map_or(Observation::Ambiguous, |s| observe(&s, expected))
}

#[test]
fn paired_initial_readiness_and_draft_observation_need_different_evidence() {
    let empty = super::super::COMPOSER_ROW;
    assert!(super::super::composer_is_ready(empty));
    assert_eq!(
        classification(Ok(empty.into())),
        "ambiguous (unresolved overlay)"
    );

    let pending = super::super::PendingCodexTurn::new("1-2-3").unwrap();
    let prompt = super::super::correlated_prompt("body", &pending);
    let screen = format!("› {}", prompt.replace('\n', "\n  "));
    assert!(!super::super::composer_is_ready(&screen));
    for tail in ["", "<!-- agent-bridge-codex-turn:4-5-6 -->"] {
        assert!(super::composer_segments(&screen, tail, Path::new(WORKSPACE)).is_err());
    }
    let segments =
        super::composer_segments(&screen, &pending.marker, Path::new(WORKSPACE)).unwrap();
    assert_eq!(segments.last(), Some(&Segment::Inline(pending.marker)));
    assert_eq!(observe(&segments, &prompt), Observation::Complete);
}

#[test]
fn paired_global_hook_veto_still_allows_plain_words_in_an_addressed_draft() {
    let pending = super::super::PendingCodexTurn::new("1-2-3").unwrap();
    for phrase in ["Review hooks", "Trust all and continue"] {
        let body = format!(
            "Explain these labels\n{}\n{phrase}",
            super::super::COMPOSER_ROW
        );
        let prompt = super::super::correlated_prompt(&body, &pending);
        let screen = frame(&prompt);
        assert!(super::super::legacy_composer_is_ready(&screen));
        assert!(!super::super::composer_is_ready(&screen));
        let segments =
            super::composer_segments(&screen, &pending.marker, Path::new(WORKSPACE)).unwrap();
        assert_eq!(observe(&segments, &prompt), Observation::Complete);
    }
}

#[test]
fn paired_structural_picker_refuses_both_initial_readiness_and_addressed_draft() {
    let pending = super::super::PendingCodexTurn::new("1-2-3").unwrap();
    let prompt = super::super::correlated_prompt("body", &pending);
    let picker: String = serde_json::from_str(include_str!("picker-screen.json")).unwrap();
    let screen = format!("{}\n{picker}", frame(&prompt));
    assert!(!super::super::composer_is_ready(&screen));
    assert_eq!(
        super::composer_segments(&screen, &pending.marker, Path::new(WORKSPACE))
            .unwrap_err()
            .0,
        "blocked"
    );
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
            "ambiguous (unresolved overlay)"
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
    write_times: Vec<Instant>,
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
        crate::native::session::test_support::write_test_manifest(root.path(), "codex");
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
                write_times: vec![],
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
        self.write_times.push(self.now);
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
        if self.fault == Some("stale") {
            bail!("stale claim at guarded submission");
        }
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
    assert_eq!(io.write_times[1] - io.write_times[0], PASTE_SPLIT_GAP);
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
    for (source, console_source, dispatch) in [
        (
            include_str!("../../../agy/mod.rs"),
            include_str!("../../../agy/delivery.rs"),
            "terminal::send_file(session, prompt, deadline)",
        ),
        (
            include_str!("../../../pi/mod.rs"),
            include_str!("../../../pi/input.rs"),
            "terminal::send_file(turn.surface, path, turn.deadline)",
        ),
    ] {
        assert!(console_source.contains(dispatch));
        assert!(source.contains("submit_count: 1"));
        assert!(!source.contains("send_file_text_only"));
        assert!(!console_source.contains("send_file_text_only"));
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
        (PICKER.into(), "blocked"),
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
    let agy_delivery = include_str!("../../../agy/delivery.rs");
    let console_io = agy_delivery
        .split_once("impl Io for ConsoleIo {")
        .unwrap()
        .1
        .split_once("// Only preparation")
        .unwrap()
        .0;
    assert!(console_io.contains("terminal::send_file(session, prompt, deadline)"));
    assert!(
        agy_delivery.contains("|runtime| runtime.split().1.send(session, prompt_path, deadline)")
    );
    assert!(!agy_delivery.contains("send_file_text_only"));
    let agy_transport = include_str!("../../../agy/mod.rs");
    assert!(agy_transport.contains("&mut *self.runtime.borrow_mut()"));
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
    for body_view in [
        "bo\ndy",
        "dy",
        "",
        "1. Summarize the changes\n2. Explain the regression\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]",
        "42. Explain the regression",
        "1. Summarize the changes\n2. Explain the regression",
    ] {
        for footer in [false, true] {
            let (mut io, claim, prompt) = Fake::new();
            let tail = prompt.rsplit_once('\n').unwrap().1;
            let shown = if body_view.is_empty() {
                tail.to_owned()
            } else {
                format!("{body_view}\n{tail}")
            };
            let screen = if footer {
                frame(&shown)
            } else {
                format!("› {}", shown.replace('\n', "\n  "))
            };
            assert_eq!(
                super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
                "populated"
            );
            io.screens = [screen].into();
            let result = io.execute(&prompt);
            assert!(result.is_ok(), "{body_view}");
            assert_eq!(&io.calls[..3], ["text", "text", "screen"]);
            assert_eq!(io.calls.iter().filter(|c| **c == "submit").count(), 1);
            settle(claim, result);
        }
    }
}

#[test]
fn numbered_menu_selections_do_not_start_an_addressed_composer() {
    for menu in [
        "› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n\n  enter confirm · esc skip",
        "› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit",
        "› 42. Other selection\n  2. Quit",
        "› 42. Other selection\n  enter confirm · esc skip",
        "› 42. Other selection\n  enter continue · esc quit",
        "› 1. Review hooks",
        "› 1. Trust and continue",
    ] {
        let (mut io, claim, prompt) = Fake::new();
        let tail = prompt.rsplit_once('\n').unwrap().1;
        let screen = format!("{menu}\n  {tail}");
        assert_eq!(
            super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
            "ambiguous (unresolved overlay)"
        );
        io.screens = [screen.clone()].into();
        io.fallback = screen;
        let error = io.execute(&prompt).unwrap_err();
        assert!(error.delivery_may_have_occurred());
        assert!(!io.calls.contains(&"submit"));
        settle(claim, Err(error));
    }
}

#[test]
fn pinned_ui_anywhere_and_extra_prompt_rows_refuse_return() {
    let mut fragments = vec![
        "› 3. Continue without trusting (hooks\n     won't run)\n\n  enter confirm · esc skip"
            .to_owned(),
        // Accepted limitation: text-only evidence cannot distinguish this draft from UI.
        "› 1. Explain the exit menu\n  2. Quit".to_owned(),
        "› 1. Review hooks".to_owned(),
    ];
    for label in [
        "Review hooks",
        "Trust all and continue",
        "Continue without trusting hooks",
        "Continue without trusting (hooks won't run)",
        "Trust and continue",
        "Open existing task",
        "Open restricted",
        "Back to Agent Command Center",
        "Keep current directory",
        "Quit",
        "Don't trust",
    ] {
        fragments.push(format!("› 1. {label}"));
        fragments.push(format!("› ordinary draft\n  2. {label}"));
    }
    for hint in [
        "enter confirm · esc skip",
        "enter continue · esc quit",
        "enter continue · esc back",
        "enter continue · esc cancel",
        "enter continue and create sandbox · esc quit",
        "enter continue and create sandbox · esc back",
        "enter continue and create sandbox · esc cancel",
    ] {
        fragments.push(format!("› ordinary draft\n  {hint}"));
    }
    for fragment in fragments {
        for after_marker in [false, true] {
            for footer in [false, true] {
                let (mut io, claim, prompt) = Fake::new();
                let tail = prompt.rsplit_once('\n').unwrap().1;
                let screen = format!(
                    "{}{}",
                    if after_marker {
                        format!("› body\n  {tail}\n{fragment}")
                    } else {
                        format!("{fragment}\n  {tail}")
                    },
                    if footer { "\n  100% context left" } else { "" }
                );
                assert_eq!(
                    super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
                    "ambiguous (unresolved overlay)",
                    "{screen}"
                );
                io.screens = [screen.clone()].into();
                io.fallback = screen;
                let error = io.execute(&prompt).unwrap_err();
                assert!(error.delivery_may_have_occurred());
                assert!(!io.calls.contains(&"submit"));
                settle(claim, Err(error));
            }
        }
    }
    for suffix in ["› 1. Open restricted", "› 42. Unknown option", "› unknown"] {
        let (mut io, claim, prompt) = Fake::new();
        let tail = prompt.rsplit_once('\n').unwrap().1;
        let screen = format!("› body\n  {tail}\n{suffix}");
        assert_eq!(
            super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
            "ambiguous (unresolved overlay)"
        );
        io.screens = [screen.clone()].into();
        io.fallback = screen;
        let error = io.execute(&prompt).unwrap_err();
        assert!(error.delivery_may_have_occurred());
        assert!(!io.calls.contains(&"submit"));
        settle(claim, Err(error));
    }
    // A pinned first prompt prevents a later ordinary prompt from becoming a composer.
    assert!(
        composer_span(
            &["› 1. Review hooks", "› body", "  marker"],
            "marker",
            Path::new(WORKSPACE)
        )
        .is_none()
    );
}

#[test]
fn confirmation_fragments_alone_anywhere_refuse_return() {
    for hint in [
        "enter continue and create sandbox ·\n  esc quit",
        "enter continue ·\n  esc back",
        "enter continue and create sandbox ·\n  esc cancel",
        "enter confirm · ctrl+c skip",
        "ctrl+j confirm · ctrl+c skip",
        "ctrl+j continue · ctrl+c cancel",
        "enter confirm · esc skip",
        "enter continue · esc quit",
        "enter continue · esc back",
        "enter continue · esc cancel",
        "enter continue and create sandbox · esc quit",
        "enter continue and create sandbox · esc back",
        "enter continue and create sandbox · esc cancel",
        "enter confirm ·",
        "ctrl+j continue ·",
        "esc skip",
        "esc quit",
        "esc back",
        "esc cancel",
        "ctrl+c skip",
        "clipped · ctrl+c skip",
        "clipped · esc quit",
        "clipped · esc back",
        "clipped · esc cancel",
        "and create sandbox",
    ] {
        for after_marker in [false, true] {
            for footer in [false, true] {
                let (mut io, claim, prompt) = Fake::new();
                let tail = prompt.rsplit_once('\n').unwrap().1;
                // No extra prompt glyph or menu option can mask a missed hint.
                let mut screen = if after_marker {
                    format!("› body\n  {tail}\n  {hint}")
                } else {
                    format!("› body\n  {hint}\n  {tail}")
                };
                if footer {
                    screen.push_str("\n  100% context left");
                }
                assert_eq!(
                    super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
                    "ambiguous (unresolved overlay)",
                    "{screen}"
                );
                io.screens = [screen.clone()].into();
                io.fallback = screen;
                let error = io.execute(&prompt).unwrap_err();
                assert!(error.delivery_may_have_occurred());
                assert!(!io.calls.contains(&"submit"));
                settle(claim, Err(error));
            }
        }
    }
}

#[test]
fn chord_and_bare_action_fragments_alone_anywhere_refuse_return() {
    for hint in [
        "ctrl+x a confirm · ctrl+x s skip",
        "ctrl+pgup confirm · alt+pgdn skip",
        "pgup confirm · pgdn skip",
        "pgup confirm",
        "pgdn skip",
        "enter confirm · ctrl+x s skip",
        "ctrl+x s skip",
        "a confirm · ctrl+x s skip",
        "enter continue and create sandbox · esc\n  quit",
        "enter continue and create sandbox · esc\n  back",
        "enter continue and create sandbox · esc\n  cancel",
        "enter continue and create sandbox · esc",
        "quit",
        "back",
        "cancel",
        "skip",
        "confirm",
        "continue",
        "ctrl+x a confirm",
        "ctrl+x a continue",
        "ctrl+x s quit",
        "ctrl+x s back",
        "ctrl+x s cancel",
    ] {
        for after_marker in [false, true] {
            for footer in [false, true] {
                let (mut io, claim, prompt) = Fake::new();
                let tail = prompt.rsplit_once('\n').unwrap().1;
                // No extra prompt glyph or menu option can mask a missed hint.
                let mut screen = if after_marker {
                    format!("› body\n  {tail}\n  {hint}")
                } else {
                    format!("› body\n  {hint}\n  {tail}")
                };
                if footer {
                    screen.push_str("\n  100% context left");
                }
                assert_eq!(
                    super::classification(Ok(screen.clone()), tail, Path::new(WORKSPACE)),
                    "ambiguous (unresolved overlay)",
                    "{screen}"
                );
                io.screens = [screen.clone()].into();
                io.fallback = screen;
                let error = io.execute(&prompt).unwrap_err();
                assert!(error.delivery_may_have_occurred());
                assert!(!io.calls.contains(&"submit"));
                settle(claim, Err(error));
            }
        }
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

fn live_frame(prompt: &str, marker: &str) -> String {
    // Exercise Bridge's real pre-framing trim, not an invented Codex LF collapse.
    format!(
        "{}\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n{marker}",
        crate::native::native_delegation_prompt("external", prompt)
    )
}
fn capture(json: &str) -> String {
    serde_json::from_str(json).unwrap()
}
#[test]
fn real_inline_and_separate_tail_captures_end_at_marker() {
    for (json, raw, body_count) in [
        (
            include_str!("fixtures/session-spnLLG.txt"),
            include_str!("fixtures/short.txt"),
            236,
        ),
        (
            include_str!("fixtures/session-prLun4.txt"),
            include_str!("fixtures/25kb.txt"),
            25595,
        ),
        (
            include_str!("fixtures/session-4349NT.txt"),
            &capture(include_str!("fixtures/multi.txt")),
            1093,
        ),
        (
            include_str!("fixtures/session-1kz1jb.txt"),
            include_str!("fixtures/body1001.txt"),
            1001,
        ),
    ] {
        let screen = capture(json);
        let marker = screen
            .lines()
            .map(str::trim)
            .find(|s| s.starts_with("<!-- agent-bridge-codex-turn:"))
            .unwrap();
        let prompt = live_frame(raw, marker);
        assert_eq!(
            prompt.rsplit_once('\n').unwrap().0.chars().count(),
            body_count
        );
        let segments = super::composer_segments(&screen, marker, Path::new(WORKSPACE)).unwrap();
        assert_eq!(segments.last(), Some(&Segment::Inline(marker.into())));
        assert!(!format!("{segments:?}").contains("OpenAI Codex"));
        assert!(!format!("{segments:?}").contains("GPT-6-Astra"));
        let (mut io, claim, _) = Fake::new();
        io.screens = [screen.clone()].into();
        let deadline = io.now + Duration::from_secs(1);
        assert_eq!(
            applied(&mut io, deadline, &prompt, marker, Path::new(WORKSPACE)).unwrap(),
            screen
        );
        claim.retain();
        // Primary marker rule ignores all later rows, even unknown footer shapes.
        let truncated = screen
            .lines()
            .take_while(|row| !row.contains("GPT-6-Astra"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            super::composer_segments(&truncated, marker, Path::new(WORKSPACE)).unwrap(),
            segments
        );
    }
}
#[test]
fn real_merged_tail_captures_use_normalized_total() {
    for (json, raw, token) in [
        (
            include_str!("fixtures/session-FXoq6U.txt"),
            include_str!("fixtures/body1000.txt"),
            "2136-1792315332023997000-0",
        ),
        (
            include_str!("fixtures/session-s5RIn4.txt"),
            include_str!("fixtures/body999.txt"),
            "18476-1792314931932060800-0",
        ),
    ] {
        // Tokens recovered read-only from each session's codex-pending-turn.json.
        let marker = format!("<!-- agent-bridge-codex-turn:{token} -->");
        let prompt = live_frame(raw, &marker);
        assert_eq!(normalized_count(&prompt), 1060);
        let screen = capture(json);
        let segments = super::composer_segments(&screen, &marker, Path::new(WORKSPACE)).unwrap();
        assert_eq!(segments, [Segment::Placeholder(1060)]);
        assert_eq!(observe(&segments, &prompt), Observation::Complete);
        let (mut io, claim, _) = Fake::new();
        io.screens = [screen.clone()].into();
        let deadline = io.now + Duration::from_secs(1);
        assert_eq!(
            applied(&mut io, deadline, &prompt, &marker, Path::new(WORKSPACE)).unwrap(),
            screen
        );
        claim.retain();
    }
}
#[test]
fn real_trailing_lf_counts_come_from_bridge_framing_not_codex_collapse() {
    for (raw, untrimmed, actual) in [
        (include_str!("fixtures/25kb.txt"), 25596, 25595),
        (&capture(include_str!("fixtures/multi.txt")), 1094, 1093),
    ] {
        assert!(raw.ends_with('\n'));
        let naive = format!(
            "[Agent Bridge native delegation]\nSource: external\n\n{raw}\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]"
        );
        assert!(naive.contains("\n\n\n"));
        assert_eq!(normalized_count(&naive), untrimmed);
        let prompt = live_frame(raw, "marker");
        let body = prompt.rsplit_once('\n').unwrap().0;
        assert_eq!(normalized_count(body), actual);
    }
    assert_eq!(normalized_count("a\r\nb\rc\n\n\nd\t\x01\x1b[31me"), 11);
    assert_eq!(
        observe(&[Segment::Placeholder(10)], "body\n> quoted\n"),
        Observation::Ambiguous
    );
}
#[test]
fn pinned_footer_variants_and_ambiguous_boundaries() {
    for hint in [
        "? for shortcuts",
        "ctrl+g for agents · ? for shortcuts",
        "100% context left",
        "12.3K used",
        "1T used",
        "⚠ 1 warning · f2 to view",
        "⚠ 2 warnings · f2 to view",
        "GPT-6-Astra xhigh · d:/dev/AGENT-BRIDGE",
        "esc to interrupt",
        "tab to queue message",
        "tab to queue",
        "ctrl+c again to quit",
        "ctrl+d again to quit",
        "Plan mode",
        "Plan mode (shift+tab to cycle)",
        "esc again to edit previous message",
        "esc esc to edit previous message",
    ] {
        assert!(footer(hint, Path::new(WORKSPACE)), "{hint}");
        let screen = format!("banner\n› first\n  \n  second\n\n\n  {hint}\n");
        assert_eq!(
            composer_segments(&screen).unwrap(),
            [
                Segment::Inline("first".into()),
                Segment::Inline("\n".into()),
                Segment::Inline("\n".into()),
                Segment::Inline("second".into())
            ],
            "{hint}"
        );
    }
    assert!(!footer("GPT-6-Astra · D:/Dev/other", Path::new(WORKSPACE)));
    assert_eq!(
        normalized_workspace(r"\\?\D:\Dev\agent-bridge"),
        normalized_workspace(WORKSPACE)
    );
    assert_eq!(
        normalized_workspace(r"\\?\UNC\server\share"),
        normalized_workspace(r"\\server\share")
    );
    assert!(
        super::composer_segments(
            "› marker\n› unresolved UI after marker",
            "marker",
            Path::new(WORKSPACE)
        )
        .is_err()
    );
    assert!(super::composer_segments("› draft\n› marker", "marker", Path::new(WORKSPACE)).is_err());
    assert!(composer_segments("› draft\n").is_err());
    assert!(composer_segments("› draft\nunrecognized\n  ? for shortcuts").is_err());
    assert!(
        super::composer_segments(
            "› draft\nunrecognized\n  marker",
            "marker",
            Path::new(WORKSPACE)
        )
        .is_err()
    );
    assert!(
        super::composer_segments("› marker\nReview hooks", "marker", Path::new(WORKSPACE)).is_err()
    );
}

// Synthetic pinned shapes supplement the raw a7-screen.txt capture below.
const PICKER: &str = "  Hooks need review\n\n› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting hooks\n\n  enter confirm · esc skip";
const TRUST: &str = "  Folder access\n  C:\\work\\project\n  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings\n  can run code automatically, even without a model request. Continue only if you trust these files. Your trust\n  decision will be saved.\n› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit";

#[test]
fn dialog_words_in_inline_and_placeholder_drafts_submit() {
    for phrase in [
        "Review hooks",
        "Trust this folder?",
        "Trust and continue",
        "Trust all and continue",
    ] {
        for placeholder in [false, true] {
            let (mut io, claim, _) = Fake::new();
            let tail = format!("<!-- agent-bridge-codex-turn:{} -->", io.token);
            let body = if placeholder {
                "x".repeat(1001)
            } else {
                "Explain this:".into()
            };
            let prompt = format!("{body}\n{phrase}\n{tail}");
            io.screens = [frame(&if placeholder {
                format!("[Pasted Content 1001 chars]\n{phrase}\n{tail}")
            } else {
                prompt.clone()
            })]
            .into();
            assert!(io.execute(&prompt).is_ok(), "{phrase}");
            assert_eq!(io.calls.iter().filter(|c| **c == "submit").count(), 1);
            claim.retain();
        }
    }
}

#[test]
fn structural_overlays_veto_mixed_frames_and_partial_overlays_wait() {
    let real_picker: String = serde_json::from_str(include_str!("picker-screen.json")).unwrap();
    for overlay in [
        PICKER,
        TRUST,
        &real_picker,
        "  Hooks need review\n  1. Review hooks",
    ] {
        for mixed in [false, true] {
            let (mut io, claim, prompt) = Fake::new();
            let screen = if mixed {
                format!("{}\n{overlay}", frame(&prompt))
            } else {
                overlay.into()
            };
            io.screens = [screen.clone()].into();
            io.fallback = screen.clone();
            let error = io.execute(&prompt).unwrap_err();
            assert!(error.delivery_may_have_occurred());
            assert!(!io.calls.contains(&"submit"));
            assert_eq!(
                classification(Ok(screen)).starts_with("blocked"),
                overlay != "  Hooks need review\n  1. Review hooks"
            );
            claim.retain();
        }
    }
    let (mut io, claim, prompt) = Fake::new();
    io.fault = Some("stale");
    let error = io.execute(&prompt).unwrap_err();
    assert!(error.delivery_may_have_occurred());
    assert!(error.error().to_string().contains("stale"));
    assert_eq!(io.calls.iter().filter(|c| **c == "submit").count(), 1);
    claim.retain();
}

#[cfg(windows)]
#[test]
fn real_console_payload_to_helper_seam_fences_late_retirement() {
    const CHILD: &str = "AB_R3_CONSOLE_SEAM";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native::provider::codex::launch::paste::tests::real_console_payload_to_helper_seam_fences_late_retirement", "--test-threads=1", "--nocapture"])
            .env(CHILD, "1").env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path()).output().unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("AGENT_BRIDGE_NATIVE_STATE_DIR").unwrap());
    for phase in [1, 2, 3] {
        for change in ["retired", "replaced", "closed", "expired"] {
            let directory = root.join(format!("session-{phase}-{change}"));
            std::fs::create_dir_all(directory.join("events")).unwrap();
            update_status(&directory, SessionState::Working, None, None).unwrap();
            let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
            let token = claim.token().to_owned();
            let prompt = format!("body\n<!-- agent-bridge-codex-turn:{token} -->");
            let session = serde_json::from_value(serde_json::json!({"terminal":"windows-console", "session_id":"1", "managed_session_id":directory.file_name().unwrap().to_str().unwrap()})).unwrap();
            let deadline = Instant::now() + Duration::from_secs(25);
            crate::native::console_control::with_in_process_helper(
                &directory,
                &session,
                agent_bridge::FirstPartyCli::Codex,
                phase,
                change,
                &frame(&prompt),
                |client| {
                    let mut io = Console {
                        client,
                        session: &session,
                        directory: &directory,
                        claim: &token,
                        deadline,
                    };
                    let error = run(&mut io, &prompt, &token, deadline).unwrap_err();
                    assert_eq!(error.delivery_may_have_occurred(), phase != 1);
                    assert!(error.error().to_string().contains("stale"), "{error:?}");
                },
            );
            claim.retain();
        }
    }
}

#[test]
fn addressed_span_keeps_footer_shaped_content_and_exposes_menu_suffixes() {
    let real_picker: String = serde_json::from_str(include_str!("picker-screen.json")).unwrap();
    for phrase in [
        "Review hooks",
        "Trust this folder?",
        "Trust and continue",
        "Trust all and continue",
    ] {
        for hint in ["Plan mode", "100% context left"] {
            for placeholder in [false, true] {
                for picker in [false, true] {
                    let (mut io, claim, _) = Fake::new();
                    let tail = format!("<!-- agent-bridge-codex-turn:{} -->", io.token);
                    let body = if placeholder {
                        "x".repeat(1001)
                    } else {
                        "Explain these UI labels:".into()
                    };
                    let prompt = format!("{body}\n{hint}\n{phrase}\n{tail}");
                    let shown = if placeholder {
                        prompt.replacen(&body, "[Pasted Content 1001 chars]", 1)
                    } else {
                        prompt.clone()
                    };
                    let screen = format!(
                        "{}{}",
                        frame(&shown),
                        if picker { &real_picker } else { "" }
                    );
                    io.screens = [screen.clone()].into();
                    io.fallback = screen.clone();
                    assert_eq!(io.execute(&prompt).is_ok(), !picker);
                    assert_eq!(io.calls.contains(&"submit"), !picker);
                    assert_eq!(
                        super::classification(Ok(screen), &tail, Path::new(WORKSPACE)),
                        if picker { "blocked" } else { "populated" }
                    );
                    claim.retain();
                }
            }
        }
    }
    for suffix in [
        "› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n\n  enter confirm · esc skip",
        "› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit",
    ] {
        for footer in [false, true] {
            for placeholder in [false, true] {
                let (mut io, claim, prompt) = Fake::new();
                let tail = prompt.rsplit_once('\n').unwrap().1;
                let shown = if placeholder {
                    format!("[Pasted Content {} chars]", normalized_count(&prompt))
                } else {
                    prompt.clone()
                };
                let screen = format!(
                    "› {}\n{}{}",
                    shown.replace('\n', "\n  "),
                    if footer { "  100% context left\n" } else { "" },
                    suffix
                );
                io.screens = [screen.clone()].into();
                io.fallback = screen.clone();
                assert!(io.execute(&prompt).is_err());
                assert!(!io.calls.contains(&"submit"));
                assert!(
                    super::classification(Ok(screen), tail, Path::new(WORKSPACE))
                        .starts_with("ambiguous")
                );
                claim.retain();
            }
        }
    }
    let tail = "<!-- agent-bridge-codex-turn:claim -->";
    assert_eq!(
        super::classification(
            Ok(include_str!("r4-picker-suffix.txt").into()),
            tail,
            Path::new(WORKSPACE)
        ),
        "ambiguous (unresolved overlay)"
    );
    assert_eq!(
        super::classification(
            Ok(include_str!("r4-footer-content.txt").into()),
            tail,
            Path::new(WORKSPACE)
        ),
        "populated"
    );
}
