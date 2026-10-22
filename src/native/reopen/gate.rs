//! Typed refusal reasons; wire spellings and error-chain recognition live here.
use serde::{Serialize, Serializer};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum ReopenGate {
    ProviderUnsupported,
    ReopenConflict,
    ReopenVerificationFailed,
    // The new session exists, but preparation failed before provider startup.
    StartFailed,
    // A launch failed after creation; its durable refusal carries the fenced Claim.
    LaunchFailed,
    // Admission refused the workspace before reserving or creating a Session.
    WorkspaceRefused,
    AlreadyReopened,
    RequestUnresolved,
    SourceIdentityMissing,
    SourceNotClosed,
    SourceNotConverged,
}

impl ReopenGate {
    pub(super) const POST_CREATION: [Self; 5] = [
        Self::StartFailed,
        Self::LaunchFailed,
        Self::ProviderUnsupported,
        Self::ReopenConflict,
        Self::ReopenVerificationFailed,
    ];

    pub(in crate::native) const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderUnsupported => "provider-unsupported",
            Self::ReopenConflict => "reopen-conflict",
            Self::ReopenVerificationFailed => "reopen-verification-failed",
            Self::StartFailed => "start-failed",
            Self::LaunchFailed => "launch-failed",
            Self::WorkspaceRefused => "workspace-refused",
            Self::AlreadyReopened => "already-reopened",
            Self::RequestUnresolved => "request-unresolved",
            Self::SourceIdentityMissing => "source-identity-missing",
            Self::SourceNotClosed => "source-not-closed",
            Self::SourceNotConverged => "source-not-converged",
        }
    }

    pub(super) fn from_wire(value: &str) -> Option<Self> {
        [
            Self::ProviderUnsupported,
            Self::ReopenConflict,
            Self::ReopenVerificationFailed,
            Self::StartFailed,
            Self::LaunchFailed,
            Self::WorkspaceRefused,
            Self::AlreadyReopened,
            Self::RequestUnresolved,
            Self::SourceIdentityMissing,
            Self::SourceNotClosed,
            Self::SourceNotConverged,
        ]
        .into_iter()
        .find(|gate| gate.as_str() == value)
    }

    pub(super) fn is_post_creation(self) -> bool {
        Self::POST_CREATION.contains(&self)
    }
}

impl std::fmt::Display for ReopenGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ReopenGate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug)]
struct ReopenRefusal {
    gate: ReopenGate,
    detail: String,
}

impl std::fmt::Display for ReopenRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reopen refused ({}): {}", self.gate, self.detail)
    }
}
impl std::error::Error for ReopenRefusal {}

pub(super) fn reopen_refusal(gate: ReopenGate, detail: String) -> anyhow::Error {
    anyhow::Error::new(ReopenRefusal { gate, detail })
}

pub(in crate::native) fn reopen_refusal_gate(error: &anyhow::Error) -> Option<ReopenGate> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ReopenRefusal>())
        .map(|refusal| refusal.gate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_keep_wire_spelling_and_typed_identity_through_context() {
        for (gate, wire, post_creation) in [
            (
                ReopenGate::ProviderUnsupported,
                "provider-unsupported",
                true,
            ),
            (ReopenGate::ReopenConflict, "reopen-conflict", true),
            (
                ReopenGate::ReopenVerificationFailed,
                "reopen-verification-failed",
                true,
            ),
            (ReopenGate::StartFailed, "start-failed", true),
            (ReopenGate::LaunchFailed, "launch-failed", true),
            (ReopenGate::WorkspaceRefused, "workspace-refused", false),
            (ReopenGate::AlreadyReopened, "already-reopened", false),
            (ReopenGate::RequestUnresolved, "request-unresolved", false),
            (
                ReopenGate::SourceIdentityMissing,
                "source-identity-missing",
                false,
            ),
            (ReopenGate::SourceNotClosed, "source-not-closed", false),
            (
                ReopenGate::SourceNotConverged,
                "source-not-converged",
                false,
            ),
        ] {
            assert_eq!(serde_json::to_value(gate).unwrap(), wire);
            assert_eq!(ReopenGate::from_wire(wire), Some(gate));
            assert_eq!(gate.is_post_creation(), post_creation);
            let error = reopen_refusal(gate, "fixture detail".into()).context("outer");
            assert_eq!(reopen_refusal_gate(&error), Some(gate));
            assert_eq!(
                format!("{error:#}"),
                format!("outer: reopen refused ({wire}): fixture detail")
            );
        }
        assert_eq!(ReopenGate::from_wire("Launch-Failed"), None);
        assert_eq!(ReopenGate::from_wire("launch-failed "), None);
        assert_eq!(
            reopen_refusal_gate(&anyhow::anyhow!("reopen refused (launch-failed): text")),
            None
        );
    }
}
