use super::super::delivery::{complete_log_lines, glog_timestamp, strip_terminal_noise};
use super::super::receipt::{ReceiptEvidence, observe_input_receipt};
use super::super::test_support::{
    FakeClock, HOOKS_LOADED, REAL_DEFERRED_RELOAD_STARTUP, REAL_MACOS_INITIAL_TURN,
    REAL_QUIET_STARTUP, REAL_SECOND_CLUSTER_RELOAD_STARTUP, REAL_STARTUP_RELOAD_AFTER_STARTUP,
    REAL_SUCCESS_STARTUP, framed_receipt_line, glog, late_reload_completion,
    late_reload_startup_log, log_sequence, settled_startup_log, some_log, successful_startup_log,
};
use super::super::trust::workspace_customizations_loaded;
use super::super::{
    HOOKS_LOADED_SOURCE, MACOS_FOLLOW_UP_READINESS_TIMING, PendingAgyTurn, STARTUP_QUIET_PERIOD,
    WINDOWS_STARTUP_READINESS_TIMING,
};
use super::*;
use crate::native::terminal;
use anyhow::Result;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

// session-ql5TVc (this machine, 2026-09-24 18:48, Agy 1.2.10; the round-6 build
// pasted at about 18:48:24.2, when its 20 s window ended, and lost the paste):
// lines 110-152 (the end of the file) verbatim after the ANSI strip, with the
// account email redacted. The startup reload precedes `CLI startup completed`;
// two plain reloads follow within 4.6 s; the quiet period ended at 18:48:12.15;
// the 20 s window ended at 18:48:24.12 and the gate pasted; and the deferred
// skills reload with its hooks line arrived at 18:48:25.481988, 21.4 s after
// startup, clearing the composer. Nothing was logged after it: no
// HandleUserInput receipt, so the adapter reported delivery-uncertain.
const REAL_LATE_DEFERRED_RELOAD_STARTUP: &str = r#"I0924 18:48:04.112210     338 manager.go:1331] Reloading system slash commands and skills
I0924 18:48:04.112210     338 manager.go:1308] Reloading system slash commands
I0924 18:48:04.112210     338 manager.go:1312] Slash commands unchanged, skipping update
I0924 18:48:04.112210     141 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 18:48:04.112725     141 gemini_extensions.go:49] No extensions found
I0924 18:48:04.112725     208 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 19:30:33.2676274 +0900 KST expired=false
I0924 18:48:04.113240     207 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 18:48:04.113240     207 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 18:48:04.113240     207 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 18:48:04.113240     207 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x1ecd003d80f0) is same as b.cliAuth (0x1ecd003d80f0)
W0924 18:48:04.114757     217 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 18:48:04.114757     359 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 18:48:04.115440       1 analytics.go:187] CLI startup completed (took 234.4106ms)
I0924 18:48:04.161713      61 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 18:48:05.439653     213 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xb4200852ff66f489
I0924 18:48:05.808465     213 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0xa2c72879b6413d5a
I0924 18:48:05.844379     207 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:05.844379     207 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:05.844959     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 18:48:05.844959     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 18:48:05.977318     246 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 18:48:05.977318     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 18:48:05.977318     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 18:48:05.977318     246 experiment_manager.go:70] Experiments refreshed after login
I0924 18:48:05.977318     326 manager.go:1308] Reloading system slash commands
I0924 18:48:07.328742     260 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x830ddd43c74d8225
I0924 18:48:08.309372     260 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x386208932c049341
I0924 18:48:08.471064     260 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:08.471064     260 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:08.471064     328 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:08.471064     328 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:08.471064     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 18:48:08.472576     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 18:48:08.646591     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 18:48:08.646591     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 18:48:08.646591     246 experiment_manager.go:70] Experiments refreshed after login
I0924 18:48:08.646591     270 manager.go:1308] Reloading system slash commands
I0924 18:48:08.649582     270 manager.go:1312] Slash commands unchanged, skipping update
I0924 18:48:09.842042     327 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x20191198b6612470
I0924 18:48:25.481988     379 manager.go:1331] Reloading system slash commands and skills
I0924 18:48:25.481988     379 manager.go:1308] Reloading system slash commands
I0924 18:48:25.482515     375 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 18:48:25.485154     379 manager.go:1312] Slash commands unchanged, skipping update
"#;

const REAL_MACOS_STARTUP_LINE: usize = 11;

const REAL_MACOS_CONVERSATION_RELOAD_LINE: usize = 44;

const REAL_MACOS_DEFERRED_RELOAD_LINE: usize = 72;

// "Deferred" skills reload latency (`Reloading system slash commands and skills`
// stamped at least 1 s after `CLI startup completed`, measured from the startup
// line) in the fixtures above, all Agy 1.2.10 on the Windows machine, 2026-09-24
// KST. Read with the 2026-10-01 finding: this is the trust reload, logged when the
// paste's Enter approved the workspace-trust dialog, so it trails every lost paste
// and is absent wherever the workspace was already trusted (the delivered rows).
//
// | fixture        | startup reload  | deferred reload      | latency  | paste                 |
// |----------------|-----------------|----------------------|----------|-----------------------|
// | session-udT6uY | +0.5 ms         | none before the paste| -        | +26.1 s, delivered    |
// | session-fMqSQc | before startup  | 16:41:20.813553      | 12.983 s | +12 s, lost           |
// | session-IQHEwf | before startup  | none in 5 minutes    | -        | none (round-1 gate)   |
// | session-IEKjtC | before startup  | 17:47:13.468286      | 9.848 s  | +9.5 s, lost          |
// | session-ql5TVc | before startup  | 18:48:25.481988      | 21.367 s | +20.1 s, lost         |
// | session-M8QFPp | +1.6 ms         | none before the paste| -        | +8.5 s, delivered     |
// | session-uqraap | before startup  | 20:44:37.855815      | 36.413 s | +35 s, lost           |
//
// session-Cf1FBY (20:36, heavy CPU load, not a fixture) logged its deferred
// reload at +36.4 s as well and lost the 35 s window's paste the same way; the
// observed latencies are 9.8, 13.0, 21.4, 36.4 and 36.4 s, a second cluster near
// 36 s under load and idle alike. session-8WjG3m (not a fixture) logged
// its receipt 49 s after startup, about 10 s after the paste at the window's
// end, and was delivered; session-M8QFPp logged its receipt 18.0 s after the
// paste, outside the former 15 s receipt window.
//
// The udT6uY `... and skills` line at 16:42:50.228104 (+26.15 s) followed the
// receipt and `Starting new conversation` by 15 ms, so it is the conversation
// reload, not the deferred one. Every delivered session shows this pattern (the
// two round-7 live asks included): a skills reload logged right after
// `HandleUserInput` and the conversation-start lines is triggered by the new
// conversation and is never evidence that the paste was cleared; the receipt
// check returns on the first read holding the receipt and ignores later lines
// (`receipt_stays_delivered_when_the_conversation_reload_follows_it`).
// The other Agy 1.2.10 logs on this machine that
// are not fixtures (2026-09-24, read once for this table) logged the deferred
// reload at 12.933, 12.942, 12.947, 12.949, 12.964, 13.082, 13.124, 13.137 and
// 13.161 s (nine sessions, none pasted before it), and seven sessions whose
// startup reload preceded startup delivered a paste at +25.7 to +34.1 s with no
// deferred reload before the receipt. Ten sessions whose startup reload followed
// `CLI startup completed` never logged a separate deferred reload before their
// receipt (+9.6 to +27.6 s).

const REPLAY_POLL: Duration = Duration::from_millis(100);

// The glog stamp of a line, on the adapter's own parser, so the replay and the
// deferred-reload rule read the same instants.
fn glog_time_of_day(line: &str) -> Option<Duration> {
    glog_timestamp(line)
}

// Replays a recorded log against the fake clock: each complete line becomes
// visible at its own glog timestamp, with the first timestamped line at `start`,
// so the readiness instants the tests assert are the ones Agy recorded. A line
// without a timestamp (`CLI ready for user input`, or a header before the first
// timestamped line) and a line stamped earlier than its predecessor (threads log
// out of order by a few milliseconds) appear together with the line before them;
// untimestamped lines before the first timestamp appear at `start`.
struct LogReplay {
    start: Instant,
    first: Duration,
    // The `mmdd` of the first timestamped line; a recorded log spans one day.
    date: String,
    lines: Vec<(Instant, String)>,
}

