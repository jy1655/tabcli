use anyhow::Result;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

// Rewind and return a byte prefix; one extra byte detects truncation.
// Callers own the limit and any budget shared with another stream.
pub(super) fn read_capped_file(reader: &mut File, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
    reader.seek(SeekFrom::Start(0))?;
    let mut output = Vec::new();
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    let truncated = output.len() > max_bytes;
    output.truncate(max_bytes);
    Ok((output, truncated))
}
