//! This adapter uses presence alone; it has no app-incarnation evidence.
use crate::native::terminal::close_evidence::{CloseEvidence, ResumableAuthority};
use crate::native::terminal::ownership::{MacTerminalAppIdentity, NativeSessionOwner};
use anyhow::{Context, Result};

pub(super) struct Evidence;
pub(super) static EVIDENCE: Evidence = Evidence;

impl CloseEvidence for Evidence {
    fn surface_absent(
        &self,
        owner: Option<&NativeSessionOwner>,
        surface_present: Box<dyn FnOnce() -> Result<bool> + '_>,
    ) -> Result<bool> {
        let absent = surface_present().map(|present| !present);
        if owner.is_none() {
            absent.context(
                "could not prove absence of the ownerless startup surface; no close was sent",
            )
        } else {
            absent
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
