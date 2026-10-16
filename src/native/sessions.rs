//! The `sessions` listing over a state root.
use super::args::{option_value, resolve_explicit_workspace, set_flag_once, set_once};
use super::*;

#[derive(Debug)]
pub(crate) struct SessionsRequest {
    pub(crate) json: bool,
    workspace: Option<PathBuf>,
    provider: Option<FirstPartyCli>,
    state: Option<String>,
    sort_updated: bool,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let mut json = false;
    let mut workspace = None;
    let mut provider = None;
    let mut state = None;
    let mut sort = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => set_flag_once(&mut json, "--json")?,
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(args, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--provider" => set_once(
                &mut provider,
                FirstPartyCli::from_str(option_value(args, &mut index, "--provider")?)
                    .map_err(anyhow::Error::msg)?,
                "--provider",
            )?,
            "--state" => set_once(
                &mut state,
                option_value(args, &mut index, "--state")?.to_owned(),
                "--state",
            )?,
            "--sort" => {
                let value = option_value(args, &mut index, "--sort")?;
                if !matches!(value, "id" | "updated") {
                    bail!("--sort must be id or updated")
                }
                set_once(&mut sort, value.to_owned(), "--sort")?;
            }
            option => bail!("unknown sessions option: {option}"),
        }
        index += 1;
    }
    let workspace = workspace.map(resolve_explicit_workspace).transpose()?;
    Ok(NativeCommand::Sessions(SessionsRequest {
        json,
        workspace,
        provider,
        state,
        sort_updated: sort.as_deref() == Some("updated"),
    }))
}

pub(super) fn run(request: SessionsRequest) -> Result<()> {
    let sessions = sessions_in(&Reader::state_root()?, &request)?;
    if request.json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else if sessions.is_empty() {
        println!("no native Agent Bridge sessions");
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{}\t{}\tterminal={}\tyolo={}\t{} result(s)",
                session["id"].as_str().unwrap_or("?"),
                terminal_safe_text(session["state"].as_str().unwrap_or("unknown"), false),
                terminal_safe_text(session["provider"].as_str().unwrap_or("?"), false),
                terminal_safe_text(session["workspace"].as_str().unwrap_or("?"), false),
                session["terminal"].as_str().unwrap_or("unknown"),
                session["yolo"].as_bool().unwrap_or(false),
                session["results"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

/// The `sessions` listing over one state root. Listing is also a lifecycle-lock holder: it
/// converges interrupted completions and closes and repairs dead owners before it reads.
pub(super) fn sessions_in(
    root: &Path,
    request: &SessionsRequest,
) -> Result<Vec<serde_json::Value>> {
    sessions_query(root, request, true)
}

// Ownership discovery must not recover, repair, or otherwise change shared records.
pub(super) fn sessions_in_read_only(
    root: &Path,
    request: &SessionsRequest,
) -> Result<Vec<serde_json::Value>> {
    sessions_query(root, request, false)
}

fn sessions_query(
    root: &Path,
    request: &SessionsRequest,
    repair: bool,
) -> Result<Vec<serde_json::Value>> {
    let mut sessions = Vec::new();
    if root.is_dir() {
        for entry in
            session::enumerate::enumerate(root, session::enumerate::CheckOrder::FileTypeFirst)?
        {
            let entry = entry.map_err(session::enumerate::Fault::into_io)?;
            let directory = entry.directory;
            let Ok(manifest) = Reader::open_unchecked(&directory).manifest() else {
                continue;
            };
            if !(session::enumerate::Scope {
                workspace: request.workspace.as_ref(),
                provider: request.provider,
            })
            .admits(&manifest)
            {
                continue;
            }
            // Repair runs completion recovery first itself, unconditionally and under the
            // same lifecycle lock, so a listing publishes every finished turn before it
            // decides on the owner without a separate recovery pass.
            if repair {
                let _ = session::close::repair_dead_owner(&Store::open_unchecked(&directory));
            }
            let status = Reader::open_unchecked(&directory).status().ok();
            let state = status
                .as_ref()
                .map(|value| value.state.as_str())
                .unwrap_or("unknown");
            if request.state.as_ref().is_some_and(|filter| filter != state) {
                continue;
            }
            let terminal = Reader::open_unchecked(&directory).terminal().ok();
            // Observation only, whether or not this listing repaired above.
            let mut origin_budget = session::origin::ReadBudget::for_query(None);
            let origin = session::origin::observe(
                root,
                session::origin::read_reference(
                    &Reader::open_unchecked(&directory),
                    &mut origin_budget,
                ),
                &mut origin_budget,
            );
            let mut value = serde_json::json!({
                "id": manifest.id,
                "provider": manifest.provider,
                "workspace": manifest.workspace,
                "title": manifest.title,
                "yolo": manifest.yolo,
                "state": status.as_ref().map(|value| value.state.as_str()).unwrap_or("unknown"),
                "terminal": terminal.as_ref().map(|value| value.kind.as_str()),
                "terminal_session_id": terminal.as_ref().map(|value| value.id.as_str()),
                "terminal_tab_id": terminal.as_ref().and_then(|value| value.tab_id.as_deref()),
                "terminal_window_id": terminal.as_ref().and_then(|value| value.window_id.as_deref()),
                "iterm_session_id": terminal.as_ref()
                    .filter(|value| value.kind == terminal::TerminalKind::Iterm2)
                    .map(|value| value.id.as_str()),
                "created_unix_ms": manifest.created_unix_ms,
                "updated_unix_ms": status.as_ref().map(|value| value.updated_unix_ms),
                "error": status.as_ref().and_then(|value| value.error.as_deref()),
                "model": manifest.model,
                "effort": manifest.effort,
                "resumed_from": read_resumed_from(&directory).ok().flatten(),
                "results": Reader::open_unchecked(&directory).published_result_count().unwrap_or(0),
            });
            origin.add_fields(&mut value);
            sessions.push(value);
        }
    }
    sessions.sort_by(|left, right| {
        let by_id = left["id"].as_str().cmp(&right["id"].as_str());
        if request.sort_updated {
            right["updated_unix_ms"]
                .as_u64()
                .cmp(&left["updated_unix_ms"].as_u64())
                .then(by_id)
        } else {
            by_id
        }
    });
    Ok(sessions)
}
