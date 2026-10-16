//! Removal of closed session records past their retention window.
use super::args::{option_value, set_flag_once, set_once};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{CoreRecord, Reader, RecordReader, SessionState};
use crate::native::{NativeCommand, session};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct PruneRequest {
    closed_before_days: u64,
    explicit: bool,
    json: bool,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let mut closed_before_days = None;
    let mut explicit = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--closed-before-days" => {
                let value = option_value(args, &mut index, "--closed-before-days")?;
                let days = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid retention days: {value}"))?;
                if days == 0 || days.checked_mul(86_400).is_none() {
                    bail!("--closed-before-days must be a positive supported day count");
                }
                set_once(&mut closed_before_days, days, "--closed-before-days")?;
            }
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown prune-sessions option: {option}"),
        }
        index += 1;
    }
    let closed_before_days =
        closed_before_days.context("prune-sessions requires --closed-before-days N")?;
    if !explicit {
        bail!("pruning closed session records requires --explicit");
    }
    Ok(NativeCommand::Prune(PruneRequest {
        closed_before_days,
        explicit,
        json,
    }))
}

pub(super) fn run(request: PruneRequest) -> Result<()> {
    if !request.explicit {
        bail!("pruning closed session records requires --explicit");
    }
    let retention_ms = u128::from(request.closed_before_days)
        .checked_mul(86_400_000)
        .context("closed-session retention window is too large")?;
    let now_unix_ms = unix_ms();
    let removed = if retention_ms > now_unix_ms {
        Vec::new()
    } else {
        prune_closed_sessions(&Reader::state_root()?, now_unix_ms - retention_ms)?
    };
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "closed_before_days": request.closed_before_days,
                "pruned": removed,
            }))?
        );
    } else if removed.is_empty() {
        println!("no eligible closed Agent Bridge sessions");
    } else {
        for id in &removed {
            println!("pruned {id}");
        }
        println!("pruned {} closed session(s)", removed.len());
    }
    Ok(())
}

pub(super) fn prune_closed_sessions(root: &Path, cutoff_unix_ms: u128) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve state directory {}", root.display()))?;
    let mut removed = Vec::new();
    for entry in session::enumerate::enumerate(root, session::enumerate::CheckOrder::FileTypeFirst)?
    {
        let entry = entry.map_err(session::enumerate::Fault::into_io)?;
        let id = entry.id;
        let directory = entry.directory;
        if !RecordReader::at(
            Reader::open_unchecked(&directory)
                .record(CoreRecord::Manifest)
                .path(),
        )
        .is_regular_file()?
        {
            continue;
        }
        let Ok(manifest) = Reader::open_unchecked(&directory).manifest() else {
            continue;
        };
        if manifest.id != id {
            continue;
        }
        let closed = match Reader::open_unchecked(&directory).closed_if_present() {
            Ok(Some(closed)) => closed,
            Ok(None) | Err(_) => continue,
        };
        let status = match Reader::open_unchecked(&directory).status_if_present() {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => continue,
        };
        if closed.state != SessionState::Closed
            || status.state != SessionState::Closed
            || closed.updated_unix_ms > cutoff_unix_ms
            || status.updated_unix_ms > cutoff_unix_ms
            || Reader::open_unchecked(&directory).has_active_session_capability()
            || Reader::open_unchecked(&directory).native_owner_blocks_prune()?
        {
            continue;
        }
        let canonical_directory = directory
            .canonicalize()
            .with_context(|| format!("failed to resolve session directory {id}"))?;
        if canonical_directory.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("failed to recheck session directory {id}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        session::RecordStore::at(&directory)
            .remove_directory_all()
            .with_context(|| format!("failed to prune closed session {id}"))?;
        removed.push(id);
    }
    removed.sort();
    Ok(removed)
}