impl LogReplay {
    fn new(text: &str, start: Instant) -> Self {
        let (first, date) = text
            .lines()
            .find_map(|line| Some((glog_time_of_day(line)?, line[1..5].to_owned())))
            .expect("a recorded log has a timestamped line");
        let mut previous = first;
        let mut lines = Vec::new();
        for line in text.lines() {
            let stamp = glog_time_of_day(line).unwrap_or(previous).max(previous);
            previous = stamp;
            lines.push((start + (stamp - first), format!("{line}\n")));
        }
        Self {
            start,
            first,
            date,
            lines,
        }
    }

    // The replay instant of a recorded `HH:MM:SS.ffffff` time of day.
    fn recorded(&self, time: &str) -> Instant {
        let stamp = glog_time_of_day(&format!("I{} {time}", self.date)).expect("a glog time");
        self.start + (stamp - self.first)
    }

    fn visible_at(&self, now: Instant) -> Option<Vec<u8>> {
        let visible: String = self
            .lines
            .iter()
            .take_while(|(at, _)| *at <= now)
            .map(|(_, line)| line.as_str())
            .collect();
        (!visible.is_empty()).then(|| visible.into_bytes())
    }

    fn reader(&self, now: Rc<Cell<Instant>>) -> impl FnMut() -> Result<Option<Vec<u8>>> + '_ {
        move || Ok(self.visible_at(now.get()))
    }

    // The state of a fresh gate stepped through the recorded instants in order.
    fn states(&self, timing: ReadinessTiming, times: &[&str]) -> Vec<ReadinessState> {
        let mut gate = ReadinessGate::new(self.start, timing);
        times
            .iter()
            .map(|time| {
                let now = self.recorded(time);
                gate.observe(self.visible_at(now).as_deref(), now)
            })
            .collect()
    }
}

// Runs the gate over the replayed log, polling every 100 ms from the first line,
// and returns the instant at which it reported ready.
fn replay_readiness(replay: &LogReplay, timing: ReadinessTiming) -> Instant {
    let mut clock = FakeClock::new(replay.start);
    wait_for_startup_readiness_with(
        &mut replay.reader(clock.shared()),
        replay.start + Duration::from_secs(300),
        timing,
        REPLAY_POLL,
        &mut clock,
    )
    .expect("the replayed log passes the gate before the deadline");
    clock.now()
}

// The gate polls every 100 ms, so it reports ready at the first poll at or after
// the exact instant.
fn assert_ready_at(replay: &LogReplay, ready: Instant, expected: &str, what: &str) {
    let expected_at = replay.recorded(expected);
    assert!(
        ready >= expected_at && ready < expected_at + REPLAY_POLL,
        "{what}: ready {:?} after the first line, expected {expected} ({:?} after the first line)",
        ready.saturating_duration_since(replay.start),
        expected_at.saturating_duration_since(replay.start)
    );
}

const SKILLS_RELOAD: &str = "Reloading system slash commands and skills";
const SLASH_RELOAD: &str = "Reloading system slash commands";
const FULL_REDRAW: &str =
    "Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)";

// The bytes of `log` before its `Full redraw completed` line: startup observed,
// the redraw not yet.
fn before_redraw(log: &str) -> &str {
    &log[..log.find(FULL_REDRAW_MARKER).unwrap()]
}

#[test]
fn startup_observation_tracks_the_markers_and_the_newest_activity_line() {
    // session-udT6uY: main-thread hooks line, startup, skills reload with its
    // hooks line, redraw, three plain reloads.
    let observation = observe_startup(successful_startup_log().as_bytes());
    assert_eq!(observation.startup_line, Some(3));
    assert_eq!(
        observation.startup_stamp,
        glog_timestamp("I0924 16:42:24.080467       1 analytics.go:187] CLI startup completed")
    );
    assert_eq!(observation.redraw_after_startup, Some(7));
    assert_eq!(
        observation.deferred_reload_after_startup, None,
        "the skills reload stamped 0.5 ms after startup is the startup reload, not the deferred one"
    );
    assert_eq!(observation.startup_reload_after_startup, Some(4));
    assert_eq!(observation.settle_line, Some(10));
    assert_eq!(observation.activity_lines, 8);
    assert!(observation.markers_observed(true));
    assert!(observation.missing_markers(true).is_empty());

    // session-fMqSQc: the startup reload precedes startup and has no hooks line;
    // the markers are still complete, the newest activity is the last reload, and
    // no skills reload has followed startup yet.
    let late = late_reload_startup_log();
    let observation = observe_startup(late.as_bytes());
    assert_eq!(observation.startup_line, Some(6));
    assert_eq!(observation.redraw_after_startup, Some(7));
    assert_eq!(observation.deferred_reload_after_startup, None);
    assert_eq!(observation.startup_reload_after_startup, None);
    assert_eq!(observation.settle_line, Some(9));
    assert_eq!(observation.activity_lines, 6);
    assert!(observation.markers_observed(true));

    // The late reload (13.0 s after startup) is the deferred skills reload; it and
    // its hooks line are also activity.
    let completed = late + &late_reload_completion();
    let observation = observe_startup(completed.as_bytes());
    assert_eq!(observation.startup_line, Some(6));
    assert_eq!(observation.redraw_after_startup, Some(7));
    assert_eq!(observation.deferred_reload_after_startup, Some(11));
    assert_eq!(observation.startup_reload_after_startup, None);
    assert_eq!(observation.settle_line, Some(13));
    assert_eq!(observation.activity_lines, 9);

    // session-IQHEwf: the healthy startup that never logs the reload/hooks pair.
    let observation = observe_startup(REAL_QUIET_STARTUP.as_bytes());
    assert_eq!(observation.startup_line, Some(15));
    assert_eq!(observation.redraw_after_startup, Some(16));
    assert_eq!(observation.deferred_reload_after_startup, None);
    assert_eq!(observation.settle_line, Some(47));
    assert_eq!(observation.activity_lines, 6);
    assert!(observation.markers_observed(true));

    // session-IEKjtC: the startup reload precedes startup; the deferred skills
    // reload and its hooks line come 9.8 s later.
    let observation = observe_startup(REAL_DEFERRED_RELOAD_STARTUP.as_bytes());
    assert_eq!(observation.startup_line, Some(12));
    assert_eq!(observation.redraw_after_startup, Some(13));
    assert_eq!(observation.deferred_reload_after_startup, Some(46));
    assert_eq!(observation.settle_line, Some(48));
    assert_eq!(observation.activity_lines, 9);
    assert!(observation.markers_observed(true));

    // session-M8QFPp: the startup reload follows startup by 1.6 ms with its hooks
    // line; it is not the deferred reload, and no deferred reload follows.
    let observation = observe_startup(REAL_STARTUP_RELOAD_AFTER_STARTUP.as_bytes());
    assert_eq!(observation.startup_line, Some(0));
    assert_eq!(observation.redraw_after_startup, Some(4));
    assert_eq!(observation.deferred_reload_after_startup, None);
    assert_eq!(observation.startup_reload_after_startup, Some(1));
    assert_eq!(observation.settle_line, Some(33));
    assert_eq!(observation.activity_lines, 7);
    assert!(observation.markers_observed(true));

    // session-uqraap: the startup reload precedes startup; the deferred skills
    // reload and its hooks line come 36.4 s later.
    let observation = observe_startup(REAL_SECOND_CLUSTER_RELOAD_STARTUP.as_bytes());
    assert_eq!(observation.startup_line, Some(22));
    assert_eq!(observation.redraw_after_startup, Some(23));
    assert_eq!(observation.deferred_reload_after_startup, Some(50));
    assert_eq!(observation.startup_reload_after_startup, None);
    assert_eq!(observation.settle_line, Some(52));
    assert_eq!(observation.activity_lines, 8);
    assert!(observation.markers_observed(true));

    // The 1 s rule on synthetic stamps: 999 ms after startup is the startup
    // reload, 1.000 s is the deferred one, a reload stamped a few milliseconds
    // before startup by another thread is the startup reload, and a reload
    // without a stamp proves nothing.
    let startup = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    );
    for (stamp, deferred) in [
        ("16:41:07.826763", false),
        ("16:41:08.830344", false),
        ("16:41:08.830345", true),
        ("16:41:20.813553", true),
    ] {
        let log = startup.clone() + &glog(stamp, 410, "manager.go:1331", SKILLS_RELOAD);
        let observation = observe_startup(log.as_bytes());
        assert_eq!(
            observation.deferred_reload_after_startup.is_some(),
            deferred,
            "a skills reload stamped {stamp} after startup at 16:41:07.830345"
        );
        assert_eq!(
            observation.startup_reload_after_startup.is_some(),
            !deferred
        );
    }
    let unstamped = startup.clone() + "manager.go:1331] " + SKILLS_RELOAD + "\n";
    assert_eq!(
        observe_startup(unstamped.as_bytes()).deferred_reload_after_startup,
        None
    );
    let startup_unstamped = "CLI startup completed (took 1ms)\n".to_owned()
        + &glog("16:41:20.813553", 410, "manager.go:1331", SKILLS_RELOAD);
    let observation = observe_startup(startup_unstamped.as_bytes());
    assert_eq!(observation.startup_stamp, None);
    assert_eq!(observation.deferred_reload_after_startup, None);
    // A stamp across midnight still measures the latency.
    let midnight = "I0924 23:59:59.500000       1 analytics.go:187] CLI startup completed\n"
        .to_owned()
        + "I0925 00:00:12.500000     410 manager.go:1331] Reloading system slash commands and skills\n";
    assert_eq!(
        observe_startup(midnight.as_bytes()).deferred_reload_after_startup,
        Some(1)
    );
    assert_eq!(
        glog_timestamp("F0924 16:42:24.080467       1 x.go:1] fatal")
            .map(|stamp| stamp
                - glog_timestamp("I0924 16:42:24.080467       1 x.go:1] info").unwrap()),
        Some(Duration::ZERO)
    );
    assert_eq!(glog_timestamp("CLI ready for user input"), None);
    assert_eq!(
        glog_timestamp("I0924 16:42:24.08046        1 x.go:1] short"),
        None
    );

    // A redraw before startup does not satisfy the second marker.
    let redraw_first = glog("16:41:07.000000", 269, "manager.go:934", FULL_REDRAW)
        + &glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        );
    let observation = observe_startup(redraw_first.as_bytes());
    assert_eq!(observation.startup_line, Some(1));
    assert_eq!(observation.redraw_after_startup, None);
    assert_eq!(observation.settle_line, Some(0));
    assert_eq!(
        observation.missing_markers(true),
        vec![REDRAW_AFTER_STARTUP_DESCRIPTION]
    );

    let no_startup = successful_startup_log().replace("CLI startup completed", "CLI startup");
    let observation = observe_startup(no_startup.as_bytes());
    assert!(!observation.startup_completed());
    assert_eq!(observation.redraw_after_startup, None);
    assert_eq!(
        StartupObservation::default().missing_markers(true),
        vec!["`CLI startup completed`", REDRAW_AFTER_STARTUP_DESCRIPTION]
    );
}

