use super::super::tests::REOPEN_TEST_CONVERSATION;
use super::*;

#[test]
fn registry_entry_with_live_pid_and_matching_start_is_the_writer_and_stale_entries_are_not() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let entry = |pid: u32, session: &str, proc_start: serde_json::Value| {
        serde_json::json!({
            "pid": pid,
            "sessionId": session,
            "cwd": "D:\\Dev\\project",
            "startedAt": 1_790_231_893_451_u64,
            "procStart": proc_start,
            "version": "2.1.281",
            "name": "agent-bridge-a0",
            "status": "idle",
        })
        .to_string()
    };
    std::fs::write(
        registry.join("4242.json"),
        entry(4242, REOPEN_TEST_CONVERSATION, "134347054929920616".into()),
    )
    .unwrap();
    // The secret beside a registry entry is never opened: a directory in its place would
    // make any read attempt fail loudly.
    std::fs::create_dir(registry.join("4242.94cd6ac7033609ded4e5043f998182dc.key")).unwrap();
    std::fs::write(
        registry.join("4243.json"),
        entry(4243, "5e58ec26-0000-4000-8000-000000000000", "1".into()),
    )
    .unwrap();
    std::fs::write(registry.join("garbage.json"), "not json").unwrap();

    let live_and_matching = |pid: u32| Ok((pid == 4242).then_some(134_347_054_929_920_616_u64));
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live_and_matching).unwrap(),
        Some(4242)
    );
    assert_eq!(
        live_conversation_writer(
            registry,
            "5e58ec26-0000-4000-8000-000000000000",
            live_and_matching
        )
        .unwrap(),
        None
    );
    // A reused pid whose live creation time differs from procStart is not a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(Some(1))).unwrap(),
        None
    );
    // A dead pid is not a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(None)).unwrap(),
        None
    );
    // A live pid that cannot be inspected refuses instead of passing as stale.
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| {
        Err(anyhow::anyhow!("access denied"))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("4242"), "{error:#}");
    assert!(format!("{error:#}").contains("access denied"), "{error:#}");
    // No registry means no writer.
    assert_eq!(
        live_conversation_writer(&registry.join("missing"), REOPEN_TEST_CONVERSATION, |_| {
            Ok(Some(1))
        })
        .unwrap(),
        None
    );
    // A live pid without a verifiable procStart cannot be cleared.
    std::fs::write(
        registry.join("4245.json"),
        serde_json::json!({"pid": 4245, "sessionId": REOPEN_TEST_CONVERSATION}).to_string(),
    )
    .unwrap();
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |pid| {
        Ok((pid == 4245).then_some(7))
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("procStart is missing"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("live process 4245"),
        "{error:#}"
    );
}

