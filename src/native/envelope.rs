//! Shared output defaults and rendering; no session reads or writes.
use anyhow::Result;
use serde_json::{Map, Value, json};

pub(super) const SCHEMA_VERSION: u64 = 1;

/// Command fields override every default, including reserved keys.
pub(super) fn ok(fields: Map<String, Value>) -> Value {
    let mut value = json!({"schema_version": SCHEMA_VERSION, "ok": true});
    value.as_object_mut().unwrap().extend(fields);
    value
}

pub(super) fn error(error: &anyhow::Error, fields: Map<String, Value>) -> Value {
    let mut value =
        json!({"schema_version": SCHEMA_VERSION, "ok": false, "error": format!("{error:#}")});
    value.as_object_mut().unwrap().extend(fields);
    value
}

pub(super) fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(super) fn parse_failure(
    args: &[String],
    error: &anyhow::Error,
    fields: Map<String, Value>,
) -> Result<()> {
    if args.iter().any(|arg| arg == "--json") {
        print(&self::error(error, fields))?;
    }
    Ok(())
}

pub(super) fn emit(json: bool, value: &Value, plain: impl FnOnce() -> Result<()>) -> Result<()> {
    if json { print(value) } else { plain() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn capture(name: &str, run: impl FnOnce()) -> Vec<u8> {
        const CHILD: &str = "AGENT_BRIDGE_ENVELOPE_TEST";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            println!("ENVELOPE_BEGIN");
            run();
            println!("ENVELOPE_END");
            return Vec::new();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("native::envelope::tests::{name}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, name)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        let start = b"ENVELOPE_BEGIN\n";
        let end = b"ENVELOPE_END\n";
        let first = output
            .stdout
            .windows(start.len())
            .position(|w| w == start)
            .unwrap()
            + start.len();
        let last = output.stdout[first..]
            .windows(end.len())
            .position(|w| w == end)
            .unwrap()
            + first;
        output.stdout[first..last].to_vec()
    }

    fn assert_bytes(name: &str, expected: &[u8], run: impl FnOnce()) {
        let bytes = capture(name, run);
        if std::env::var("AGENT_BRIDGE_ENVELOPE_TEST").as_deref() != Ok(name) {
            assert_eq!(bytes, expected);
        }
    }

    #[test]
    fn print_preserves_pretty_utf8_escapes_null_and_omission() {
        assert_bytes("print_preserves_pretty_utf8_escapes_null_and_omission",
            "{\n  \"a\": null,\n  \"ok\": true,\n  \"schema_version\": 1,\n  \"z\": \"한글\\n\\\"\\\\\"\n}\n".as_bytes(), || {
                print(&ok(json!({"z": "한글\n\"\\", "a": null}).as_object().unwrap().clone())).unwrap();
            });
    }

    #[test]
    fn parse_failure_preserves_chain_and_optional_json() {
        assert_bytes("parse_failure_preserves_chain_and_optional_json",
            b"{\n  \"error\": \"outer: inner\",\n  \"hits\": [],\n  \"ok\": false,\n  \"query\": null,\n  \"schema_version\": 1\n}\n", || {
                let error = anyhow::anyhow!("inner").context("outer");
                let fields = json!({"query": null, "hits": []}).as_object().unwrap().clone();
                parse_failure(&[], &error, fields.clone()).unwrap();
                parse_failure(&["--bogus".into()], &error, fields.clone()).unwrap();
                parse_failure(&["--json".into()], &error, fields).unwrap();
            });
    }

    #[test]
    fn fields_override_all_defaults_without_adding_error() {
        assert_bytes("fields_override_all_defaults_without_adding_error",
            b"{\n  \"ok\": false,\n  \"schema_version\": 7\n}\n{\n  \"error\": null,\n  \"ok\": true,\n  \"schema_version\": 8\n}\n", || {
                print(&ok(json!({"ok": false, "schema_version": 7}).as_object().unwrap().clone())).unwrap();
                print(&error(&anyhow::anyhow!("ignored"), json!({"ok": true, "error": null, "schema_version": 8}).as_object().unwrap().clone())).unwrap();
            });
    }

    #[test]
    fn emit_selects_once_and_propagates_plain_error() {
        assert_bytes(
            "emit_selects_once_and_propagates_plain_error",
            b"{\n  \"ok\": true,\n  \"schema_version\": 1\n}\nplain\nbefore error\n",
            || {
                let count = Cell::new(0);
                let value = ok(Map::new());
                emit(true, &value, || {
                    count.set(count.get() + 1);
                    panic!("JSON called plain")
                })
                .unwrap();
                assert_eq!(count.get(), 0);
                emit(false, &value, || {
                    count.set(count.get() + 1);
                    println!("plain");
                    Ok(())
                })
                .unwrap();
                assert_eq!(count.get(), 1);
                let result = emit(false, &value, || {
                    count.set(count.get() + 1);
                    println!("before error");
                    Err(anyhow::anyhow!("inner").context("outer"))
                });
                assert_eq!(count.get(), 2);
                assert_eq!(format!("{:#}", result.unwrap_err()), "outer: inner");
            },
        );
    }
}
