use super::*;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bridge() -> &'static Path {
    Path::new(if cfg!(windows) {
        r"C:\tabcli.exe"
    } else {
        "/opt/tabcli"
    })
}
fn reply(status: &str, mismatch: bool) -> Value {
    let command = hooks::hook_command(bridge()).unwrap();
    json!({"data":[{"hooks":(["user_prompt_submit", "stop"].map(|label| json!({
        "key":hooks::session_flags_state_key(label),"trustStatus":status,
        "currentHash":if mismatch { "sha256:changed".into() } else { hooks::trust_hash(label, &command) }
    })))}]})
}
fn respond(result: Value) -> (Result<Value>, Capture) {
    let capture = Capture(Arc::default());
    let replies = format!(
        "{}\n{}\n",
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":result})
    );
    (
        exchange(
            capture.clone(),
            Cursor::new(replies.into_bytes()),
            Path::new("workspace"),
            Instant::now() + TIMEOUT,
        ),
        capture,
    )
}

#[test]
fn hook_trust_fake_responder_trusted_untrusted_and_modified_hashes() {
    for (status, mismatch) in [("trusted", false), ("untrusted", false), ("modified", true)] {
        let (result, capture) = respond(reply(status, mismatch));
        let evidence = summarize(&result.unwrap(), bridge()).unwrap();
        for hook in evidence["hooks"].as_array().unwrap() {
            assert_eq!(hook["trustStatus"], status);
            assert_eq!(hook["hash_matches"], !mismatch);
            assert_eq!(
                hook["diagnostic"],
                if mismatch {
                    json!("normalization drift")
                } else {
                    Value::Null
                }
            );
        }
        let sent: Vec<Value> = capture
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(
            sent,
            vec![
                json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"tabcli","version":env!("CARGO_PKG_VERSION")}}}),
                json!({"method":"initialized"}),
                json!({"id":2,"method":"hooks/list","params":{"cwds":["workspace"]}}),
            ]
        );
    }
}

#[test]
fn hook_trust_fake_responder_rejects_malformed_missing_keys_and_rpc_errors() {
    for bytes in [
        b"not JSON\n".as_slice(),
        b"[]\n",
        b"{\"id\":1,\"error\":{}}\n",
        b"{\"id\":1}\n",
        b"",
    ] {
        assert!(
            exchange(
                Vec::new(),
                Cursor::new(bytes.to_vec()),
                Path::new("."),
                Instant::now() + TIMEOUT
            )
            .is_err()
        );
    }
    for result in [
        json!({}),
        json!({"data":[{"hooks":[]}]}),
        json!({"data":[{}]}),
    ] {
        let (result, _) = respond(result);
        assert!(summarize(&result.unwrap(), bridge()).is_err());
    }
}

#[test]
fn hook_trust_fake_responder_timeout_is_bounded() {
    struct Blocked(mpsc::Receiver<()>);
    impl Read for Blocked {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }
    let (release, blocked) = mpsc::channel();
    let error = exchange(Vec::new(), Blocked(blocked), Path::new("."), Instant::now()).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    drop(release);
}

#[test]
fn hook_trust_missing_executable_reports_unavailable_without_files() {
    let root = tempfile::tempdir().unwrap();
    let row = check(&Context {
        directory: Some(root.path()),
        manifest: None,
        executable: None,
        current_version: None,
        workspace: root.path(),
        probe: true,
        deadline: Instant::now(),
        environment: &crate::native::consent::Environment::default(),
    });
    assert_eq!(row.availability, Availability::Unavailable);
    assert_eq!(row.reason_code, "codex_hook_trust_unavailable");
    let value = serde_json::to_value(row).unwrap();
    assert_eq!(value["evidence"]["source"], "current_probe");
    assert!(
        value["detail"]
            .as_str()
            .unwrap()
            .contains("Codex executable unavailable")
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
