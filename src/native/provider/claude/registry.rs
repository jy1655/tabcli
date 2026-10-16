use super::valid_claude_conversation_id;
use crate::native::session::RecordReader;
#[cfg(windows)]
use crate::native::terminal;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

// Claude Code registers every live interactive session as `<config dir>/sessions/<pid>.json`
// with `pid`, `sessionId`, `procStart` (the process creation FILETIME on Windows), and the
// session's `name`. The `<pid>.<hash>.key` files beside those records are secrets and are
// never opened.
#[derive(Deserialize)]
struct SessionRegistryEntry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "procStart", default)]
    proc_start: Option<serde_json::Value>,
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
thread_local! {
    static SESSION_REGISTRY_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::native) fn override_session_registry_for_test(registry: Option<PathBuf>) {
    SESSION_REGISTRY_OVERRIDE.with(|slot| *slot.borrow_mut() = registry);
}

pub(super) fn claude_sessions_registry_dir() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(registry) = SESSION_REGISTRY_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Ok(registry);
    }
    let config_dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("neither HOME nor USERPROFILE is set")?;
            PathBuf::from(home).join(".claude")
        }
    };
    Ok(config_dir.join("sessions"))
}

// A registry record whose identity fields all validated against the file it was read from.
struct ValidatedRegistryRecord {
    pid: u32,
    session_id: String,
    proc_start: u64,
    name: Option<String>,
}

// Parses and validates one `<pid>.json` record; `stem` is the file name's numeric stem.
// Every defect is reported as text so the caller can apply the file-name-pid refusal rule.
// A deserialized record is never trusted before its `pid`, `sessionId`, and `procStart` are
// checked: a structurally valid record with a foreign or reused identity would otherwise
// clear a live holder of the conversation.
fn validated_registry_record(
    stem: &str,
    text: &str,
) -> std::result::Result<ValidatedRegistryRecord, String> {
    let record =
        serde_json::from_str::<SessionRegistryEntry>(text).map_err(|error| error.to_string())?;
    if stem.parse::<u32>().ok() != Some(record.pid) {
        return Err(format!(
            "payload pid {} does not match the file name pid {stem}",
            record.pid
        ));
    }
    if !valid_claude_conversation_id(&record.session_id) {
        return Err(format!(
            "sessionId {:?} is not a Claude session UUID",
            record.session_id
        ));
    }
    let proc_start = record
        .proc_start
        .as_ref()
        .ok_or_else(|| "procStart is missing".to_owned())?;
    let proc_start = proc_start_filetime(proc_start)
        .ok_or_else(|| format!("procStart {proc_start} is not a process creation FILETIME"))?;
    Ok(ValidatedRegistryRecord {
        pid: record.pid,
        session_id: record.session_id,
        proc_start,
        name: record.name,
    })
}

// A registered Claude Code process that is alive and whose creation time still matches its
// record, so the record describes that process and not a reused pid.
#[derive(Debug, Eq, PartialEq)]
struct LiveConversationHolder {
    pid: u32,
    name: Option<String>,
}

// The pid of a registered live Claude Code process that still holds the conversation, if
// any. `live_creation_time` answers `Ok(None)` for a dead pid, `Ok(Some(_))` with the live
// process's creation time, and `Err` when the process exists but cannot be inspected, which
// refuses the reopen rather than treating the entry as stale.
pub(super) fn live_conversation_writer(
    registry: &Path,
    provider_session_id: &str,
    live_creation_time: impl Fn(u32) -> Result<Option<u64>>,
) -> Result<Option<u32>> {
    Ok(
        live_conversation_holders(registry, provider_session_id, live_creation_time)?
            .into_iter()
            .next()
            .map(|holder| holder.pid),
    )
}

