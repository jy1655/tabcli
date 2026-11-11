use super::records::{
    self, Binding, Deflection, LaunchMode, MODE_FILE, Mode, QUESTION_MODE_FILE, QuestionMode,
    QuestionRecord,
};
use crate::native::provider::QuestionEmission;
use crate::native::session::{Store, requests::Receipt};
use std::path::{Path, PathBuf};

/// A Claude completion record that a test outside the completion module writes into a session
/// directory, or locates there. The module owns the records' names and schemas; such a test
/// chooses only what a launch recorded or what a Claim's records hold.
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

    /// Where the module records the turn binding of the Claim `token`.
    pub(in crate::native) fn binding_path(directory: &Path, token: &str) -> PathBuf {
        directory.join(records::binding_name(token))
    }

    /// Writes, through the module's own writer, a valid question record of the Receipt's
    /// Request that retains `items`, bound to a turn of the Receipt's Claim.
    pub(in crate::native) fn write_questions(
        directory: &Path,
        receipt: &Receipt,
        items: Vec<QuestionEmission>,
    ) {
        let binding = Binding::new(
            &receipt.claim_token,
            "claude-turn-fixture",
            "00000000-0000-4000-8000-000000000249",
            "turn-1",
        );
        let mut record = QuestionRecord::empty(receipt.request_id.clone(), &binding);
        record.items = items;
        record.write(&Store::open_unchecked(directory)).unwrap();
    }

    /// Replaces the question record of the Claim `token` with bytes that are not JSON.
    pub(in crate::native) fn write_malformed_questions(directory: &Path, token: &str) {
        replace(
            &Store::open_unchecked(directory),
            &records::questions_name(token),
            b"{",
        );
    }
}

fn replace(store: &Store, file: &str, bytes: &[u8]) {
    let record = store.private(file);
    record.remove().unwrap();
    record.write_private(bytes).unwrap();
}
