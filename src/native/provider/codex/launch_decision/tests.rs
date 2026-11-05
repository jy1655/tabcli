use super::*;

fn write(directory: &Path, file: &str, record: &serde_json::Value) {
    Store::open_unchecked(directory)
        .private(file)
        .write_json(record)
        .unwrap();
}

fn write_bytes(directory: &Path, file: &str, bytes: &[u8]) {
    let record = Store::open_unchecked(directory).private(file);
    record.remove().unwrap();
    record.write_private(bytes).unwrap();
}

fn error<T: std::fmt::Debug>(decision: Decision<T>) -> String {
    match decision {
        Decision::Unreadable(error) => format!("{error:#}"),
        other => panic!("expected an unreadable launch decision, found {other:?}"),
    }
}

#[test]
fn notice_mode_reader_rejects_invalid_schema_values_and_inconsistent_reason() {
    let temp = tempfile::tempdir().unwrap();
    // A session launched by an earlier binary has no record: no notice hook was injected.
    assert!(notice(temp.path()).is_absent());
    assert!(!notice(temp.path()).notice_hook_injected().unwrap());
    let valid = serde_json::json!({"schema":1,"notice_hook":"injected","reason":null,
        "vector_codex_version":"0.166.0"});
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("notice_hook", serde_json::json!("maybe")),
        ("reason", serde_json::json!("settings_off")),
        ("reason", serde_json::json!("version_below_0_163")),
        ("vector_codex_version", serde_json::json!("0.163.0")),
        ("vector_codex_version", serde_json::json!(166)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        write(temp.path(), NOTICE_FILE, &invalid);
        assert!(notice(temp.path()).is_unreadable(), "{invalid}");
        assert!(
            notice(temp.path()).notice_hook_injected().is_err(),
            "{invalid}"
        );
    }
    for reason in [
        "lifecycle_hooks_not_injected",
        "version_below_0_166",
        "settings_off",
    ] {
        let off = serde_json::json!({"schema":1,"notice_hook":"not_injected","reason":reason,
            "vector_codex_version":"0.166.0"});
        write(temp.path(), NOTICE_FILE, &off);
        let read = notice(temp.path()).unwrap();
        assert_eq!(serde_json::to_value(read).unwrap(), off);
        assert!(!notice(temp.path()).notice_hook_injected().unwrap());
    }
    let off_without_reason = serde_json::json!({"schema":1,"notice_hook":"not_injected",
        "reason":null,"vector_codex_version":"0.166.0"});
    write(temp.path(), NOTICE_FILE, &off_without_reason);
    assert_eq!(
        error(notice(temp.path())),
        "Agent Bridge rejected invalid Codex question notice mode"
    );
    write(temp.path(), NOTICE_FILE, &valid);
    assert!(notice(temp.path()).notice_hook_injected().unwrap());
    std::fs::write(temp.path().join(NOTICE_FILE), b"{").unwrap();
    assert!(
        error(notice(temp.path())).starts_with("failed to parse Codex question notice mode: "),
        "the parse failure keeps its text"
    );
}

#[test]
fn launch_mode_reader_rejects_invalid_schema_values_and_inconsistent_reason() {
    let temp = tempfile::tempdir().unwrap();
    assert!(lifecycle(temp.path()).is_absent());
    assert!(!lifecycle(temp.path()).lifecycle_hooks_injected().unwrap());
    let valid = serde_json::json!({"schema":1,"lifecycle_hooks":"injected","reason":null,"codex_version":"0.163.0","vector_codex_version":"0.163.0"});
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("lifecycle_hooks", serde_json::json!("maybe")),
        ("reason", serde_json::json!("settings_off")),
        ("reason", serde_json::json!("prerelease")),
        ("vector_codex_version", serde_json::json!("other")),
        ("codex_version", serde_json::json!(3)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        write(temp.path(), FILE, &invalid);
        assert!(lifecycle(temp.path()).is_unreadable(), "{invalid}");
        assert!(
            lifecycle(temp.path()).lifecycle_hooks_injected().is_err(),
            "{invalid}"
        );
    }
    write(temp.path(), FILE, &valid);
    assert!(lifecycle(temp.path()).lifecycle_hooks_injected().unwrap());
    let prerelease = serde_json::json!({"schema":1,"lifecycle_hooks":"not_injected","reason":"prerelease","codex_version":"0.164.0-alpha.1","vector_codex_version":"0.163.0"});
    write(temp.path(), FILE, &prerelease);
    let read = lifecycle(temp.path()).unwrap();
    assert_eq!(read.reason, Some(Reason::Prerelease));
    assert_eq!(serde_json::to_value(read).unwrap(), prerelease);
    assert!(!lifecycle(temp.path()).lifecycle_hooks_injected().unwrap());
    let without_reason = serde_json::json!({"schema":1,"lifecycle_hooks":"not_injected","reason":null,"codex_version":null,"vector_codex_version":"0.163.0"});
    write(temp.path(), FILE, &without_reason);
    assert_eq!(
        error(lifecycle(temp.path())),
        "Agent Bridge rejected invalid Codex launch mode"
    );
    std::fs::write(temp.path().join(FILE), b"{").unwrap();
    assert!(
        error(lifecycle(temp.path())).starts_with("failed to parse Codex launch mode: "),
        "the parse failure keeps its text"
    );
}

