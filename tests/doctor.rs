use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
}

impl Fixture {
    fn new(provider: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-doctor");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let version = match provider {
            "codex" => "0.153.2",
            "claude" => "2.1.280",
            "agy" => "1.1.12",
            _ => "0.84.1",
        };
        write(
            &directory.join("manifest.json"),
            &json!({
                "schema": 1, "id": "session-doctor", "provider": provider,
                "provider_path": root.path().join("provider"), "provider_version": version,
                "workspace": root.path(), "title": "doctor fixture", "model": "recorded-model",
                "effort": null, "yolo": false, "created_unix_ms": 1
            }),
        );
        write(
            &directory.join("status.json"),
            &json!({"state":"ready", "generation":2,
            "updated_unix_ms":2, "exit_code":null, "error":null}),
        );
        Self { root, directory }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .output()
            .unwrap()
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, Option<&str>)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path());
        for (key, value) in env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command.output().unwrap()
    }

    fn doctor(&self) -> Value {
        report(self.run(&["doctor", "session-doctor", "--json"]))
    }

    fn claim(&self, error: Option<&str>) {
        fs::write(self.directory.join("turn.claim"), "123-456-0\n").unwrap();
        fs::create_dir(self.directory.join("requests")).unwrap();
        write(
            &self.directory.join("requests/123-456-0.json"),
            &json!({
                "schema":1, "request_id":"request-exact", "claim_token":"123-456-0",
                "event_file":"event-1.json", "created_unix_ms":3
            }),
        );
        write(
            &self.directory.join("status.json"),
            &json!({"state":"working", "generation":3,
            "updated_unix_ms":3, "exit_code":null, "error":error}),
        );
    }
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn report(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    report
}
fn check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}
fn files(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut result = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.push((path.clone(), fs::read(path).unwrap()));
        }
    }
    result.sort();
    result
}

