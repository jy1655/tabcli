//! A read-only note for an initial prompt that was not delivered: whether the provider's
//! version differs from the one that last completed a request on this state root. A provider
//! update can change a startup screen that Bridge waits behind, and the failure alone does
//! not say so. The note states the two recorded versions and nothing about the cause; it
//! reads manifests and events only and writes nothing.
//!
//! The start's own error must not wait on this read or depend on it. The scan runs on a
//! thread of its own and the caller waits for it at most `TIME`; a scan that has not
//! answered by then is abandoned and there is no note. Every step is bounded as well, and
//! anything the scan cannot read ends it without a note: a comparison that skipped an
//! unreadable newer session could name an older one as the last that worked.
use crate::native::session::primitives::valid_session_id;
use crate::native::session::{EventListing, Reader, record};
use agent_bridge::{FirstPartyCli, PUBLIC_COMMAND, terminal_safe_text};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Bounds of one scan. Past any of them there is no note. A debug build read a state root of
// 4,700 sessions in 1.1 s (2026-11-10).
const TIME: Duration = Duration::from_secs(5);
// Entries of the state root looked at, whatever they are.
const ENTRIES: usize = 20_000;
const MANIFEST_BYTES: u64 = 64 * 1024;
// Newest sessions of the provider examined for a completed request.
const SESSIONS_EXAMINED: usize = 64;
const EVENTS_PER_SESSION: usize = 1024;
const EVENT_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn note(
    root: &Path,
    session: &str,
    provider: FirstPartyCli,
    version: &str,
) -> Option<String> {
    let (root, session, version) = (root.to_owned(), session.to_owned(), version.to_owned());
    let (answer, waiting) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("provider-version-note".into())
        .spawn(move || {
            let deadline = Instant::now() + TIME;
            let note = note_until(&root, &session, provider, &version, deadline)
                .filter(|_| Instant::now() < deadline);
            // The caller may have stopped waiting.
            let _ = answer.send(note);
        })
        .ok()?;
    waiting.recv_timeout(TIME).ok().flatten()
}

fn note_until(
    root: &Path,
    session: &str,
    provider: FirstPartyCli,
    version: &str,
    deadline: Instant,
) -> Option<String> {
    // (created, id, directory, version), newest first, never more than twice the limit.
    let mut sessions: Vec<(u128, String, PathBuf, String)> = Vec::new();
    let newest_first = |sessions: &mut Vec<(u128, String, PathBuf, String)>| {
        sessions.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        sessions.truncate(SESSIONS_EXAMINED);
    };
    for (seen, entry) in std::fs::read_dir(root).ok()?.enumerate() {
        if seen >= ENTRIES || Instant::now() >= deadline {
            return None;
        }
        let entry = entry.ok()?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Bridge creates session directories only; another name or kind is not history.
        if !valid_session_id(&id) || id == session || !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let directory = entry.path();
        // A directory without a manifest is not a session; one that cannot be read or
        // decoded within the bound may be a newer session of this provider.
        let Some(manifest) = Reader::open_unchecked(&directory)
            .manifest_within(&id, MANIFEST_BYTES)
            .ok()?
        else {
            continue;
        };
        if manifest.provider != provider.as_str() {
            continue;
        }
        sessions.push((
            manifest.created_unix_ms,
            id,
            directory,
            manifest.provider_version,
        ));
        if sessions.len() >= SESSIONS_EXAMINED * 2 {
            newest_first(&mut sessions);
        }
    }
    newest_first(&mut sessions);
    let mut remaining = EVENT_BYTES;
    for (_, id, directory, last) in sessions {
        if !completed(&directory, &mut remaining, deadline)? {
            continue;
        }
        if last == version {
            return None;
        }
        return Some(format!(
            "this session runs {provider} {}; the last {provider} session with a completed request on this state root, {id}, ran {}: if the provider changed its startup, `{PUBLIC_COMMAND} screen {session}` shows where it stopped",
            terminal_safe_text(version, false),
            terminal_safe_text(&last, false),
            provider = provider.as_str(),
        ));
    }
    None
}

