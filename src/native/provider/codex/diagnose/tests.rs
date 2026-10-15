#[cfg(unix)]
use super::super::test_support::write_established_thread;
use super::*;
#[cfg(unix)]
use std::time::Instant;

#[test]
fn diagnostics_keep_launch_version_and_unprobed_daemon_observations_explicit() {
    use crate::native::doctor::{Availability, Context};
    for (version, expected) in [
        (Some("0.148.0"), Availability::Unavailable),
        (Some("0.153.2"), Availability::Available),
        (Some("unknown"), Availability::Unknown),
        (None, Availability::Unknown),
    ] {
        let checks = super::diagnose_codex(Context {
            directory: None,
            manifest: None,
            executable: None,
            current_version: version,
            workspace: std::path::Path::new("."),
            probe: false,
            deadline: std::time::Instant::now(),
        });
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_queue_version")
                .unwrap()
                .availability,
            expected
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_daemon")
                .unwrap()
                .availability,
            Availability::Unknown
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_thread")
                .unwrap()
                .reason_code,
            "session_required"
        );
        let mcp = checks.iter().find(|c| c.id == "codex_mcp").unwrap();
        assert_eq!(mcp.availability, Availability::Unknown);
        assert_eq!(mcp.reason_code, "codex_mcp_not_observed");
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_terminal_follow_up")
                .unwrap()
                .availability,
            Availability::Unavailable
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn diagnostic_daemon_availability_describes_the_optional_shared_server() {
    use crate::native::doctor::Availability::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    for (success, payload, expected) in [
        (
            true,
            r#"{"status":"running","appServerVersion":"0.153.2"}"#,
            Available,
        ),
        (true, r#"{"status":"stopped"}"#, Unavailable),
        (
            true,
            r#"{"status":"running","appServerVersion":"0.148.0"}"#,
            Unavailable,
        ),
        (true, r#"{"status":"running"}"#, Unknown),
        (true, "invalid json", Unknown),
        (false, "", Unavailable),
    ] {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: payload.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        let check = super::diagnose_daemon_output(&output, "0.153.2");
        assert_eq!(check.availability, expected, "{payload}");
        let reported = serde_json::to_value(&check).unwrap();
        assert!(
            reported["next_action"]
                .as_str()
                .unwrap()
                .contains("a shared daemon is not required")
        );
        assert_eq!(
            check.availability == Available,
            super::classify_codex_daemon_probe(
                success,
                &output.stdout,
                &output.stderr,
                false,
                "0.153.2"
            )
            .is_ok()
        );
    }
}

#[cfg(unix)]
#[test]
fn successful_model_result_does_not_mark_mcp_connected() {
    use crate::native::doctor::{Availability, Context};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
    let checks = diagnose_codex(Context {
        directory: Some(&directory),
        manifest: None,
        executable: None,
        current_version: Some("0.159.3"),
        workspace: root.path(),
        probe: false,
        deadline: Instant::now(),
    });
    assert_eq!(
        checks
            .iter()
            .find(|c| c.id == "codex_thread")
            .unwrap()
            .availability,
        Availability::Available
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c.id == "codex_mcp")
            .unwrap()
            .availability,
        Availability::Unknown
    );
}

#[test]
fn codex_daemon_probe_requires_a_running_compatible_app_server() {
    assert!(
        classify_codex_daemon_probe(
            true,
            br#"{"status":"running","appServerVersion":"0.149.0"}"#,
            b"",
            false,
            "codex-cli 0.153.2",
        )
        .is_ok()
    );

    for (success, stdout, stderr, truncated) in [
        (false, b"".as_slice(), b"socket missing".as_slice(), false),
        (
            true,
            br#"{"status":"running","appServerVersion":"0.153.2"}"#.as_slice(),
            b"".as_slice(),
            true,
        ),
        (true, b"not json".as_slice(), b"".as_slice(), false),
        (
            true,
            br#"{"status":"stopped","appServerVersion":"0.153.2"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
        (
            true,
            br#"{"status":"running"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
        (
            true,
            br#"{"status":"running","appServerVersion":"0.148.9"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
    ] {
        assert!(
            classify_codex_daemon_probe(success, stdout, stderr, truncated, "codex-cli 0.153.2",)
                .is_err()
        );
    }
}
