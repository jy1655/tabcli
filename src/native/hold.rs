//! Set or release the user's follow-up hold.
use super::*;
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
        session::hold::change(&store, request.release)
    })();
    match outcome {
        Ok(change) => {
            if request.json {
                let mut value = serde_json::to_value(change)?;
                value["schema_version"] = json!(1);
                value["ok"] = json!(true);
                value["session"] = json!(request.id);
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "session {}: {}",
                    request.id,
                    if change.held { "held" } else { "hold released" }
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
