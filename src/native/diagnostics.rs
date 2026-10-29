//! Diagnostic vocabulary shared by observations and provider adapters.
pub(super) mod vocabulary;
use super::{consent, provider};
use crate::native::session::primitives::unix_ms;
use crate::native::session::schema::SessionManifest;
use agent_bridge::FirstPartyCli;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::Instant;
use vocabulary::{CheckId, Reason};

// A provider owns which observation can explain a timed-out result and whether it
// needs an explicit probe. The caller owns the bound on executing the command.
pub(super) struct ResultTimeoutDiagnostic {
    pub(super) probe: bool,
    pub(super) detail: for<'a> fn(&'a TimeoutCheck, &str) -> Option<&'a str>,
}

// A row from the child doctor's JSON, including sparse or malformed observations.
// Read each field independently so an unrelated row cannot hide a later diagnosis.
pub(super) struct TimeoutCheck {
    pub(super) id: Option<String>,
    pub(super) reason_code: Option<String>,
    pub(super) request_id: Option<String>,
    pub(super) detail: Option<String>,
}

impl TimeoutCheck {
    fn from_row(row: &Value) -> Self {
        Self {
            id: row["id"].as_str().map(str::to_owned),
            reason_code: row["reason_code"].as_str().map(str::to_owned),
            request_id: row["evidence"]["request_id"].as_str().map(str::to_owned),
            detail: row["detail"].as_str().map(str::to_owned),
        }
    }
}

