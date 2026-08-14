use std::process::Command;

use anyhow::{Context, Result, bail};

pub(super) fn run(application: &str, script: &str, arguments: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(arguments)
        .output()
        .context("failed to execute /usr/bin/osascript")?;
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
