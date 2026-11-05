use super::launch_decision::{self, NoticeReason, Reason};
use crate::native::session::Store;
use crate::native::session::primitives::write_json_atomic;
use crate::native::session::schema::SessionEvent;
use std::path::Path;

pub(super) fn write_established_thread(directory: &Path, thread_id: &str) {
    std::fs::create_dir_all(directory.join("events")).unwrap();
    write_json_atomic(
        &directory.join("events/event-1.json"),
        &SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "initial result".to_owned(),
            error: None,
            provider_session_id: Some(thread_id.to_owned()),
            turn_id: Some("initial-turn".to_owned()),
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        },
    )
    .unwrap();
}

// A private session fixture shared by launch and real-shell hook tests.
pub(super) fn launch_session(root: &Path, version: Option<&str>) -> std::path::PathBuf {
    use crate::native::session::turn;
    let directory = root.join("session-hooks");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    if let Some(version) = version {
        Store::open_unchecked(&directory)
            .private("manifest.json")
            .write_json(&serde_json::json!({
                "schema": 1, "id": "session-hooks", "provider": "codex", "provider_path": "codex",
                "provider_version": version, "workspace": root, "title": "hooks fixture",
                "yolo": false, "created_unix_ms": 1
            }))
            .unwrap();
    }
    directory
}

/// A Codex launch decision that a test outside the adapter writes into a session directory. The
/// adapter owns the records' names and schema; such a test chooses only what a launch recorded.
#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct LaunchDecisionFixture(Fixture);

#[derive(Clone, Copy, Debug)]
enum Fixture {
    Lifecycle(Option<Reason>),
    Notice(Option<NoticeReason>),
    LifecycleBytes(&'static [u8]),
    NoticeBytes(&'static [u8]),
}

impl LaunchDecisionFixture {
    /// The lifecycle hooks were injected.
    pub(in crate::native) const LIFECYCLE_INJECTED: Self = Self(Fixture::Lifecycle(None));
    /// The lifecycle hooks were left out because their setting was off.
    pub(in crate::native) const LIFECYCLE_NOT_INJECTED: Self =
        Self(Fixture::Lifecycle(Some(Reason::SettingsOff)));
    /// A lifecycle record that is not JSON.
    pub(in crate::native) const LIFECYCLE_MALFORMED: Self = Self(Fixture::LifecycleBytes(b"{"));
    /// A lifecycle record that is JSON but fails the validity floor: injected, with a reason.
    pub(in crate::native) const LIFECYCLE_INVALID: Self = Self(Fixture::LifecycleBytes(
        br#"{"schema":1,"lifecycle_hooks":"injected","reason":"settings_off","codex_version":null,"vector_codex_version":"0.163.0"}"#,
    ));
    /// The question notice hook was injected.
    pub(in crate::native) const NOTICE_INJECTED: Self = Self(Fixture::Notice(None));
    /// One record for each reason a launch gives for leaving the notice hook out.
    pub(in crate::native) const NOTICE_NOT_INJECTED: [Self; 4] = [
        Self(Fixture::Notice(Some(NoticeReason::SettingsOff))),
        Self(Fixture::Notice(Some(
            NoticeReason::LifecycleHooksNotInjected,
        ))),
        Self(Fixture::Notice(Some(NoticeReason::VersionBelow0_166))),
        Self(Fixture::Notice(Some(NoticeReason::LauncherUnavailable))),
    ];
    /// Notice records the reader refuses: not JSON, an unknown schema, an unknown value, and a
    /// record measured with another hash vector version.
    pub(in crate::native) const NOTICE_UNREADABLE: [Self; 4] = [
        Self(Fixture::NoticeBytes(b"{")),
        Self(Fixture::NoticeBytes(
            br#"{"schema":2,"notice_hook":"injected","reason":null,"vector_codex_version":"0.166.0"}"#,
        )),
        Self(Fixture::NoticeBytes(
            br#"{"schema":1,"notice_hook":"maybe","reason":null,"vector_codex_version":"0.166.0"}"#,
        )),
        Self(Fixture::NoticeBytes(
            br#"{"schema":1,"notice_hook":"injected","reason":null,"vector_codex_version":"0.163.0"}"#,
        )),
    ];

    /// Replaces the record this fixture describes in `directory`; the other record is untouched.
    /// A well-formed record is written by the adapter's own writer.
    pub(in crate::native) fn write(self, directory: &Path) {
        let store = Store::open_unchecked(directory);
        match self.0 {
            Fixture::Lifecycle(reason) => {
                launch_decision::record_lifecycle(
                    &store,
                    reason,
                    Some("codex-cli 0.163.0".to_owned()),
                )
                .unwrap();
            }
            Fixture::Notice(reason) => launch_decision::record_notice(&store, reason).unwrap(),
            Fixture::LifecycleBytes(bytes) => replace(&store, launch_decision::FILE, bytes),
            Fixture::NoticeBytes(bytes) => replace(&store, launch_decision::NOTICE_FILE, bytes),
        }
    }
}

fn replace(store: &Store, file: &str, bytes: &[u8]) {
    let record = store.private(file);
    record.remove().unwrap();
    record.write_private(bytes).unwrap();
}
