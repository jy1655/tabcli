use super::records::{
    self, Deflection, LaunchMode, MODE_FILE, Mode, QUESTION_MODE_FILE, QuestionMode,
};
use crate::native::session::Store;
use std::path::Path;

/// A Claude completion record that a test outside the completion module writes into a session
/// directory. The module owns the records' names and schemas; such a test chooses only what a
/// launch recorded.
#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct CompletionRecordFixture(Fixture);

#[derive(Clone, Copy, Debug)]
enum Fixture {
    NativeMode,
    LegacyStopMode,
    ModeBytes(&'static [u8]),
    Question(Deflection),
    QuestionBytes(&'static [u8]),
    NoQuestionMode,
}

impl CompletionRecordFixture {
    /// The native completion record a launch on this platform writes.
    pub(in crate::native) const NATIVE_MODE: Self = Self(Fixture::NativeMode);
    /// A classic Stop completion record.
    pub(in crate::native) const LEGACY_STOP_MODE: Self = Self(Fixture::LegacyStopMode);
    /// A completion-mode record that is not JSON.
    pub(in crate::native) const MALFORMED_MODE: Self = Self(Fixture::ModeBytes(b"{"));
    /// The launch recorded question deflection on.
    pub(in crate::native) const DEFLECTION_ON: Self = Self(Fixture::Question(Deflection::On));
    /// The launch recorded question deflection off.
    pub(in crate::native) const DEFLECTION_OFF: Self = Self(Fixture::Question(Deflection::Off));
    /// No question-mode record: a launch before the record existed.
    pub(in crate::native) const NO_QUESTION_MODE: Self = Self(Fixture::NoQuestionMode);
    /// Question-mode records the reader refuses: not JSON, an unknown schema, an unknown value.
    pub(in crate::native) const UNREADABLE_QUESTION_MODES: [Self; 3] = [
        Self(Fixture::QuestionBytes(b"{")),
        Self(Fixture::QuestionBytes(br#"{"schema":2,"deflection":"on"}"#)),
        Self(Fixture::QuestionBytes(
            br#"{"schema":1,"deflection":"maybe"}"#,
        )),
    ];

    /// Replaces the record this fixture describes in `directory`; other records are untouched.
    /// A well-formed record goes through the module's own writer.
    pub(in crate::native) fn write(self, directory: &Path) {
        let store = Store::open_unchecked(directory);
        match self.0 {
            Fixture::NativeMode => {
                records::write(&store, &LaunchMode::native(cfg!(windows), "2.1.302".into()))
                    .unwrap()
            }
            Fixture::LegacyStopMode => records::write(
                &store,
                &LaunchMode {
                    schema: 1,
                    mode: Mode::LegacyStop,
                    reason: "platform_legacy".into(),
                    claude_version: None,
                },
            )
            .unwrap(),
            Fixture::ModeBytes(bytes) => replace(&store, MODE_FILE, bytes),
            Fixture::Question(deflection) => {
                records::write(&store, &QuestionMode::new(deflection)).unwrap()
            }
            Fixture::QuestionBytes(bytes) => replace(&store, QUESTION_MODE_FILE, bytes),
            Fixture::NoQuestionMode => store.private(QUESTION_MODE_FILE).remove().unwrap(),
        }
    }
}

fn replace(store: &Store, file: &str, bytes: &[u8]) {
    let record = store.private(file);
    record.remove().unwrap();
    record.write_private(bytes).unwrap();
}