#[test]
fn diagnostics_preserve_uncertain_delivery_and_address_the_exact_request() {
    let fixture = Fixture::new("claude");
    fixture.claim(Some("delivery could not be confirmed"));
    write(
        &fixture.directory.join("claude-settings.json"),
        &json!({"crossSessionInbound":"accept"}),
    );
    let before = files(fixture.root.path());
    let value = fixture.doctor();
    assert_eq!(check(&value, "turn")["availability"], "unknown");
    assert_eq!(check(&value, "turn")["reason_code"], "delivery_unconfirmed");
    assert_eq!(
        check(&value, "turn")["next_action"],
        "agent-bridge result session-doctor --request request-exact --json"
    );
    assert_eq!(check(&value, "claude_messaging")["availability"], "unknown");
    assert_eq!(
        check(&value, "claude_inbound_setting")["reason_code"],
        "claude_inbound_configured"
    );
    assert_eq!(value["configured"]["model"], "recorded-model");
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn active_and_recovery_states_are_diagnosed_without_repair() {
    let fixture = Fixture::new("codex");
    fixture.claim(None);
    assert_eq!(
        check(&fixture.doctor(), "turn")["reason_code"],
        "turn_in_progress"
    );
    write(
        &fixture.directory.join("turn.completion.json"),
        &json!({
            "schema":1,"claim_token":"123-456-0","event_file":"event-1.json",
            "event":{"provider":"codex","message":"not published","error":null,
                "provider_session_id":null,"turn_id":null,"created_unix_ms":4},
            "status_error":null,"status_state":"ready"
        }),
    );
    let before = files(fixture.root.path());
    let value = fixture.doctor();
    assert_eq!(
        check(&value, "completion")["reason_code"],
        "recovery_required"
    );
    assert_eq!(
        check(&value, "completion")["next_command"],
        json!([
            "agent-bridge",
            "sessions",
            "--workspace",
            fixture.root.path(),
            "--json"
        ])
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn missing_corrupt_and_locked_records_produce_unknown_without_writes() {
    let fixture = Fixture::new("claude");
    let absent = report(fixture.run(&["doctor", "session-absent", "--json"]));
    assert_eq!(
        check(&absent, "session_records")["reason_code"],
        "session_unavailable"
    );
    assert!(!fixture.root.path().join("session-absent").exists());
    let lock = fs::File::create(fixture.directory.join("turn.claim.lock")).unwrap();
    let before = files(fixture.root.path());
    lock.lock().unwrap();
    let value = fixture.doctor();
    assert_eq!(check(&value, "session_records")["availability"], "unknown");
    assert_eq!(
        check(&value, "session_records")["reason_code"],
        "records_busy"
    );
    for id in ["turn", "completion", "session_state"] {
        assert_eq!(check(&value, id)["availability"], "unknown");
    }
    // Windows locks also prevent a second handle from reading the locked bytes.
    drop(lock);
    assert_eq!(files(fixture.root.path()), before);
    fs::write(fixture.directory.join("status.json"), "not json").unwrap();
    let before = files(fixture.root.path());
    assert_eq!(
        check(&fixture.doctor(), "session_records")["reason_code"],
        "records_unreadable"
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn owner_missing_dead_or_bound_elsewhere_is_not_reported_as_live() {
    let fixture = Fixture::new("pi");
    assert_eq!(check(&fixture.doctor(), "owner")["availability"], "unknown");
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":0,"managed_session_id":"session-doctor"}),
    );
    assert_eq!(
        check(&fixture.doctor(), "owner")["reason_code"],
        "owner_exited"
    );
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":std::process::id(),"managed_session_id":"session-unrelated"}),
    );
    assert_eq!(
        check(&fixture.doctor(), "owner")["reason_code"],
        "owner_session_mismatch"
    );
    fs::write(fixture.directory.join("native-session.json"), "bad owner").unwrap();
    assert_eq!(check(&fixture.doctor(), "owner")["availability"], "unknown");
}

#[test]
fn provider_fallbacks_and_unreadable_request_index_remain_explicit() {
    for provider in ["agy", "pi"] {
        let fixture = Fixture::new(provider);
        fs::create_dir(fixture.directory.join("requests")).unwrap();
        fs::write(
            fixture.directory.join("requests/123-456-0.json"),
            "bad receipt",
        )
        .unwrap();
        let value = fixture.doctor();
        assert_eq!(
            check(&value, &format!("{provider}_follow_up"))["reason_code"],
            format!("{provider}_terminal_fallback")
        );
        assert_eq!(check(&value, "request_index")["availability"], "unknown");
    }
}

#[test]
fn doctor_requires_an_explicit_unambiguous_target() {
    let fixture = Fixture::new("codex");
    for args in [
        vec!["doctor"],
        vec!["doctor", "../session-doctor"],
        vec!["doctor", "session-doctor", "--provider", "codex"],
        vec!["doctor", "--provider", "codex", "--probe", "--probe"],
        vec!["doctor", "session-doctor", "session-other"],
        vec!["doctor", "--provider", "other"],
    ] {
        assert!(!fixture.run(&args).status.success(), "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn only_opt_in_probe_executes_local_cli_and_keeps_session_records_identical() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("codex");
    let executable = fixture.root.path().join("provider");
    // Any extra invocation (queue, login, daemon start, etc.) fails the reported check.
    fs::write(&executable, "#!/bin/sh\ncase \"$*\" in\n--version) printf 'codex-cli 0.153.2\\n';;\n'app-server daemon version') printf '{\"status\":\"running\",\"appServerVersion\":\"0.153.2\"}\\n';;\n*) exit 99;;\nesac\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    write(
        &fixture.directory.join("events/event-1.json"),
        &json!({
            "provider":"codex","message":"complete","error":null,
            "provider_session_id":"00000000-0000-0000-0000-000000000123","turn_id":"turn-1","created_unix_ms":3
        }),
    );
    let before = files(fixture.root.path());
    let passive = fixture.doctor();
    assert_eq!(
        check(&passive, "provider_version")["availability"],
        "unknown"
    );
    assert_eq!(
        check(&passive, "codex_daemon")["reason_code"],
        "probe_not_requested"
    );
    let value = report(fixture.run(&["doctor", "session-doctor", "--probe", "--json"]));
    assert_eq!(
        check(&value, "provider_version")["reason_code"],
        "version_supported"
    );
    assert_eq!(
        check(&value, "codex_daemon")["reason_code"],
        "codex_daemon_compatible"
    );
    assert_eq!(
        check(&value, "codex_thread")["reason_code"],
        "codex_thread_recorded"
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[cfg(unix)]
#[test]
fn default_never_invokes_provider_even_when_executable_would_mutate_state() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("claude");
    let executable = fixture.root.path().join("provider");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf called > \"$0.called\"\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let before = files(fixture.root.path());
    fixture.doctor();
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn provider_only_diagnostics_do_not_create_or_scan_a_session_store() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("absent-store");
    let value = report(
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(["doctor", "--provider", "claude", "--json"])
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &missing)
            .output()
            .unwrap(),
    );
    assert!(value["session"].is_null());
    assert_eq!(check(&value, "claude_messaging")["availability"], "unknown");
    assert!(!missing.exists());
}

#[cfg(unix)]
#[test]
fn probe_does_not_hold_the_completion_lifecycle_lock() {
    use std::{
        os::unix::fs::PermissionsExt,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new("claude");
    let executable = fixture.root.path().join("provider");
    fs::write(
        &executable,
        r#"#!/bin/sh
test "$1" = --version || exit 99
: > "$AB_DOCTOR_TEST_DIR/probe-started"
while test ! -f "$AB_DOCTOR_TEST_DIR/probe-release"; do sleep 0.02; done
printf '2.1.280\n'
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = fs::File::create(fixture.directory.join("turn.claim.lock")).unwrap();
    let before = files(&fixture.directory);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["doctor", "session-doctor", "--probe", "--json"])
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", fixture.root.path())
        .env("AB_DOCTOR_TEST_DIR", fixture.root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.root.path().join("probe-started").exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "probe did not reach the handshake: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    // The probe is deliberately paused: this assertion does not depend on a fast CLI.
    let acquired = lock.try_lock().is_ok();
    if acquired {
        lock.unlock().unwrap();
    }
    fs::write(fixture.root.path().join("probe-release"), b"continue").unwrap();
    let value = report(child.wait_with_output().unwrap());
    assert!(acquired, "doctor blocked completion writers while probing");
    assert_eq!(
        check(&value, "provider_version")["reason_code"],
        "version_supported"
    );
    assert_eq!(files(&fixture.directory), before);
}

#[cfg(unix)]
#[test]
fn missing_workspace_does_not_hide_the_installed_version_or_probe_another_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("codex");
    let executable = fixture.root.path().join("provider");
    fs::write(&executable, "#!/bin/sh\nif test \"$1\" = --version; then printf 'codex-cli 0.156.0\\n'; else printf called > \"$0.daemon-called\"; exit 99; fi\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let path = fixture.directory.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["workspace"] = json!(fixture.root.path().join("removed-worktree"));
    write(&path, &manifest);
    let before = files(fixture.root.path());
    let value = report(fixture.run(&["doctor", "session-doctor", "--probe", "--json"]));
    assert_eq!(
        check(&value, "provider_version")["reason_code"],
        "version_supported"
    );
    assert_eq!(check(&value, "workspace")["availability"], "unavailable");
    assert_eq!(
        check(&value, "codex_daemon")["reason_code"],
        "workspace_unavailable"
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn terminal_close_markers_are_reported_without_consuming_or_resuming_them() {
    let fixture = Fixture::new("claude");
    write(
        &fixture.directory.join("terminal.closing.json"),
        &json!({
            "terminal":"iterm2", "session_id":"owned-surface", "managed_session_id":"session-doctor"
        }),
    );
    let before = files(fixture.root.path());
    assert_eq!(
        check(&fixture.doctor(), "terminal_record")["reason_code"],
        "terminal_close_in_progress"
    );
    assert_eq!(files(fixture.root.path()), before);
    write(
        &fixture.directory.join("terminal.closed.json"),
        &json!({"consumed":true,"terminal":"iterm2"}),
    );
    let before = files(fixture.root.path());
    assert_eq!(
        check(&fixture.doctor(), "terminal_record")["reason_code"],
        "terminal_consumed"
    );
    assert_eq!(files(fixture.root.path()), before);
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":0}),
    );
    let value = fixture.doctor();
    assert_eq!(
        check(&value, "owner")["reason_code"],
        "owner_unbound_legacy"
    );
    assert_eq!(check(&value, "owner")["evidence"]["process_alive"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn legacy_iterm_handle_uses_the_existing_binding_contract() {
    let fixture = Fixture::new("claude");
    write(
        &fixture.directory.join("terminal.json"),
        &json!({"iterm_session_id":"legacy-surface"}),
    );
    assert_eq!(
        check(&fixture.doctor(), "terminal_record")["reason_code"],
        "terminal_record_found"
    );
    assert_eq!(check(&fixture.doctor(), "owner")["availability"], "unknown");
}

#[test]
fn claude_doctor_reports_inherited_claude_code_session_markers_without_changing_files() {
    let fixture = Fixture::new("claude");
    let before = files(fixture.root.path());
    // The test process itself may run inside a Claude Code session, so every marker is
    // cleared first and only the two under test are reintroduced.
    const MARKERS: [&str; 10] = [
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_EFFORT",
    ];
    let cleared = MARKERS.map(|marker| (marker, None));
    let mut inside_env = cleared.to_vec();
    inside_env.extend([
        ("CLAUDE_CODE_CHILD_SESSION", Some("1")),
        ("CLAUDECODE", Some("1")),
    ]);
    let inside = report(fixture.run_with_env(&["doctor", "session-doctor", "--json"], &inside_env));
    let markers = check(&inside, "claude_caller_markers");
    assert_eq!(markers["availability"], "available");
    assert_eq!(
        markers["reason_code"],
        "claude_caller_markers_removed_at_launch"
    );
    assert_eq!(
        markers["evidence"]["inherited"],
        json!(["CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE"])
    );

    let outside = report(fixture.run_with_env(&["doctor", "session-doctor", "--json"], &cleared));
    assert_eq!(
        check(&outside, "claude_caller_markers")["reason_code"],
        "claude_caller_markers_absent"
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn reopen_marker_is_reported_with_its_release_condition_without_writes() {
    let fixture = Fixture::new("claude");
    let refused = fixture.root.path().join("session-reopened");
    fs::create_dir(&refused).unwrap();
    write(
        &refused.join("status.json"),
        &json!({"state":"failed", "generation":3, "updated_unix_ms":3, "exit_code":null,
            "error":"reopen refused (reopen-conflict): held by pid 4242"}),
    );
    let marker = |reopened_by: Value| {
        write(
            &fixture.directory.join("reopen.marker.json"),
            &json!({"schema":1, "claim":"1-2-3",
                "provider_session_id":"6928ca1c-1234-4abc-8def-0123456789ab",
                "reopened_by":reopened_by, "created_unix_ms":1}),
        );
    };
    let refusal = |gate: &str, cleanup: Option<&str>| {
        write(
            &refused.join("reopen.refusal.json"),
            &json!({"schema":2, "phase":"launch", "gate":gate,
                "detail":"detected; no prompt was delivered", "created_unix_ms":2,
                "cleanup":cleanup, "cleanup_detail":cleanup.map(|_| "no owner record")}),
        );
    };
    let observe = |reason: &str, availability: &str| -> Value {
        let before = files(fixture.root.path());
        let value = fixture.doctor();
        assert_eq!(files(fixture.root.path()), before, "{reason}");
        let check = check(&value, "reopen_marker");
        assert_eq!(check["reason_code"], reason, "{check}");
        assert_eq!(check["availability"], availability, "{check}");
        check.clone()
    };
    assert!(
        fixture.doctor()["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["id"] != "reopen_marker"),
        "a session without a marker has no marker check"
    );
    marker(Value::Null);
    observe("reopen_in_progress", "unavailable");
    marker(json!("session-reopened"));
    let consumed = observe("reopen_marker_consumed", "unavailable");
    assert_eq!(consumed["evidence"]["reopened_by"], "session-reopened");
    assert_eq!(consumed["evidence"]["gate"], Value::Null);
    refusal("reopen-conflict", Some("pending"));
    let retained = observe("reopen_marker_retained", "unavailable");
    assert_eq!(retained["evidence"]["gate"], "reopen-conflict");
    assert_eq!(retained["evidence"]["recorded_cleanup"], "pending");
    assert!(
        retained["evidence"]["cleanup"]
            .as_str()
            .unwrap()
            .contains("no provider process record"),
        "{retained}"
    );
    assert!(
        retained["detail"]
            .as_str()
            .unwrap()
            .contains("until the provider process of session-reopened is verified gone"),
        "{retained}"
    );
    // A dead launch wrapper and a closed surface are not evidence while the recorded
    // provider process (this test process stands in for it) is still running.
    write(
        &refused.join("native-session.json"),
        &json!({"pid":0, "managed_session_id":"session-reopened"}),
    );
    write(
        &refused.join("status.json"),
        &json!({"state":"closed", "generation":4, "updated_unix_ms":4, "exit_code":null,
            "error":"reopen refused (reopen-conflict): held by pid 4242"}),
    );
    write(
        &refused.join("closed.json"),
        &json!({"state":"closed", "generation":4, "updated_unix_ms":4, "exit_code":null,
            "error":null}),
    );
    write(
        &refused.join("terminal.closed.json"),
        &json!({"consumed":true}),
    );
    let provider_record = |pid: u32| {
        write(
            &refused.join("provider-process.json"),
            &json!({"schema":1, "managed_session_id":"session-reopened", "pid":pid,
                "spawned_unix_ms":2}),
        );
    };
    provider_record(std::process::id());
    let retained = observe("reopen_marker_retained", "unavailable");
    assert_eq!(
        retained["evidence"]["cleanup"],
        format!(
            "the refused launch may still hold the conversation: provider process {} of refused session session-reopened is still running",
            std::process::id()
        )
    );
    provider_record(0);
    let reconcilable = observe("reopen_marker_reconcilable", "available");
    assert_eq!(
        reconcilable["evidence"]["cleanup"],
        "provider process 0 is verified gone (it has exited) and the refused session's surface was closed"
    );
    fs::remove_file(refused.join("provider-process.json")).unwrap();
    fs::remove_file(refused.join("native-session.json")).unwrap();
    refusal("provider-unsupported", None);
    let reconcilable = observe("reopen_marker_reconcilable", "available");
    assert_eq!(
        reconcilable["evidence"]["cleanup"],
        "no provider process was spawned"
    );
    assert_eq!(reconcilable["evidence"]["recorded_cleanup"], Value::Null);
    fs::write(fixture.directory.join("reopen.marker.json"), "not json").unwrap();
    observe("reopen_marker_unreadable", "unknown");
}

// A probe is a bridge-run provider process like any other: the adapter's removal list is
// applied to it, so a `claude --version` started from inside Claude Code does not carry
// the caller's session markers, while a Codex probe (empty list) inherits its caller's
// environment unchanged (Codex review of PR #44).
#[cfg(unix)]
#[test]
fn version_probes_drop_the_adapters_environment_removals() {
    use std::os::unix::fs::PermissionsExt;
    for (provider, expected_version, expected_reason) in [
        ("claude", "2.1.281 (Claude Code)", "version_supported"),
        (
            "codex",
            "nested CLAUDE_CODE_CHILD_SESSION=1",
            "version_unrecognized",
        ),
    ] {
        let fixture = Fixture::new(provider);
        let executable = fixture.root.path().join("provider");
        fs::write(
            &executable,
            "#!/bin/sh\nif [ -n \"$CLAUDE_CODE_CHILD_SESSION\" ]; then echo \"nested CLAUDE_CODE_CHILD_SESSION=$CLAUDE_CODE_CHILD_SESSION\"; else echo '2.1.281 (Claude Code)'; fi\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let report = report(fixture.run_with_env(
            &["doctor", "session-doctor", "--probe", "--json"],
            &[("CLAUDE_CODE_CHILD_SESSION", Some("1"))],
        ));
        let version = check(&report, "provider_version");
        assert_eq!(
            version["evidence"]["current_version"], expected_version,
            "{provider}: {version}"
        );
        assert_eq!(
            version["reason_code"], expected_reason,
            "{provider}: {version}"
        );
    }
}
