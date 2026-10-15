//! Lifecycle output for ask, tell and reopen: accepted, completed and failed,
//! with or without a request address.
use super::*;

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
                    query::request_result(&Reader::open_unchecked(&directory), request_id)
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

pub(super) fn emit_session_result(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    receipt: &requests::Receipt,
    event: Option<&SessionEvent>,
) -> Result<()> {
    emit_session_result_with(
        json,
        id,
        terminal_session,
        provider,
        receipt,
        event,
        &serde_json::Map::new(),
    )
}

pub(super) fn emit_session_result_with(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    receipt: &requests::Receipt,
    event: Option<&SessionEvent>,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let request_id = &receipt.request_id;
    let (elapsed, elapsed_reason) = query::observed_elapsed(Some(receipt), event);
    let mut value = envelope::ok(
        serde_json::json!({
                "session": id,
                "request_id": request_id,
                "context_sources": receipt.context_sources,
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
                "request_state": if event.is_some() { "completed" } else { "accepted" },
                "provider": provider.as_str(),
                "terminal": terminal_session.kind.as_str(),
                "terminal_session_id": terminal_session.id,
                "terminal_tab_id": terminal_session.tab_id,
                "terminal_window_id": terminal_session.window_id,
                "iterm_session_id": (terminal_session.kind == terminal::TerminalKind::Iterm2)
                    .then_some(terminal_session.id.as_str()),
                "result": event.map(|value| value.message.as_str()),
                "provider_session_id": event.and_then(|value| value.provider_session_id.as_deref()),
                "turn_id": event.and_then(|value| value.turn_id.as_deref()),
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    for (key, field) in extra {
        value[key.as_str()] = field.clone();
    }
    envelope::emit(json, &value, || {
        println!("session: {id}\nrequest: {request_id}");
        for (key, field) in extra {
            if let Some(field) = field.as_str() {
                println!("{}: {}", key, terminal_safe_text(field, false));
            }
        }
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
        Ok(())
    })
}
