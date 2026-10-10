//! Record the user's intent to interrupt the active request.
use super::*;
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
    if let Err(error) = &parsed
        && args.iter().any(|arg| arg == "--json")
    {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": 1, "ok": false, "error": format!("{error:#}")
            }))?
        );
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
        Ok(change) => {
            if request.json {
                let mut value = json!({"request_id": change.request_id, "state": "requested", "created_unix_ms": change.created_unix_ms});
                value["schema_version"] = json!(1);
                value["ok"] = json!(true);
                value["session"] = json!(request.id);
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "session {}: cancel requested for {}",
                    request.id, change.request_id
                );
            }
            Ok(())
        }
        Err(error) => {
            if request.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": 1, "ok": false, "session": request.id, "error": format!("{error:#}")
                    }))?
                );
            }
            Err(error)
        }
    }
}
