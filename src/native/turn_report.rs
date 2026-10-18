//! Lifecycle output for ask, tell and reopen: accepted, completed and failed,
//! with or without a request address.

use crate::native::session::Reader;
use crate::native::{envelope, query};
use agent_bridge::PUBLIC_COMMAND;
use anyhow::{Context, Result};
/// The command's known fields and the address acquired once its receipt exists.
pub(super) struct TurnReport {
    json: bool,
    before_address: serde_json::Map<String, serde_json::Value>,
    address: Option<(String, String)>,
}

impl TurnReport {
    pub(super) fn new(
        json: bool,
        before_address: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            json,
            before_address,
            address: None,
        }
    }

    pub(super) fn addressed(&mut self, session: String, request_id: String) {
        self.address = Some((session, request_id));
    }

    pub(super) fn address(&self) -> Option<(&str, &str)> {
        self.address
            .as_ref()
            .map(|(session, request)| (session.as_str(), request.as_str()))
    }

    pub(super) fn finish(
        self,
        outcome: Result<()>,
        extra: serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        if let Some((session, request_id)) = self.address {
            return finish_request_with_extra(outcome, self.json, &session, &request_id, extra);
        }
        if self.json
            && let Err(error) = &outcome
        {
            let mut fields = self.before_address;
            fields.extend(extra);
            envelope::print(&envelope::error(error, fields))?;
        }
        outcome
    }
}

pub(super) fn finish_request_with_extra(
    outcome: Result<()>,
    json: bool,
    session: &str,
    request_id: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    if let Err(error) = outcome {
        if json {
            let mut value = Reader::session_directory(session)
                .and_then(|directory| {
                    let reader = Reader::open_unchecked(&directory);
                    query::observe_snapshot(&reader)?
                        .result(&reader, &query::Selector::Request(request_id.to_owned()))
                })
                .unwrap_or_else(|_| {
                    serde_json::json!({
                        "session": session, "request_id": request_id,
                        "request_state": "unknown", "result": null
                    })
                });
            let fields = value.as_object_mut().unwrap();
            fields.remove("ok");
            fields.remove("error");
            let mut value = envelope::error(&error, std::mem::take(fields));
            for (key, field) in extra {
                value[key] = field;
            }
            envelope::print(&value)?;
        }
        return Err(error).with_context(|| format!(
            "session {session}, request {request_id}; inspect with `{PUBLIC_COMMAND} result {session} --request {request_id} --json`; this error alone is not proof of non-delivery; inspect the recorded outcome before deciding whether to retry"
        ));
    }
    Ok(())
}