// Every registered live process that holds the conversation. The scan fails closed: a
// numeric `<pid>.json` whose record cannot be parsed, lacks a required field, or carries an
// identity that does not describe the file it sits in (a payload `pid` other than the file
// name's, a `sessionId` that is not a Claude session UUID, or a `procStart` that is missing
// or not a FILETIME) is judged by the pid in its file name, because that record could belong
// to a live holder of this very conversation. A dead pid clears it; a live or uninspectable
// pid refuses the reopen and names the record. Only a record whose identity fields all
// validate is ever compared against the conversation.
fn live_conversation_holders(
    registry: &Path,
    provider_session_id: &str,
    live_creation_time: impl Fn(u32) -> Result<Option<u64>>,
) -> Result<Vec<LiveConversationHolder>> {
    let entries = match std::fs::read_dir(registry) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read the Claude session registry {}",
                    registry.display()
                )
            });
        }
    };
    let mut holders = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        let Some(text) = RecordReader::at(&path).text()? else {
            continue;
        };
        let record = match validated_registry_record(stem, &text) {
            Ok(record) => record,
            Err(defect) => {
                let Ok(file_pid) = stem.parse::<u32>() else {
                    bail!(
                        "reopen unsupported: Claude session registry entry {} is malformed ({defect}) and its file name is not a pid",
                        path.display()
                    )
                };
                let live = live_creation_time(file_pid).with_context(|| {
                    format!(
                        "reopen unsupported: Claude session registry entry {} is malformed ({defect}) and process {file_pid} could not be inspected",
                        path.display()
                    )
                })?;
                if live.is_some() {
                    bail!(
                        "reopen unsupported: Claude session registry entry {} is malformed ({defect}) and names live process {file_pid}; the conversation it holds cannot be verified",
                        path.display()
                    )
                }
                continue;
            }
        };
        if record.session_id != provider_session_id {
            continue;
        }
        use crate::native::process_identity::{self, Gone, ProcessObservation};
        match process_identity::classify(
            record.pid,
            Some(&record.proc_start),
            || false, // A recorded incarnation never uses PID-only liveness.
            || live_creation_time(record.pid),
        ) {
            ProcessObservation::Live(_) => holders.push(LiveConversationHolder {
                pid: record.pid,
                name: record.name,
            }),
            ProcessObservation::Gone(Gone::Exited | Gone::Reused(_)) => continue,
            ProcessObservation::Unknown(reason) => {
                return Err(anyhow::anyhow!(reason)).with_context(|| {
                    format!(
                        "failed to verify the Claude Code process {} registered at {}",
                        record.pid,
                        path.display()
                    )
                });
            }
        }
    }
    holders.sort_by_key(|holder| holder.pid);
    Ok(holders)
}

// The other live holders of a conversation once the reopened process itself has registered
// under its managed `--name`. Waits for that registration until `deadline`, because Claude
// writes its registry record shortly after startup; a reopened process that never registers,
// or that registered a different conversation, is an error rather than a clean answer.
pub(super) fn wait_for_resumed_conversation_holders(
    registry: &Path,
    provider_session_id: &str,
    managed_name: &str,
    live_creation_time: impl Fn(u32) -> Result<Option<u64>>,
    deadline: Instant,
) -> Result<Vec<u32>> {
    loop {
        if let Some(others) = other_resumed_conversation_holders_once(
            registry,
            provider_session_id,
            managed_name,
            &live_creation_time,
        )? {
            return Ok(others);
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
        else {
            bail!(
                "reopened Claude session {managed_name} did not register conversation {provider_session_id} under {} before the deadline",
                registry.display()
            )
        };
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

// The delivery-boundary re-scan of a reopened session that already registered. It does not
// wait: a registration that has disappeared means the reopened process is no longer a
// verifiable holder, and nothing may be delivered to it. This is the check that runs
// immediately before the initial prompt and before every `tell`; a foreign resume that
// registered since the previous scan is reported here.
pub(super) fn recheck_resumed_conversation_holders(
    registry: &Path,
    provider_session_id: &str,
    managed_name: &str,
    live_creation_time: impl Fn(u32) -> Result<Option<u64>>,
) -> Result<Vec<u32>> {
    other_resumed_conversation_holders_once(
        registry,
        provider_session_id,
        managed_name,
        live_creation_time,
    )?
    .with_context(|| {
        format!(
            "reopened Claude session {managed_name} is no longer registered for conversation {provider_session_id} under {}",
            registry.display()
        )
    })
}

// `Ok(None)` while the reopened process has not registered yet; otherwise the pids of every
// other live holder of the conversation.
fn other_resumed_conversation_holders_once(
    registry: &Path,
    provider_session_id: &str,
    managed_name: &str,
    live_creation_time: impl Fn(u32) -> Result<Option<u64>>,
) -> Result<Option<Vec<u32>>> {
    let holders = live_conversation_holders(registry, provider_session_id, live_creation_time)?;
    let own = holders
        .iter()
        .filter(|holder| holder.name.as_deref() == Some(managed_name))
        .map(|holder| holder.pid)
        .collect::<Vec<_>>();
    let own_pid = match own.as_slice() {
        [] => return Ok(None),
        [pid] => *pid,
        _ => bail!(
            "Claude session registry under {} names {} live processes as {managed_name} for conversation {provider_session_id}",
            registry.display(),
            own.len()
        ),
    };
    Ok(Some(
        holders
            .into_iter()
            .filter(|holder| holder.pid != own_pid)
            .map(|holder| holder.pid)
            .collect(),
    ))
}

fn proc_start_filetime(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => text.trim().parse().ok(),
        serde_json::Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

#[cfg(windows)]
pub(super) fn live_process_creation_time(pid: u32) -> Result<Option<u64>> {
    terminal::observe_windows_process_identity(pid)
        .map(|identity| identity.map(|identity| identity.creation_time))
}

#[cfg(not(windows))]
pub(super) fn live_process_creation_time(pid: u32) -> Result<Option<u64>> {
    bail!("Claude process {pid} creation time is only verified on native Windows")
}

#[cfg(test)]
mod tests;
