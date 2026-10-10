//! The user's follow-up intent, independent of lifecycle state.
use super::{CoreRecord, Reader, Store, unix_ms};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Hold {
    held: bool,
    created_unix_ms: u128,
}

fn decode(bytes: &[u8]) -> Result<()> {
    let record: Hold = serde_json::from_slice(bytes)?;
    if !record.held {
        bail!("hold record must have held: true");
    }
    Ok(())
}

pub(in crate::native) fn read(reader: &Reader) -> Result<bool> {
    match reader.record(CoreRecord::Hold).bytes()? {
        None => Ok(false),
        Some(text) => {
            decode(&text)?;
            Ok(true)
        }
    }
}

/// Legacy sessions may have no lifecycle lock yet. Detect a writer entering that
/// case without making Hold a required publication input for other queries.
pub(in crate::native) fn observe(reader: &Reader, locked: bool) -> Result<bool> {
    if locked {
        return read(reader);
    }
    let record = reader.record(CoreRecord::Hold);
    let before = record.bytes()?;
    let held = read(reader)?;
    if record.bytes()? != before || reader.lock_shared()?.is_some() {
        bail!("hold record changed during observation; retry the query");
    }
    Ok(held)
}

/// A pre-claim Hold refusal has a JSON envelope without changing other tell errors.
#[derive(Debug)]
pub(in crate::native) struct Refusal(String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refusal {}

/// No lock acquisition: admission callers own their lifecycle ordering.
pub(in crate::native) fn permit(reader: &Reader, id: &str) -> Result<()> {
    match read(reader) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Refusal(format!(
            "session {id} is held; release the hold before sending a follow-up"
        ))
        .into()),
        Err(_) => Err(Refusal(format!(
            "session {id} has an unreadable hold record; inspect it before sending a follow-up"
        ))
        .into()),
    }
}

pub(in crate::native) fn follow_up_admission(
    reader: &Reader,
    id: &str,
) -> Result<super::SessionState> {
    let state = reader.status()?.state;
    if !state.accepts_prompt() {
        bail!("session {id} is {state}; tell requires the ready state");
    }
    permit(reader, id)?;
    Ok(state)
}

#[derive(Serialize)]
pub(in crate::native) struct Change {
    pub(in crate::native) held: bool,
    changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<&'static str>,
}

/// The command validates the manifest before entering this record transition.
pub(in crate::native) fn change(store: &Store, release: bool) -> Result<Change> {
    let _lock = store.lock()?;
    if store.closed_if_present()?.is_some() {
        bail!("the session is closed; a hold cannot be set or released");
    }
    let record = store.record(CoreRecord::Hold);
    let text = record.bytes()?;
    let previous = text.as_deref().map(decode).transpose();
    let malformed = previous.is_err();
    let was_held = matches!(previous, Ok(Some(())));
    if release {
        record.remove()?;
    } else if !was_held {
        record.write_json(&Hold {
            held: true,
            created_unix_ms: unix_ms(),
        })?;
    }
    Ok(Change {
        held: !release,
        changed: malformed || was_held == release,
        previous: malformed.then_some("malformed"),
    })
}

pub(in crate::native) fn add_fields(value: &mut serde_json::Value, held: &Result<bool>) {
    match held {
        Ok(held) => value["held"] = serde_json::json!(held),
        Err(error) => {
            value["held"] = serde_json::Value::Null;
            value["hold_error"] = serde_json::json!(format!("{error:#}"));
        }
    }
}
