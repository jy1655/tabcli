// Claude-owned turn correlation fallback until SendMessage supplies addressed turn identity.
use super::{claude_owned_string, claude_string};
use crate::native::session::{Reader, RecordStore, Store, turn as session_turn};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static CROSS_SESSION_TURN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const PENDING_TURN_FILE: &str = "claude-pending-turn.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ClaudeTurn {
    schema: u32,
    request_id: String,
    marker: String,
}

impl ClaudeTurn {
    pub(super) fn new(request_id: &str) -> Result<Self> {
        if !(request_id.starts_with("claude-turn-")
            && request_id.len() <= 128
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        {
            bail!("invalid Claude cross-session turn id")
        }
        Ok(Self {
            schema: 1,
            request_id: request_id.to_owned(),
            marker: format!("<!-- agent-bridge-claude-turn:{request_id} -->"),
        })
    }
}

pub(super) struct Installed {
    path: PathBuf,
    retained: bool,
}

impl Installed {
    pub(super) fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        if !self.retained {
            let _ = RecordStore::at(&self.path).remove();
        }
    }
}

pub(super) fn new_cross_session_turn_id() -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(format!(
        "claude-turn-{now}-{}-{}",
        std::process::id(),
        CROSS_SESSION_TURN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

impl ClaudeTurn {
    pub(super) fn id(&self) -> &str {
        &self.request_id
    }
    pub(super) fn marker(&self) -> &str {
        &self.marker
    }
    pub(super) fn target_message(&self, prompt: &str) -> String {
        format!(
            "{prompt}\n\n[Agent Bridge Claude turn protocol]\nComplete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.\n{}",
            self.marker
        )
    }

    pub(super) fn correlated_response<'a>(&self, message: &'a str) -> Result<&'a str> {
        let trimmed = message.trim_end();
        let body = trimmed
            .strip_suffix(&self.marker)
            .context("Claude response did not end with the expected turn marker")?
            .trim_end();
        if body.is_empty() {
            bail!("Claude correlated response contained no assistant text")
        }
        Ok(body)
    }

    pub(super) fn install(&self, store: &Store) -> Result<Installed> {
        let path = store.private(PENDING_TURN_FILE).path().to_owned();
        store.private(PENDING_TURN_FILE).write_json(self)?;
        Ok(Installed {
            path,
            retained: false,
        })
    }
}

pub(super) fn read(reader: &Reader) -> Result<Option<ClaudeTurn>> {
    let Some(pending_text) = reader.private(PENDING_TURN_FILE).text()? else {
        return Ok(None);
    };
    let pending: ClaudeTurn =
        serde_json::from_str(&pending_text).context("failed to parse the pending Claude turn")?;
    let valid_pending = matches!(
        ClaudeTurn::new(&pending.request_id),
        Ok(expected) if expected.schema == pending.schema && expected.marker == pending.marker
    );
    if !valid_pending {
        bail!("Agent Bridge rejected invalid Claude turn correlation state")
    }
    Ok(Some(pending))
}

pub(super) fn on_stop(store: &Store, payload: &serde_json::Value) -> Result<()> {
    let Some(pending) = read(store)? else {
        return handle_uncorrelated_stop(store, payload);
    };
    let Some(message) = claude_string(payload, "last_assistant_message") else {
        return Ok(());
    };
    let Ok(message) = pending.correlated_response(message) else {
        return Ok(());
    };
    session_turn::Report::for_claim(store, agent_bridge::FirstPartyCli::Claude, None)
        .complete(
            message,
            claude_owned_string(payload, "session_id"),
            Some(pending.request_id),
        )
        .context("failed to record the correlated Claude result")
}

fn handle_uncorrelated_stop(store: &Store, payload: &serde_json::Value) -> Result<()> {
    let message = claude_string(payload, "last_assistant_message")
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .context("Claude Stop hook payload has no assistant result")?;
    session_turn::Report::initial(store, agent_bridge::FirstPartyCli::Claude).complete(
        message,
        claude_owned_string(payload, "session_id"),
        None,
    )
}

pub(super) fn on_stop_failure(store: &Store, payload: &serde_json::Value) -> Result<()> {
    if store.private(PENDING_TURN_FILE).text()?.is_some() {
        // SendMessage does not return the target prompt identity that would let
        // Agent Bridge bind this failure to the delivered request. Leave the
        // pending turn claimed instead of attributing an unrelated failure.
        return Ok(());
    }
    let error = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown Claude API error");
    let detail = payload
        .get("error_details")
        .and_then(serde_json::Value::as_str)
        .filter(|detail| !detail.trim().is_empty());
    let error = detail.map_or_else(
        || format!("Claude turn failed: {error}"),
        |detail| format!("Claude turn failed: {error}: {detail}"),
    );
    session_turn::Report::initial(store, agent_bridge::FirstPartyCli::Claude).fail(
        &error,
        claude_owned_string(payload, "session_id"),
        None,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
