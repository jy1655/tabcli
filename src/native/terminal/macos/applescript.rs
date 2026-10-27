use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::super::TerminalSendFailure;
use super::{CloseOutcome, TerminalKind};

pub(in crate::native::terminal) trait Automation {
    fn run(&self, application: &str, script: &str, arguments: &[&str]) -> Result<String>;
    fn run_until(
        &self,
        application: &str,
        script: &str,
        arguments: &[&str],
        deadline: Instant,
    ) -> Result<String>;
    fn run_send_until(
        &self,
        application: &str,
        script: &str,
        arguments: &[&str],
        deadline: Instant,
    ) -> std::result::Result<String, TerminalSendFailure>;
}

pub(super) struct Osascript;

impl Automation for Osascript {
    fn run(&self, application: &str, script: &str, arguments: &[&str]) -> Result<String> {
        let output = command(script, arguments)
            .output()
            .context("failed to execute /usr/bin/osascript")?;
        parse_output(application, output)
    }

    fn run_until(
        &self,
        application: &str,
        script: &str,
        arguments: &[&str],
        deadline: Instant,
    ) -> Result<String> {
        if Instant::now() >= deadline {
            bail!("{application} automation timed out before it started");
        }
        let mut command = command(script, arguments);
        let output = crate::native::process_env::command_output_until_classified(
        &mut command,
        deadline,
        &format!("{application} automation"),
    )
    .map_err(|failure| {
        if !failure.timed_out() {
            return failure.into_error();
        }
        // The call ran until Bridge's deadline. A new Ghostty surface was never observed
        // to become ready while the screen was locked (2026-10-11), and a first launch can
        // be waiting on an Automation prompt, so the message names the lock and the page
        // that says what to check.
        // Half a second: the launcher keeps two seconds for cleanup after a timeout,
        // and the IOKit query answers in well under that.
        let lock = match timeout_screen_lock(super::screen_locked) {
            Some(true) => "; the screen is locked",
            _ => "",
        };
        failure.into_error().context(format!(
            "{application} did not answer before the deadline{lock}; see docs/macos-permissions.md, \"Automation timeouts\", for what to check and record before retrying"
        ))
    })?;
        parse_output(application, output)
    }

    fn run_send_until(
        &self,
        application: &str,
        script: &str,
        arguments: &[&str],
        deadline: Instant,
    ) -> std::result::Result<String, TerminalSendFailure> {
        if Instant::now() >= deadline {
            return Err(TerminalSendFailure::not_sent(anyhow::anyhow!(
                "{application} automation timed out before it started"
            )));
        }
        let mut command = command(script, arguments);
        let output = crate::native::process_env::command_output_until_classified(
            &mut command,
            deadline,
            &format!("{application} automation"),
        )
        .map_err(|failure| {
            if failure.process_started() {
                TerminalSendFailure::delivery_uncertain(failure.into_error())
            } else {
                TerminalSendFailure::not_sent(failure.into_error())
            }
        })?;
        parse_output(application, output).map_err(TerminalSendFailure::delivery_uncertain)
    }
}

pub(super) fn run_until(
    application: &str,
    script: &str,
    arguments: &[&str],
    deadline: Instant,
) -> Result<String> {
    Osascript.run_until(application, script, arguments, deadline)
}
pub(super) fn run_send_until(
    application: &str,
    script: &str,
    arguments: &[&str],
    deadline: Instant,
) -> std::result::Result<String, TerminalSendFailure> {
    Osascript.run_send_until(application, script, arguments, deadline)
}

fn timeout_screen_lock(probe: impl FnOnce(Instant) -> Option<bool>) -> Option<bool> {
    let diagnostic_deadline = Instant::now() + Duration::from_millis(500);
    let deadline = match crate::native::session::close::budget::deadline() {
        Some(close) => {
            crate::native::session::close::budget::close_remaining(close).ok()?;
            diagnostic_deadline.min(close)
        }
        None => diagnostic_deadline,
    };
    probe(deadline)
}

fn command(script: &str, arguments: &[&str]) -> Command {
    let mut command = crate::native::process_env::helper_command("/usr/bin/osascript");
    command.arg("-e").arg(script).args(arguments);
    command
}