// A record that deserializes is still judged by its file-name pid when its identity
// fields do not describe that file: the payload pid, the sessionId, and procStart are
// validated before the record is compared against any conversation.
#[test]
fn structurally_valid_records_with_inconsistent_identities_fail_closed_by_file_pid() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let fixtures: [(&str, serde_json::Value, &str); 8] = [
        (
            "8001.json",
            serde_json::json!({"pid": 8999, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "8001"}),
            "payload pid 8999 does not match the file name pid 8001",
        ),
        (
            "8002.json",
            serde_json::json!({"pid": 8002, "sessionId": "", "procStart": "8002"}),
            "sessionId \"\" is not a Claude session UUID",
        ),
        (
            "8003.json",
            serde_json::json!({"pid": 8003, "sessionId": "not-a-uuid", "procStart": "8003"}),
            "sessionId \"not-a-uuid\" is not a Claude session UUID",
        ),
        (
            "8004.json",
            serde_json::json!({"pid": 8004, "sessionId": REOPEN_TEST_CONVERSATION}),
            "procStart is missing",
        ),
        (
            "8005.json",
            serde_json::json!({"pid": 8005, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "later"}),
            "procStart \"later\" is not a process creation FILETIME",
        ),
        (
            "8006.json",
            serde_json::json!({"pid": 8006, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": []}),
            "procStart [] is not a process creation FILETIME",
        ),
        (
            "8007.json",
            serde_json::json!({"pid": 8007, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": -1}),
            "procStart -1 is not a process creation FILETIME",
        ),
        (
            "8008.json",
            serde_json::json!({"pid": 8008, "sessionId": "5e58ec26-0000-4000-8000-00000000000", "procStart": "8008"}),
            "is not a Claude session UUID",
        ),
    ];
    for (name, record, _) in &fixtures {
        std::fs::write(registry.join(name), record.to_string()).unwrap();
    }
    // Every inconsistent record names a dead file pid: none is a writer, for this
    // conversation or any other.
    for conversation in [
        REOPEN_TEST_CONVERSATION,
        "5e58ec26-0000-4000-8000-000000000000",
    ] {
        assert_eq!(
            live_conversation_writer(registry, conversation, |_| Ok(None)).unwrap(),
            None
        );
    }
    for (name, _, defect) in &fixtures {
        let file_pid: u32 = name.strip_suffix(".json").unwrap().parse().unwrap();
        // A live file pid refuses and names the record and its defect, whatever
        // conversation is asked and whatever the payload pid says.
        for conversation in [
            REOPEN_TEST_CONVERSATION,
            "5e58ec26-0000-4000-8000-000000000000",
        ] {
            let error = live_conversation_writer(registry, conversation, |candidate| {
                Ok((candidate == file_pid).then_some(u64::from(candidate)))
            })
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("reopen unsupported"), "{name}: {text}");
            assert!(text.contains(name), "{name}: {text}");
            assert!(text.contains(defect), "{name}: {text}");
            assert!(
                text.contains(&format!("live process {file_pid}")),
                "{name}: {text}"
            );
        }
        // An uninspectable file pid refuses as well.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            if candidate == file_pid {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(name), "{name}: {text}");
        assert!(text.contains(defect), "{name}: {text}");
        assert!(text.contains("access denied"), "{name}: {text}");
    }
    // The reviewer's case in full: `<livePID>.json` whose payload names a dead pid is
    // refused by the live file pid, never cleared by the dead payload pid.
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
        Ok((candidate == 8001).then_some(1))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("8001.json"), "{error:#}");
    // A consistent record beside them is still recognized as the writer.
    std::fs::write(
        registry.join("8100.json"),
        serde_json::json!({"pid": 8100, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "8100"}).to_string(),
    )
    .unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            Ok((candidate == 8100).then_some(8100))
        })
        .unwrap(),
        Some(8100)
    );
    assert_eq!(
        validated_registry_record("8100", "{\"pid\": 8100, \"sessionId\": \"6928CA1C-1234-4ABC-8DEF-0123456789AB\", \"procStart\": 42, \"name\": \"x\"}")
            .unwrap()
            .proc_start,
        42
    );
}

// Records that cannot be parsed or lack a required field are judged by the pid in their
// file name: only a positively dead pid clears them.
#[test]
fn malformed_registry_records_fail_closed_unless_their_file_pid_is_dead() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let malformed: [(&str, &str); 4] = [
        ("5001.json", "{\"pid\": 5001, \"sessionId\": \"6928ca1c-12"),
        ("5002.json", "{\"pid\": 5002}"),
        (
            "5003.json",
            "{\"pid\": \"5003\", \"sessionId\": 7, \"procStart\": []}",
        ),
        ("5004.json", ""),
    ];
    for (name, text) in malformed {
        std::fs::write(registry.join(name), text).unwrap();
    }
    // Every malformed record names a dead pid: none is a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(None)).unwrap(),
        None
    );
    for (name, _) in malformed {
        let pid: u32 = name.strip_suffix(".json").unwrap().parse().unwrap();
        // A live file pid refuses and names the record, whatever conversation is asked.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            Ok((candidate == pid).then_some(1))
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("reopen unsupported"), "{text}");
        assert!(text.contains("is malformed"), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains(&format!("live process {pid}")), "{text}");
        let error = live_conversation_writer(
            registry,
            "5e58ec26-0000-4000-8000-000000000000",
            |candidate| Ok((candidate == pid).then_some(1)),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(name), "{error:#}");
        // An uninspectable file pid refuses as well.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            if candidate == pid {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("is malformed"), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains("access denied"), "{text}");
    }
    // Non-numeric names are still not records at all: they are never judged by a pid.
    std::fs::write(registry.join("notes.json"), "").unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |pid| {
            assert!((5001..=5004).contains(&pid), "judged {pid}");
            Ok(None)
        })
        .unwrap(),
        None
    );
}

// The pre-launch scan cannot reserve the conversation: a holder that registers after
// it must be caught by the same gate when it runs again at the launch boundary.
#[test]
fn a_holder_that_registers_after_the_initial_scan_is_caught_by_the_recheck() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let live = |pid: u32| Ok((pid == 6001).then_some(77_u64));
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live).unwrap(),
        None
    );
    std::fs::write(
        registry.join("6001.json"),
        serde_json::json!({
            "pid": 6001,
            "sessionId": REOPEN_TEST_CONVERSATION,
            "procStart": "77",
            "name": "foreign-resume",
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live).unwrap(),
        Some(6001)
    );
}

