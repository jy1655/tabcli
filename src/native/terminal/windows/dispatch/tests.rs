use super::*;
use crate::native::failure_kind::{FailureKind, classify};
use crate::native::terminal::herdr::{self, Connection, Core};
use crate::native::terminal::record::HerdrHostBirth;
use crate::native::terminal::{DialogKey, HerdrSurface};
use anyhow::anyhow;
use std::{cell::RefCell, path::PathBuf, rc::Rc};

/// Answers every operation as a working surface would, and records which ones reached it.
struct Recording {
    kind: TerminalKind,
    calls: RefCell<Vec<&'static str>>,
}

impl Recording {
    fn new(kind: TerminalKind) -> Self {
        Self {
            kind,
            calls: RefCell::default(),
        }
    }

    fn record(&self, call: &'static str) {
        self.calls.borrow_mut().push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
}

impl WindowsTerminalAdapter for Recording {
    fn open_bound_tab(
        &self,
        _: &str,
        _: &Path,
        _: Instant,
        bind: BindSurface<'_>,
        _: UnbindSurface<'_>,
    ) -> Result<TerminalSession> {
        self.record("open");
        let mut session = TerminalSession::new(self.kind, "created".into());
        bind(&mut session)?;
        Ok(session)
    }
    fn send_file(&self, _: &TerminalSession, _: &Path, _: Instant) -> TerminalSendResult {
        self.record("send");
        Ok(())
    }
    fn read_screen(&self, _: &TerminalSession, _: Instant) -> Result<String> {
        self.record("screen");
        Ok("screen".into())
    }
    fn guarded_dialog_input(
        &self,
        _: &Path,
        _: &TerminalSession,
        _: &GuardedDialogInput,
        _: Instant,
    ) -> Result<bool> {
        self.record("dialog");
        Ok(true)
    }
    fn verify_surface(&self, _: &TerminalSession, _: Option<Duration>) -> Result<()> {
        self.record("verify");
        Ok(())
    }
    fn close_session_until(
        &self,
        _: &Path,
        _: &TerminalSession,
        _: Instant,
    ) -> Result<CloseOutcome> {
        self.record("close");
        Ok(CloseOutcome::Closed)
    }
    fn surface_present(&self, _: &Path, _: &TerminalSession, _: Duration) -> Result<bool> {
        self.record("present");
        Ok(true)
    }
}

fn input() -> GuardedDialogInput {
    GuardedDialogInput {
        expected_claim: None,
        expires_unix_ms: None,
        screen: "screen".into(),
        key: DialogKey::Enter,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// Runs the seven operations on a handle of `kind`, in order, and returns each answer as
/// text: `ok` or the refusal.
fn every_operation(adapters: &dyn Adapters, kind: TerminalKind) -> Vec<String> {
    let directory = Path::new("unused");
    let session = TerminalSession::new(kind, "handle".into());
    let answer = |result: Result<()>| match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("{error:#}"),
    };
    let bound = RefCell::new(0);
    vec![
        answer(
            open_bound_tab(
                adapters,
                kind,
                "command",
                directory,
                deadline(),
                |_| {
                    *bound.borrow_mut() += 1;
                    Ok(())
                },
                || panic!("a bound surface is not unbound"),
            )
            .map(|session| {
                assert_eq!(session.kind, kind);
                assert_eq!(*bound.borrow(), 1, "the binding reaches the adapter");
            }),
        ),
        answer(
            send_file(adapters, &session, Path::new("unused"), deadline()).map_err(|failure| {
                assert!(
                    !failure.delivery_may_have_occurred(),
                    "a lookup refusal sends nothing"
                );
                failure.into_error()
            }),
        ),
        answer(read_screen(adapters, &session, deadline()).map(|_| ())),
        answer(
            guarded_dialog_input(adapters, directory, &session, &input(), deadline()).map(|_| ()),
        ),
        answer(verify_surface(adapters, &session, None)),
        answer(close_session_until(adapters, directory, &session, deadline()).map(|_| ())),
        answer(surface_present(adapters, directory, &session, Duration::from_secs(1)).map(|_| ())),
    ]
}

// One lookup decides the adapter of every operation: a managed console handle reaches only
// the console adapter, a Herdr handle only the Herdr adapter, after Herdr's declared
// guarded-input refusal.
#[test]
fn each_windows_kind_reaches_only_its_own_adapter() {
    let dispatch = Dispatch::new(
        Recording::new(TerminalKind::WindowsConsole),
        Recording::new(TerminalKind::Herdr),
    );
    assert_eq!(
        every_operation(&dispatch, TerminalKind::WindowsConsole),
        ["ok"; 7]
    );
    assert_eq!(
        dispatch.console.calls(),
        [
            "open", "send", "screen", "dialog", "verify", "close", "present"
        ]
    );
    assert!(dispatch.herdr.calls().is_empty());

    let herdr_refusal = TerminalKind::Herdr
        .registration()
        .guarded_input_refusal
        .expect("Herdr declares its guarded-input refusal");
    assert_eq!(
        every_operation(&dispatch, TerminalKind::Herdr),
        ["ok", "ok", "ok", herdr_refusal, "ok", "ok", "ok"]
    );
    assert_eq!(
        dispatch.herdr.calls(),
        ["open", "send", "screen", "verify", "close", "present"]
    );
    assert_eq!(
        dispatch.console.calls().len(),
        7,
        "the console was not reached again"
    );
}

// A handle of a kind that Windows does not run (a record can name another platform) is
// refused by the lookup before any adapter, bind or unbind, with the unsupported hint; a
// refused send is known not sent.
#[test]
fn the_lookup_refuses_every_other_kind_before_any_adapter() {
    let dispatch = Dispatch::new(
        Recording::new(TerminalKind::WindowsConsole),
        Recording::new(TerminalKind::Herdr),
    );
    for kind in TerminalKind::ALL {
        if matches!(kind, TerminalKind::WindowsConsole | TerminalKind::Herdr) {
            continue;
        }
        let unsupported = format!("{} is not available on Windows", kind.display_name());
        let registration = kind.registration();
        let unsupported = unsupported.as_str();
        let expected = [
            unsupported,
            unsupported,
            registration.screen_read_refusal.unwrap_or(unsupported),
            registration.guarded_input_refusal.unwrap_or(unsupported),
            unsupported,
            unsupported,
            unsupported,
        ];
        assert_eq!(every_operation(&dispatch, kind), expected, "{kind:?}");
        let refused = open_bound_tab(
            &dispatch,
            kind,
            "command",
            Path::new("unused"),
            deadline(),
            |_| panic!("a refused kind is never bound"),
            || panic!("a refused kind is never unbound"),
        )
        .unwrap_err();
        assert_eq!(
            classify(&refused),
            Some(FailureKind::Unsupported),
            "{kind:?}"
        );
    }
    assert!(dispatch.console.calls().is_empty());
    assert!(dispatch.herdr.calls().is_empty());
}

/// Hands its one adapter out for every kind, so that only the declared refusals can keep
/// an operation from it.
struct EveryKind(Recording);

impl Adapters for EveryKind {
    fn adapter(&self, _: TerminalKind) -> Result<&dyn WindowsTerminalAdapter> {
        Ok(&self.0)
    }
}

// A kind's declared screen-read and guarded-input refusals answer before the lookup, as
// in the POSIX dispatch; a kind that declares none reaches its adapter.
#[test]
fn declared_refusals_answer_before_the_adapter() {
    for kind in TerminalKind::ALL {
        let adapters = EveryKind(Recording::new(kind));
        let session = TerminalSession::new(kind, "handle".into());
        let registration = kind.registration();
        let mut reached = Vec::new();
        match registration.screen_read_refusal {
            Some(refusal) => assert_eq!(
                read_screen(&adapters, &session, deadline())
                    .unwrap_err()
                    .to_string(),
                refusal
            ),
            None => {
                read_screen(&adapters, &session, deadline()).unwrap();
                reached.push("screen");
            }
        }
        match registration.guarded_input_refusal {
            Some(refusal) => assert_eq!(
                guarded_dialog_input(
                    &adapters,
                    Path::new("unused"),
                    &session,
                    &input(),
                    deadline()
                )
                .unwrap_err()
                .to_string(),
                refusal
            ),
            None => {
                guarded_dialog_input(
                    &adapters,
                    Path::new("unused"),
                    &session,
                    &input(),
                    deadline(),
                )
                .unwrap();
                reached.push("dialog");
            }
        }
        assert_eq!(adapters.0.calls(), reached, "{kind:?}");
    }
}

/// A core that nothing answers at: it records each endpoint asked for, and every connection
/// fails, as a core that is not running does.
struct UnansweredCore {
    endpoint: PathBuf,
    connects: Rc<RefCell<Vec<PathBuf>>>,
}

const UNANSWERED: &str = "the scripted Herdr Core does not answer";

impl Core for UnansweredCore {
    fn endpoint(&self) -> Result<PathBuf> {
        Ok(self.endpoint.clone())
    }
    fn connect(&self, socket: &Path, _: Instant) -> Result<Box<dyn Connection + '_>> {
        self.connects.borrow_mut().push(socket.to_owned());
        Err(anyhow!(UNANSWERED))
    }
    fn executable(&self) -> Result<PathBuf> {
        unreachable!("nothing is created without a core")
    }
    fn caller_terminal(&self) -> Option<String> {
        None
    }
    fn current_pid(&self) -> u32 {
        unreachable!("no caller is verified without a core")
    }
    fn parent_pid(&self, _: u32) -> Result<Option<u32>> {
        unreachable!("no caller is verified without a core")
    }
    fn process_start(&self, _: u32) -> Result<Option<HerdrHostBirth>> {
        unreachable!("no program is read without a core")
    }
    fn launch_nonce(&self) -> Result<String> {
        unreachable!("nothing is created without a core")
    }
}

// The Windows dispatch over the Herdr adapter itself, on a scripted core: every operation on
// a Herdr handle is the Herdr adapter's, on the recorded endpoint, and the console adapter is
// never reached. Without an answering core a send is known unsent and a close is unknown
// (never Missing); the guarded-input refusal opens no connection.
#[test]
fn herdr_handles_reach_their_core_through_the_windows_dispatch() {
    let socket = if cfg!(windows) {
        r"\\.\pipe\tabcli-dispatch-test"
    } else {
        "/tabcli-dispatch-test/core.sock"
    };
    let connects = Rc::default();
    let dispatch = Dispatch::new(
        Recording::new(TerminalKind::WindowsConsole),
        herdr::Adapter::with_core(UnansweredCore {
            endpoint: PathBuf::from("/tabcli-dispatch-test/endpoint.sock"),
            connects: Rc::clone(&connects),
        }),
    );
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-herdr");
    std::fs::create_dir(&directory).unwrap();
    let prompt = directory.join("prompt.txt");
    std::fs::write(&prompt, "hello").unwrap();
    let handle = TerminalSession::new(TerminalKind::Herdr, "t7".into())
        .with_herdr_core(
            HerdrSurface {
                socket: socket.into(),
                core_id: "core-a".into(),
                created_ms: 1,
                host_pid: 4,
                birth: HerdrHostBirth::Windows {
                    host_creation_time: 1,
                },
            },
            "nonce",
        )
        .with_binding(Some("session-herdr".into()));

    let error = open_bound_tab(
        &dispatch,
        TerminalKind::Herdr,
        "command",
        &directory,
        deadline(),
        |_| panic!("no terminal was created to bind"),
        || panic!("no terminal was created to unbind"),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("the Herdr terminal was not created"),
        "{error:#}"
    );
    let failure = send_file(&dispatch, &handle, &prompt, deadline()).unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
    let mut errors = vec![failure.into_error()];
    errors.push(read_screen(&dispatch, &handle, deadline()).unwrap_err());
    errors.push(verify_surface(&dispatch, &handle, Some(Duration::from_secs(5))).unwrap_err());
    errors.push(close_session_until(&dispatch, &directory, &handle, deadline()).unwrap_err());
    errors
        .push(surface_present(&dispatch, &directory, &handle, Duration::from_secs(5)).unwrap_err());
    for error in errors {
        assert!(format!("{error:#}").contains(UNANSWERED), "{error:#}");
    }
    assert_eq!(
        guarded_dialog_input(&dispatch, &directory, &handle, &input(), deadline())
            .unwrap_err()
            .to_string(),
        TerminalKind::Herdr
            .registration()
            .guarded_input_refusal
            .unwrap()
    );

    assert!(dispatch.console.calls().is_empty());
    let mut expected = vec![PathBuf::from("/tabcli-dispatch-test/endpoint.sock")];
    expected.extend(vec![PathBuf::from(socket); 5]);
    assert_eq!(*connects.borrow(), expected);
}
