use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::super::TerminalSendFailure;

pub(super) fn run(application: &str, script: &str, arguments: &[&str]) -> Result<String> {
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
        let lock = match super::screen_locked(Instant::now() + Duration::from_millis(500)) {
            Some(true) => "; the screen is locked",
            _ => "",
        };
        failure.into_error().context(format!(
            "{application} did not answer before the deadline{lock}; see docs/macos-permissions.md, \"Automation timeouts\", for what to check and record before retrying"
        ))
    })?;
    parse_output(application, output)
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
    let mut command = Command::new("/usr/bin/osascript");
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
