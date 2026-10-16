use super::delivery::read_log_bytes;
use super::monitor::{
    ResultReader, brain_root, parse_conversation_id, pending_turn_log, validated_file_metadata,
};
use super::readiness::observe_startup;
use super::receipt::{input_receipts, receipt_matches};
use super::trust::workspace_trust_missing;
use super::{
    AGY_LOG_FILE, FULL_TRANSCRIPT_FILE, TRANSCRIPT_FILE, TRUST_RECOVERY, TRUST_STORE_MISSING,
    read_pending_turn,
};
use crate::native::session::{Reader, SessionState};
use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub(super) fn workspace_trust_check(
    workspace: &Path,
    env: &super::super::super::consent::Environment,
    directory: &Path,
) -> super::super::super::doctor::Check {
    use super::super::super::doctor::{Availability::*, Check};
    // A caller without a user home has no store to observe; the row says so and the
    // doctor command goes on.
    let home = match super::Home::resolve(env) {
        Ok(home) => home,
        Err(error) => {
            return Check::new(
                "agy_workspace_trust",
                Unknown,
                "agy_trust_store_unlocated",
                format!("{error:#}"),
                "Inspect the managed terminal; doctor never answers the dialog.",
            );
        }
    };
    const OBSERVATION: &str =
        "Observation only: doctor never answers the dialog or edits Agy's settings.";
    const WITHHELD: &str = "The Windows initial paste and every macOS follow-up are withheld meanwhile, because a paste onto the dialog is lost and its Enter would approve the folder; an argument-delivered initial prompt still runs behind the dialog.";
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let (availability, reason, detail) = match workspace_trust_missing(workspace, env, &log_path) {
        Ok(None) => (
            Available,
            "agy_workspace_trusted",
            "Agy's own trust store lists this exact workspace and this session's agy.log shows that it loaded the workspace customizations, so no workspace-trust dialog covers the composer.".to_owned(),
        ),
        Ok(Some(TRUST_STORE_MISSING)) => (
            Unavailable,
            "agy_workspace_untrusted",
            format!("Agy's own trust store does not list this exact workspace, and Agy keeps its workspace-trust dialog over the composer until such a workspace is approved. {WITHHELD}"),
        ),
        Ok(Some(_)) => (
            Unknown,
            "agy_session_trust_unverified",
            format!("Agy's own trust store lists this exact workspace, but this session's agy.log does not show that this session loaded the workspace customizations. Either this session still shows its own workspace-trust dialog, as it does after the workspace was approved in another session, or the log is missing or no longer holds that line. {WITHHELD}"),
        ),
        Err(error) => (
            Unknown,
            "agy_trust_evidence_unreadable",
            format!("Agy's trust store or this session's agy.log could not be read, so the dialog state is unobserved: {error:#}"),
        ),
    };
    let next_action = if availability == Available {
        OBSERVATION.to_owned()
    } else {
        format!(
            "{OBSERVATION} {TRUST_RECOVERY}. A workspace another provider already trusts is answered from shared consent on a later ask."
        )
    };
    Check::new(
        "agy_workspace_trust",
        availability,
        reason,
        detail,
        next_action,
    )
    .evidence(
        serde_json::json!({ "workspace": workspace, "store": home.settings, "log": log_path }),
    )
}

pub(super) fn input_receipt_check(directory: Option<&Path>) -> super::super::super::doctor::Check {
    input_receipt_check_for_platform(directory, cfg!(windows))
}

