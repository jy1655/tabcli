//! Read-only Session observations within a workspace and a shared scan budget.
use super::*;
use crate::native::session;
use crate::native::session::OwnerObservation;
use observation::{Observation, StatusObservation, SurfaceRecord};

#[derive(Debug)]
pub(crate) struct StatusRequest {
    scope: SearchScope,
    provider: Option<FirstPartyCli>,
    include_closed: bool,
    json: bool,
}

fn error_value(error: &anyhow::Error) -> Value {
    json!({"schema_version": 1, "ok": false, "error": format!("{error:#}"), "sessions": []})
}

pub(in crate::native) fn parse(args: &[String]) -> Result<NativeCommand> {
    match parse_options(args) {
        Ok(request) => Ok(NativeCommand::Status(request)),
        Err(error) => {
            if args.iter().any(|option| option == "--json") {
                print_json(&error_value(&error))?;
            }
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
    let scope = match (workspace, all_workspaces) {
        (Some(_), true) => bail!("status accepts only one of --workspace or --all-workspaces"),
        (None, true) => SearchScope::AllWorkspaces,
        (Some(path), false) => SearchScope::Workspace(path.canonicalize().or_else(|_| {
            if path.is_absolute() {
                Ok(path)
            } else {
                std::env::current_dir().map(|cwd| cwd.join(path))
            }
        })?),
        (None, false) => SearchScope::Workspace(
            std::env::current_dir()
                .and_then(|cwd| cwd.canonicalize())
                .context(
                    "failed to resolve the current workspace; pass --workspace or --all-workspaces",
                )?,
        ),
    };
    Ok(StatusRequest {
        scope,
        provider,
        include_closed,
        json,
    })
}

fn attention(
    observed: &StatusObservation,
    owner: &OwnerObservation,
    bound: bool,
) -> Vec<&'static str> {
    let observation = &observed.observation;
    let records = &observation.records;
    let mut flags = Vec::new();
    if observation
        .cancel
        .as_ref()
        .is_some_and(|c| c.state == "requested" && c.active)
    {
        flags.push("cancel_requested");
    }
    if matches!(observation.evidence.held, Ok(true)) {
        flags.push("held");
    }
    if records.status.state == SessionState::Working
        && records.claim.is_some()
        && records.status.error.is_some()
        && records.pending.is_none()
    {
        flags.push("delivery_unconfirmed");
    }
    if let Some((reason, _)) = &observation.judgments.launch_failure {
        flags.push(reason);
    }
    if records.claim.is_some() && observation.active_request().is_none() {
        flags.push("claim_without_receipt");
    }
    if records.pending.is_some() {
        flags.push("recovery_required");
    }
    if records.status.state != SessionState::Closed {
        if owner.process_alive == Some(false) {
            flags.push("owner_exited");
        }
        if owner.identity_matches == Some(false) {
            flags.push("owner_identity_mismatch");
        }
        if !bound
            || owner.error.is_some()
            || (owner.process_alive.is_none() && owner.identity_matches.is_none())
        {
            flags.push("owner_unverified");
        }
    }
    let index_incomplete = records.unreadable_requests > 0 || records.request_index_error.is_some();
    if observation.evidence.held.is_err()
        || observation.evidence.owner.is_err()
        || matches!(
            &observation.evidence.surface,
            SurfaceRecord::Closed(Err(_))
                | SurfaceRecord::Closing(Err(_))
                | SurfaceRecord::Active(Err(_))
        )
        || matches!(&observation.evidence.surface,
            SurfaceRecord::Closed(Ok(value)) if value.get("consumed").and_then(Value::as_bool) != Some(true))
        || observation.resumed_from.is_err()
        || observed.latest.is_err()
        || observed.active.as_ref().is_some_and(Result::is_err)
        || index_incomplete
    {
        flags.push("records_partially_unreadable");
    }
    if index_incomplete {
        flags.push("request_index_incomplete");
    }
    if records.status.residual_surface().is_some() {
        flags.push("residual_surface_unverified");
    }
    match records.status.state {
        SessionState::Failed => flags.push("session_failed"),
        SessionState::Exited => flags.push("session_exited"),
        _ => (),
    }
    flags
}

fn result_command(observed: &StatusObservation, id: &str) -> Option<String> {
    if let Some(receipt) = observed.observation.active_request() {
        Some(format!(
            "{PUBLIC_COMMAND} result {id} --request {} --json",
            receipt.request_id
        ))
    } else if observed.observation.records.claim.is_some() {
        Some(format!("{PUBLIC_COMMAND} result {id} --list --json"))
    } else {
        observed
            .latest
            .as_ref()
            .ok()
            .and_then(|latest| latest["event_id"].as_str())
            .map(|event| format!("{PUBLIC_COMMAND} result {id} --event {event} --json"))
    }
}

fn entry(mut observed: StatusObservation, id: &str) -> Value {
    let bound = matches!(&observed.observation.evidence.owner,
        Ok(Some(owner)) if owner.managed_session_id.as_deref() == Some(id));
    let owner = if bound {
        let owner = observed.observation.evidence.observe_owner();
        OwnerObservation {
            process_alive: owner.process_alive,
            identity_matches: owner.identity_matches,
            error: owner.error.clone(),
        }
    } else {
        OwnerObservation {
            error: Some(match &observed.observation.evidence.owner {
                Err(error) => format!("{error:#}"),
                Ok(None) => "no owner record".to_owned(),
                Ok(Some(owner)) if owner.managed_session_id.is_none() => {
                    "owner record is unbound".to_owned()
                }
                Ok(Some(_)) => "owner record belongs to another session".to_owned(),
            }),
            ..OwnerObservation::default()
        }
    };
    let flags = attention(&observed, &owner, bound);
    let command = result_command(&observed, id);
    let observation = &observed.observation;
    let records = &observation.records;
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
        "recovery_required": records.pending.is_some(), "unreadable_requests": records.unreadable_requests,
        "request_index_error": records.request_index_error,
        "active_request": active, "latest_result": observed.latest.as_ref().ok(),
        "owner": owner, "attention": flags, "derived_from": "observation", "result_command": command,
    });
    session::hold::add_fields(&mut value, &observation.evidence.held);
    if let Some(residual) = records.status.residual_surface() {
        value["residual_surface"] = json!(residual);
    }
    value
}