// Whether the session holds an event that is neither a failure nor a cancellation. `None`
// ends the scan: a bound is spent, `events` is not a real directory of the session, or an
// entry or event could not be read or decoded.
fn completed(directory: &Path, remaining: &mut u64, deadline: Instant) -> Option<bool> {
    let reader = Reader::open_unchecked(directory);
    for name in reader
        .event_names(EventListing::Complete {
            limit: EVENTS_PER_SESSION,
        })
        .ok()??
        .iter()
        .rev()
    {
        if Instant::now() >= deadline {
            return None;
        }
        // Strict UTF-8 within the remaining bytes: a lossy read could turn a damaged event
        // into a completed one.
        let Some(bytes) = reader.event(name).timeline(*remaining).ok()? else {
            continue;
        };
        *remaining = remaining.saturating_sub(bytes.len() as u64);
        let event = record::event(std::str::from_utf8(&bytes).ok()?).ok()?;
        if event.error.is_none() && !event.cancelled {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::primitives::write_json_atomic;
    use crate::native::session::schema::{SESSION_SCHEMA, SessionManifest};
    use std::fs;

    // The scan itself, without the caller's bounded wait, so that a slow runner cannot
    // turn an expected note into none.
    fn note(root: &Path, session: &str, provider: FirstPartyCli, version: &str) -> Option<String> {
        note_until(
            root,
            session,
            provider,
            version,
            Instant::now() + Duration::from_secs(120),
        )
    }

    // The caller's entry point answers from its own thread within the wait.
    #[test]
    fn the_callers_entry_point_returns_the_scans_note() {
        let root = seeded();
        assert_eq!(
            super::note(root.path(), "session-new", FirstPartyCli::Claude, "2.1.322"),
            note(root.path(), "session-new", FirstPartyCli::Claude, "2.1.322")
        );
    }

    fn session(root: &Path, id: &str, provider: &str, version: &str, created: u128) {
        let directory = root.join(id);
        fs::create_dir_all(directory.join("events")).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: id.to_owned(),
                provider: provider.to_owned(),
                provider_path: provider.into(),
                provider_version: version.to_owned(),
                workspace: root.to_owned(),
                title: "drift".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: created,
            },
        )
        .unwrap();
    }
    fn event(root: &Path, id: &str, name: &str, error: Option<&str>, cancelled: bool) {
        let mut event = serde_json::json!({
            "provider": "claude", "message": "answer", "error": error,
            "provider_session_id": null, "turn_id": null, "created_unix_ms": 1,
        });
        if cancelled {
            event["cancelled"] = true.into();
        }
        fs::write(root.join(id).join("events").join(name), event.to_string()).unwrap();
    }

    // The note names the newest other session of the same provider that completed a
    // request, and only when its version differs. Failed, cancelled and empty sessions,
    // other providers and the failing session itself are passed over.
    #[test]
    fn the_note_compares_with_the_last_session_that_completed_a_request() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        assert_eq!(
            note(root, "session-new", FirstPartyCli::Claude, "2.1.322"),
            None
        );
        session(root, "session-old", "claude", "2.1.319", 10);
        event(root, "session-old", "event-1-1.json", None, false);
        session(root, "session-failed", "claude", "2.1.321", 20);
        event(
            root,
            "session-failed",
            "event-1-1.json",
            Some("not sent"),
            false,
        );
        session(root, "session-cancelled", "claude", "2.1.321", 21);
        event(root, "session-cancelled", "event-1-1.json", None, true);
        session(root, "session-empty", "claude", "2.1.321", 22);
        session(root, "session-codex", "codex", "0.167.0", 30);
        event(root, "session-codex", "event-1-1.json", None, false);
        session(root, "session-new", "claude", "2.1.322", 40);
        event(root, "session-new", "event-1-1.json", None, false);
        let text = note(root, "session-new", FirstPartyCli::Claude, "2.1.322").unwrap();
        assert_eq!(
            text,
            "this session runs claude 2.1.322; the last claude session with a completed request on this state root, session-old, ran 2.1.319: if the provider changed its startup, `tabcli screen session-new` shows where it stopped"
        );
        assert_eq!(
            note(root, "session-new", FirstPartyCli::Claude, "2.1.319"),
            None
        );
        assert_eq!(note(root, "session-new", FirstPartyCli::Agy, "1.5.3"), None);
        // A newer completed session on the current version ends the comparison.
        session(root, "session-same", "claude", "2.1.322", 35);
        event(root, "session-same", "event-1-1.json", None, false);
        assert_eq!(
            note(root, "session-new", FirstPartyCli::Claude, "2.1.322"),
            None
        );
    }

    fn seeded() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        session(root.path(), "session-old", "claude", "2.1.319", 10);
        event(root.path(), "session-old", "event-1-1.json", None, false);
        assert!(note(root.path(), "session-new", FirstPartyCli::Claude, "2.1.322").is_some());
        root
    }

    // Anything the scan cannot read ends it without a note, because the unread session
    // could be the newest one: a manifest or event that is damaged or over its bound, an
    // `events` that is a link or holds too many entries, and a spent deadline. A directory
    // without a manifest is not a session and changes nothing.
    #[test]
    fn an_unreadable_or_unbounded_history_gives_no_note() {
        let claude = || note_args();
        fn note_args() -> (FirstPartyCli, &'static str) {
            (FirstPartyCli::Claude, "2.1.322")
        }
        let silent = |root: &Path| {
            let (provider, version) = claude();
            note(root, "session-new", provider, version).is_none()
        };

        let root = seeded();
        fs::create_dir(root.path().join("session-bare")).unwrap();
        assert!(!silent(root.path()));

        let root = seeded();
        fs::create_dir(root.path().join("session-damaged")).unwrap();
        fs::write(root.path().join("session-damaged/manifest.json"), b"{").unwrap();
        assert!(silent(root.path()));

        let root = seeded();
        fs::create_dir(root.path().join("session-large")).unwrap();
        fs::write(
            root.path().join("session-large/manifest.json"),
            vec![b' '; MANIFEST_BYTES as usize + 1],
        )
        .unwrap();
        assert!(silent(root.path()));

        let root = seeded();
        session(root.path(), "session-newer", "claude", "2.1.321", 20);
        fs::write(
            root.path().join("session-newer/events/event-1-1.json"),
            b"{",
        )
        .unwrap();
        assert!(silent(root.path()));

        let root = seeded();
        session(root.path(), "session-newer", "claude", "2.1.321", 20);
        for index in 0..=EVENTS_PER_SESSION {
            event(
                root.path(),
                "session-newer",
                &format!("event-{index}-1.json"),
                Some("failed"),
                false,
            );
        }
        assert!(silent(root.path()));

        #[cfg(unix)]
        {
            let root = seeded();
            let outside = tempfile::tempdir().unwrap();
            session(outside.path(), "session-other", "claude", "2.1.300", 1);
            event(
                outside.path(),
                "session-other",
                "event-1-1.json",
                None,
                false,
            );
            session(root.path(), "session-linked", "claude", "2.1.321", 20);
            fs::remove_dir(root.path().join("session-linked/events")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("session-other/events"),
                root.path().join("session-linked/events"),
            )
            .unwrap();
            assert!(silent(root.path()));
        }

        let root = seeded();
        session(root.path(), "session-newer", "claude", "2.1.321", 20);
        let mut invalid = br#"{"provider":"claude","message":""#.to_vec();
        invalid.extend_from_slice(&[0xff, 0xfe]);
        invalid.extend_from_slice(
            br#"","error":null,"provider_session_id":null,"turn_id":null,"created_unix_ms":1}"#,
        );
        fs::write(
            root.path().join("session-newer/events/event-1-1.json"),
            invalid,
        )
        .unwrap();
        assert!(silent(root.path()));

        #[cfg(unix)]
        {
            let root = seeded();
            session(root.path(), "session-newer", "claude", "2.1.321", 20);
            std::os::unix::fs::symlink(
                root.path().join("session-old/events/event-1-1.json"),
                root.path().join("session-newer/events/event-9-1.json"),
            )
            .unwrap();
            assert!(silent(root.path()));
        }

        // Entries that are not sessions count toward the bound on the state root.
        let root = seeded();
        for index in 0..ENTRIES {
            fs::write(root.path().join(format!("stray-{index}")), b"").unwrap();
        }
        assert!(silent(root.path()));

        let root = seeded();
        assert_eq!(
            note_until(
                root.path(),
                "session-new",
                FirstPartyCli::Claude,
                "2.1.322",
                Instant::now()
            ),
            None
        );
    }

    // The scan is bounded: completed requests older than the examined sessions say nothing.
    #[test]
    fn the_note_examines_a_bounded_number_of_sessions() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        session(root, "session-old", "claude", "2.1.319", 1);
        event(root, "session-old", "event-1-1.json", None, false);
        for index in 0..SESSIONS_EXAMINED {
            session(
                root,
                &format!("session-f{index}"),
                "claude",
                "2.1.321",
                10 + index as u128,
            );
        }
        assert_eq!(
            note(root, "session-new", FirstPartyCli::Claude, "2.1.322"),
            None
        );
        fs::remove_dir_all(root.join("session-f0")).unwrap();
        assert!(note(root, "session-new", FirstPartyCli::Claude, "2.1.322").is_some());
    }
}
