//! Read-only Session observations within a workspace and a shared scan budget.
use super::search::{IncompleteReason, SEARCH_TIME_BUDGET, SearchScope};
use super::*;
use crate::native::args::{option_value, set_flag_once, set_once, workspace_scope};
use crate::native::clock::SystemClock;
use crate::native::session;
use crate::native::session::OwnerObservation;
use crate::native::session::enumerate::{CheckOrder, Fault, Scope, enumerate};
use crate::native::session::origin::{ObservationCache, OriginEnvironment, OriginObservation};
use agent_bridge::PUBLIC_COMMAND;
use observation::judgment::{
    self, BoundOwner, Cancel, CancelState, Ended, FOREIGN_OWNER, Hold, OwnerJudgment,
    ResultCandidate, Turn,
};
use observation::{Observation, StatusObservation};

#[derive(Debug)]
pub(crate) struct StatusRequest {
    scope: SearchScope,
    provider: Option<FirstPartyCli>,
    include_closed: bool,
    json: bool,
}

fn error_value(error: &anyhow::Error) -> Value {
    envelope::error(error, json!({"sessions": []}).as_object().unwrap().clone())
}

pub(in crate::native) fn parse(args: &[String]) -> Result<NativeCommand> {
    match parse_options(args) {
        Ok(request) => Ok(NativeCommand::Status(request)),
        Err(error) => {
            envelope::parse_failure(
                args,
                &error,
                json!({"sessions": []}).as_object().unwrap().clone(),
            )?;
            Err(error)
        }
    }
}

fn parse_options(args: &[String]) -> Result<StatusRequest> {
    let mut workspace = None;
    let mut all_workspaces = false;
    let mut provider = None;
    let mut include_closed = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(args, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--all-workspaces" => set_flag_once(&mut all_workspaces, "--all-workspaces")?,
            "--provider" => set_once(
                &mut provider,
                FirstPartyCli::from_str(option_value(args, &mut index, "--provider")?)
                    .map_err(anyhow::Error::msg)?,
                "--provider",
            )?,
            "--all" => set_flag_once(&mut include_closed, "--all")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            other => bail!("unknown status option: {other}"),
        }
        index += 1;
    }
    let scope = SearchScope::from(workspace_scope("status", workspace, all_workspaces)?);
    Ok(StatusRequest {
        scope,
        provider,
        include_closed,
        json,
    })
}

/// Status's owner attention: a live owner whose identity was not compared is not flagged,
/// and an Unknown attestation is unverified, never exited (B3).
fn owner_flag(owner: &OwnerJudgment) -> Option<&'static str> {
    match owner.bound() {
        Some(BoundOwner::Exited) => Some("owner_exited"),
        Some(BoundOwner::Reused | BoundOwner::ControlChanged) => Some("owner_identity_mismatch"),
        Some(BoundOwner::Verified | BoundOwner::Uncompared) => None,
        Some(BoundOwner::Unknown) | None => Some("owner_unverified"),
    }
}

/// Status's `owner` object: the attestation's projection for a bound record, otherwise
/// null facts with the reason no attestation was made.
fn owner_value(owner: &OwnerJudgment) -> OwnerObservation {
    let reason = match owner {
        OwnerJudgment::Bound(_) => return owner.observation(),
        OwnerJudgment::UnboundLegacy { .. } => "owner record is unbound",
        OwnerJudgment::Foreign => FOREIGN_OWNER,
        OwnerJudgment::Absent => "no owner record",
        OwnerJudgment::Unreadable(reason) => reason,
    };
    OwnerObservation {
        error: Some(reason.to_owned()),
        ..OwnerObservation::default()
    }
}