fn value_in(root: &Path, request: &StatusRequest, budget: Duration) -> Result<Value> {
    let deadline = Instant::now() + budget;
    let mut ids = Vec::new();
    let mut reasons = Vec::new();
    match fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        reasons.push(IncompleteReason {
                            session: None,
                            reason: format!("failed to read session entry: {error}"),
                        });
                        continue;
                    }
                };
                let id = entry.file_name().to_string_lossy().into_owned();
                if !valid_session_id(&id) {
                    continue;
                }
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => ids.push(id),
                    Ok(_) => (),
                    Err(error) => reasons.push(IncompleteReason {
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
    for id in ids {
        let read = (|| -> Result<Option<Value>> {
            if Instant::now() >= deadline {
                bail!("status time budget exhausted");
            }
            let reader = Reader::open_unchecked(root.join(&id));
            let manifest = reader.manifest()?;
            if matches!(&request.scope, SearchScope::Workspace(path) if path != &manifest.workspace)
                || request
                    .provider
                    .is_some_and(|provider| provider.as_str() != manifest.provider)
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
            Ok(Some(entry(observed, &id)))
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
    Ok(json!({"schema_version": 1, "ok": true,
        "filters": {"workspace": workspace, "all_workspaces": all_workspaces,
            "provider": request.provider.map(FirstPartyCli::as_str), "include_closed": request.include_closed},
        "scanned": {"sessions": scanned, "listed": sessions.len()}, "sessions": sessions,
        "incomplete": !reasons.is_empty(), "incomplete_reasons": reasons}))
}

pub(in crate::native) fn run(request: StatusRequest) -> Result<()> {
    let result =
        Reader::state_root().and_then(|root| value_in(&root, &request, SEARCH_TIME_BUDGET));
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            if request.json {
                print_json(&error_value(&error))?;
            }
            return Err(error);
        }
    };
    if request.json {
        return print_json(&value);
    }
    for line in human_lines(&value) {
        println!("{}", terminal_safe_text(&line, false));
    }
    Ok(())
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
