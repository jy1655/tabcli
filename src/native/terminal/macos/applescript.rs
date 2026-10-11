use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::super::TerminalSendFailure;

pub(super) fn run(application: &str, script: &str, arguments: &[&str]) -> Result<String> {
    #[cfg(test)]
    if let Some(result) = testing::run(None) {
        return result;
    }
    let output = command(script, arguments)
        .output()
        .context("failed to execute /usr/bin/osascript")?;
    parse_output(application, output)
}

pub(super) fn run_until(
    application: &str,
    script: &str,
    arguments: &[&str],
    deadline: Instant,
) -> Result<String> {
    #[cfg(test)]
    if let Some(result) = testing::run(Some(deadline)) {
        return result;
    }
    if Instant::now() >= deadline {
        bail!("{application} automation timed out before it started");
    }
    let mut command = command(script, arguments);
    let output = crate::native::command_output_until_classified(
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

fn timeout_screen_lock(probe: impl FnOnce(Instant) -> Option<bool>) -> Option<bool> {
    let diagnostic_deadline = Instant::now() + Duration::from_millis(500);
    let deadline = match crate::native::session::close::budget::deadline() {
        Some(close) => {
            crate::native::close_remaining(close).ok()?;
            diagnostic_deadline.min(close)
        }
        None => diagnostic_deadline,
    };
    probe(deadline)
}

pub(super) fn run_send_until(
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
    let output = crate::native::command_output_until_classified(
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

#[cfg(test)]
mod tests {
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
        let _clock = crate::native::TestCloseClock::new(now);
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

// Substitute only the external automation boundary; close dispatch remains production code.
#[cfg(test)]
pub(in crate::native) mod testing {
    use super::*;
    type Runner = Box<dyn FnMut(Option<Instant>) -> Result<String>>;
    thread_local! {
        static RUNNER: std::cell::RefCell<Option<Runner>> = const { std::cell::RefCell::new(None) };
    }
    pub(super) fn run(deadline: Option<Instant>) -> Option<Result<String>> {
        RUNNER.with(|runner| runner.borrow_mut().as_mut().map(|run| run(deadline)))
    }
    pub(in crate::native) fn with_runner<T>(
        runner: impl FnMut(Option<Instant>) -> Result<String> + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Reset(Option<Runner>);
        impl Drop for Reset {
            fn drop(&mut self) {
                RUNNER.with(|runner| *runner.borrow_mut() = self.0.take());
            }
        }
        let _reset = Reset(RUNNER.with(|slot| slot.replace(Some(Box::new(runner)))));
        run()
    }
}