fn parse_output(application: &str, output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "{application} automation failed: {}",
            if error.is_empty() {
                output.status.to_string()
            } else {
                error
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(super) fn close_response(terminal: TerminalKind, response: &str) -> Result<CloseOutcome> {
    match response {
        "closed" => Ok(CloseOutcome::Closed),
        "missing" => Ok(CloseOutcome::Missing),
        _ => bail!(
            "unexpected {} close response: {response:?}",
            terminal.display_name()
        ),
    }
}

pub(super) fn screen_text(text: &str) -> Result<String> {
    Ok(text
        .strip_prefix("AB_SCREEN_BEGIN")
        .and_then(|s| s.strip_suffix("AB_SCREEN_END"))
        .context("invalid managed screen response")?
        .to_owned())
}
pub(super) fn guarded_answer(answer: &str) -> Result<bool> {
    match answer {
        "sent" => Ok(true),
        "changed" => Ok(false),
        _ => bail!("unexpected guarded dialog input response"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recording_replay_identifies_each_adapter_script_uniquely() {
        use super::super::{apple_terminal as terminal, iterm2};
        use super::testing::ScriptId::{self, *};
        for (application, script, expected) in [
            ("iTerm2", iterm2::OPEN_TAB_SCRIPT, ItermOpen),
            ("iTerm2", iterm2::SEND_FILE_SCRIPT, ItermSend),
            ("iTerm2", iterm2::VERIFY_SESSION_SCRIPT, ItermVerify),
            ("iTerm2", iterm2::PRESENCE_SCRIPT, ItermPresence),
            ("iTerm2", iterm2::CLOSE_SESSION_SCRIPT, ItermClose),
            ("iTerm2", iterm2::SCREEN_SCRIPT, ItermScreen),
            ("Terminal.app", terminal::OPEN_TAB_SCRIPT, TerminalOpen),
            ("Terminal.app", terminal::SEND_FILE_SCRIPT, TerminalSend),
            ("Terminal.app", terminal::VERIFY_TAB_SCRIPT, TerminalVerify),
            ("Terminal.app", terminal::CLOSE_TAB_SCRIPT, TerminalClose),
            (
                "Terminal.app",
                terminal::WAIT_FOR_CLOSE_SCRIPT,
                TerminalWait,
            ),
            ("Terminal.app", terminal::SCREEN_SCRIPT, TerminalScreen),
        ] {
            assert_eq!(ScriptId::identify(application, script), expected);
        }
    }
    #[test]
    fn recording_replay_rejects_unknown_ambiguous_exhausted_mismatched_and_leftover_replies() {
        use super::super::iterm2;
        use super::testing::{Call, Method, Replay, Reply, ScriptId};
        let deadline = Instant::now();
        for case in [
            "unknown",
            "ambiguous",
            "exhausted",
            "normal-for-send",
            "send-for-run",
            "send-for-until",
            "leftover",
        ] {
            assert!(
                std::panic::catch_unwind(|| {
                    match case {
                        "unknown" => {
                            ScriptId::identify("iTerm2", "return 1");
                        }
                        "ambiguous" => {
                            ScriptId::identify(
                                "iTerm2",
                                &format!(
                                    "{}{}",
                                    iterm2::CLOSE_SESSION_SCRIPT,
                                    iterm2::SCREEN_SCRIPT
                                ),
                            );
                        }
                        "exhausted" => {
                            let _ = Replay::normal(&[]).run(
                                "iTerm2",
                                iterm2::CLOSE_SESSION_SCRIPT,
                                &[],
                            );
                        }
                        "normal-for-send" => {
                            let _ = Replay::normal(&["sent"]).run_send_until(
                                "iTerm2",
                                iterm2::SEND_FILE_SCRIPT,
                                &[],
                                deadline,
                            );
                        }
                        "send-for-run" => {
                            let _ = Replay::new([Reply::Send(Ok("sent".into()))]).run(
                                "iTerm2",
                                iterm2::CLOSE_SESSION_SCRIPT,
                                &[],
                            );
                        }
                        "send-for-until" => {
                            let _ = Replay::new([Reply::Send(Ok("sent".into()))]).run_until(
                                "iTerm2",
                                iterm2::CLOSE_SESSION_SCRIPT,
                                &[],
                                deadline,
                            );
                        }
                        _ => Replay::normal(&["leftover"]).assert_finished(),
                    }
                })
                .is_err(),
                "{case}"
            );
        }
        let replay = Replay::new([
            Reply::Normal(Ok("first".into())),
            Reply::Send(Ok("second".into())),
            Reply::Normal(Err(anyhow::anyhow!("third"))),
        ]);
        assert_eq!(
            replay
                .run("iTerm2", iterm2::VERIFY_SESSION_SCRIPT, &["id"])
                .unwrap(),
            "first"
        );
        assert_eq!(
            replay
                .run_send_until(
                    "iTerm2",
                    iterm2::SEND_FILE_SCRIPT,
                    &["id", "file"],
                    deadline
                )
                .unwrap(),
            "second"
        );
        assert_eq!(
            replay
                .run_until("iTerm2", iterm2::CLOSE_SESSION_SCRIPT, &["id"], deadline)
                .unwrap_err()
                .to_string(),
            "third"
        );
        assert_eq!(
            replay.calls(),
            [
                Call::new("iTerm2", Method::Run, ScriptId::ItermVerify, &["id"], None),
                Call::new(
                    "iTerm2",
                    Method::RunSendUntil,
                    ScriptId::ItermSend,
                    &["id", "file"],
                    Some(deadline)
                ),
                Call::new(
                    "iTerm2",
                    Method::RunUntil,
                    ScriptId::ItermClose,
                    &["id"],
                    Some(deadline)
                ),
            ]
        );
        replay.assert_finished();
    }

    #[test]
    fn automation_drops_caller_session_environment() {
        crate::native::process_env::tests::assert_helper_environment(
            &super::command("return 1", &[]),
            &[],
        );
    }

    use super::*;

    #[test]
    fn a_call_that_outlives_the_deadline_names_the_deadline_and_the_permissions_page() {
        let error = run_until(
            "Test",
            "delay 5",
            &[],
            Instant::now() + Duration::from_millis(300),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("Test did not answer before the deadline"),
            "{text}"
        );
        assert!(text.contains("docs/macos-permissions.md"), "{text}");
        assert!(text.contains("Test automation timed out"), "{text}");
    }

    #[test]
    fn close_deadline_timeout_skips_diagnostic_probe_only_inside_close() {
        let now = Instant::now();
        let _scope = crate::native::session::close::budget::Scope::with_clock(
            crate::native::clock::FakeClock::new(now),
        );
        {
            let _budget = crate::native::session::close::budget::Scope::enter(now);
            assert_eq!(
                timeout_screen_lock(|_| panic!("expired close must not start a probe")),
                None
            );
        }
        let mut probed = false;
        assert_eq!(
            timeout_screen_lock(|_| {
                probed = true;
                Some(true)
            }),
            Some(true)
        );
        assert!(probed, "ordinary automation keeps its diagnostic");
    }

    #[test]
    fn a_script_error_keeps_its_own_message() {
        let error = run_until(
            "Test",
            "error \"nope\" number 7",
            &[],
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("Test automation failed"), "{text}");
        assert!(!text.contains("did not answer"), "{text}");
    }
}

// The recording automation that tests inject in place of `Osascript`; the adapters'
// own dispatch and close protocols stay production code.
#[cfg(test)]
pub(in crate::native) mod testing {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::native::terminal) enum ScriptId {
        ItermOpen,
        ItermSend,
        ItermVerify,
        ItermPresence,
        ItermClose,
        ItermScreen,
        TerminalOpen,
        TerminalSend,
        TerminalVerify,
        TerminalClose,
        TerminalWait,
        TerminalScreen,
    }
    impl ScriptId {
        pub(in crate::native::terminal) fn identify(application: &str, script: &str) -> Self {
            use ScriptId::*;
            let matches: Vec<_> = [
                ("iTerm2", "on itermCreateWindow(bridgeCommand)", ItermOpen),
                ("iTerm2", "write targetSession contents of file", ItermSend),
                (
                    "iTerm2",
                    "set matchedTty to tty of targetSession",
                    ItermVerify,
                ),
                ("iTerm2", "return \"present\"", ItermPresence),
                ("iTerm2", "close targetSession", ItermClose),
                ("iTerm2", "on screenOf(targetSession)", ItermScreen),
                ("Terminal.app", "on soleNewWindowWithTty(", TerminalOpen),
                (
                    "Terminal.app",
                    "do script promptText in targetTab",
                    TerminalSend,
                ),
                (
                    "Terminal.app",
                    "if matchCount is 1 then return wantedTty",
                    TerminalVerify,
                ),
                ("Terminal.app", "on isOwnedTty(", TerminalClose),
                (
                    "Terminal.app",
                    "set attemptCount to item 2 of argv as integer",
                    TerminalWait,
                ),
                ("Terminal.app", "on screenOf(w, wantedTty)", TerminalScreen),
            ]
            .into_iter()
            .filter(|(app, needle, _)| *app == application && script.contains(needle))
            .collect();
            assert_eq!(
                matches.len(),
                1,
                "unknown or ambiguous {application} script"
            );
            matches[0].2
        }
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::native::terminal) enum Method {
        Run,
        RunUntil,
        RunSendUntil,
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::native::terminal) struct Call {
        pub application: String,
        pub method: Method,
        pub script: ScriptId,
        pub arguments: Vec<String>,
        pub deadline: Option<Instant>,
    }
    impl Call {
        pub(in crate::native::terminal) fn new(
            application: &str,
            method: Method,
            script: ScriptId,
            arguments: &[&str],
            deadline: Option<Instant>,
        ) -> Self {
            Self {
                application: application.into(),
                method,
                script,
                arguments: arguments.iter().map(|s| (*s).into()).collect(),
                deadline,
            }
        }
    }
    pub(in crate::native::terminal) enum Reply {
        Normal(Result<String>),
        Send(std::result::Result<String, TerminalSendFailure>),
    }
    struct Recording {
        calls: Vec<Call>,
        replies: VecDeque<Reply>,
    }
    impl Drop for Recording {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(self.replies.is_empty(), "unconsumed automation replies");
            }
        }
    }
    #[derive(Clone)]
    pub(in crate::native::terminal) struct Replay(Rc<RefCell<Recording>>);
    impl Replay {
        pub(in crate::native::terminal) fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
            Self(Rc::new(RefCell::new(Recording {
                calls: Vec::new(),
                replies: replies.into_iter().collect(),
            })))
        }
        pub(in crate::native::terminal) fn normal(replies: &[&str]) -> Self {
            Self::new(replies.iter().map(|s| Reply::Normal(Ok((*s).into()))))
        }
        pub(in crate::native::terminal) fn calls(&self) -> Vec<Call> {
            self.0.borrow().calls.clone()
        }
        pub(in crate::native::terminal) fn assert_finished(&self) {
            assert!(
                self.0.borrow().replies.is_empty(),
                "unconsumed automation replies"
            );
        }
        fn reply(
            &self,
            application: &str,
            method: Method,
            script: &str,
            arguments: &[&str],
            deadline: Option<Instant>,
        ) -> Reply {
            let call = Call::new(
                application,
                method,
                ScriptId::identify(application, script),
                arguments,
                deadline,
            );
            let mut recording = self.0.borrow_mut();
            recording.calls.push(call);
            recording
                .replies
                .pop_front()
                .expect("automation reply queue exhausted")
        }
    }
    impl Automation for Replay {
        fn run(&self, application: &str, script: &str, arguments: &[&str]) -> Result<String> {
            match self.reply(application, Method::Run, script, arguments, None) {
                Reply::Normal(result) => result,
                Reply::Send(_) => panic!("send reply for normal automation method"),
            }
        }
        fn run_until(
            &self,
            application: &str,
            script: &str,
            arguments: &[&str],
            deadline: Instant,
        ) -> Result<String> {
            match self.reply(
                application,
                Method::RunUntil,
                script,
                arguments,
                Some(deadline),
            ) {
                Reply::Normal(result) => result,
                Reply::Send(_) => panic!("send reply for normal automation method"),
            }
        }
        fn run_send_until(
            &self,
            application: &str,
            script: &str,
            arguments: &[&str],
            deadline: Instant,
        ) -> std::result::Result<String, TerminalSendFailure> {
            match self.reply(
                application,
                Method::RunSendUntil,
                script,
                arguments,
                Some(deadline),
            ) {
                Reply::Send(result) => result,
                Reply::Normal(_) => panic!("normal reply for send automation method"),
            }
        }
    }
}