// The one accepted change from the earlier lossy reader: a record over the bound is unreadable,
// however valid its JSON. Whitespace pads a valid record to the bound and one byte past it.
#[test]
fn both_records_are_read_within_their_bound() {
    for (file, record) in [
        (
            FILE,
            br#"{"schema":1,"lifecycle_hooks":"injected","reason":null,"codex_version":"codex-cli 0.163.0","vector_codex_version":"0.163.0"}"#.as_slice(),
        ),
        (
            NOTICE_FILE,
            br#"{"schema":1,"notice_hook":"injected","reason":null,"vector_codex_version":"0.166.0"}"#.as_slice(),
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let injected = |directory: &Path| {
            if file == FILE {
                lifecycle(directory).lifecycle_hooks_injected()
            } else {
                notice(directory).notice_hook_injected()
            }
        };
        let mut padded = record.to_vec();
        padded.resize(READ_LIMIT as usize, b' ');
        write_bytes(temp.path(), file, &padded);
        assert!(injected(temp.path()).unwrap(), "{file} at the bound");
        padded.push(b' ');
        write_bytes(temp.path(), file, &padded);
        let error = format!("{:#}", injected(temp.path()).unwrap_err());
        assert!(
            error.contains(&format!("over the {READ_LIMIT} byte read limit")),
            "{file}: {error}"
        );
    }
}

// The other accepted change: a record is strict UTF-8. The earlier reader replaced an invalid
// byte and accepted this lifecycle record, whose version string carries one.
#[test]
fn both_records_refuse_invalid_utf8() {
    let mut lifecycle_record =
        br#"{"schema":1,"lifecycle_hooks":"injected","reason":null,"codex_version":"codex-cli 0.163.0"#
            .to_vec();
    lifecycle_record.push(0xff);
    lifecycle_record.extend_from_slice(br#"","vector_codex_version":"0.163.0"}"#);
    let mut notice_record =
        br#"{"schema":1,"notice_hook":"injected","reason":null,"vector_codex_version":"0.166.0"}"#
            .to_vec();
    notice_record.extend_from_slice(b" \xff");
    let temp = tempfile::tempdir().unwrap();
    write_bytes(temp.path(), FILE, &lifecycle_record);
    write_bytes(temp.path(), NOTICE_FILE, &notice_record);
    assert!(error(lifecycle(temp.path())).starts_with("invalid UTF-8"));
    assert!(error(notice(temp.path())).starts_with("invalid UTF-8"));
    assert!(lifecycle(temp.path()).lifecycle_hooks_injected().is_err());
    assert!(notice(temp.path()).notice_hook_injected().is_err());
}

// A directory or a link at a record's path is unreadable, never absent.
#[test]
fn a_non_regular_record_is_unreadable() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(FILE)).unwrap();
    std::fs::create_dir(temp.path().join(NOTICE_FILE)).unwrap();
    assert!(error(lifecycle(temp.path())).starts_with("refusing non-regular session file"));
    assert!(error(notice(temp.path())).starts_with("refusing non-regular session file"));
}

// The writers produce exactly what the readers accept, with a reason only when not injected.
#[test]
fn the_writers_round_trip_through_the_readers() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(temp.path());
    for reason in [
        None,
        Some(Reason::VersionBelow0_163),
        Some(Reason::VersionUnknown),
        Some(Reason::Prerelease),
        Some(Reason::SettingsOff),
        Some(Reason::LauncherUnavailable),
    ] {
        record_lifecycle(&store, reason, Some("codex-cli 0.163.0".into())).unwrap();
        let read = lifecycle(temp.path()).unwrap();
        assert_eq!(read.reason, reason);
        assert_eq!(read.lifecycle_hooks_injected(), reason.is_none());
        assert_eq!(read.vector_codex_version, VECTOR_VERSION);
    }
    for reason in [
        None,
        Some(NoticeReason::SettingsOff),
        Some(NoticeReason::LifecycleHooksNotInjected),
        Some(NoticeReason::VersionBelow0_166),
        Some(NoticeReason::LauncherUnavailable),
    ] {
        record_notice(&store, reason).unwrap();
        let read = notice(temp.path()).unwrap();
        assert_eq!(read.reason, reason);
        assert_eq!(read.notice_hook_injected(), reason.is_none());
        assert_eq!(read.vector_codex_version, NOTICE_VECTOR_VERSION);
    }
}

// The lifecycle record carries the whole `codex --version` output. A launch whose Codex prints a
// long banner there must still read its own record: the bound leaves room for it.
#[test]
fn a_launch_with_a_long_version_output_reads_its_own_record() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(temp.path());
    let version = format!("{}\ncodex-cli 0.166.0", "banner line\n".repeat(700));
    assert!(version.len() > 8 * 1024);
    record_lifecycle(&store, None, Some(version.clone())).unwrap();
    let read = lifecycle(temp.path()).unwrap();
    assert!(read.lifecycle_hooks_injected());
    assert_eq!(read.codex_version.as_deref(), Some(version.as_str()));
}