fn attention(
    observed: &StatusObservation,
    owner: &OwnerJudgment,
    origin: &OriginObservation,
) -> Vec<&'static str> {
    let observation = &observed.observation;
    let judgments = &observation.judgments;
    let closed = observation.records.status.state == SessionState::Closed;
    let mut flags = Vec::new();
    if judgments.cancel == Cancel::Recorded(Some(CancelState::RequestedActive)) {
        flags.push("cancel_requested");
    }
    if matches!(judgments.hold, Hold::Held { .. }) {
        flags.push("held");
    }
    match &judgments.turn {
        Turn::DeliveryUnconfirmed => flags.push("delivery_unconfirmed"),
        Turn::LaunchFailed { reason, .. } => flags.push(reason),
        Turn::TurnInProgress | Turn::NoActiveTurn => (),
    }
    if judgments.claim_without_receipt {
        flags.push("claim_without_receipt");
    }
    if judgments.recovery_required {
        flags.push("recovery_required");
    }
    if !closed {
        flags.extend(owner_flag(owner));
        if observed
            .questions
            .as_ref()
            .is_some_and(|q| !q.items.is_empty())
        {
            flags.push("question_emitted");
        }
    }
    // Status unions its own two reads with the Observation's unreadable parts (D12).
    if !judgments.unreadable.is_empty()
        || observed.latest.is_err()
        || observed.active.as_ref().is_some_and(Result::is_err)
        || observed
            .questions
            .as_ref()
            .is_some_and(|q| q.observation == QuestionObservation::Unreadable)
    {
        flags.push("records_partially_unreadable");
    }
    if judgments.request_index.is_some() {
        flags.push("request_index_incomplete");
    }
    if judgments.residual_surface.is_some() {
        flags.push("residual_surface_unverified");
    }
    match judgments.ended {
        Ended::Failed => flags.push("session_failed"),
        Ended::Exited => flags.push("session_exited"),
        Ended::Neither => (),
    }
    // Derived from the Origin's records; neither an open surface nor a close authority.
    if !closed && origin.ended == Some(true) {
        flags.push("origin_ended");
    }
    flags
}

fn result_command(candidate: ResultCandidate, id: &str) -> Option<String> {
    match candidate {
        ResultCandidate::ActiveRequest(request) => Some(format!(
            "{PUBLIC_COMMAND} result {id} --request {request} --json"
        )),
        ResultCandidate::ClaimOnly => Some(format!("{PUBLIC_COMMAND} result {id} --list --json")),
        ResultCandidate::LatestDecoded(event) => Some(format!(
            "{PUBLIC_COMMAND} result {id} --event {event} --json"
        )),
        ResultCandidate::None => None,
    }
}

fn entry(observed: StatusObservation, id: &str, origin: OriginObservation) -> Value {
    let judgment = observed.observation.evidence.judge_owner(id);
    let owner = owner_value(judgment);
    let flags = attention(&observed, judgment, &origin);
    let observation = &observed.observation;
    let records = &observation.records;
    let judgments = &observation.judgments;
    // The latest Result status has already decoded; nothing more is decoded (D16).
    let latest_event = observed
        .latest
        .as_ref()
        .ok()
        .and_then(|latest| latest["event_id"].as_str());
    let command = result_command(judgment::result_candidate(records, latest_event), id);
    let manifest = &records.manifest;
    let active = observation.active_request().map(|receipt| {
        let value = observed.active.as_ref().and_then(|active| active.as_ref().ok());
        json!({"request_id": receipt.request_id,
            "request_state": value.map(|v| &v["request_state"]),
            "bridge_observed_elapsed_ms": value.map(|v| &v["bridge_observed_elapsed_ms"]),
            "bridge_observed_elapsed_reason": value.map(|v| v["bridge_observed_elapsed_reason"].clone())
                .unwrap_or_else(|| json!("unreadable_result"))})
    });
    let mut value = json!({
        "id": id, "provider": manifest.provider, "workspace": manifest.workspace,
        "title": manifest.title, "model": manifest.model, "effort": manifest.effort, "yolo": manifest.yolo,
        "created_unix_ms": manifest.created_unix_ms, "state": records.status.state,
        "generation": records.status.generation, "updated_unix_ms": records.status.updated_unix_ms,
        "error": records.status.error, "turn_claimed": records.claim.is_some(),
        "recovery_required": judgments.recovery_required, "unreadable_requests": records.unreadable_requests,
        "request_index_error": records.request_index_error,
        "active_request": active, "latest_result": observed.latest.as_ref().ok(),
        "owner": owner, "attention": flags, "derived_from": "observation", "result_command": command,
    });
    session::hold::add_fields(&mut value, &observation.evidence.held);
    if let Some(questions) = &observed.questions {
        question_observation(&mut value, questions);
        // The count describes the focus Request, closed sessions included; attention does not.
        let focus = if records.claim.is_some() {
            "active_request"
        } else {
            "latest_result"
        };
        if value[focus].is_object() {
            question_count(&mut value[focus], questions);
        }
    }
    if let Some(residual) = judgments.residual_surface {
        value["residual_surface"] = json!(residual);
    }
    origin.add_fields(&mut value);
    value
}