#[test]
fn startup_readiness_ignores_partial_writes_and_terminal_noise() {
    let mut partial = late_reload_startup_log();
    partial.push_str(
        "I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills\nI0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named ho",
    );
    let observation = observe_startup(partial.as_bytes());
    assert_eq!(observation.settle_line, Some(11));
    assert_eq!(observation.activity_lines, 7);
    partial.push_str("oks from 0 hooks.json file(s)\n");
    let observation = observe_startup(partial.as_bytes());
    assert_eq!(
        observation.settle_line,
        Some(12),
        "a hooks line restarts the quiet period once it is complete"
    );
    assert_eq!(observation.activity_lines, 8);

    let mut partial_startup =
        REAL_QUIET_STARTUP[..REAL_QUIET_STARTUP.find("I0924 17:20:28.663151").unwrap()].to_owned();
    partial_startup.push_str("I0924 17:20:28.663151     342 manager.go:934] Full redraw comp");
    assert_eq!(
        observe_startup(partial_startup.as_bytes()).redraw_after_startup,
        None
    );

    let noisy = successful_startup_log()
        .lines()
        .map(|line| format!("\u{1b}[32m{line}\u{1b}[0m\r\n"))
        .collect::<String>()
        .replace("CLI startup", "CLI\u{1b}]0;title\u{7} startup")
        .replace(
            "redraw completed",
            "redraw\u{1b}]8;;file:///x\u{1b}\\ completed",
        );
    let observation = observe_startup(noisy.as_bytes());
    assert!(observation.startup_completed());
    assert_eq!(observation.redraw_after_startup, Some(7));
    assert_eq!(observation.activity_lines, 8);
    assert_eq!(strip_terminal_noise("a\u{1b}[1;31mb\u{1b}Kc\r"), "abc");
    assert_eq!(
        strip_terminal_noise("\u{1b}]0;title\u{1b}\\CLI startup completed"),
        "CLI startup completed",
        "an OSC sequence ends at the string terminator ESC backslash"
    );
    assert_eq!(strip_terminal_noise("a\u{1b}]0;title\u{7}b"), "ab");
    assert_eq!(
        strip_terminal_noise("a\u{1b}]0;title\u{1b}[0mb"),
        "ab",
        "another escape ends an unterminated OSC and is stripped on its own"
    );
    assert_eq!(strip_terminal_noise("a\u{1b}]0;unterminated"), "a");
}

#[test]
fn readiness_gate_is_ready_after_the_quiet_period_on_the_quiet_healthy_startup() {
    // session-IQHEwf: the round-1/2 rule required a hooks_manager.go line after
    // the latest `... and skills` reload. This log has none, so that rule would
    // have waited until the deadline; the receipt-less silence was a healthy
    // idle composer.
    let lines: Vec<String> = complete_log_lines(REAL_QUIET_STARTUP.as_bytes()).collect();
    let latest_skills_reload = lines
        .iter()
        .rposition(|line| line.contains(SKILLS_RELOAD_MARKER))
        .unwrap();
    assert!(
        lines[latest_skills_reload..]
            .iter()
            .all(|line| !line.contains(HOOKS_LOADED_SOURCE)),
        "the old readiness discriminator never appears in this healthy log"
    );

    // The trust reload never comes either: the workspace was trusted before
    // launch. The gate is ready at 17:20:38.45, 3.5 s after the last plain
    // reload and 9.8 s after `CLI startup completed` (17:20:28.616574).
    let replay = LogReplay::new(REAL_QUIET_STARTUP, Instant::now());
    let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, ready, "17:20:38.450075", "session-IQHEwf");
    assert_eq!(
        replay.states(
            WINDOWS_STARTUP_READINESS_TIMING,
            &[
                "17:20:28.610124",
                "17:20:28.616574",
                "17:20:28.663151",
                "17:20:30.481001",
                "17:20:33.155749",
                "17:20:34.950075",
                "17:20:38.450074",
                "17:20:38.450075",
            ]
        ),
        vec![
            ReadinessState::AwaitingStartup,
            ReadinessState::AwaitingRedraw,
            ReadinessState::Settling,
            ReadinessState::Settling,
            ReadinessState::Settling,
            ReadinessState::Settling,
            ReadinessState::Settling,
            ReadinessState::Ready,
        ]
    );

    let mut gate = ReadinessGate::new(replay.start, WINDOWS_STARTUP_READINESS_TIMING);
    assert_eq!(
        gate.observe(None, replay.start),
        ReadinessState::AwaitingLog
    );
}

