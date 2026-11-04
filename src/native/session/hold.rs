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

/// Why a follow-up admission refused before a claim. A pre-claim Hold refusal has a JSON
/// envelope without changing other tell errors. `Display` is the text `tell` has always
/// printed; observers report `code` and render the same text as their detail.
pub(in crate::native) enum FollowUpRefusal {
    Held(String),
    HoldUnreadable(String),
    NotReady {
        session: String,
        state: super::SessionState,
    },
}

impl FollowUpRefusal {
    pub(in crate::native) fn code(&self) -> &'static str {
        match self {
            Self::Held(_) => "session_held",
            Self::HoldUnreadable(_) => "hold_unreadable",
            Self::NotReady { .. } => "state_not_ready",
        }
    }
}

impl std::fmt::Display for FollowUpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held(id) => write!(
                f,
                "session {id} is held; release the hold before sending a follow-up"
            ),
            Self::HoldUnreadable(id) => write!(
                f,
                "session {id} has an unreadable hold record; inspect it before sending a follow-up"
            ),
            Self::NotReady { session, state } => write!(
                f,
                "session {session} is {state}; tell requires the ready state"
            ),
        }
    }
}

// Every error rendering stays what it was before the refusals were typed: the Hold refusals
// were a `Refusal(String)` and the state refusal an anyhow message leaf.
impl std::fmt::Debug for FollowUpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = self.to_string();
        match self {
            Self::Held(_) | Self::HoldUnreadable(_) => {
                f.debug_tuple("Refusal").field(&message).finish()
            }
            Self::NotReady { .. } => std::fmt::Debug::fmt(&message, f),
        }
    }
}
impl std::error::Error for FollowUpRefusal {}

/// The Hold gate judged from one read of the record. Reads nothing and takes no lock.
pub(in crate::native) fn hold_gate(held: &Result<bool>, id: &str) -> Result<(), FollowUpRefusal> {
    match held {
        Ok(false) => Ok(()),
        Ok(true) => Err(FollowUpRefusal::Held(id.to_owned())),
        Err(_) => Err(FollowUpRefusal::HoldUnreadable(id.to_owned())),
    }
}

/// The lifecycle gate of a follow-up: only a ready session accepts one.
pub(in crate::native) fn ready_gate(
    state: &super::SessionState,
    id: &str,
) -> Result<(), FollowUpRefusal> {
    if state.accepts_prompt() {
        return Ok(());
    }
    Err(FollowUpRefusal::NotReady {
        session: id.to_owned(),
        state: state.clone(),
    })
}

/// No lock acquisition: admission callers own their lifecycle ordering.
pub(in crate::native) fn permit(reader: &Reader, id: &str) -> Result<()> {
    hold_gate(&read(reader), id)?;
    Ok(())
}

pub(in crate::native) fn follow_up_admission(
    reader: &Reader,
    id: &str,
) -> Result<super::SessionState> {
    let state = reader.status()?.state;
    ready_gate(&state, id)?;
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