pub(super) fn result_timeout_detail(
    provider: FirstPartyCli,
    session: &str,
    request_id: &str,
    mut observe: impl FnMut(&[String]) -> Option<Value>,
) -> Option<String> {
    let diagnostic = provider::result_timeout_diagnostic(provider)?;
    let mut args = vec!["doctor".to_owned(), session.to_owned()];
    if diagnostic.probe {
        args.push("--probe".to_owned());
    }
    args.push("--json".to_owned());
    let report = observe(&args)?;
    if report["session"] != session {
        return None;
    }
    report["checks"].as_array()?.iter().find_map(|row| {
        let check = TimeoutCheck::from_row(row);
        (diagnostic.detail)(&check, request_id).map(str::to_owned)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Availability {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Debug, Serialize)]
pub(super) struct Check {
    pub(super) id: &'static str,
    pub(super) availability: Availability,
    pub(super) reason_code: &'static str,
    observed_unix_ms: u128,
    pub(super) detail: String,
    pub(super) next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_command: Option<Vec<String>>,
    pub(super) evidence: Value,
}

impl Check {
    pub(super) fn new(
        id: CheckId,
        availability: Availability,
        reason_code: Reason,
        detail: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        debug_assert!(CheckId::ALL.contains(&id));
        debug_assert!(Reason::ALL.contains(&reason_code));
        Self {
            id: id.name(),
            availability,
            reason_code: reason_code.name(),
            observed_unix_ms: unix_ms(),
            detail: detail.into(),
            next_action: next_action.into(),
            next_command: None,
            evidence: Value::Null,
        }
    }

    pub(super) fn evidence(mut self, evidence: Value) -> Self {
        self.evidence = evidence;
        self
    }

    pub(super) fn command(mut self, arguments: Vec<String>) -> Self {
        self.next_command = Some(arguments);
        self
    }
}

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub(super) directory: Option<&'a Path>,
    pub(super) manifest: Option<&'a SessionManifest>,
    pub(super) executable: Option<&'a Path>,
    pub(super) current_version: Option<&'a str>,
    pub(super) workspace: &'a Path,
    pub(super) probe: bool,
    pub(super) deadline: Instant,
    // The caller's environment, from which an adapter locates its home for a row.
    pub(super) environment: &'a consent::Environment,
}

#[cfg(test)]
mod timeout_tests {
    use super::{TimeoutCheck, result_timeout_detail};
    use crate::native::provider;
    use agent_bridge::FirstPartyCli;
    use serde_json::{Value, json};

    #[test]
    fn agy_timeout_predicate_requires_only_reason_and_matching_request() {
        let diagnostic = provider::result_timeout_diagnostic(FirstPartyCli::Agy).unwrap();
        assert!(!diagnostic.probe);
        let row = json!({
            "reason_code": "agy_tool_confirmation_observed",
            "evidence": {"request_id": "request-current"},
            "detail": "approval needed"
        });
        let check = TimeoutCheck::from_row(&row);
        assert_eq!(
            (diagnostic.detail)(&check, "request-current"),
            Some("approval needed")
        );
        assert_eq!((diagnostic.detail)(&check, "request-other"), None);
        for (field, value) in [
            ("reason_code", json!("other")),
            ("reason_code", json!(17)),
            ("evidence", json!({"request_id": 17})),
            ("evidence", Value::Null),
            ("detail", json!(17)),
        ] {
            let mut changed = row.clone();
            changed[field] = value;
            assert_eq!(
                (diagnostic.detail)(&TimeoutCheck::from_row(&changed), "request-current"),
                None
            );
        }
        let mut unrelated = row;
        unrelated["id"] = json!("unrelated");
        unrelated["availability"] = json!("available");
        assert_eq!(
            (diagnostic.detail)(&TimeoutCheck::from_row(&unrelated), "request-current"),
            Some("approval needed")
        );
    }

    #[test]
    fn pi_timeout_predicate_requires_only_credentials_id() {
        let diagnostic = provider::result_timeout_diagnostic(FirstPartyCli::Pi).unwrap();
        assert!(diagnostic.probe);
        for availability in ["available", "unavailable", "unknown"] {
            let row = json!({
                "id": "pi_provider_credentials", "detail": "credentials observation",
                "availability": availability, "reason_code": "unrelated",
                "evidence": {"request_id": "request-other"}
            });
            let check = TimeoutCheck::from_row(&row);
            assert_eq!(
                (diagnostic.detail)(&check, "request-current"),
                Some("credentials observation")
            );
        }
        for row in [
            json!({"id": "other", "detail": "ignored"}),
            json!({"id": 17, "detail": "ignored"}),
            json!({"id": "pi_provider_credentials", "detail": false}),
            json!({"detail": "ignored"}),
        ] {
            assert_eq!(
                (diagnostic.detail)(&TimeoutCheck::from_row(&row), "request-current"),
                None
            );
        }
    }

    #[test]
    fn timeout_rows_preserve_sparse_matches_and_first_matching_detail() {
        for (provider, row) in [
            (
                FirstPartyCli::Agy,
                json!({"reason_code": "agy_tool_confirmation_observed", "evidence": {"request_id": "request-current"}, "detail": "first"}),
            ),
            (
                FirstPartyCli::Pi,
                json!({"id": "pi_provider_credentials", "detail": "first"}),
            ),
        ] {
            let mut missing_detail = row.clone();
            missing_detail["detail"] = json!(3);
            let mut later = row.clone();
            later["detail"] = json!("second");
            assert_eq!(
                result_timeout_detail(provider, "session-owned", "request-current", |_| Some(
                    json!({
                        "session": "session-owned",
                        "checks": [null, 7, [], {}, missing_detail, row, later]
                    })
                )),
                Some("first".to_owned())
            );
        }
    }

    #[test]
    fn codex_and_claude_timeout_diagnostics_do_not_observe() {
        for provider in [FirstPartyCli::Codex, FirstPartyCli::Claude] {
            assert!(provider::result_timeout_diagnostic(provider).is_none());
            assert_eq!(
                result_timeout_detail(provider, "session-owned", "request-current", |_| panic!(
                    "no diagnostic may run"
                )),
                None
            );
        }
    }
}

#[cfg(test)]
mod vocabulary_tests {
    use super::vocabulary::{CheckId, Reason};
    use crate::native::{query::status::AttentionFlag, self_test};

    #[test]
    fn public_vocabulary_matches_committed_fixture() {
        let vocabulary = serde_json::json!({
            "attention_flags": AttentionFlag::ALL.iter().map(|flag| flag.name()).collect::<Vec<_>>(),
            "check_ids": CheckId::ALL.iter().map(|id| id.name()).collect::<Vec<_>>(),
            "reasons": Reason::ALL.iter().map(|reason| reason.name()).collect::<Vec<_>>(),
            "self_test_outcomes": self_test::OUTCOMES,
            "self_test_steps": self_test::STEPS,
            "self_test_skip_reason": self_test::SKIP_REASON,
        });
        let expected = format!("{}\n", serde_json::to_string_pretty(&vocabulary).unwrap());
        assert_eq!(
            include_str!("../../tests/fixtures/public_vocabulary.json"),
            expected,
            "Update tests/fixtures/public_vocabulary.json deliberately to:\n{expected}"
        );
    }
}
