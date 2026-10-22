use std::time::Duration;

// Complete log lines only: a line without its terminating newline may still be
// written, so it is neither a marker nor a receipt yet. Terminal escape sequences and
// carriage returns are dropped so a marker is recognised through console noise.
pub(super) fn complete_log_lines(log: &[u8]) -> impl Iterator<Item = String> + '_ {
    let complete = match log.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &log[..end],
        None => &log[..0],
    };
    complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| strip_terminal_noise(&String::from_utf8_lossy(line)))
}

pub(super) fn strip_terminal_noise(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut clean = String::with_capacity(line.len());
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '\u{1b}' => {
                index += 1;
                match characters.get(index) {
                    Some('[') => {
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            index += 1;
                            if ('\u{40}'..='\u{7e}').contains(next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC ends with BEL or with the string terminator `ESC \`.
                        // Any other escape ends the OSC and is processed on its own.
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            if *next == '\u{7}' {
                                index += 1;
                                break;
                            }
                            if *next == '\u{1b}' {
                                if characters.get(index + 1) == Some(&'\\') {
                                    index += 2;
                                }
                                break;
                            }
                            index += 1;
                        }
                    }
                    Some(_) => index += 1,
                    None => {}
                }
            }
            '\r' => index += 1,
            other => {
                clean.push(other);
                index += 1;
            }
        }
    }
    clean
}

// The glog `[IWEF]MMDD HH:MM:SS.uuuuuu` stamp of a line as an offset from an
// arbitrary epoch (months counted as 31 days); only the difference between two
// stamps of the same log is meaningful, and a difference across a year boundary is
// not. Agy stamps in local time. A line without a stamp (`CLI ready for user
// input`, a file header, console noise) yields `None`.
pub(super) fn glog_timestamp(line: &str) -> Option<Duration> {
    let bytes = line.as_bytes();
    if !matches!(bytes.first(), Some(b'I' | b'W' | b'E' | b'F')) || bytes.get(5) != Some(&b' ') {
        return None;
    }
    let date = line.get(1..5)?;
    let month: u64 = date.get(..2)?.parse().ok()?;
    let day: u64 = date.get(2..)?.parse().ok()?;
    let stamp = line.get(6..21)?;
    let (hours, rest) = stamp.split_once(':')?;
    let (minutes, seconds) = rest.split_once(':')?;
    let (seconds, micros) = seconds.split_once('.')?;
    if micros.len() != 6 {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: u64 = seconds.parse().ok()?;
    let micros: u64 = micros.parse().ok()?;
    Some(
        Duration::from_secs((month * 31 + day) * 86_400 + hours * 3600 + minutes * 60 + seconds)
            + Duration::from_micros(micros),
    )
}
