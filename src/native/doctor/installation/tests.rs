use super::*;
use anyhow::{Context, anyhow};
use serde_json::Value;
use std::process::ExitStatus;

const EXECUTABLE_UNAVAILABLE_NEXT: &str = "Check the provider installation and PATH; existing sessions retain their recorded executable path.";
const FAILED_NEXT: &str =
    "Inspect the provider installation; a failed probe does not prove availability.";

/// A check as the report prints it, without its observation time.
fn row(check: &Check) -> Value {
    let mut row = serde_json::to_value(check).unwrap();
    let observed = row.as_object_mut().unwrap().remove("observed_unix_ms");
    assert!(observed.and_then(|value| value.as_u64()).unwrap() > 0);
    row
}

fn expected(
    id: &str,
    availability: &str,
    reason: &str,
    detail: &str,
    next_action: &str,
    evidence: Value,
) -> Value {
    json!({
        "id": id, "availability": availability, "reason_code": reason, "detail": detail,
        "next_action": next_action, "evidence": evidence,
    })
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

#[test]
fn executable_rows_follow_the_observed_path() {
    let usable = std::env::temp_dir().join("bin").join("pi");
    let (check, executable) = executable_check(Ok(Executable::Usable(usable.clone())));
    assert_eq!(
        row(&check),
        expected(
            "provider_executable",
            "available",
            "executable_found",
            "Provider executable found; presence alone does not establish runtime capability.",
            "Use --probe to read its current version.",
            json!({"path": usable}),
        )
    );
    assert_eq!(executable, Some(usable));

    let (check, executable) =
        executable_check(Ok(Executable::Relative(PathBuf::from("relative-pi"))));
    assert_eq!(
        row(&check),
        expected(
            "provider_executable",
            "unknown",
            "executable_path_unverified",
            "The recorded executable path is relative; doctor does not substitute a PATH lookup for it.",
            "Inspect the original launch configuration.",
            json!({"path": "relative-pi"}),
        )
    );
    assert_eq!(executable, None);

    let unusable = std::env::temp_dir().join("missing-pi");
    let (check, executable) = executable_check(Ok(Executable::Unusable(unusable.clone())));
    assert_eq!(
        row(&check),
        expected(
            "provider_executable",
            "unavailable",
            "executable_unavailable",
            &format!(
                "Provider executable is missing, not executable, or not absolute: {}",
                unusable.display()
            ),
            EXECUTABLE_UNAVAILABLE_NEXT,
            Value::Null,
        )
    );
    assert_eq!(executable, None);

    // A resolution failure is reported with its whole chain.
    let error = Err::<Executable, _>(anyhow!("permission denied"))
        .context("failed to canonicalize provider path /bin/pi")
        .unwrap_err();
    for (error, detail) in [
        (
            anyhow!("pi was not found on an absolute PATH entry"),
            "pi was not found on an absolute PATH entry",
        ),
        (
            error,
            "failed to canonicalize provider path /bin/pi: permission denied",
        ),
    ] {
        let (check, executable) = executable_check(Err(error));
        assert_eq!(
            row(&check),
            expected(
                "provider_executable",
                "unavailable",
                "executable_unavailable",
                detail,
                EXECUTABLE_UNAVAILABLE_NEXT,
                Value::Null,
            )
        );
        assert_eq!(executable, None);
    }
}

#[test]
fn version_rows_follow_the_probe_outcome() {
    let read = |text: &str, support| {
        version_check(
            FirstPartyCli::Agy,
            VersionProbe::Completed(VersionReport::Read {
                text: text.to_owned(),
                support,
            }),
        )
    };
    for ((check, version), availability, reason, text) in [
        (
            read("agy 1.1.12", VersionSupport::Supported),
            "available",
            "version_supported",
            "agy 1.1.12",
        ),
        (
            read("agy 1.1.11", VersionSupport::TooOld),
            "unavailable",
            "version_unsupported",
            "agy 1.1.11",
        ),
        (
            read(
                "development build",
                VersionSupport::Unrecognized(anyhow!("unparsed")),
            ),
            "unknown",
            "version_unrecognized",
            "development build",
        ),
        (
            read("", VersionSupport::Unrecognized(anyhow!("unparsed"))),
            "unknown",
            "version_unrecognized",
            "",
        ),
    ] {
        assert_eq!(
            row(&check),
            expected(
                "provider_version",
                availability,
                reason,
                "Current CLI version; required minimum 1.1.12",
                "CLI version is only one gate; inspect the provider-specific checks.",
                json!({"current_version": text}),
            ),
            "{text}"
        );
        assert_eq!(version.as_deref(), Some(text));
    }

    for (probe, detail) in [
        (
            VersionProbe::Completed(VersionReport::Exited(exit_status(3))),
            format!("Version probe exited with {}", exit_status(3)),
        ),
        (
            VersionProbe::Failed(anyhow!("diagnostic probe output limit exceeded")),
            "diagnostic probe output limit exceeded".to_owned(),
        ),
        (
            VersionProbe::Failed(anyhow!("diagnostic probe deadline exhausted")),
            "diagnostic probe deadline exhausted".to_owned(),
        ),
    ] {
        let (check, version) = version_check(FirstPartyCli::Agy, probe);
        assert_eq!(
            row(&check),
            expected(
                "provider_version",
                "unknown",
                "version_probe_failed",
                &detail,
                FAILED_NEXT,
                Value::Null,
            )
        );
        assert_eq!(version, None);
    }

    let (check, version) = version_check(FirstPartyCli::Pi, VersionProbe::NoExecutable);
    assert_eq!(
        row(&check),
        expected(
            "provider_version",
            "unknown",
            "executable_unavailable",
            "No unambiguous executable is available for a version probe.",
            "Inspect the provider_executable check.",
            Value::Null,
        )
    );
    assert_eq!(version, None);

    for (provider, minimum) in [
        (FirstPartyCli::Pi, "0.84.1"),
        (FirstPartyCli::Claude, "2.1.302"),
    ] {
        let (check, version) = version_check(provider, VersionProbe::NotRequested);
        assert_eq!(
            row(&check),
            expected(
                "provider_version",
                "unknown",
                "probe_not_requested",
                "Current CLI version was not queried. A launch-recorded version is historical evidence only.",
                "Add --probe for bounded local version checks; no model or message is sent.",
                json!({"minimum_version": minimum}),
            )
        );
        assert_eq!(version, None);
    }
}

#[test]
fn workspace_rows_follow_the_observed_kind() {
    let workspace = std::env::temp_dir().join("workspace");
    for (observed, availability, reason) in [
        (Ok(true), "available", "workspace_present"),
        (Ok(false), "unavailable", "workspace_not_directory"),
        (
            Err(std::io::ErrorKind::NotFound.into()),
            "unavailable",
            "workspace_missing",
        ),
        (
            Err(std::io::ErrorKind::PermissionDenied.into()),
            "unknown",
            "workspace_unreadable",
        ),
        (
            Err(std::io::ErrorKind::NotADirectory.into()),
            "unknown",
            "workspace_unreadable",
        ),
    ] {
        assert_eq!(
            row(&workspace_check(&workspace, observed)),
            expected(
                "workspace",
                availability,
                reason,
                "Working directory observation; CLI version checks do not depend on this directory.",
                "Restore or inspect the recorded directory before workspace-relative operations.",
                json!({"path": workspace}),
            ),
            "{reason}"
        );
    }
}
