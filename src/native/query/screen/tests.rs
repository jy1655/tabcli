use super::*;
use crate::native::failure_kind::{FailureKind, classify};
use crate::native::query::test_support::entries;
use crate::native::session::test_support::{
    CLOSED_STATUS_FILE, TERMINAL_CLOSING_FILE, TERMINAL_HANDLE_FILE, TERMINAL_TOMBSTONE_FILE,
    write_test_manifest,
};
use crate::native::session::{SessionStatus, Store, update_status};
use std::cell::RefCell;
use std::fs;

const ID: &str = "session-screen";

#[derive(Debug, PartialEq)]
enum Call {
    Verify {
        directory: PathBuf,
        id: String,
        surface: terminal::TerminalSession,
        deadline: Instant,
        requested: Duration,
    },
    Read {
        surface: terminal::TerminalSession,
        deadline: Instant,
    },
}

const TEXT: &str = "$ codex\n> Allow this directory? (y/n)\n";

/// Records what reaches the terminal and proves that no lifecycle lock is held then.
struct Fake {
    directory: PathBuf,
    /// The outcome of each ownership check in turn; a check past the list passes.
    verified: Vec<std::result::Result<(), &'static str>>,
    screen: std::result::Result<String, &'static str>,
    /// What happens to the session's records while the terminal answers the read.
    during_read: Option<fn(&Path)>,
    calls: RefCell<Vec<Call>>,
    /// The pid and birth of the owner each passing check verified.
    verified_owners: RefCell<Vec<(u32, Option<u64>)>>,
}

impl Fake {
    fn new(fixture: &Fixture) -> Self {
        Self {
            directory: fixture.directory.clone(),
            verified: Vec::new(),
            screen: Ok(TEXT.to_owned()),
            during_read: None,
            calls: RefCell::new(Vec::new()),
            verified_owners: RefCell::new(Vec::new()),
        }
    }

    fn assert_unlocked(&self) {
        assert!(
            Store::open_unchecked(&self.directory)
                .try_lock()
                .unwrap()
                .is_some(),
            "the lifecycle lock is held while the terminal is addressed"
        );
    }
}

impl SurfaceAccess for Fake {
    fn verify(
        &self,
        directory: &Path,
        id: &str,
        surface: &terminal::TerminalSession,
        deadline: Instant,
        requested: Duration,
    ) -> Result<Option<NativeSessionOwner>> {
        self.assert_unlocked();
        let checks = self
            .calls
            .borrow()
            .iter()
            .filter(|call| matches!(call, Call::Verify { .. }))
            .count();
        self.calls.borrow_mut().push(Call::Verify {
            directory: directory.to_owned(),
            id: id.to_owned(),
            surface: surface.clone(),
            deadline,
            requested,
        });
        self.verified
            .get(checks)
            .copied()
            .unwrap_or(Ok(()))
            .map_err(|error| anyhow::anyhow!(error))?;
        // As the real check does, verify whatever owner the record names now.
        let owner =
            observation::SessionEvidence::read_owner(&Reader::open_unchecked(directory)).unwrap();
        if let Some(owner) = &owner {
            self.verified_owners
                .borrow_mut()
                .push((owner.pid, owner.process_start_seconds));
        }
        Ok(owner)
    }

    fn read(&self, surface: &terminal::TerminalSession, deadline: Instant) -> Result<String> {
        self.assert_unlocked();
        self.calls.borrow_mut().push(Call::Read {
            surface: surface.clone(),
            deadline,
        });
        if let Some(change) = self.during_read {
            change(&self.directory);
        }
        self.screen.clone().map_err(|error| anyhow::anyhow!(error))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
}

impl Fixture {
    fn new(state: SessionState, surface: Option<&terminal::TerminalSession>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(ID);
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory, "codex");
        update_status(&directory, state, None, None).unwrap();
        if let Some(surface) = surface {
            Store::open_unchecked(&directory)
                .write_terminal(surface)
                .unwrap();
        }
        Store::open_unchecked(&directory)
            .write_owner(&owner(4242, 1_790_000_000))
            .unwrap();
        // The lifecycle lock exists, as in every current session, so that the query's
        // shared lock is real and the fake's exclusive probe creates nothing.
        drop(Store::open_unchecked(&directory).try_lock().unwrap());
        Self { root, directory }
    }

