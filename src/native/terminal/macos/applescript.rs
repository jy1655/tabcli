use std::{process::Command, time::Instant};

use anyhow::{Context, Result, bail};

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
    let output = crate::native::command_output_until(
        &mut command,
        deadline,
        &format!("{application} automation"),
    )?;
    parse_output(application, output)
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
