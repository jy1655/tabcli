//! Existing read policies. They deliberately preserve decoding and budget differences.
use super::*;

/// The attached body must be the recorded bytes, so the event is decoded strictly here
/// instead of through the lossy snapshot reader that decides publication and state.
pub(in crate::native) fn read_event_strictly(
    directory: &Path,
    event_id: &str,
) -> std::result::Result<SessionEvent, String> {
    let path = directory
        .join(crate::native::session::EVENTS_DIRECTORY)
        .join(event_id);
    let bytes = read_regular_bytes_if_present(&path)
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| format!("recorded event {event_id} is missing"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("recorded event {event_id} is not valid UTF-8"))?;
    serde_json::from_str(&text).map_err(|error| format!("invalid JSON in {event_id}: {error}"))
}

/// Reads one event without exceeding the remaining byte budget. `Ok(None)` means the
/// file disappeared during the scan; `Err(Ok(bytes))` means the file is too large for the
/// budget and was not consumed; `Err(Err(error))` is an I/O failure.
pub(in crate::native) fn read_event_within_budget(
    path: &Path,
    remaining: u64,
) -> Result<Option<String>, Result<u64>> {
    use std::io::Read as _;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Err(
                anyhow::Error::new(error).context(format!("failed to inspect {}", path.display()))
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Err(anyhow::anyhow!(
            "refusing non-regular session file: {}",
            path.display()
        )));
    }
    if metadata.len() > remaining {
        return Err(Ok(metadata.len()));
    }
    // The file may have grown since the metadata read: never read past the budget.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Err(
                anyhow::Error::new(error).context(format!("failed to read {}", path.display()))
            ));
        }
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.take(remaining + 1).read_to_end(&mut bytes) {
        return Err(Err(
            anyhow::Error::new(error).context(format!("failed to read {}", path.display()))
        ));
    }
    if bytes.len() as u64 > remaining {
        return Err(Ok(bytes.len() as u64));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

pub(in crate::native) fn read_timeline_record(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    if metadata.len() > limit {
        bail!(
            "record is {} bytes, over the {limit} byte read limit",
            metadata.len()
        );
    }
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    let size = fs::metadata(path)?.len();
    if size != bytes.len() as u64 && size <= limit {
        return Err(SnapshotBusy.into());
    }
    if size > limit {
        bail!("record is {size} bytes, over the {limit} byte read limit");
    }
    std::str::from_utf8(&bytes).context("invalid UTF-8")?;
    Ok(Some(bytes))
}

/// [`journaled_event_state`] that reads at most `limit + 1` bytes of the event. The size
/// is checked before the file is opened, and a file that grows under the read is still
/// reported as oversized: the read is cut after `limit + 1` bytes, and that one extra byte
/// is what detects the overflow, so a caller charging a byte budget may see one byte more
/// than `limit` in `bytes_read`. The `events` directory is validated before anything under
/// it is opened, so a link planted there is never followed by a publication read.
pub(in crate::native) fn journaled_event_state_within(
    directory: &Path,
    pending: &PendingTurnCompletion,
    limit: u64,
) -> Result<JournaledEventRead> {
    use std::io::Read as _;
    require_events_directory(directory)?;
    let path = directory.join(EVENTS_DIRECTORY).join(&pending.event_file);
    let outcome = |state, bytes_read, committed_text| JournaledEventRead {
        state,
        bytes_read,
        committed_text,
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(outcome(JournaledEventState::Absent, 0, None));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    if metadata.len() > limit {
        return Ok(outcome(
            JournaledEventState::Oversized(metadata.len()),
            0,
            None,
        ));
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(outcome(JournaledEventState::Absent, 0, None));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    record_publication_read(&path);
    let mut stored = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut stored)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let bytes_read = stored.len() as u64;
    let state = turn::journaled_event_state_of(pending, &stored, limit)?;
    let text = if state == JournaledEventState::Committed {
        // The journal's bytes are canonical JSON, so the stored text is valid UTF-8.
        let text = String::from_utf8_lossy(&stored).into_owned();
        Some(text)
    } else {
        None
    };
    Ok(outcome(state, bytes_read, text))
}
