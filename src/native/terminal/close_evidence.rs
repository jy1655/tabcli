//! Read-only close evidence supplied by each terminal adapter.
#![cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]

use super::ownership::{MacTerminalAppIdentity, NativeSessionOwner};
use anyhow::Result;

/// Creation-proven host evidence is supplied by its owning adapter, before the Close
/// decision reads owner/intent records. Other kinds have no such evidence. This is a
/// read-only observation, not a held process or permission to signal it.
pub(in crate::native::terminal) fn created_host_incarnation(
    expected_session_id: &str,
    session: &super::TerminalSession,
) -> Result<Option<crate::native::process_identity::ProcessObservation>> {
    if session.kind.registration().close_evidence
        != super::facts::CloseEvidenceChoice::BridgeCreatedHost
    {
        return Ok(None);
    }
    session.verify_managed_session(expected_session_id)?;
    #[cfg(target_os = "linux")]
    return super::linux::tmux::created_host_incarnation(session).map(Some);
    // The host is this close's first look at the surface. A kind this OS does not run gets
    // the refusal its lookup gives every such kind, as a close of any other such handle does.
    #[cfg(not(target_os = "linux"))]
    match super::OsTerminals::CURRENT {
        Some(os) if !os.runs(session.kind) => Err(os.not_here(session.kind)),
        _ => anyhow::bail!("TAB-created host evidence is unsupported on this platform"),
    }
}

pub(in crate::native) enum ResumableAuthority {
    // Only Terminal.app evidence proves a resumable surface absent.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Absent,
    SurfaceOnly,
}