#[test]
fn post_launch_check_reports_every_other_live_holder_once_the_reopened_process_registers() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let managed = "session-reopennew7";
    let write_entry = |pid: u32, session: &str, name: &str| {
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": session,
                "procStart": pid.to_string(),
                "name": name,
            })
            .to_string(),
        )
        .unwrap();
    };
    let live = |pid: u32| Ok(Some(u64::from(pid)));

    // Not registered yet: no answer, and the wait gives up at the deadline.
    assert_eq!(
        other_resumed_conversation_holders_once(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        None
    );
    let error = wait_for_resumed_conversation_holders(
        registry,
        REOPEN_TEST_CONVERSATION,
        managed,
        live,
        Instant::now() + Duration::from_millis(150),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not register"),
        "{error:#}"
    );

    // Registered alone, beside a stale entry of the same conversation and a live entry
    // of another conversation: exclusive.
    write_entry(7001, REOPEN_TEST_CONVERSATION, managed);
    write_entry(7002, "5e58ec26-0000-4000-8000-000000000000", "other-work");
    std::fs::write(
        registry.join("7003.json"),
        serde_json::json!({
            "pid": 7003,
            "sessionId": REOPEN_TEST_CONVERSATION,
            "procStart": "1",
            "name": "stale-closed-source",
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        wait_for_resumed_conversation_holders(
            registry,
            REOPEN_TEST_CONVERSATION,
            managed,
            live,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap(),
        Vec::<u32>::new()
    );

    // A foreign live resume of the same conversation is a detected conflict.
    write_entry(7004, REOPEN_TEST_CONVERSATION, "foreign-resume");
    assert_eq!(
        wait_for_resumed_conversation_holders(
            registry,
            REOPEN_TEST_CONVERSATION,
            managed,
            live,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap(),
        vec![7004]
    );
    // A live holder that could not be inspected refuses instead of passing.
    let error = wait_for_resumed_conversation_holders(
        registry,
        REOPEN_TEST_CONVERSATION,
        managed,
        |pid| {
            if pid == 7004 {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(Some(u64::from(pid)))
            }
        },
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("7004"), "{error:#}");

    // The delivery re-scan answers from the registry as it is now: a holder that
    // registered after the post-launch scan is reported, and it never waits.
    std::fs::remove_file(registry.join("7004.json")).unwrap();
    assert_eq!(
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        Vec::<u32>::new()
    );
    write_entry(7006, REOPEN_TEST_CONVERSATION, "late-foreign-resume");
    assert_eq!(
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        vec![7006]
    );
    std::fs::remove_file(registry.join("7006.json")).unwrap();
    // A managed registration that disappeared is a failure of the re-scan, not a clean
    // answer: the reopened process can no longer be shown to hold the conversation.
    std::fs::remove_file(registry.join("7001.json")).unwrap();
    let error =
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("is no longer registered"),
        "{error:#}"
    );
    write_entry(7001, REOPEN_TEST_CONVERSATION, managed);

    // Two live processes under the managed name cannot be told apart.
    write_entry(7005, REOPEN_TEST_CONVERSATION, managed);
    let error =
        other_resumed_conversation_holders_once(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("2 live processes"),
        "{error:#}"
    );
}

#[cfg(windows)]
#[test]
fn windows_creation_time_check_distinguishes_the_live_process_from_a_stale_entry() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let pid = std::process::id();
    let live = live_process_creation_time(pid).unwrap().unwrap();
    let write_entry = |pid: u32, proc_start: u64| {
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": REOPEN_TEST_CONVERSATION,
                "procStart": proc_start.to_string(),
            })
            .to_string(),
        )
        .unwrap();
    };
    write_entry(pid, live);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        Some(pid)
    );
    write_entry(pid, live - 1);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        None
    );
    std::fs::remove_file(registry.join(format!("{pid}.json"))).unwrap();
    write_entry(4_294_967_294, live);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        None
    );
}

#[cfg(windows)]
#[test]
fn c9_windows_absence_is_stale_but_inspection_error_is_not() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(format!("{}.json", u32::MAX)),
        serde_json::json!({
            "pid": u32::MAX, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "123"
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        live_conversation_writer(
            root.path(),
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        None
    );
    assert!(
        live_conversation_writer(root.path(), REOPEN_TEST_CONVERSATION, |_| anyhow::bail!(
            "access denied"
        ))
        .is_err()
    );
}
