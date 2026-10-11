//! Set or release the user's follow-up hold.
use super::*;
use crate::native::session::primitives::require_valid_session_id;
use serde_json::json;

#[derive(Debug)]
pub(crate) struct Request {
    id: String,
    release: bool,
    json: bool,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let parsed = (|| {
        let mut id = None;
        let mut release = false;
        let mut json = false;
        for arg in args {
            match arg.as_str() {
                "--release" => set_flag_once(&mut release, "--release")?,
                "--json" => set_flag_once(&mut json, "--json")?,
                value if !value.starts_with('-') => {
                    require_valid_session_id(value)?;
                    set_once(&mut id, value.to_owned(), "session")?;
                }
                value => bail!("unknown hold option: {value}"),
            }
        }
        Ok(NativeCommand::Hold(Request {
            id: id.context("hold requires one session id")?,
            release,
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
        session::hold::change(&store, request.release)
    })();
    match outcome {
        Ok(change) => {
            let mut fields = serde_json::to_value(&change)?.as_object().unwrap().clone();
            fields.insert("session".into(), json!(request.id));
            envelope::emit(request.json, &envelope::ok(fields), || {
                println!(
                    "session {}: {}",
                    request.id,
                    if change.held { "held" } else { "hold released" }
                );
                Ok(())
            })
        }
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
