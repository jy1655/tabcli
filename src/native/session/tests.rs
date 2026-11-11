thread_local! {
    pub(in crate::native) static FIXED_UNIX_MS: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) };
}

pub(super) fn with_fixed_time<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(Option<u128>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIXED_UNIX_MS.set(self.0);
        }
    }
    let _reset = Reset(FIXED_UNIX_MS.replace(Some(123456789)));
    f()
}

// The general listing of `events`: regular files with an event name, in byte order of their
// names; any other entry is passed over, and a missing directory lists nothing.
#[test]
fn the_event_listing_keeps_regular_event_files_in_name_order() {
    let directory = tempfile::tempdir().unwrap();
    let reader = super::Reader::open_unchecked(directory.path());
    assert!(reader.events().unwrap().is_empty());
    let events = directory.path().join(super::EVENTS_DIRECTORY);
    std::fs::create_dir(&events).unwrap();
    for name in [
        "event-3-1.json",
        "event-10-1.json",
        "event-1-1.json",
        "event-2-1.json",
        "notes.json",
        "event-4-1.json.tmp",
        "unpublished-event-5-1.json",
    ] {
        std::fs::write(events.join(name), b"{}").unwrap();
    }
    std::fs::create_dir(events.join("event-0-1.json")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(events.join("event-1-1.json"), events.join("event-9-1.json"))
        .unwrap();
    let expected = [
        "event-1-1.json",
        "event-10-1.json",
        "event-2-1.json",
        "event-3-1.json",
    ]
    .map(|name| events.join(name));
    assert_eq!(reader.events().unwrap(), expected);
    assert_eq!(super::event_paths(directory.path()).unwrap(), expected);
}

// The bounded listing a history scan uses: a missing directory lists nothing, the bound
// counts every entry, and an entry with an event name that is not a regular file is an
// error rather than passed over.
#[test]
fn the_bounded_event_listing_answers_completely_or_not_at_all() {
    let directory = tempfile::tempdir().unwrap();
    let reader = super::Reader::open_unchecked(directory.path());
    assert_eq!(
        reader
            .event_names(super::EventListing::Complete { limit: 0 })
            .unwrap(),
        Some(Vec::new())
    );
    let events = directory.path().join(super::EVENTS_DIRECTORY);
    std::fs::create_dir(&events).unwrap();
    for name in ["event-2-1.json", "event-1-1.json", "notes.json"] {
        std::fs::write(events.join(name), b"{}").unwrap();
    }
    assert_eq!(
        reader
            .event_names(super::EventListing::Complete { limit: 3 })
            .unwrap(),
        Some(vec![
            "event-1-1.json".to_owned(),
            "event-2-1.json".to_owned()
        ])
    );
    assert_eq!(
        reader
            .event_names(super::EventListing::Complete { limit: 2 })
            .unwrap(),
        None
    );
    std::fs::create_dir(events.join("not-an-event")).unwrap();
    assert_eq!(
        reader
            .event_names(super::EventListing::Complete { limit: 4 })
            .unwrap(),
        Some(vec![
            "event-1-1.json".to_owned(),
            "event-2-1.json".to_owned()
        ])
    );
    std::fs::create_dir(events.join("event-0-1.json")).unwrap();
    assert_eq!(
        format!(
            "{:#}",
            reader
                .event_names(super::EventListing::Complete { limit: 5 })
                .unwrap_err()
        ),
        format!(
            "refusing non-regular session file: {}",
            events.join("event-0-1.json").display()
        )
    );
    std::fs::remove_dir_all(&events).unwrap();
    std::fs::write(&events, b"not a directory").unwrap();
    assert_eq!(
        format!(
            "{:#}",
            reader
                .event_names(super::EventListing::Complete { limit: 5 })
                .unwrap_err()
        ),
        "events is not a directory"
    );
}

#[test]
fn the_event_listing_refuses_an_events_path_that_is_not_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join(super::EVENTS_DIRECTORY),
        b"not a directory",
    )
    .unwrap();
    let reader = super::Reader::open_unchecked(directory.path());
    assert_eq!(
        format!("{:#}", reader.events().unwrap_err()),
        "events is not a directory"
    );
}

#[cfg(unix)]
#[test]
fn the_event_listing_refuses_a_linked_events_directory() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("event-1-1.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        directory.path().join(super::EVENTS_DIRECTORY),
    )
    .unwrap();
    let reader = super::Reader::open_unchecked(directory.path());
    assert_eq!(
        format!("{:#}", reader.events().unwrap_err()),
        "events directory is a symlink"
    );
}