fn value_in(root: &Path, request: &StatusRequest, budget: Duration) -> Result<Value> {
    let deadline = Instant::now() + budget;
    let mut ids = Vec::new();
    let mut reasons = Vec::new();
    match enumerate(root, CheckOrder::IdFirst) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => ids.push(entry.id),
                    Err(Fault::Entry(error)) => reasons.push(IncompleteReason {
                        session: None,
                        reason: format!("failed to read session entry: {error}"),
                    }),
                    Err(Fault::FileType { id, error }) => reasons.push(IncompleteReason {
                        session: Some(id),
                        reason: format!("failed to read session entry file type: {error}"),
                    }),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read the native state root {}", root.display())
            });
        }
    }
    ids.sort();
    let mut sessions = Vec::new();
    let mut scanned = 0;
    let mut origins = ObservationCache::default();
    let mut clock = SystemClock;
    let mut origin_environment = OriginEnvironment::production(&mut clock);
    for id in ids {
        let read = (|| -> Result<Option<Value>> {
            if Instant::now() >= deadline {
                bail!("status time budget exhausted");
            }
            let reader = Reader::open_unchecked(root.join(&id));
            let manifest = reader.manifest()?;
            if !(Scope {
                workspace: match &request.scope {
                    SearchScope::Workspace(path) => Some(path),
                    SearchScope::AllWorkspaces => None,
                },
                provider: request.provider,
            })
            .admits(&manifest)
            {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                bail!("status time budget exhausted");
            }
            scanned += 1;
            let Some(observed) =
                Observation::read_for_status(&reader, deadline, request.include_closed)?
            else {
                return Ok(None);
            };
            if Instant::now() >= deadline {
                bail!("status time budget exhausted");
            }
            let origin = origins.observe(
                root,
                observed.observation.origin.clone(),
                Some(deadline),
                &mut origin_environment,
            );
            Ok(Some(entry(observed, &id, origin)))
        })();
        match read {
            Ok(Some(value)) => sessions.push(value),
            Ok(None) => (),
            Err(error) => reasons.push(IncompleteReason {
                session: Some(id),
                reason: if error.is::<SnapshotBusy>() {
                    format!("session observation busy: {error:#}")
                } else {
                    format!("{error:#}")
                },
            }),
        }
    }
    sessions.sort_by(|left, right| {
        let attention = |v: &Value| !v["attention"].as_array().unwrap().is_empty();
        attention(right)
            .cmp(&attention(left))
            .then_with(|| {
                right["updated_unix_ms"]
                    .as_u64()
                    .cmp(&left["updated_unix_ms"].as_u64())
            })
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    let (workspace, all_workspaces) = match &request.scope {
        SearchScope::Workspace(path) => (Some(path), false),
        SearchScope::AllWorkspaces => (None, true),
    };
    Ok(envelope::ok(json!({"filters": {"workspace": workspace, "all_workspaces": all_workspaces,
            "provider": request.provider.map(FirstPartyCli::as_str), "include_closed": request.include_closed},
        "scanned": {"sessions": scanned, "listed": sessions.len()}, "sessions": sessions,
        "incomplete": !reasons.is_empty(), "incomplete_reasons": reasons}).as_object().unwrap().clone()))
}

pub(in crate::native) fn run(request: StatusRequest) -> Result<()> {
    let result =
        Reader::state_root().and_then(|root| value_in(&root, &request, SEARCH_TIME_BUDGET));
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            if request.json {
                envelope::print(&error_value(&error))?;
            }
            return Err(error);
        }
    };
    envelope::emit(request.json, &value, || {
        for line in human_lines(&value) {
            println!("{}", terminal_safe_text(&line, false));
        }
        Ok(())
    })
}

fn human_lines(value: &Value) -> Vec<String> {
    let sessions = value["sessions"].as_array().unwrap();
    let mut lines = Vec::new();
    if sessions.is_empty() {
        lines.push("no sessions to show".to_owned());
    }
    for session in sessions {
        let flags = session["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "{} {} {} {} {} {} {}",
            if flags.is_empty() { " " } else { "!" },
            session["id"].as_str().unwrap_or("-"),
            session["provider"].as_str().unwrap_or("-"),
            session["state"].as_str().unwrap_or("-"),
            session["active_request"]["request_state"]
                .as_str()
                .unwrap_or("-"),
            session["latest_result"]["request_state"]
                .as_str()
                .unwrap_or("-"),
            flags
        ));
    }
    lines.push(format!(
        "{} listed / {} scanned",
        value["scanned"]["listed"], value["scanned"]["sessions"]
    ));
    for reason in value["incomplete_reasons"].as_array().unwrap() {
        lines.push(format!(
            "incomplete: {}: {}",
            reason["session"].as_str().unwrap_or("state root"),
            reason["reason"].as_str().unwrap_or("-")
        ));
    }
    lines
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::native) fn test_value_in(root: &Path) -> Value {
    value_in(
        root,
        &StatusRequest {
            scope: SearchScope::AllWorkspaces,
            provider: None,
            include_closed: true,
            json: true,
        },
        SEARCH_TIME_BUDGET,
    )
    .unwrap()
}