#[test]
fn readiness_gate_starts_the_quiet_period_at_the_newest_activity_line() {
    let start = Instant::now();
    let timing = WINDOWS_STARTUP_READINESS_TIMING;
    let at = |millis: u64| start + Duration::from_millis(millis);

    // session-fMqSQc with its trust reload logged: markers complete; ready
    // after one quiet period.
    let mut gate = ReadinessGate::new(start, timing);
    let success = settled_startup_log();
    assert_eq!(
        gate.observe(Some(success.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(success.as_bytes()), at(3_400)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(success.as_bytes()), at(3_500)),
        ReadinessState::Ready
    );

    // Any activity line restarts the quiet period: a plain reload, a skills
    // reload, a hooks line, or a redraw.
    let mut restarted = success.clone();
    for (millis, line) in [
        (
            3_600,
            glog("16:42:29.500000", 600, "manager.go:1308", SLASH_RELOAD),
        ),
        (
            7_200,
            glog("16:42:33.100000", 601, "manager.go:1331", SKILLS_RELOAD),
        ),
        (
            10_800,
            glog("16:42:36.700000", 602, "hooks_manager.go:53", HOOKS_LOADED),
        ),
        (
            14_400,
            glog("16:42:40.300000", 603, "manager.go:934", FULL_REDRAW),
        ),
    ] {
        restarted.push_str(&line);
        assert_eq!(
            gate.observe(Some(restarted.as_bytes()), at(millis)),
            ReadinessState::Settling,
            "{line:?} restarts the quiet period"
        );
        assert_eq!(
            gate.observe(Some(restarted.as_bytes()), at(millis + 3_499)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(restarted.as_bytes()), at(millis + 3_500)),
            ReadinessState::Ready
        );
    }

    // The quiet period counts from when the gate first saw the newest activity
    // line, so a log that is already old when the gate starts is ready after one
    // quiet period, not immediately.
    let mut aged = ReadinessGate::new(start, timing);
    assert_eq!(
        aged.observe(Some(success.as_bytes()), at(60_000)),
        ReadinessState::Settling
    );
    assert_eq!(
        aged.observe(Some(success.as_bytes()), at(63_500)),
        ReadinessState::Ready
    );

    // Startup without a redraw after it is not ready however quiet the log is.
    let mut no_redraw = ReadinessGate::new(start, timing);
    let startup_only = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    );
    assert_eq!(
        no_redraw.observe(Some(startup_only.as_bytes()), at(0)),
        ReadinessState::AwaitingRedraw
    );
    assert_eq!(
        no_redraw.observe(Some(startup_only.as_bytes()), at(55_000)),
        ReadinessState::AwaitingRedraw
    );
    let redrawn =
        startup_only.clone() + &glog("16:42:02.000000", 269, "manager.go:934", FULL_REDRAW);
    assert_eq!(
        no_redraw.observe(Some(redrawn.as_bytes()), at(55_100)),
        ReadinessState::Settling
    );
    assert_eq!(
        no_redraw.observe(Some(redrawn.as_bytes()), at(58_600)),
        ReadinessState::Ready
    );

    // A skills reload restarts the quiet period; a hooks line is not required.
    let startup_redraw =
        startup_only.clone() + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
    let mut reloaded = ReadinessGate::new(start, timing);
    assert_eq!(
        reloaded.observe(Some(startup_redraw.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    let with_reload = startup_redraw.clone()
        + &glog("16:41:12.000000", 410, "manager.go:1331", SKILLS_RELOAD)
        + &glog("16:41:12.000000", 410, "manager.go:1308", SLASH_RELOAD);
    assert_eq!(
        reloaded.observe(Some(with_reload.as_bytes()), at(4_200)),
        ReadinessState::Settling
    );
    assert_eq!(
        reloaded.observe(Some(with_reload.as_bytes()), at(7_699)),
        ReadinessState::Settling
    );
    assert_eq!(
        reloaded.observe(Some(with_reload.as_bytes()), at(7_700)),
        ReadinessState::Ready
    );

    let mut no_startup = ReadinessGate::new(start, timing);
    let redraw_only = glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
    assert_eq!(
        no_startup.observe(Some(redraw_only.as_bytes()), at(10_000)),
        ReadinessState::AwaitingStartup
    );
}

#[test]
fn log_continuity_accepts_only_a_log_that_extends_the_observed_content() {
    let observed = successful_startup_log();
    let continuity = LogContinuity::of(observed.as_bytes());
    assert_eq!(continuity.len, observed.len());
    assert_eq!(continuity.digest, log_digest(observed.as_bytes()));

    // Growth, including a partial trailing line that later completes, continues.
    let grown = observed.clone() + "I0924 16:42:30.000000     540 manager.go:1308] Reloading";
    assert_eq!(continuity.discontinuity(grown.as_bytes()), None);
    assert_eq!(
        LogContinuity::of(grown.as_bytes())
            .discontinuity((grown.clone() + " system slash commands\n").as_bytes()),
        None
    );
    assert_eq!(continuity.discontinuity(observed.as_bytes()), None);

    // A shorter file, or one with different leading bytes, does not.
    assert_eq!(
        continuity.discontinuity(&observed.as_bytes()[..observed.len() - 1]),
        Some(LogDiscontinuity::Shrunk {
            from: observed.len(),
            to: observed.len() - 1
        })
    );
    let replaced = observed.replace("16:42:24", "16:52:24");
    assert_eq!(replaced.len(), observed.len());
    assert_eq!(
        continuity.discontinuity(replaced.as_bytes()),
        Some(LogDiscontinuity::Replaced {
            observed: observed.len()
        })
    );

    // Every observed byte is compared, not a leading window: in a log well beyond
    // 4 KiB, a one-byte change in a late line (the round-5 counterexample, which
    // the 4 KiB prefix accepted as continuation), a rewrite of the last line that
    // keeps its index and the length, and a change in the first line are all
    // discontinuities.
    let long = REAL_QUIET_STARTUP.to_owned() + REAL_DEFERRED_RELOAD_STARTUP;
    assert!(long.len() > 4096);
    let continuity = LogContinuity::of(long.as_bytes());
    let changed_late = long.replace("17:47:13.468286", "17:47:13.468287");
    assert_ne!(changed_late, long);
    assert_eq!(changed_late.len(), long.len());
    assert_eq!(
        continuity.discontinuity(changed_late.as_bytes()),
        Some(LogDiscontinuity::Replaced {
            observed: long.len()
        }),
        "a rewrite beyond 4 KiB is a discontinuity"
    );
    let last_line = long.lines().last().unwrap();
    let rewritten_tail = long[..long.len() - last_line.len() - 1].to_owned()
        + &last_line.chars().rev().collect::<String>()
        + "\n";
    assert_eq!(rewritten_tail.len(), long.len());
    assert_eq!(rewritten_tail.lines().count(), long.lines().count());
    assert_eq!(
        continuity.discontinuity(rewritten_tail.as_bytes()),
        Some(LogDiscontinuity::Replaced {
            observed: long.len()
        }),
        "a suffix rewrite that keeps every line index is a discontinuity"
    );
    assert_eq!(
        continuity.discontinuity((rewritten_tail + "more\n").as_bytes()),
        Some(LogDiscontinuity::Replaced {
            observed: long.len()
        }),
        "a longer log whose observed prefix was rewritten is a discontinuity"
    );
    let changed_early = long.replacen("17:20:28.610124", "17:20:28.610125", 1);
    assert_eq!(
        continuity.discontinuity(changed_early.as_bytes()),
        Some(LogDiscontinuity::Replaced {
            observed: long.len()
        })
    );
    assert_eq!(
        continuity.discontinuity((long.clone() + &changed_late).as_bytes()),
        None,
        "only the observed prefix is digested; appended bytes are free"
    );
}

#[test]
fn readiness_gate_restarts_its_evidence_when_the_log_is_replaced() {
    let start = Instant::now();
    let timing = WINDOWS_STARTUP_READINESS_TIMING;
    let at = |millis: u64| start + Duration::from_millis(millis);
    let startup_redraw = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    ) + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);

    // Startup and redraw at t=0, the log missing at t=19.9, and at t=20 a
    // replacement with the same content and indices: the quiet period must restart.
    let mut gate = ReadinessGate::new(start, timing);
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(gate.observe(None, at(19_900)), ReadinessState::AwaitingLog);
    assert_eq!(
        gate.discontinuities,
        vec![(
            Duration::from_millis(19_900),
            LogDiscontinuity::Missing {
                observed: startup_redraw.len()
            }
        )]
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(20_000)),
        ReadinessState::Settling,
        "a replacement with identical indices at t=20 is not ready"
    );
    assert_eq!(gate.settled_at, at(20_000));
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(23_499)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(23_500)),
        ReadinessState::Ready,
        "the replacement is ready when its own quiet period ends"
    );
    assert_eq!(
        gate.discontinuities.len(),
        1,
        "the reappearance is not counted again"
    );

    // Replacement without an observed missing interval: the same lines at the
    // same indices and the same length, re-stamped by a fresh process. The
    // leading bytes differ, so the evidence restarts at t=20 as well.
    let restamped = startup_redraw.replace("16:41:07", "16:51:07");
    assert_eq!(restamped.len(), startup_redraw.len());
    let mut gate = ReadinessGate::new(start, timing);
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(19_900)),
        ReadinessState::Ready
    );
    assert_eq!(
        gate.observe(Some(restamped.as_bytes()), at(20_000)),
        ReadinessState::Settling,
        "a replaced log with identical indices at t=20 is not ready"
    );
    assert_eq!(
        gate.discontinuities,
        vec![(
            Duration::from_millis(20_000),
            LogDiscontinuity::Replaced {
                observed: startup_redraw.len()
            }
        )]
    );
    assert_eq!(
        gate.observe(Some(restamped.as_bytes()), at(23_499)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(restamped.as_bytes()), at(23_500)),
        ReadinessState::Ready
    );

    // A replacement with a skills reload still owes a full quiet period from the
    // observation of the new content, not from the old settle instant.
    let reloaded = startup_redraw.clone()
        + &glog("16:41:12.000000", 410, "manager.go:1331", SKILLS_RELOAD)
        + &glog("16:41:12.000000", 410, "manager.go:1308", SLASH_RELOAD);
    let mut gate = ReadinessGate::new(start, timing);
    assert_eq!(
        gate.observe(Some(reloaded.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(reloaded.as_bytes()), at(3_500)),
        ReadinessState::Ready
    );
    let fresh = reloaded.replace("16:41:", "16:51:");
    assert_eq!(
        gate.observe(Some(fresh.as_bytes()), at(3_600)),
        ReadinessState::Settling,
        "a fresh reload at the old index is not settled"
    );
    assert_eq!(
        gate.observe(Some(fresh.as_bytes()), at(7_099)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(fresh.as_bytes()), at(7_100)),
        ReadinessState::Ready
    );

    // Rotation to a shorter file: the tail of session-udT6uY without its startup
    // is a new log with no startup line, and a startup appended to it later is a
    // new startup.
    let observed = settled_startup_log();
    let tail: String = observed
        .lines()
        .skip(8)
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(tail.len() < observed.len());
    assert!(!tail.contains(STARTUP_COMPLETED_MARKER));
    let mut gate = ReadinessGate::new(start, timing);
    assert_eq!(
        gate.observe(Some(observed.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(tail.as_bytes()), at(5_000)),
        ReadinessState::AwaitingStartup
    );
    assert_eq!(
        gate.discontinuities,
        vec![(
            Duration::from_secs(5),
            LogDiscontinuity::Shrunk {
                from: observed.len(),
                to: tail.len()
            }
        )]
    );
    let rotated_startup = tail.clone()
        + &glog(
            "16:42:35.000000",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        )
        + &glog("16:42:35.050000", 269, "manager.go:934", FULL_REDRAW);
    assert_eq!(
        gate.observe(Some(rotated_startup.as_bytes()), at(6_000)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(rotated_startup.as_bytes()), at(9_499)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(rotated_startup.as_bytes()), at(9_500)),
        ReadinessState::Ready
    );
    assert_eq!(gate.discontinuities.len(), 1);

    // A log that is missing before it was ever observed is not a discontinuity.
    let mut gate = ReadinessGate::new(start, timing);
    assert_eq!(gate.observe(None, at(0)), ReadinessState::AwaitingLog);
    assert_eq!(gate.observe(None, at(100)), ReadinessState::AwaitingLog);
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(200)),
        ReadinessState::Settling
    );
    assert!(gate.discontinuities.is_empty());

    // The rule in force, on the review scenario: ready one quiet period after
    // the redraw, and the replacement at t=20 owes a quiet period of its own.
    let mut gate = ReadinessGate::new(start, WINDOWS_STARTUP_READINESS_TIMING);
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(3_500)),
        ReadinessState::Ready
    );
    assert_eq!(gate.observe(None, at(19_900)), ReadinessState::AwaitingLog);
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(20_000)),
        ReadinessState::Settling,
        "a replacement with identical indices at t=20 is not ready"
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(23_499)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(startup_redraw.as_bytes()), at(23_500)),
        ReadinessState::Ready
    );
    assert_eq!(gate.discontinuities.len(), 1);
}

