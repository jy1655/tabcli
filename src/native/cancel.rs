//! Record the user's intent to interrupt the active request.
use super::args::{set_flag_once, set_once};
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::{Reader, Store};
use crate::native::{NativeCommand, envelope, session};
use anyhow::{Context, Result, bail};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct Request {
    id: String,
    json: bool,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let parsed = (|| {
        let mut id = None;
        let mut json = false;
        for arg in args {
            match arg.as_str() {
                "--json" => set_flag_once(&mut json, "--json")?,
                value if !value.starts_with('-') => {
                    require_valid_session_id(value)?;
                    set_once(&mut id, value.to_owned(), "session")?;
                }
                value => bail!("unknown cancel option: {value}"),
            }
        }
        Ok(NativeCommand::Cancel(Request {
            id: id.context("cancel requires one session id")?,
            json,
        }))
    })();
    if let Err(error) = &parsed {
        envelope::parse_failure(args, error, serde_json::Map::new())?;
    }
    parsed
}

pub(super) fn run(request: Request) -> Result<()> {
    let outcome = (|| {
        let directory = Reader::session_directory(&request.id)?;
        let store = Store::open_unchecked(directory);
        store.manifest()?;
        session::cancel::request(&store, &request.id)
    })();
    match outcome {
        Ok(change) => envelope::emit(
            request.json,
            &envelope::ok(
                json!({
                    "request_id": change.request_id, "state": "requested",
                    "created_unix_ms": change.created_unix_ms, "session": request.id,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            || {
                println!(
                    "session {}: cancel requested for {}",
                    request.id, change.request_id
                );
                Ok(())
            },
        ),
        Err(error) => {
            if request.json {
                envelope::print(&envelope::error(
                    &error,
                    serde_json::Map::from_iter([("session".into(), json!(request.id))]),
                ))?;
            }
            Err(error)
        }
    }
}
