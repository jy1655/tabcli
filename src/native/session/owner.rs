//! Observation of a session's recorded owner process. Read-only: no record changes.
use super::{CoreRecord, Reader, RecordReader};
#[cfg(windows)]
use crate::native::terminal;
use crate::native::terminal::ownership::NativeSessionOwner;
#[cfg(target_os = "macos")]
use crate::native::terminal::ownership::mac_native_owner_is_live;
use agent_bridge::process_is_alive;
#[cfg(not(any(target_os = "macos", windows)))]
use anyhow::Result;
use serde::Serialize;

#[derive(Default, Serialize)]
pub(in crate::native) struct OwnerObservation {
    pub(in crate::native) process_alive: Option<bool>,
    pub(in crate::native) identity_matches: Option<bool>,
    pub(in crate::native) error: Option<String>,
}

pub(in crate::native) fn observe_owner(reader: &Reader) -> OwnerObservation {
    let directory = reader.directory();
    let owner = match RecordReader::at(
        Reader::open_unchecked(directory)
            .record(CoreRecord::Owner)
            .path(),
    )
    .optional_json::<NativeSessionOwner>()
    {
        Ok(Some(owner)) => owner,
        Ok(None) => return OwnerObservation::default(),
        Err(error) => {
            return OwnerObservation {
                error: Some(format!("{error:#}")),
                ..OwnerObservation::default()
            };
        }
    };
    observe_owner_record(&owner)
}

pub(in crate::native) fn observe_owner_record(owner: &NativeSessionOwner) -> OwnerObservation {
    let observation = OwnerObservation {
        process_alive: Some(process_is_alive(owner.pid)),
        ..OwnerObservation::default()
    };
    if observation.process_alive == Some(false) {
        return observation;
    }
    #[cfg(target_os = "macos")]
    let identity = (owner.process_start_seconds.is_some()
        && owner.process_start_microseconds.is_some()
        && owner.terminal_tty_device.is_some()
        && owner.process_group.is_some()
        && owner.terminal_process_group.is_some())
    .then(|| mac_native_owner_is_live(owner));
    #[cfg(windows)]
    let identity = owner.windows_process_identity.as_ref().map(|identity| {
        terminal::verify_windows_process_identity(owner.pid, identity).map(|()| true)
    });
    #[cfg(not(any(target_os = "macos", windows)))]
    let identity: Option<Result<bool>> = None;
    match identity {
        Some(Ok(matches)) => OwnerObservation {
            identity_matches: Some(matches),
            ..observation
        },
        Some(Err(error)) => OwnerObservation {
            error: Some(format!("{error:#}")),
            ..observation
        },
        None => observation,
    }
}