#[test]
fn readiness_wait_restarts_after_every_log_discontinuity_within_the_deadline() {
    let start = Instant::now();
    let poll = Duration::from_millis(100);
    let deadline = start + Duration::from_secs(300);
    let observed = settled_startup_log();
    let half = before_redraw(&observed);
    assert!(half.contains(STARTUP_COMPLETED_MARKER));
    assert!(!half.contains(FULL_REDRAW_MARKER));

    // Two replacements without a missing interval: each restarts the evidence,
    // and the log observed after the second one is ready one quiet period after
    // it was first seen. The round-5 gate failed `not_sent` at 200 ms here. The
    // returned offset is the length of the read that passed.
    let restamped = observed.replace("16:41:", "16:51:");
    let restamped_again = observed.replace("16:41:", "17:01:");
    let mut clock = FakeClock::new(start);
    let offset = wait_for_startup_readiness_with(
        &mut log_sequence(vec![
            some_log(&observed),
            some_log(&restamped),
            some_log(&restamped_again),
        ]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(offset, restamped_again.len());
    assert_eq!(
        clock.slept,
        Duration::from_millis(200) + STARTUP_QUIET_PERIOD
    );

    // Disappearance, reappearance, then rotation to a tail without the redraw:
    // the wait continues past the second discontinuity and fails only at the
    // deadline, naming both discontinuities in order and the missing marker.
    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![
            some_log(&observed),
            Ok(None),
            some_log(&observed),
            some_log(half),
        ]),
        start + Duration::from_secs(2),
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("Agy did not report startup readiness before the deadline"));
    assert!(message.contains(&format!(
        "the readiness evidence was restarted after 2 log discontinuities (agy.log disappeared after {} bytes had been observed at 100 ms; then agy.log shrank from {} to {} bytes (rotated or truncated) at 300 ms)",
        observed.len(),
        observed.len(),
        half.len()
    )));
    assert!(message.contains(&format!(
        "missing markers: {REDRAW_AFTER_STARTUP_DESCRIPTION}, "
    )));
    assert!(message.contains("the prompt was not pasted"));
    assert!(!message.contains("tolerated"));
    assert_eq!(clock.slept, Duration::from_secs(2));

    // Three discontinuities, then a stable log: still ready within the deadline.
    let mut clock = FakeClock::new(start);
    let offset = wait_for_startup_readiness_with(
        &mut log_sequence(vec![
            some_log(&observed),
            some_log(half),
            Ok(None),
            some_log(&restamped),
            some_log(&restamped_again),
        ]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(offset, restamped_again.len());
    assert_eq!(
        clock.slept,
        Duration::from_millis(400) + STARTUP_QUIET_PERIOD
    );

    // A single replacement restarts the wait: the restamped log is ready one
    // quiet period after it was first observed, not at once.
    let mut clock = FakeClock::new(start);
    let offset = wait_for_startup_readiness_with(
        &mut log_sequence(vec![
            some_log(&observed),
            some_log(&observed),
            some_log(&observed),
            some_log(&restamped),
        ]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(offset, restamped.len());
    assert_eq!(
        clock.slept,
        Duration::from_millis(300) + STARTUP_QUIET_PERIOD
    );

    // The deadline report names a single discontinuity that was tolerated.
    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&observed), Ok(None), some_log(&observed)]),
        start + Duration::from_secs(2),
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("Agy did not report startup readiness before the deadline"));
    assert!(message.contains(&format!(
        "the readiness evidence was restarted after 1 log discontinuity (agy.log disappeared after {} bytes had been observed at 100 ms)",
        observed.len()
    )));
    assert!(message.contains(&format!(
        "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period"
    )));
}

