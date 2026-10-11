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

#[cfg(target_os = "macos")]
#[path = "macos/ghostty/close_evidence.rs"]
mod ghostty;
#[cfg(target_os = "macos")]
#[path = "macos/iterm2/close_evidence.rs"]
mod iterm2;
#[cfg(target_os = "macos")]
#[path = "macos/warp/close_evidence.rs"]
mod warp;
#[cfg(target_os = "macos")]
#[path = "macos/wezterm/close_evidence.rs"]
mod wezterm;
#[cfg(target_os = "macos")]
#[path = "windows/close_evidence.rs"]
mod windows;

#[cfg(target_os = "macos")]
pub(in crate::native) fn for_kind(kind: super::TerminalKind) -> &'static dyn CloseEvidence {
    use super::{TerminalKind, macos::apple_terminal};
    match kind {
        TerminalKind::AppleTerminal => &apple_terminal::close_evidence::EVIDENCE,
        TerminalKind::Iterm2 => &iterm2::EVIDENCE,
        TerminalKind::Ghostty => &ghostty::EVIDENCE,
        TerminalKind::Warp => &warp::EVIDENCE,
        TerminalKind::WezTerm => &wezterm::EVIDENCE,
        TerminalKind::WindowsConsole => &windows::EVIDENCE,
    }
}

// Inject only Terminal.app's observations; every other kind still answers through
// its own real adapter. The borrowed evidence may capture a test's Cell/RefCell.
#[cfg(all(test, target_os = "macos"))]
pub(in crate::native) fn with_apple(
    kind: super::TerminalKind,
    apple: &dyn CloseEvidence,
) -> &dyn CloseEvidence {
    if kind == super::TerminalKind::AppleTerminal {
        apple
    } else {
        for_kind(kind)
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
}