fn input_receipt_check_for_platform(
    directory: Option<&Path>,
    windows: bool,
) -> super::super::super::doctor::Check {
    use super::super::super::doctor::{Availability::Unknown, Check};
    const CHECK_ID: &str = "agy_input_receipt";
    const NEXT_ACTION: &str = "Observation only. A gated paste (the Windows initial prompt, every macOS follow-up) first waits until Agy's own trust store lists the exact workspace and this session's agy.log shows the workspace customization load (agy_workspace_trust), then for CLI startup completed and a 3.5 s quiet period without reload, redraw, or hooks lines. The Windows console also requires a Full redraw completed after startup. Neither platform waits for the trust reload (Reloading system slash commands and skills stamped at least 1 s after startup and not within 1 s after a Starting new conversation line): Agy logs it only when a trust dialog is approved, so it never comes once the trust evidence holds. The log evidence is re-measured from the new content whenever agy.log disappears, shrinks, or no longer holds the bytes observed earlier (any number of times within the timeout), and the paste follows the read that passed the gate with that read's length as the receipt offset; every paste requires a HandleUserInput receipt carrying the complete pending turn marker within 60 s of the paste (Agy logs the receipt when it processes the paste, which took 18 s under load); a missing receipt leaves delivery uncertain with the session working and the reason in status.error, and the paste is never repeated. Inspect the session and close it explicitly or launch a new one.";
    let redraw_required = windows;
    let startup_markers = if windows {
        "CLI startup completed followed by a Full redraw completed"
    } else {
        "CLI startup completed; macOS logs no Full redraw completed and none is required"
    };
    let Some(directory) = directory else {
        return Check::new(
            CHECK_ID,
            Unknown,
            "agy_log_unavailable",
            "No session was given, so no agy.log startup readiness or input receipt can be observed.",
            NEXT_ACTION,
        );
    };
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let log = match read_log_bytes(&log_path) {
        Ok(Some(log)) => log,
        Ok(None) => {
            return Check::new(
                CHECK_ID,
                Unknown,
                "agy_log_missing",
                "The session has no agy.log yet; startup readiness and input receipts are unobserved.",
                NEXT_ACTION,
            )
            .evidence(serde_json::json!({ "log": log_path }));
        }
        Err(error) => {
            return Check::new(
                CHECK_ID,
                Unknown,
                "agy_log_unreadable",
                format!("{error:#}"),
                NEXT_ACTION,
            )
            .evidence(serde_json::json!({ "log": log_path }));
        }
    };
    let startup = observe_startup(&log);
    let receipts = input_receipts(&log);
    let pending = read_pending_turn(directory).ok().flatten();
    let last = receipts.last();
    // The doctor has no pre-paste offset, so this only states whether the complete
    // marker appears anywhere in the log.
    let pending_received = pending.as_ref().map(|pending| {
        receipts
            .iter()
            .any(|receipt| receipt_matches(receipt, pending))
    });
    // The doctor reads the log once, so it reports the two startup markers only; the
    // quiet period is timed live by the paste gate and cannot be judged here.
    let gate_rule = if windows {
        "the paste gate also waits for a 3.5 s quiet period without reload, redraw, or hooks lines, after the workspace trust evidence (agy_workspace_trust)"
    } else {
        "the follow-up gate also waits for a 3.5 s quiet period without reload or hooks lines, after the workspace trust evidence (agy_workspace_trust)"
    };
    let (reason, detail) = match (startup.markers_observed(redraw_required), last) {
        (false, _) => (
            "agy_startup_markers_missing",
            format!("agy.log does not yet show the startup markers ({startup_markers}); {gate_rule}."),
        ),
        (true, None) => (
            "agy_no_input_receipt",
            "agy.log shows the startup markers but no HandleUserInput receipt.".to_owned(),
        ),
        (true, Some(_)) => (
            "agy_input_receipt_observed",
            "agy.log shows the startup markers and at least one HandleUserInput receipt; the evidence states whether any receipt carries the complete pending turn marker.".to_owned(),
        ),
    };
    Check::new(CHECK_ID, Unknown, reason, detail, NEXT_ACTION).evidence(serde_json::json!({
        "log": log_path,
        "startup_completed": startup.startup_completed(),
        "redraw_required": redraw_required,
        "redraw_after_startup_observed": startup.redraw_after_startup.is_some(),
        "deferred_reload_after_startup_observed": startup.deferred_reload_after_startup.is_some(),
        "startup_reload_after_startup_ignored": startup.startup_reload_after_startup.is_some(),
        "conversation_reload_after_startup_ignored": startup.conversation_reload_after_startup.is_some(),
        "activity_lines": startup.activity_lines,
        "input_receipts": receipts.len(),
        "last_receipt_text": last.map(|receipt| {
            agent_bridge::terminal_safe_text(&receipt.text.chars().take(160).collect::<String>(), false)
        }),
        "last_receipt_truncated": last.map(|receipt| receipt.truncated),
        "pending_marker": pending.as_ref().map(|pending| pending.marker.as_str()),
        "pending_marker_received": pending_received,
    }))
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

pub(super) fn tool_confirmation_check(
    directory: &Path,
    env: &super::super::super::consent::Environment,
) -> super::super::super::doctor::Check {
    use super::super::super::doctor::{Availability::Unknown, Check};
    let observation =
        brain_root(env).and_then(|brain| pending_tool_confirmation(directory, &brain));
    let (reason, detail, evidence) = match observation {
        Ok(Some((tool, request_id))) => (
            super::AGY_TOOL_CONFIRMATION_OBSERVED,
            format!(
                "This session's {AGY_LOG_FILE} shows the pending turn waiting for user approval of {tool} in the terminal. Bridge does not answer it. This is the last observed confirmation, not proof that the dialog is still open."
            ),
            serde_json::json!({"log": directory.join(AGY_LOG_FILE), "tool": tool, "request_id": request_id}),
        ),
        Ok(None) => (
            "agy_tool_confirmation_unobserved",
            format!(
                "No tool confirmation is attributable to the pending turn in this session's {AGY_LOG_FILE}."
            ),
            serde_json::Value::Null,
        ),
        Err(error) => (
            "agy_tool_confirmation_unverified",
            format!("The pending turn's tool confirmation could not be verified: {error:#}"),
            serde_json::Value::Null,
        ),
    };
    Check::new("agy_tool_confirmation", Unknown, reason, detail,
        "Observation only. Review any approval prompt in the managed terminal yourself; Bridge does not approve, resend, or release the turn claim.")
        .evidence(evidence)
}

fn pending_tool_confirmation(directory: &Path, brain: &Path) -> Result<Option<(String, String)>> {
    let reader = Reader::open_unchecked(directory);
    let snapshot = super::super::super::query::observe_snapshot(&reader)?;
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(None);
    };
    if snapshot.status.state != SessionState::Working
        || snapshot.status.error.is_some()
        || snapshot.pending.is_some()
        || snapshot.claim.as_deref() != Some(&pending.claim_token)
    {
        return Ok(None);
    }
    let Some(receipt) = snapshot
        .receipts
        .iter()
        .find(|r| r.claim_token == pending.claim_token)
    else {
        return Ok(None);
    };
    let Some(log) = reader.private(AGY_LOG_FILE).text()? else {
        return Ok(None);
    };
    let observed = pending_turn_log(log.as_bytes(), &pending);
    let Some(tool) = observed.confirmation.filter(|_| observed.failure.is_none()) else {
        return Ok(None);
    };
    let Some(id) = parse_conversation_id(&log) else {
        return Ok(None);
    };
    let logs = brain.join(id).join(".system_generated").join("logs");
    let full = logs.join(FULL_TRANSCRIPT_FILE);
    if !observed.pasted && !only_user_input_carries(&full, brain, &pending.marker)? {
        return Ok(None);
    }
    // A transcript result may precede the monitor's publication. Observe it without
    // running the monitor or publishing anything, using its parser and correlation.
    if (ResultReader {
        full_path: &full,
        brain_root: brain,
    })
    .contains(&[logs.join(TRANSCRIPT_FILE), full.clone()], &pending)?
    {
        return Ok(None);
    }
    Ok(Some((tool, receipt.request_id.clone())))
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
