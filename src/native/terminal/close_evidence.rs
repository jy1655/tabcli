//! Read-only close evidence supplied by each terminal adapter.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::ownership::{MacTerminalAppIdentity, NativeSessionOwner};
use anyhow::Result;

pub(in crate::native) enum ResumableAuthority {
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
// it; the one declared variation is the context that Warp and WezTerm add when the
// owner record is absent and the surface is not proven absent.
#[cfg(target_os = "macos")]
struct PresenceOnly {
    ownerless_context: Option<&'static str>,
}

#[cfg(target_os = "macos")]
static PRESENCE_ONLY: PresenceOnly = PresenceOnly {
    ownerless_context: None,
};

#[cfg(target_os = "macos")]
static PRESENCE_ONLY_WITH_OWNERLESS_CONTEXT: PresenceOnly = PresenceOnly {
    ownerless_context: Some(
        "could not prove absence of the ownerless startup surface; no close was sent",
    ),
};

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
pub(in crate::native) fn for_kind(kind: super::TerminalKind) -> &'static dyn CloseEvidence {
    use super::{facts::CloseEvidenceChoice, macos::apple_terminal};
    match kind.registration().close_evidence {
        CloseEvidenceChoice::AppleTerminal => &apple_terminal::close_evidence::EVIDENCE,
        CloseEvidenceChoice::PresenceOnly => &PRESENCE_ONLY,
        CloseEvidenceChoice::PresenceOnlyWithOwnerlessContext => {
            &PRESENCE_ONLY_WITH_OWNERLESS_CONTEXT
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
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