// The read that supplies the paste offset is a gate observation: when the read
// at which the quiet period would end shows a fresh reload, a replacement, or no
// log at all, nothing is pasted; the gate waits for a later read that passes
// both the readiness rule and continuity and returns that read's length.
#[test]
fn readiness_wait_pastes_only_after_a_read_that_passes_readiness_and_continuity() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(300);
    let poll = Duration::from_millis(100);
    let startup = settled_startup_log();
    let fresh_reload = glog("16:42:32.000000", 540, "manager.go:1308", SLASH_RELOAD);
    let reloaded = startup.clone() + &fresh_reload;
    let restamped = startup.replace("16:41:", "16:51:");
    assert_eq!(restamped.len(), startup.len());
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    // The static startup log is ready when its quiet period ends.
    let ready_at = STARTUP_QUIET_PERIOD;
    let mut clock = FakeClock::new(start);
    let offset = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup)]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(offset, startup.len());
    assert_eq!(clock.slept, ready_at);

    // A reader whose observation at `ready_at` is `at_ready`, and `after` from
    // then on; every earlier read is the startup log.
    let waited = |at_ready: Result<Option<Vec<u8>>>, after: &str| -> (usize, FakeClock) {
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let mut at_ready = Some(at_ready);
        let after = after.to_owned();
        let startup = startup.clone();
        let mut read_log = move || -> Result<Option<Vec<u8>>> {
            let elapsed = now.get().saturating_duration_since(start);
            if elapsed < ready_at {
                some_log(&startup)
            } else if elapsed == ready_at {
                at_ready
                    .take()
                    .expect("the ready-instant read happens once")
            } else {
                some_log(&after)
            }
        };
        let offset = wait_for_startup_readiness_with(
            &mut read_log,
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        (offset, clock)
    };

    // The pre-paste read carries a fresh reload: no paste at `ready_at`; the
    // reload owes its own quiet period, and the offset covers the reload line.
    let (offset, clock) = waited(some_log(&reloaded), &reloaded);
    assert_eq!(clock.slept, ready_at + STARTUP_QUIET_PERIOD);
    assert_eq!(offset, reloaded.len());
    let receipt = framed_receipt_line("hello", &pending);
    assert_eq!(
        observe_input_receipt(
            Some((reloaded.clone() + &receipt).as_bytes()),
            offset,
            &pending
        ),
        ReceiptEvidence::Delivered,
        "the later paste is confirmed by a receipt after the reload line"
    );
    assert_eq!(
        observe_input_receipt(Some(reloaded.as_bytes()), startup.len(), &pending),
        ReceiptEvidence::NoReceipt {
            appended: fresh_reload.len(),
            partial_tail: false
        },
        "the reload line is not evidence at the round-5 offset either"
    );

    // The pre-paste read finds no log: a discontinuity, never offset zero; the
    // reappearing log is a new observation with its own quiet period.
    let (offset, clock) = waited(Ok(None), &startup);
    assert_eq!(clock.slept, ready_at + poll + STARTUP_QUIET_PERIOD);
    assert_eq!(offset, startup.len());

    // The pre-paste read is a re-stamped replacement of the same length: the
    // same indices, but a discontinuity that restarts the quiet period.
    let (offset, clock) = waited(some_log(&restamped), &restamped);
    assert_eq!(clock.slept, ready_at + STARTUP_QUIET_PERIOD);
    assert_eq!(offset, restamped.len());

    // The pre-paste read is a plain continuation: pasted at once at its length,
    // including a partial line appended since the previous read.
    let grown = startup.clone() + "I0924 16:42:33.000000     560 manager.go:1308] Reloading";
    let (offset, clock) = waited(some_log(&grown), &grown);
    assert_eq!(clock.slept, ready_at);
    assert_eq!(offset, grown.len());
}

#[test]
fn log_replay_anchors_its_origin_to_the_first_timestamped_line() {
    // A glog file header before the first timestamped line: the replay used to
    // take zero as the origin, scheduling the startup line 16 h 42 min after
    // `start` and past the 300 s replay deadline.
    let header = "Log line format: [IWEF]mmdd hh:mm:ss.uuuuuu threadid file:line] msg\n";
    assert_eq!(glog_time_of_day(header.trim_end()), None);
    let start = Instant::now();
    let replay = LogReplay::new(&format!("{header}{REAL_SUCCESS_STARTUP}"), start);
    assert_eq!(replay.lines[0], (start, header.to_owned()));
    assert_eq!(
        replay.lines[1].0, start,
        "the first timestamped line is the origin"
    );
    assert!(replay.lines[1].1.starts_with("I0924 16:42:24.068931"));
    let (startup_at, startup_line) = replay
        .lines
        .iter()
        .find(|(_, line)| line.contains(STARTUP_COMPLETED_MARKER))
        .unwrap();
    assert!(startup_line.starts_with("I0924 16:42:24.080467"));
    assert_eq!(
        *startup_at,
        start + Duration::from_micros(11_536),
        "16:42:24.080467 is 11.536 ms after the first timestamp 16:42:24.068931"
    );
    assert_eq!(*startup_at, replay.recorded("16:42:24.080467"));
    let visible = String::from_utf8(replay.visible_at(start).unwrap()).unwrap();
    assert!(visible.starts_with(header));
    assert!(visible.contains("16:42:24.068931"));
    assert!(!visible.contains(STARTUP_COMPLETED_MARKER));

    // session-udT6uY: the gate is ready 3.5 s after the last plain reload at
    // 16:42:29.104592, whether or not the log starts with a header.
    let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(
        &replay,
        ready,
        "16:42:32.604592",
        "session-udT6uY with a header",
    );
    let without_header = LogReplay::new(REAL_SUCCESS_STARTUP, start);
    assert_eq!(
        replay_readiness(&without_header, WINDOWS_STARTUP_READINESS_TIMING),
        ready
    );
}

// The recorded lost pastes (2026-09-24, Agy 1.2.10, Windows console) under the
// rule in force. Each paste landed on the
// workspace-trust dialog, and the trust reload these logs show is what its Enter
// caused. The gate cannot see the dialog: it is ready one quiet period after the
// startup burst, before that reload in every one of them, and a window only moved
// the paste, and the loss, later. What withholds these pastes is the trust
// evidence, which none of these logs holds before its paste
// (`workspace_customization_load_is_read_from_the_sessions_own_log`).
#[test]
fn readiness_gate_is_ready_before_the_trust_reload_of_the_lost_fixture() {
    // session-fMqSQc: ready at 16:41:17.24, 3.5 s after the 16:41:13 reload; the
    // fixed 12 s delay pasted at about 16:41:19, and the trust reload followed at
    // 16:41:20.81, 13.0 s after startup.
    let log = late_reload_startup_log() + &late_reload_completion();
    let replay = LogReplay::new(&log, Instant::now());
    let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, ready, "16:41:17.237805", "session-fMqSQc");
    assert!(
        ready < replay.recorded("16:41:20.813553"),
        "the gate is ready before the trust reload"
    );
    assert!(
        !workspace_customizations_loaded(late_reload_startup_log().as_bytes()),
        "the trust evidence is missing before the paste"
    );

    // A skills reload during the quiet period restarts the period; a hooks line
    // after it is never demanded.
    let start = Instant::now();
    let at = |millis: u64| start + Duration::from_millis(millis);
    let mut gate = ReadinessGate::new(start, WINDOWS_STARTUP_READINESS_TIMING);
    let late = late_reload_startup_log();
    assert_eq!(
        gate.observe(Some(late.as_bytes()), at(0)),
        ReadinessState::Settling
    );
    let reloading = late.clone()
        + &glog("16:41:15.000000", 410, "manager.go:1331", SKILLS_RELOAD)
        + &glog("16:41:15.000000", 410, "manager.go:1308", SLASH_RELOAD);
    assert_eq!(
        gate.observe(Some(reloading.as_bytes()), at(1_300)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(reloading.as_bytes()), at(4_700)),
        ReadinessState::Settling
    );
    assert_eq!(
        gate.observe(Some(reloading.as_bytes()), at(4_800)),
        ReadinessState::Ready,
        "silence after a reload without a hooks line is readiness"
    );
}

#[test]
fn readiness_gate_is_ready_where_the_recorded_paste_met_the_trust_dialog() {
    // session-IEKjtC: ready at 17:47:13.12, 3.5 s after the 17:47:09.62 plain
    // reload, which is where the round-3 build pasted. The trust reload at
    // 17:47:13.468 (9.8 s after startup) followed that paste by 0.35 s and no
    // receipt came.
    let replay = LogReplay::new(REAL_DEFERRED_RELOAD_STARTUP, Instant::now());
    let trust_reload = replay.recorded("17:47:13.468286");
    let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, ready, "17:47:13.116209", "session-IEKjtC");
    assert!(
        ready < trust_reload,
        "the gate is ready before the trust reload"
    );
}

#[test]
fn readiness_gate_is_ready_after_the_quiet_period_before_the_ql5tvc_trust_reload() {
    // session-ql5TVc: the quiet period ended at 18:48:12.15, 3.5 s after the
    // 18:48:08.65 plain reload, which is when the rule in force is ready. The
    // round-6 rule's 20 s window ended at 18:48:24.12, when it pasted, and the
    // trust reload at 18:48:25.481988 (21.4 s after startup) followed that paste
    // with no receipt. The 45 s rule on the recording is ready 3.5 s after the
    // reload's hooks line.
    let replay = LogReplay::new(REAL_LATE_DEFERRED_RELOAD_STARTUP, Instant::now());
    let deferred_reload = replay.recorded("18:48:25.481988");
    let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, in_force, "18:48:12.146591", "session-ql5TVc");
    assert!(in_force < deferred_reload);
}

