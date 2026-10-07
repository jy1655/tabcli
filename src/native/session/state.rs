//! The recorded session state and its transition contract.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{convert::Infallible, fmt, str::FromStr};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum SessionState {
    Launching,
    Running,
    AwaitingInitialInput,
    Ready,
    Claimed,
    Working,
    ResumePending,
    Exited,
    Failed,
    Closed,
    // Timeline uses an unknown sentinel for unreadable status; old records also accepted
    // arbitrary strings. Preserve those bytes and the same-state transition rule.
    Unknown(String),
}

// A state is a JSON string and nothing else, exactly as the `String` field it replaces: a
// map, array, number or null in the state field is a damaged record, not a state.
impl Serialize for SessionState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for SessionState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.parse::<Self>() {
            Ok(state) => state,
            Err(never) => match never {},
        })
    }
}

impl SessionState {
    pub(in crate::native) fn as_str(&self) -> &str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::AwaitingInitialInput => "awaiting-initial-input",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Working => "working",
            Self::ResumePending => "resume-pending",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Closed => "closed",
            Self::Unknown(value) => value,
        }
    }

    /// The session status transition contract. A same-state write is always allowed (it
    /// refreshes the timestamp or error and still takes a new generation); every other write
    /// must appear in this table or `update_status` rejects it without advancing the
    /// generation. `exited`, `failed`, and `closed` are terminal except that the first two may
    /// still be closed; `closed` accepts nothing else. The one exception to the generation
    /// increment is the `closed.json` tombstone: once it exists, `update_status` no longer
    /// consults this table and rewrites `status.json` as a copy of the tombstone, so the
    /// tombstone's generation, timestamp, and error are preserved rather than advanced. The
    /// README section "권한과 세션 경계" carries the same table for operators.
    ///
    /// | From                    | To                                                 |
    /// | ----------------------- | -------------------------------------------------- |
    /// | `launching`             | `running`, `awaiting-initial-input`, `failed`, `closed` |
    /// | `awaiting-initial-input`| `working`, `exited`, `failed`, `closed`            |
    /// | `running`               | `ready`, `exited`, `failed`, `closed`              |
    /// | `ready`                 | `claimed`, `exited`, `failed`, `closed`            |
    /// | `claimed`               | `working`, `ready`, `exited`, `failed`, `closed`   |
    /// | `working`               | `ready`, `exited`, `failed`, `closed`              |
    /// | `resume-pending`        | `working`, `ready`, `exited`, `failed`, `closed`   |
    /// | `exited`, `failed`      | `closed`                                           |
    /// | `closed`                | (none)                                             |
    pub(in crate::native) fn transition_allowed(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Launching,
                    Self::Running | Self::AwaitingInitialInput | Self::Failed | Self::Closed
                ) | (
                    Self::AwaitingInitialInput,
                    Self::Working | Self::Exited | Self::Failed | Self::Closed
                ) | (
                    Self::Running,
                    Self::Ready | Self::Exited | Self::Failed | Self::Closed
                ) | (
                    Self::Ready,
                    Self::Claimed | Self::Exited | Self::Failed | Self::Closed
                ) | (
                    Self::Claimed,
                    Self::Working | Self::Ready | Self::Exited | Self::Failed | Self::Closed
                ) | (
                    Self::Working,
                    Self::Ready | Self::Exited | Self::Failed | Self::Closed
                ) | (
                    Self::ResumePending,
                    Self::Working | Self::Ready | Self::Exited | Self::Failed | Self::Closed
                ) | (Self::Exited | Self::Failed, Self::Closed)
                    | (Self::Closed, Self::Closed)
            )
    }
}
impl FromStr for SessionState {
    type Err = Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "launching" => Self::Launching,
            "running" => Self::Running,
            "awaiting-initial-input" => Self::AwaitingInitialInput,
            "ready" => Self::Ready,
            "claimed" => Self::Claimed,
            "working" => Self::Working,
            "resume-pending" => Self::ResumePending,
            "exited" => Self::Exited,
            "failed" => Self::Failed,
            "closed" => Self::Closed,
            other => Self::Unknown(other.to_owned()),
        })
    }
}
impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::SessionStatus;

    macro_rules! round_trip {
        ($test:ident, $variant:ident, $text:literal) => {
            #[test]
            fn $test() {
                let state = SessionState::$variant;
                let encoded = concat!("\"", $text, "\"");
                assert_eq!(serde_json::to_string(&state).unwrap(), encoded);
                assert_eq!(serde_json::from_str::<SessionState>(encoded).unwrap(), state);
                assert_eq!($text.parse::<SessionState>().unwrap(), state);
                assert_eq!(state.as_str(), $text);
                assert_eq!(state.to_string(), $text);
                let status = SessionStatus {
                    state, generation: 7, updated_unix_ms: 100,
                    exit_code: None, error: None, residual_surface: None,
                };
                // The exact pretty JSON previously written with a String state.
                let expected = concat!("{\n  \"state\": \"", $text,
                    "\",\n  \"generation\": 7,\n  \"updated_unix_ms\": 100,\n  \"exit_code\": null,\n  \"error\": null\n}");
                assert_eq!(serde_json::to_vec_pretty(&status).unwrap(), expected.as_bytes());
                assert_eq!(serde_json::from_str::<SessionStatus>(expected).unwrap().state, status.state);
            }
        };
    }
    round_trip!(launching_round_trip, Launching, "launching");
    round_trip!(running_round_trip, Running, "running");
    round_trip!(
        awaiting_initial_input_round_trip,
        AwaitingInitialInput,
        "awaiting-initial-input"
    );
    round_trip!(ready_round_trip, Ready, "ready");
    round_trip!(claimed_round_trip, Claimed, "claimed");
    round_trip!(working_round_trip, Working, "working");
    round_trip!(resume_pending_round_trip, ResumePending, "resume-pending");
    round_trip!(exited_round_trip, Exited, "exited");
    round_trip!(failed_round_trip, Failed, "failed");
    round_trip!(closed_round_trip, Closed, "closed");

    #[test]
    fn non_string_state_fields_are_rejected_as_before() {
        // The replaced `String` field accepted only a JSON string; a tagged map, an array, a
        // number or null must still fail so a damaged record is reported, not acted on.
        for malformed in [
            r#"{"ready":null}"#,
            r#"{"closed":null}"#,
            r#"{"failed":null}"#,
            r#"["ready"]"#,
            "null",
            "1",
            "true",
        ] {
            assert!(
                serde_json::from_str::<SessionState>(malformed).is_err(),
                "{malformed} must not deserialize as a state"
            );
            let status = format!(
                r#"{{"state":{malformed},"generation":1,"updated_unix_ms":1,"exit_code":null,"error":null}}"#
            );
            assert!(
                serde_json::from_str::<SessionStatus>(&status).is_err(),
                "{status} must not deserialize as a status"
            );
        }
    }

    #[test]
    fn unknown_states_keep_the_original_string_and_transition_rules() {
        for value in ["unknown", "future-state", "", "quoted\"state\n한글"] {
            let state = value.parse::<SessionState>().unwrap();
            assert_eq!(state, SessionState::Unknown(value.to_owned()));
            let json = serde_json::to_string(value).unwrap();
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<SessionState>(&json).unwrap(), state);
            assert!(state.clone().transition_allowed(state.clone()));
            assert!(!state.clone().transition_allowed(SessionState::Ready));
            assert!(!SessionState::Ready.transition_allowed(state));
        }
    }
}
