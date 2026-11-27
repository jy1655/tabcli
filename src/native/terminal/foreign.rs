//! Positive evidence of a copy comes from the bound active handle, strict marker or bound owner.
use super::ownership::OwnerBinding;
use super::{OsTerminals, TerminalKind, TerminalSession};
use crate::native::session::{
    CoreRecord, Reader,
    owner::{OwnerRecord, session_of},
    record,
};
use anyhow::Result;

#[derive(Clone, Copy, Debug)]
pub(in crate::native) enum CopyEvidence {
    Kind(TerminalKind),
    Origin(&'static str),
}

impl CopyEvidence {
    pub(in crate::native) fn refusal(self) -> anyhow::Error {
        let os = OsTerminals::CURRENT.expect("copy evidence requires a terminal platform");
        match self {
            Self::Kind(kind) => os.unavailable(kind),
            Self::Origin(origin) => {
                let here = os.os;
                crate::native::failure_kind::Unsupported::new(format!(
                    "This session was recorded on {origin} and cannot be closed on {here} while its records leave it running."
                )).into()
            }
        }
    }
}

type StatedHandle = (TerminalSession, Option<TerminalKind>);

pub(in crate::native) fn judge(
    id: &str,
    active: Option<&StatedHandle>,
    marker: Option<TerminalKind>,
    owner: &OwnerRecord,
) -> Option<CopyEvidence> {
    let os = OsTerminals::CURRENT?;
    if let Some((surface, Some(kind))) = active
        && surface.managed_session_id.as_deref() == Some(id)
        && !surface.id.is_empty()
    {
        if !os.runs(*kind) {
            return Some(CopyEvidence::Kind(*kind));
        }
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        if *kind == TerminalKind::Herdr
            && let Some(core) = surface.herdr_core()
        {
            use super::record::HerdrHostBirth;
            let origin = match core.birth {
                HerdrHostBirth::Windows { .. } => OsTerminals::WINDOWS,
                HerdrHostBirth::Linux { .. } => OsTerminals::LINUX,
                HerdrHostBirth::Macos { .. } => OsTerminals::MACOS,
            };
            if origin.os != os.os {
                return Some(CopyEvidence::Origin(origin.os));
            }
        }
    }
    if let Some(kind) = marker
        && !os.runs(kind)
    {
        return Some(CopyEvidence::Kind(kind));
    }
    let OwnerRecord::Recorded(record) = owner else {
        return None;
    };
    if record.binding != OwnerBinding::Bound {
        return None;
    }
    let identity = &record.owner;
    let mut origins = [
        (
            OsTerminals::MACOS,
            identity.process_start_seconds.is_some()
                || identity.process_start_microseconds.is_some(),
        ),
        (OsTerminals::LINUX, identity.linux_birth.is_some()),
        (
            OsTerminals::WINDOWS,
            identity.windows_process_identity.is_some(),
        ),
    ]
    .into_iter()
    .filter_map(|(os, present)| present.then_some(os.os));
    let first = origins.next()?;
    if origins.next().is_some() {
        Some(CopyEvidence::Origin("multiple operating systems"))
    } else {
        (first != os.os).then_some(CopyEvidence::Origin(first))
    }
}

pub(in crate::native) struct SurfaceRecords {
    pub(in crate::native) active: Result<Option<StatedHandle>>,
    pub(in crate::native) marker: Result<Option<record::ConsumptionMarker>>,
}

impl SurfaceRecords {
    pub(in crate::native) fn read(reader: &Reader) -> Self {
        Self {
            active: reader
                .record(CoreRecord::Terminal)
                .optional_json_with(TerminalSession::decode_with_stated_kind),
            marker: reader
                .record(CoreRecord::TerminalClosed)
                .optional_json_with(record::consumption_marker),
        }
    }
    pub(in crate::native) fn completed_kind(&self) -> Option<TerminalKind> {
        self.marker
            .as_ref()
            .ok()
            .and_then(|m| m.as_ref())
            .and_then(|m| m.completed())
            .flatten()
    }

    pub(in crate::native) fn evidence(
        &self,
        reader: &Reader,
        owner: &OwnerRecord,
    ) -> Option<CopyEvidence> {
        judge(
            session_of(reader),
            self.active.as_ref().ok().and_then(|slot| slot.as_ref()),
            self.completed_kind(),
            owner,
        )
    }
}