// The recorded delivered pastes of 2026-09-24 (Agy 1.2.10, Windows console): the
// workspace was trusted before launch, the customization load is logged at
// startup, and no trust reload ever follows. The rule in force is ready one quiet
// period after the last plain reload; the 45 s rule waited out its window for a
// reload that was not coming.
#[test]
fn readiness_gate_is_ready_after_the_quiet_period_where_the_recorded_pastes_were_delivered() {
    // session-udT6uY: the skills reload 0.5 ms after `CLI startup completed` is
    // the startup reload, with the customization load right behind it. Ready
    // 3.5 s after the last plain reload at 16:42:29.10; the paste of the fixed
    // 12 s delay was received at 16:42:50.21. The 45 s window ended at
    // 16:43:09.08.
    assert!(workspace_customizations_loaded(
        REAL_SUCCESS_STARTUP.as_bytes()
    ));
    let replay = LogReplay::new(REAL_SUCCESS_STARTUP, Instant::now());
    let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, in_force, "16:42:32.604592", "session-udT6uY");
    // session-M8QFPp: the same shape 1.6 ms after startup. The round-8 rule took
    // the startup reload for the deferred one and pasted at 20:37:49.15, 3.5 s
    // after the last plain reload; that paste was received. It is the instant of
    // the rule in force. The 45 s window ended at 20:38:25.65.
    assert!(workspace_customizations_loaded(
        REAL_STARTUP_RELOAD_AFTER_STARTUP.as_bytes()
    ));
    let replay = LogReplay::new(REAL_STARTUP_RELOAD_AFTER_STARTUP, Instant::now());
    let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(
        &replay,
        in_force,
        "20:37:49.146941",
        "session-M8QFPp (the instant of the recorded, delivered paste)",
    );
}

#[test]
fn readiness_gate_is_ready_after_the_quiet_period_before_the_uqraap_trust_reload() {
    // session-uqraap: the quiet period ended at 20:44:09.93, 3.5 s after the
    // 20:44:06.43 plain reload, which is when the rule in force is ready. The
    // round-8 rule's 35 s window ended at 20:44:36.44, when it pasted, and the
    // trust reload at 20:44:37.855815 (36.4 s after startup) followed that paste
    // with no receipt. The 45 s rule on the recording is ready 3.5 s after the
    // reload's hooks line.
    let replay = LogReplay::new(REAL_SECOND_CLUSTER_RELOAD_STARTUP, Instant::now());
    let deferred_reload = replay.recorded("20:44:37.855815");
    let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
    assert_ready_at(&replay, in_force, "20:44:09.927564", "session-uqraap");
    assert!(in_force < deferred_reload);
}

#[test]
fn readiness_gate_deadline_report_names_every_missing_marker() {
    let start = Instant::now();
    let poll = Duration::from_millis(100);
    let deadline = start + Duration::from_secs(1);
    let no_quiet_period = ReadinessTiming {
        quiet_period: Duration::ZERO,
        ..WINDOWS_STARTUP_READINESS_TIMING
    };

    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![Ok(None)]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert_eq!(
        message,
        format!(
            "Agy did not report startup readiness before the deadline: agy.log has not been created; missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}, {QUIET_PERIOD_DESCRIPTION}; the quiet period is 3500 ms, timed from the newest reload, redraw, or hooks line; the prompt was not pasted"
        )
    );
    assert!(message.contains("agy.log has not been created"));
    assert!(message.contains(&format!(
        "missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}, {QUIET_PERIOD_DESCRIPTION}; the quiet period is 3500 ms, timed from the newest reload, redraw, or hooks line; the prompt was not pasted"
    )));
    assert!(
        !message.contains("deferred reload"),
        "the rule in force has no window to report"
    );
    assert!(!message.contains("hooks completion"));
    assert_eq!(clock.slept, Duration::from_secs(1));

    let startup_only = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    );
    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup_only)]),
        deadline,
        no_quiet_period,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("no `Full redraw completed` line after `CLI startup completed`"));
    assert!(!message.contains("missing markers: `CLI startup completed`"));
    assert!(message.contains(&format!(
        "missing markers: {REDRAW_AFTER_STARTUP_DESCRIPTION}; the quiet period"
    )));

    let redraw_only = glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&redraw_only)]),
        deadline,
        no_quiet_period,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("agy.log has no `CLI startup completed` line"));
    assert!(message.contains(&format!(
        "missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}; the quiet period"
    )));

    // session-fMqSQc with its trust reload logged: only the quiet period is
    // missing at 1 s.
    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&settled_startup_log())]),
        deadline,
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("logged a reload, redraw, or hooks line within the quiet period"));
    assert!(message.contains(&format!(
        "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period"
    )));

    let startup_redraw = startup_only.clone() + &redraw_only;

    // Startup seen at once, the redraw a poll later, no skills reload: ready one
    // quiet period after the redraw was first seen, at 3.6 s.
    let mut clock = FakeClock::new(start);
    wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup_only), some_log(&startup_redraw)]),
        start + Duration::from_secs(60),
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, poll + STARTUP_QUIET_PERIOD);

    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![Err(anyhow::anyhow!(
            "refusing non-regular session file"
        ))]),
        deadline,
        no_quiet_period,
        poll,
        &mut FakeClock::new(start),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("startup readiness could not be observed"));
}

// The rule, stated as deadlines: with startup and its redraw already logged and
// nothing after them, the quiet period ends at 3.5 s and the gate is ready then.
// A deadline past it reaches the paste; a shorter one fails `not_sent` naming the
// quiet period as the only missing condition.
#[test]
fn readiness_wait_pastes_when_the_quiet_period_ends_and_not_before() {
    let start = Instant::now();
    let poll = Duration::from_millis(100);
    let startup_redraw = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    ) + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);

    let mut clock = FakeClock::new(start);
    let offset = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup_redraw)]),
        start + Duration::from_secs(4),
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap();
    assert_eq!(offset, startup_redraw.len());
    assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);

    let mut clock = FakeClock::new(start);
    let error = wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(&startup_redraw)]),
        start + Duration::from_secs(3),
        WINDOWS_STARTUP_READINESS_TIMING,
        poll,
        &mut clock,
    )
    .unwrap_err();
    assert_eq!(clock.slept, Duration::from_secs(3));
    let message = format!("{error:#}");
    assert!(message.contains("logged a reload, redraw, or hooks line within the quiet period"));
    assert!(message.contains(&format!(
        "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period is 3500 ms, timed from the newest reload, redraw, or hooks line; the prompt was not pasted"
    )));
    let failure = terminal::TerminalSendFailure::not_sent(error);
    assert!(
        !failure.delivery_may_have_occurred(),
        "a gate that never passed is not_sent, never uncertain"
    );
}