pub(in crate::native) trait CloseEvidence {
    fn surface_absent(
        &self,
        owner: Option<&NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<bool>;
    fn incarnation_ended(&self, owner: &NativeSessionOwner) -> Result<bool>;
    fn resumable_authority(
        &self,
        owner: &dyn Fn() -> Result<NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<ResumableAuthority>;
    fn startup_retained_reason(&self) -> &'static str;
    fn derive_app_incarnation(
        &self,
        owner: &NativeSessionOwner,
    ) -> Result<Option<MacTerminalAppIdentity>>;
}

// Presence alone: the adapter has no app-incarnation evidence, so an absent surface
// is one its presence query does not find, nothing ever ends an incarnation, a
// recorded close intent resumes as surface-only authority without rereading the owner
// or the presence, and no incarnation is derived. Every kind but Terminal.app selects
// it; the declared variations are the context that Warp and WezTerm add when the
// owner record is absent and the surface is not proven absent.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct PresenceOnly {
    ownerless_context: Option<&'static str>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
static PRESENCE_ONLY: PresenceOnly = PresenceOnly {
    ownerless_context: None,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
static PRESENCE_ONLY_WITH_OWNERLESS_CONTEXT: PresenceOnly = PresenceOnly {
    ownerless_context: Some(
        "could not prove absence of the ownerless startup surface; no close was sent",
    ),
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl CloseEvidence for PresenceOnly {
    fn surface_absent(
        &self,
        owner: Option<&NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<bool> {
        use anyhow::Context as _;
        let absent = surface_present().map(|present| !present);
        match self.ownerless_context {
            Some(context) if owner.is_none() => absent.context(context),
            _ => absent,
        }
    }

    fn incarnation_ended(&self, _owner: &NativeSessionOwner) -> Result<bool> {
        Ok(false)
    }

    fn resumable_authority(
        &self,
        _owner: &dyn Fn() -> Result<NativeSessionOwner>,
        _surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<ResumableAuthority> {
        Ok(ResumableAuthority::SurfaceOnly)
    }

    fn startup_retained_reason(&self) -> &'static str {
        "startup has no native owner or close intent and its surface is not proven gone; no close was sent and the surface handle is retained"
    }

    fn derive_app_incarnation(
        &self,
        _owner: &NativeSessionOwner,
    ) -> Result<Option<MacTerminalAppIdentity>> {
        Ok(None)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn for_kind(kind: super::TerminalKind) -> &'static dyn CloseEvidence {
    use super::facts::CloseEvidenceChoice;
    #[cfg(target_os = "macos")]
    use super::macos::apple_terminal;
    match kind.registration().close_evidence {
        #[cfg(target_os = "macos")]
        CloseEvidenceChoice::AppleTerminal => &apple_terminal::close_evidence::EVIDENCE,
        #[cfg(target_os = "linux")]
        CloseEvidenceChoice::AppleTerminal => &PRESENCE_ONLY,
        CloseEvidenceChoice::BridgeCreatedHost => &PRESENCE_ONLY,
        CloseEvidenceChoice::PresenceOnly => &PRESENCE_ONLY,
        CloseEvidenceChoice::PresenceOnlyWithOwnerlessContext => {
            &PRESENCE_ONLY_WITH_OWNERLESS_CONTEXT
        }
    }
}

// Only Linux runs tmux, the one kind whose close begins with its created host. Elsewhere a
// stored tmux handle gets the refusal that the OS's lookup gives every kind it does not run,
// after the binding check, so that a close of it reads as a close of any other such handle.
#[cfg(all(test, any(target_os = "macos", windows)))]
mod foreign_host_tests {
    use super::*;
    use crate::native::failure_kind::{FailureKind, classify};
    use crate::native::terminal::{TerminalKind, TerminalSession};

    #[test]
    fn a_created_host_kind_this_os_does_not_run_gets_the_lookup_refusal() {
        let unbound = TerminalSession::new(TerminalKind::Tmux, "%0".into());
        let error = created_host_incarnation("session-tmux", &unbound).unwrap_err();
        assert!(
            format!("{error:#}").contains("missing its managed session binding"),
            "{error:#}"
        );
        let bound = unbound.with_binding(Some("session-tmux".into()));
        let error = created_host_incarnation("session-tmux", &bound).unwrap_err();
        let expected = if cfg!(windows) {
            "tmux is not available on Windows"
        } else {
            "tmux is not available on macOS"
        };
        assert_eq!(format!("{error:#}"), expected);
        assert_eq!(classify(&error), Some(FailureKind::Unsupported));
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::native::terminal::TerminalKind;

    #[test]
    fn presence_only_adapters_do_not_read_resumable_owner_or_surface() {
        for kind in [
            TerminalKind::Iterm2,
            TerminalKind::Ghostty,
            TerminalKind::Warp,
            TerminalKind::WezTerm,
            TerminalKind::WindowsConsole,
            TerminalKind::Herdr,
        ] {
            let evidence = for_kind(kind);
            assert!(matches!(
                evidence
                    .resumable_authority(
                        &|| panic!("{kind:?}: resumable must not reread the owner"),
                        Box::new(|| panic!("{kind:?}: resumable must not query presence")),
                    )
                    .unwrap(),
                ResumableAuthority::SurfaceOnly
            ));
            let owner = NativeSessionOwner::default();
            assert!(!evidence.incarnation_ended(&owner).unwrap());
            assert!(evidence.derive_app_incarnation(&owner).unwrap().is_none());
            // The callback consumes borrowed state: no Fn or 'static requirement.
            let mut observed = false;
            assert!(
                evidence
                    .surface_absent(
                        Some(&owner),
                        Box::new(|| {
                            observed = true;
                            Ok(false)
                        })
                    )
                    .unwrap()
            );
            assert!(observed);
        }
    }

    // The one variation among the presence-only kinds: Warp and WezTerm add the
    // ownerless-startup context to a failed presence query, and only when no owner
    // record exists. Every other answer is the same for every presence-only kind.
    #[test]
    fn presence_only_ownerless_context_is_added_by_warp_and_wezterm_only() {
        let owner = NativeSessionOwner::default();
        for (kind, with_context) in [
            (TerminalKind::Iterm2, false),
            (TerminalKind::Ghostty, false),
            (TerminalKind::WindowsConsole, false),
            (TerminalKind::Warp, true),
            (TerminalKind::WezTerm, true),
        ] {
            let evidence = for_kind(kind);
            let chain = |owner: Option<&NativeSessionOwner>| {
                evidence
                    .surface_absent(owner, Box::new(|| Err(anyhow::anyhow!("presence cause"))))
                    .unwrap_err()
                    .chain()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            };
            let ownerless: &[&str] = if with_context {
                &[
                    "could not prove absence of the ownerless startup surface; no close was sent",
                    "presence cause",
                ]
            } else {
                &["presence cause"]
            };
            assert_eq!(chain(None), ownerless, "{kind:?}");
            assert_eq!(chain(Some(&owner)), ["presence cause"], "{kind:?}");
            assert!(
                evidence
                    .surface_absent(None, Box::new(|| Ok(false)))
                    .unwrap()
            );
            assert!(
                !evidence
                    .surface_absent(None, Box::new(|| Ok(true)))
                    .unwrap()
            );
            assert_eq!(
                evidence.startup_retained_reason(),
                "startup has no native owner or close intent and its surface is not proven gone; no close was sent and the surface handle is retained"
            );
        }
    }
}