#[cfg(test)]
pub(super) mod replay {
    fn osascript(script: &str) -> String {
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    // Replay the shipped decision handler; only its screen and key handlers are
    // models. No application dictionary or live terminal is used by this script.
    fn replay(
        script: &str,
        screen: &str,
        key: &str,
        read_parameters: &str,
        read_arguments: &str,
        target_arguments: &str,
    ) -> String {
        let (_, flow) = script.split_once("-- The flow.").unwrap();
        assert!(!flow.contains("application"));
        osascript(&format!(
            r#"
property calls : ""
on screenOf({read_parameters})
    set calls to calls & "read=" & ({read_arguments}) & ";"
    return "{screen}"
end screenOf
on sendKey(target, keyName)
    set calls to calls & "send=" & target & ":" & keyName & ";"
end sendKey
{flow}
on run
    set answer to my guardedAnswer({target_arguments}, "trust this directory?", "{key}")
    return answer & ";" & calls
end run
"#
        ))
    }

    pub(in crate::native::terminal) fn assert_guarded_replay(
        script: &str,
        read_parameters: &str,
        read_arguments: &str,
        target_arguments: &str,
        read_target: &str,
        send_target: &str,
    ) {
        for key in ["enter", "down-enter"] {
            assert_eq!(
                replay(
                    script,
                    "trust this directory?",
                    key,
                    read_parameters,
                    read_arguments,
                    target_arguments
                ),
                format!("sent;read={read_target};send={send_target}:{key};")
            );
            // A one-character repaint between capture and comparison must send
            // nothing, for either key sequence.
            assert_eq!(
                replay(
                    script,
                    "trust this directory!",
                    key,
                    read_parameters,
                    read_arguments,
                    target_arguments
                ),
                format!("changed;read={read_target};")
            );
            assert_eq!(
                replay(
                    script,
                    "Trust this directory?",
                    key,
                    read_parameters,
                    read_arguments,
                    target_arguments
                ),
                format!("changed;read={read_target};"),
                "a case-only repaint must not receive either key sequence"
            );
        }
    }
}
