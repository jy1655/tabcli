use super::monitor::validated_file_metadata;
use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub(super) fn transcript_logs(brain_root: &Path, conversation_id: &str) -> PathBuf {
    brain_root
        .join(conversation_id)
        .join(".system_generated")
        .join("logs")
}

// Observed once: Agy 1.3.0, 2026-10-07, issue #82. The line identifies a tool,
// not its command or a documented approval lifecycle. Do not infer approval from
// unrelated later log activity; this is a log observation, not a failure signal.
pub(super) fn parse_tool_confirmation(line: &str) -> Option<&str> {
    let (_, tail) = line.split_once("Surfacing tool confirmation: \"")?;
    let (tool, step) = tail.split_once("\" at step ")?;
    (!tool.is_empty()
        && tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !step.is_empty()
        && step.bytes().all(|c| c.is_ascii_digit()))
    .then_some(tool)
}

pub(super) fn only_user_input_carries(
    path: &Path,
    brain_root: &Path,
    marker: &str,
) -> Result<bool> {
    if validated_file_metadata(path, brain_root)?.is_none() {
        return Ok(false);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read full Agy transcript: {}", path.display()))?;
    let (mut inputs, mut carries) = (0_usize, false);
    for line in BufReader::new(file).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line?) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("USER_INPUT") {
            inputs += 1;
            carries = value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains(marker));
        }
    }
    Ok(inputs == 1 && carries)
}

#[cfg(test)]
mod tests;