    fn observe(&self, access: &Fake) -> (Result<Screen>, Instant, Duration) {
        let before = entries(self.root.path());
        let observed = self.observe_while_records_change(access);
        assert_eq!(
            entries(self.root.path()),
            before,
            "screen changed the state root"
        );
        observed
    }

    /// For a test whose fake changes the records itself, as a concurrent writer would.
    fn observe_while_records_change(&self, access: &Fake) -> (Result<Screen>, Instant, Duration) {
        let requested = Duration::from_secs(7);
        let deadline = Instant::now() + requested;
        let outcome = observe(&self.directory, ID, deadline, requested, access);
        (outcome, deadline, requested)
    }
}

/// An owner record of this session; a check of the fake verifies whichever the record names.
fn owner(pid: u32, start_seconds: u64) -> NativeSessionOwner {
    NativeSessionOwner {
        pid,
        managed_session_id: Some(ID.into()),
        process_start_seconds: Some(start_seconds),
        process_start_microseconds: Some(0),
        ..NativeSessionOwner::default()
    }
}

fn handle(kind: terminal::TerminalKind, bound_to: Option<&str>) -> terminal::TerminalSession {
    terminal::TerminalSession {
        kind,
        id: "surface-1".into(),
        tab_id: None,
        window_id: Some("1001".into()),
        managed_session_id: bound_to.map(str::to_owned),
        windows_process_identity: None,
        wezterm_mux: None,
    }
}

/// The kind whose screen this platform reads, when it reads any.
#[cfg(any(target_os = "macos", windows))]
fn host_kind() -> terminal::TerminalKind {
    if cfg!(windows) {
        terminal::TerminalKind::WindowsConsole
    } else {
        terminal::TerminalKind::Iterm2
    }
}