// The macOS follow-up rule on the real macOS log: the redraw is not required and
// there is no window, so the log-only gate is ready one quiet period after the
// newest activity line. That is before the instant at which the smoke pasted onto
// the trust dialog: the log of an untrusted session shows nothing until the dialog
// is answered, which is why the paste waits for Agy's trust store first. The
// Windows rule never passes this log because no redraw line ever arrives.
#[test]
fn macos_follow_up_gate_needs_only_startup_and_a_quiet_period() {
    let observation = observe_startup(REAL_MACOS_INITIAL_TURN.as_bytes());
    assert_eq!(observation.startup_line, Some(REAL_MACOS_STARTUP_LINE));
    assert_eq!(
        observation.redraw_after_startup, None,
        "Agy logs no Full redraw completed on macOS"
    );
    assert_eq!(
        observation.conversation_reload_after_startup,
        Some(REAL_MACOS_CONVERSATION_RELOAD_LINE)
    );
    assert_eq!(
        observation.deferred_reload_after_startup,
        Some(REAL_MACOS_DEFERRED_RELOAD_LINE)
    );
    assert_eq!(observation.startup_reload_after_startup, None);
    assert!(observation.markers_observed(false));
    assert!(observation.missing_markers(false).is_empty());
    assert!(!observation.markers_observed(true));
    assert_eq!(
        observation.missing_markers(true),
        vec![REDRAW_AFTER_STARTUP_DESCRIPTION]
    );

    let replay = LogReplay::new(REAL_MACOS_INITIAL_TURN, Instant::now());
    // The newest activity line before the paste is the plain reload at
    // 21:32:14.739; one quiet period later the log-only rule is ready, although
    // the dialog was still up when the smoke pasted at 21:32:18.79.
    assert_eq!(
        replay.states(
            MACOS_FOLLOW_UP_READINESS_TIMING,
            &["21:32:14.800000", "21:32:18.790000"]
        ),
        vec![ReadinessState::Settling, ReadinessState::Ready]
    );
    let ready = replay_readiness(&replay, MACOS_FOLLOW_UP_READINESS_TIMING);
    assert_ready_at(
        &replay,
        ready,
        "21:32:18.239083",
        "session-QMFk6F, macOS rule",
    );
    assert!(
        ready < replay.recorded("21:32:19.637013"),
        "the trust reload is the paste's consequence, not something the gate waits for"
    );

    let mut clock = FakeClock::new(replay.start);
    let error = wait_for_startup_readiness_with(
        &mut replay.reader(clock.shared()),
        replay.start + Duration::from_secs(120),
        WINDOWS_STARTUP_READINESS_TIMING,
        REPLAY_POLL,
        &mut clock,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(REDRAW_AFTER_STARTUP_DESCRIPTION),
        "{message}"
    );
    assert!(message.contains("the prompt was not pasted"), "{message}");
}

// A skills reload right after a conversation start is the conversation reload on
// Windows too, and a reload that follows a conversation start by more than the
// latency bound is still the deferred one.
#[test]
fn conversation_reload_rule_is_bounded_by_its_latency() {
    let startup = glog(
        "21:32:08.459241",
        1,
        "analytics.go:187",
        "CLI startup completed (took 285.555917ms)",
    );
    let started = glog(
        "21:32:11.355776",
        337,
        "conversation_manager.go:512",
        "Starting new conversation (agent=false)",
    );
    let prompt_reload = glog("21:32:11.362782", 423, "manager.go:1331", SKILLS_RELOAD);
    let log = startup.clone() + &started + &prompt_reload;
    let observation = observe_startup(log.as_bytes());
    assert_eq!(observation.conversation_reload_after_startup, Some(2));
    assert_eq!(observation.deferred_reload_after_startup, None);

    let late_reload = glog("21:32:12.400000", 623, "manager.go:1331", SKILLS_RELOAD);
    let log = startup.clone() + &started + &late_reload;
    let observation = observe_startup(log.as_bytes());
    assert_eq!(observation.conversation_reload_after_startup, None);
    assert_eq!(
        observation.deferred_reload_after_startup,
        Some(2),
        "1.04 s after the conversation start is past the bound"
    );

    // A reload stamped before the conversation start (threads out of order) is
    // judged against startup only.
    let early_reload = glog("21:32:11.300000", 623, "manager.go:1331", SKILLS_RELOAD);
    let log = startup + &started + &early_reload;
    let observation = observe_startup(log.as_bytes());
    assert_eq!(observation.conversation_reload_after_startup, None);
    assert_eq!(observation.deferred_reload_after_startup, Some(2));
}

// Old and recent logs both owe one quiet period from their first observation.
// Keep the historical test name; only the current rules are exercised.
#[test]
fn readiness_wait_requires_one_quiet_period_from_first_observation_regardless_of_log_age() {
    let quota_line = glog(
        "17:21:20.000000",
        248,
        "quota_manager.go:41",
        "doRefreshQuota: skipped (throttled)",
    );
    let aged = REAL_QUIET_STARTUP.to_owned() + &quota_line;
    let observation = observe_startup(aged.as_bytes());
    assert_eq!(observation.deferred_reload_after_startup, None);
    assert_eq!(observation.newest_stamp, glog_timestamp(&quota_line));

    let start = Instant::now();
    // The rules in force pass both logs after one quiet period.
    for log in [aged.as_str(), REAL_QUIET_STARTUP] {
        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(log)]),
            start + Duration::from_secs(300),
            WINDOWS_STARTUP_READINESS_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
    }

    // The macOS rule passes the un-aged macOS log after one quiet period.
    let before_reload = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
        .rfind("I0924 21:32:19.637013")
        .unwrap()];
    let mut clock = FakeClock::new(start);
    wait_for_startup_readiness_with(
        &mut log_sequence(vec![some_log(before_reload)]),
        start + Duration::from_secs(300),
        MACOS_FOLLOW_UP_READINESS_TIMING,
        Duration::from_millis(100),
        &mut clock,
    )
    .unwrap();
    assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
}

// session-C2fMs7 and session-uTpvwY (the Windows machine, 2026-10-01 22:43 KST, Agy
// 1.2.14, native Windows console in a Windows Terminal tab, the rule without a
// window): the startup of two sessions in a workspace trusted before launch,
// verbatim. The omitted lines are HTTP, auth, model and quota chatter. Each logs
// the workspace customization load at startup (goroutines 327 and 311) and no
// trust reload afterwards. The pastes followed the quiet period, at 22:43:26.27
// and 22:43:48.67 (8.8 and 7.9 s after startup), and Agy logged their receipts
// at 22:43:27.485085 and 22:43:50.112237. This is the LIVE paste right after the
// quiet period that the 45 s window was kept waiting for; session-jkxi48 (22:38,
// Agy 1.2.10, the 45 s rule) had logged the same startup, no trust reload, and
// its receipt 2.1 s after the paste at +45 s.
const REAL_WINDOWS_TRUSTED_STARTUP: &str = r"I1001 22:43:17.413565       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:17.418295       1 common.go:448] Starting CLI program
CLI ready for user input
I1001 22:43:17.423669     328 manager.go:1333] Reloading system slash commands and skills
I1001 22:43:17.423669     328 manager.go:1310] Reloading system slash commands
I1001 22:43:17.423669     328 manager.go:1314] Slash commands unchanged, skipping update
I1001 22:43:17.426377     327 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:17.426377       1 analytics.go:189] CLI startup completed (took 286.5143ms)
I1001 22:43:17.472179     499 manager.go:935] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I1001 22:43:19.933070     614 manager.go:1310] Reloading system slash commands
I1001 22:43:22.687836     654 manager.go:1310] Reloading system slash commands
I1001 22:43:22.693045     654 manager.go:1314] Slash commands unchanged, skipping update
";
const REAL_WINDOWS_TRUSTED_STARTUP_AGAIN: &str = r"I1001 22:43:40.797304       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:40.802125       1 common.go:448] Starting CLI program
CLI ready for user input
I1001 22:43:40.808780     312 manager.go:1333] Reloading system slash commands and skills
I1001 22:43:40.808780     312 manager.go:1310] Reloading system slash commands
I1001 22:43:40.808780     312 manager.go:1314] Slash commands unchanged, skipping update
I1001 22:43:40.808780       1 analytics.go:189] CLI startup completed (took 281.1586ms)
I1001 22:43:40.808780     311 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:40.856868     247 manager.go:935] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I1001 22:43:42.633579      55 manager.go:1310] Reloading system slash commands
I1001 22:43:44.991892     448 manager.go:1310] Reloading system slash commands
I1001 22:43:44.994160     448 manager.go:1314] Slash commands unchanged, skipping update
";

#[test]
fn a_windows_paste_right_after_the_quiet_period_needs_no_window_in_a_trusted_workspace() {
    for (session, log, ready_at) in [
        (
            "session-C2fMs7",
            REAL_WINDOWS_TRUSTED_STARTUP,
            "22:43:26.187836",
        ),
        (
            "session-uTpvwY",
            REAL_WINDOWS_TRUSTED_STARTUP_AGAIN,
            "22:43:48.491892",
        ),
    ] {
        assert!(
            workspace_customizations_loaded(log.as_bytes()),
            "{session}: the customization load is logged at startup"
        );
        let observation = observe_startup(log.as_bytes());
        assert!(observation.markers_observed(true), "{session}");
        assert_eq!(
            observation.deferred_reload_after_startup, None,
            "{session}: no trust reload follows the startup of a trusted workspace"
        );
        // Ready 3.5 s after the last plain reload, where the recorded paste was
        // delivered; the former rule would have held it until the window ended.
        let replay = LogReplay::new(log, Instant::now());
        let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, ready, ready_at, session);
    }
}
