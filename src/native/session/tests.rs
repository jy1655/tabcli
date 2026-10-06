use super::*;

thread_local! {
    pub(in crate::native) static FIXED_UNIX_MS: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) };
}

fn with_fixed_time<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(Option<u128>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIXED_UNIX_MS.set(self.0);
        }
    }
    let _reset = Reset(FIXED_UNIX_MS.replace(Some(123456789)));
    f()
}

#[test]
fn converge_publishes_completion_before_repairing_dead_owner() {
    with_fixed_time(|| {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        fs::create_dir(store.record(CoreRecord::Events).path()).unwrap();
        store
            .write_status(&SessionStatus {
                state: SessionState::Working,
                generation: 7,
                updated_unix_ms: 100,
                exit_code: None,
                error: None,
            })
            .unwrap();
        store
            .write_owner(&NativeSessionOwner {
                pid: 0,
                ..Default::default()
            })
            .unwrap();
        store
            .record(CoreRecord::TurnClaim)
            .write_private(b"1-2-3\n")
            .unwrap();
        store
            .write_completion(&PendingTurnCompletion {
                schema: 1,
                claim_token: "1-2-3".to_owned(),
                event_file: "event-fixture.json".to_owned(),
                event: SessionEvent {
                    provider: "codex".to_owned(),
                    message: "completed before owner exit".to_owned(),
                    error: None,
                    provider_session_id: Some("provider-session".to_owned()),
                    turn_id: Some("provider-turn".to_owned()),
                    created_unix_ms: Some(99),
                },
                status_error: None,
                status_state: SessionState::Ready,
            })
            .unwrap();

        store.converge().unwrap();

        // Captured by running the old recover-then-repair pair before introducing converge.
        for (name, expected) in [
            (
                "status.json",
                include_bytes!("fixtures/converge/status.json").as_slice(),
            ),
            (
                "closed.json",
                include_bytes!("fixtures/converge/closed.json").as_slice(),
            ),
            (
                "events/event-fixture.json",
                include_bytes!("fixtures/converge/events/event-fixture.json").as_slice(),
            ),
        ] {
            assert_eq!(
                fs::read(directory.path().join(name)).unwrap(),
                expected,
                "{name}"
            );
        }
        assert_eq!(store.events().unwrap().len(), 1);
        assert!(!store.record(CoreRecord::TurnClaim).path().exists());
        assert!(!store.record(CoreRecord::Completion).path().exists());
    });
}
