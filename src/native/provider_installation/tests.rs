use super::*;
use std::fs;

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

fn output(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: exit_status(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// A report as [outcome, text, verdict or error].
fn summary(report: VersionReport) -> (String, Option<String>, String) {
    match report {
        VersionReport::Exited(status) => (format!("exited: {status}"), None, String::new()),
        VersionReport::Read { text, support } => (
            "read".to_owned(),
            Some(text),
            match support {
                VersionSupport::Supported => "supported".to_owned(),
                VersionSupport::TooOld => "too old".to_owned(),
                VersionSupport::Unrecognized(error) => format!("unrecognized: {error:#}"),
            },
        ),
    }
}

#[test]
fn a_version_probe_is_read_only_after_a_successful_exit() {
    let exited = summary(read_version_probe(
        FirstPartyCli::Agy,
        &output(7, "agy 1.1.12", "failed"),
    ));
    assert_eq!(exited.0, format!("exited: {}", exit_status(7)));
    assert_eq!(exited.1, None);
    for (stdout, stderr, text, verdict) in [
        ("agy 1.1.12", "agy 1.1.11", "agy 1.1.12", "supported"),
        (" \t\n", "agy 1.1.12", "agy 1.1.12", "supported"),
        ("agy 1.1.11", "agy 1.1.12", "agy 1.1.11", "too old"),
        (
            "development build",
            "agy 1.1.12",
            "development build",
            "unrecognized: could not parse a semantic version from \"development build\"",
        ),
        (
            " \t\n",
            " \r\n",
            "",
            "unrecognized: could not parse a semantic version from \"\"",
        ),
    ] {
        assert_eq!(
            summary(read_version_probe(
                FirstPartyCli::Agy,
                &output(0, stdout, stderr)
            )),
            ("read".to_owned(), Some(text.to_owned()), verdict.to_owned()),
            "{stdout:?} {stderr:?}"
        );
    }
}

#[test]
fn the_support_verdict_follows_each_providers_minimum() {
    for (provider, text, verdict) in [
        (FirstPartyCli::Claude, "2.1.301 (Claude Code)", "too old"),
        (FirstPartyCli::Claude, "2.1.309-beta.1", "too old"),
        (FirstPartyCli::Claude, "2.1.302 (Claude Code)", "supported"),
        (FirstPartyCli::Codex, "codex-cli 0.147.0", "supported"),
        (FirstPartyCli::Pi, "0.84.0", "too old"),
    ] {
        let actual = match version_support(provider, text) {
            VersionSupport::Supported => "supported",
            VersionSupport::TooOld => "too old",
            VersionSupport::Unrecognized(_) => "unrecognized",
        };
        assert_eq!(actual, verdict, "{provider:?} {text}");
    }
}

#[test]
fn an_executable_is_observed_against_the_validity_test() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("provider");
    fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Executable::observe(file.clone()),
            Executable::Unusable(file.clone())
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(
        Executable::observe(file.clone()),
        Executable::Usable(file.clone())
    );
    let missing = root.path().join("missing");
    assert_eq!(
        Executable::observe(missing.clone()),
        Executable::Unusable(missing)
    );
    let directory = root.path().to_path_buf();
    assert_eq!(
        Executable::observe(directory.clone()),
        Executable::Unusable(directory)
    );
    assert_eq!(
        Executable::observe(PathBuf::from("provider")),
        Executable::Relative(PathBuf::from("provider"))
    );
}