fn refusal(outcome: Result<Screen>) -> anyhow::Error {
    match outcome {
        Ok(_) => panic!("the screen was read"),
        Err(error) => error,
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_bound_surface_returns_the_adapter_text_in_every_unclosed_state() {
    // The stored state never decides: an exited or failed session whose owner still holds
    // its surface is read like a working one, and the identity check decides for all.
    for state in [
        SessionState::Launching,
        SessionState::Running,
        SessionState::AwaitingInitialInput,
        SessionState::Ready,
        SessionState::Claimed,
        SessionState::Working,
        SessionState::ResumePending,
        SessionState::Exited,
        SessionState::Failed,
        SessionState::Unknown("future-state".into()),
    ] {
        let surface = handle(host_kind(), Some(ID));
        let fixture = Fixture::new(state.clone(), Some(&surface));
        let access = Fake::new(&fixture);
        let (outcome, deadline, requested) = fixture.observe(&access);
        let screen = outcome.unwrap_or_else(|error| panic!("{state}: {error:#}"));
        assert_eq!(screen.text, TEXT);
        assert_eq!(screen.kind, host_kind());
        assert!(screen.observed_unix_ms > 0);
        // A proof, one read and the same proof again, all with the caller's one deadline;
        // no retry.
        let verify = Call::Verify {
            directory: fixture.directory.clone(),
            id: ID.into(),
            surface: surface.clone(),
            deadline,
            requested,
        };
        let read = Call::Read { surface, deadline };
        let ordinary = [
            verify,
            read,
            Call::Verify {
                directory: fixture.directory.clone(),
                id: ID.into(),
                surface: handle(host_kind(), Some(ID)),
                deadline,
                requested,
            },
        ];
        assert_eq!(*access.calls.borrow(), ordinary, "{state}");
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_failed_identity_check_or_read_returns_no_text_and_is_not_retried() {
    for state in [
        SessionState::Working,
        SessionState::Exited,
        SessionState::Failed,
    ] {
        let surface = handle(host_kind(), Some(ID));
        let fixture = Fixture::new(state.clone(), Some(&surface));
        let mut access = Fake::new(&fixture);
        access.verified = vec![Err(
            "terminal surface is attached to a different native-session TTY",
        )];
        let error = refusal(fixture.observe(&access).0);
        assert_eq!(
            format!("{error:#}"),
            "the recorded terminal surface was not verified as this session's; nothing was read: terminal surface is attached to a different native-session TTY",
            "{state}"
        );
        assert!(
            matches!(access.calls.borrow().as_slice(), [Call::Verify { .. }]),
            "{state}"
        );

        let mut access = Fake::new(&fixture);
        access.screen = Err("managed iTerm2 screen is missing");
        let error = refusal(fixture.observe(&access).0);
        assert_eq!(
            format!("{error:#}"),
            format!(
                "{} did not return the session's screen: managed iTerm2 screen is missing",
                host_kind().display_name()
            )
        );
        assert!(matches!(
            access.calls.borrow().as_slice(),
            [Call::Verify { .. }, Call::Read { .. }]
        ));
    }
}

/// The confirmation error of a read whose text is withheld, with its cause.
#[cfg(any(target_os = "macos", windows))]
fn withheld(access: &Fake, fixture: &Fixture) -> String {
    let error = format!(
        "{:#}",
        refusal(fixture.observe_while_records_change(access).0)
    );
    assert!(
        error.starts_with(
            "the terminal surface could not be confirmed as this session's after the read; the text is withheld: "
        ),
        "{error}"
    );
    assert!(!error.contains("Allow this directory"), "{error}");
    error
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_second_ownership_check_that_fails_withholds_the_text() {
    let surface = handle(host_kind(), Some(ID));
    let fixture = Fixture::new(SessionState::Working, Some(&surface));
    let mut access = Fake::new(&fixture);
    access.verified = vec![
        Ok(()),
        Err("native-session owner birth, TTY, or foreground identity changed"),
    ];
    let before = entries(fixture.root.path());
    assert!(
        withheld(&access, &fixture)
            .ends_with("native-session owner birth, TTY, or foreground identity changed")
    );
    assert_eq!(entries(fixture.root.path()), before);
    assert!(matches!(
        access.calls.borrow().as_slice(),
        [Call::Verify { .. }, Call::Read { .. }, Call::Verify { .. }]
    ));
}

// The handle stays the same and each check passes for the owner its record names, but the
// record names another incarnation after the read: another process of the session, or the
// same pid with a later birth.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn an_owner_replaced_during_the_read_withholds_the_text() {
    type Change = fn(&Path);
    let replacements: [(Change, (u32, Option<u64>)); 2] = [
        (
            |directory| {
                Store::open_unchecked(directory)
                    .write_owner(&owner(5151, 1_790_000_000))
                    .unwrap()
            },
            (5151, Some(1_790_000_000)),
        ),
        (
            |directory| {
                Store::open_unchecked(directory)
                    .write_owner(&owner(4242, 1_790_000_009))
                    .unwrap()
            },
            (4242, Some(1_790_000_009)),
        ),
    ];
    for (replace, second) in replacements {
        let surface = handle(host_kind(), Some(ID));
        let fixture = Fixture::new(SessionState::Working, Some(&surface));
        let mut access = Fake::new(&fixture);
        access.during_read = Some(replace);
        let error = withheld(&access, &fixture);
        assert!(
            error
                .ends_with("the owner verified after the read is not the owner verified before it"),
            "{error}"
        );
        assert_eq!(
            *access.verified_owners.borrow(),
            [(4242, Some(1_790_000_000)), second]
        );
        assert!(matches!(
            access.calls.borrow().as_slice(),
            [
                Call::Verify { surface: first, .. },
                Call::Read { .. },
                Call::Verify { surface: again, .. },
            ] if first == &surface && again == &surface
        ));
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_close_during_the_read_withholds_the_text_without_a_second_terminal_call() {
    type Change = fn(&Path);
    let closes: [(Change, &str); 2] = [
        (
            |directory| update_status(directory, SessionState::Closed, None, None).unwrap(),
            "session session-screen is closed; its terminal surface is not read",
        ),
        (
            |directory| {
                fs::rename(
                    directory.join(TERMINAL_HANDLE_FILE),
                    directory.join(TERMINAL_CLOSING_FILE),
                )
                .unwrap()
            },
            "the terminal surface of session session-screen is closed or being closed",
        ),
    ];
    for (close, cause) in closes {
        let surface = handle(host_kind(), Some(ID));
        let fixture = Fixture::new(SessionState::Working, Some(&surface));
        let mut access = Fake::new(&fixture);
        access.during_read = Some(close);
        assert!(withheld(&access, &fixture).ends_with(cause), "{cause}");
        assert!(
            matches!(
                access.calls.borrow().as_slice(),
                [Call::Verify { .. }, Call::Read { .. }]
            ),
            "{cause}"
        );
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_handle_record_that_changes_during_the_read_withholds_the_text() {
    type Change = fn(&Path);
    let changes: [(Change, &str); 2] = [
        (
            |directory| {
                let mut replaced = handle(host_kind(), Some(ID));
                replaced.id = "surface-2".into();
                Store::open_unchecked(directory)
                    .write_terminal(&replaced)
                    .unwrap();
            },
            "the recorded terminal handle changed during the read",
        ),
        (
            |directory| fs::remove_file(directory.join(TERMINAL_HANDLE_FILE)).unwrap(),
            "has no active terminal handle",
        ),
    ];
    for (change, cause) in changes {
        let surface = handle(host_kind(), Some(ID));
        let fixture = Fixture::new(SessionState::Working, Some(&surface));
        let mut access = Fake::new(&fixture);
        access.during_read = Some(change);
        let error = withheld(&access, &fixture);
        assert!(error.contains(cause), "{error}");
        assert!(
            matches!(
                access.calls.borrow().as_slice(),
                [Call::Verify { .. }, Call::Read { .. }]
            ),
            "{error}"
        );
    }
}

// The binding read after the capture runs late in the budget: a busy Snapshot is not
// retried past the deadline (250 ms otherwise).
#[test]
fn a_busy_snapshot_is_not_retried_past_the_deadline() {
    let surface = handle(terminal::TerminalKind::Iterm2, Some(ID));
    let fixture = Fixture::new(SessionState::Working, Some(&surface));
    let _writer = Store::open_unchecked(&fixture.directory)
        .try_lock()
        .unwrap()
        .unwrap();
    let started = Instant::now();
    let error =
        bound_surface(&Reader::open_unchecked(&fixture.directory), ID, started).unwrap_err();
    assert!(error.is::<SnapshotBusy>(), "{error:#}");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn closed_missing_and_unreadable_surfaces_are_refused_before_any_terminal_call() {
    let bound = || handle(terminal::TerminalKind::Iterm2, Some(ID));
    type Seed = fn(&Path);
    let cases: [(&str, SessionState, bool, Seed, &str); 6] = [
        (
            "closed",
            SessionState::Closed,
            true,
            |_| {},
            "session session-screen is closed; its terminal surface is not read",
        ),
        (
            "close tombstone ahead of status",
            SessionState::Working,
            true,
            |directory| {
                let closed = SessionStatus {
                    state: SessionState::Closed,
                    generation: 9,
                    updated_unix_ms: 1,
                    exit_code: None,
                    error: None,
                    residual_surface: None,
                };
                fs::write(
                    directory.join(CLOSED_STATUS_FILE),
                    serde_json::to_vec(&closed).unwrap(),
                )
                .unwrap();
            },
            "session session-screen is closed; its terminal surface is not read",
        ),
        (
            "surface tombstone",
            SessionState::Working,
            true,
            |directory| {
                fs::write(
                    directory.join(TERMINAL_TOMBSTONE_FILE),
                    br#"{"consumed":true}"#,
                )
                .unwrap();
            },
            "the terminal surface of session session-screen is closed or being closed",
        ),
        (
            "claimed close handle",
            SessionState::Working,
            false,
            |directory| {
                fs::write(
                    directory.join(TERMINAL_CLOSING_FILE),
                    serde_json::to_vec(&handle(terminal::TerminalKind::Iterm2, Some(ID))).unwrap(),
                )
                .unwrap();
            },
            "the terminal surface of session session-screen is closed or being closed",
        ),
        (
            "never bound",
            SessionState::Launching,
            false,
            |_| {},
            "session session-screen (launching) has no active terminal handle; a launching session records it once its surface is bound",
        ),
        (
            "unreadable handle",
            SessionState::Ready,
            false,
            |directory| fs::write(directory.join(TERMINAL_HANDLE_FILE), "not json").unwrap(),
            "the terminal surface record of session session-screen is unreadable",
        ),
    ];
    for (label, state, with_handle, seed, expected) in cases {
        let surface = bound();
        let fixture = Fixture::new(state, with_handle.then_some(&surface));
        seed(&fixture.directory);
        let access = Fake::new(&fixture);
        let error = format!("{:#}", refusal(fixture.observe(&access).0));
        assert!(error.starts_with(expected), "{label}: {error}");
        assert!(access.calls.borrow().is_empty(), "{label}");
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_handle_not_bound_to_this_session_is_refused_before_any_terminal_call() {
    // Terminal.app on macOS: only a legacy iTerm2 handle may lack its binding.
    let kind = if cfg!(windows) {
        terminal::TerminalKind::WindowsConsole
    } else {
        terminal::TerminalKind::AppleTerminal
    };
    let mut empty = handle(kind, Some(ID));
    empty.id = String::new();
    for surface in [
        handle(kind, Some("session-other")),
        handle(kind, None),
        empty,
    ] {
        let fixture = Fixture::new(SessionState::Working, Some(&surface));
        let access = Fake::new(&fixture);
        let error = refusal(fixture.observe(&access).0);
        assert_eq!(
            error.to_string(),
            "the recorded terminal handle is not bound to session session-screen",
            "{surface:?}"
        );
        assert!(access.calls.borrow().is_empty());
    }
}

#[test]
fn unsupported_and_declared_refusals_answer_before_any_terminal_call() {
    let mut refused = Vec::new();
    for kind in terminal::TerminalKind::ALL {
        let surface = handle(kind, Some(ID));
        let fixture = Fixture::new(SessionState::Working, Some(&surface));
        let access = Fake::new(&fixture);
        let outcome = fixture.observe(&access).0;
        let declared = kind.registration().screen_read_refusal;
        if kind.supported_on_this_platform() && declared.is_none() {
            outcome.unwrap_or_else(|error| panic!("{kind:?}: {error:#}"));
            assert_eq!(access.calls.borrow().len(), 3, "{kind:?}");
            continue;
        }
        let error = refusal(outcome);
        let text = error.to_string();
        if kind.supported_on_this_platform() {
            assert_eq!(
                text,
                format!(
                    "{} screen reads are unsupported: {}",
                    kind.display_name(),
                    declared.unwrap()
                )
            );
        } else {
            assert_eq!(
                text,
                format!(
                    "the recorded terminal, {}, is not supported on this platform",
                    kind.display_name()
                )
            );
        }
        assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{kind:?}");
        assert!(access.calls.borrow().is_empty(), "{kind:?}");
        refused.push(kind);
    }
    let expected: &[terminal::TerminalKind] = if cfg!(target_os = "macos") {
        &[
            terminal::TerminalKind::Ghostty,
            terminal::TerminalKind::Warp,
            terminal::TerminalKind::WindowsConsole,
        ]
    } else if cfg!(windows) {
        &[
            terminal::TerminalKind::Iterm2,
            terminal::TerminalKind::AppleTerminal,
            terminal::TerminalKind::Ghostty,
            terminal::TerminalKind::Warp,
            terminal::TerminalKind::WezTerm,
        ]
    } else {
        &terminal::TerminalKind::ALL
    };
    assert_eq!(refused, expected);
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn an_oversized_screen_fails_without_returning_its_text() {
    let surface = handle(host_kind(), Some(ID));
    let fixture = Fixture::new(SessionState::Working, Some(&surface));
    let mut access = Fake::new(&fixture);
    access.screen = Ok("x".repeat(MAX_SCREEN_BYTES));
    assert_eq!(
        fixture.observe(&access).0.unwrap().text.len(),
        MAX_SCREEN_BYTES
    );
    let mut access = Fake::new(&fixture);
    access.screen = Ok(format!("{}é", "y".repeat(MAX_SCREEN_BYTES - 1)));
    let error = format!("{:#}", refusal(fixture.observe(&access).0));
    assert_eq!(
        error,
        format!(
            "the screen is {} bytes, over the {MAX_SCREEN_BYTES}-byte limit; nothing is returned",
            MAX_SCREEN_BYTES + 1
        )
    );
    assert!(!error.contains("yy"));
}

// The production access makes the check `tell` makes: its deadline is checked before
// anything, and a handle bound to another session is refused before any terminal call.
#[test]
fn the_installed_identity_check_is_tells_and_refuses_without_a_terminal_call() {
    let fixture = Fixture::new(SessionState::Working, None);
    let requested = Duration::from_secs(3);
    let error = Installed
        .verify(
            &fixture.directory,
            ID,
            &handle(terminal::TerminalKind::Iterm2, Some(ID)),
            Instant::now(),
            requested,
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "timed out after 3 seconds");
    assert_eq!(classify(&error), Some(FailureKind::TimedOut));
    let error = Installed
        .verify(
            &fixture.directory,
            ID,
            &handle(terminal::TerminalKind::Iterm2, Some("session-other")),
            Instant::now() + requested,
            requested,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "terminal handle belongs to managed session session-other, not session-screen"
    );
}

#[test]
fn json_carries_the_text_exactly_with_its_terminal_and_time() {
    let text = "한글 row\n\u{1b}[31m\trow two\r\n";
    let value = value(
        ID,
        &Screen {
            kind: terminal::TerminalKind::AppleTerminal,
            text: text.to_owned(),
            observed_unix_ms: 1_790_000_000_000,
        },
    );
    assert_eq!(
        value,
        json!({
            "schema_version": 1, "ok": true, "session": ID, "terminal": "apple-terminal",
            "observed_unix_ms": 1_790_000_000_000_u64, "screen": text,
        })
    );
}

#[test]
fn parse_defaults_to_a_ten_second_text_read() {
    let parse = |args: &[&str]| match super::parse(
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
    ) {
        Ok(NativeCommand::Screen(request)) => request,
        other => panic!("{other:?}"),
    };
    let request = parse(&[ID]);
    assert_eq!(
        (request.id.as_str(), request.timeout, request.json),
        (ID, Duration::from_secs(10), false)
    );
    let request = parse(&[ID, "--json", "--timeout-secs", "3"]);
    assert_eq!(
        (request.id.as_str(), request.timeout, request.json),
        (ID, Duration::from_secs(3), true)
    );
}
