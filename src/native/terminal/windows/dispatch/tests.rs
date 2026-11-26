use super::*;
use crate::native::terminal::DialogKey;
use std::cell::{Cell, RefCell};

/// Answers every operation as a working surface would, and records which ones reached it.
#[derive(Default)]
struct Recording {
    calls: RefCell<Vec<&'static str>>,
}

impl Recording {
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
        kind: TerminalKind,
        _: &str,
        _: &Path,
        _: Instant,
        bind: BindSurface<'_>,
        _: UnbindSurface<'_>,
    ) -> Result<TerminalSession> {
        self.record("open");
        let mut session = TerminalSession::new(kind, "created".into());
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
    fn launch_surface(
        &self,
        _: &Path,
        _: &TerminalSession,
        _: Duration,
    ) -> Result<crate::native::terminal::LaunchSurface> {
        self.record("launch");
        Ok(crate::native::terminal::LaunchSurface::Present)
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

fn recording_dispatch() -> Dispatch<Recording, Recording> {
    Dispatch::new(Recording::default(), Recording::default())
}

/// Runs the eight operations on a handle of `kind`, in order, and returns each answer as
/// text: `ok` or the refusal. An open passes the asked kind to its adapter and binds once.
fn every_operation(adapters: &dyn Adapters, kind: TerminalKind) -> Vec<String> {
    let directory = Path::new("unused");
    let session = TerminalSession::new(kind, "handle".into());
    let answer = |result: Result<()>| match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("{error:#}"),
    };
    let bound = Cell::new(0);
    vec![
        answer(
            open_bound_tab(
                adapters,
                kind,
                "command",
                directory,
                deadline(),
                |_| {
                    bound.set(bound.get() + 1);
                    Ok(())
                },
                || panic!("a bound surface is not unbound"),
            )
            .map(|session| {
                assert_eq!(session.kind, kind, "the asked kind reaches the adapter");
                assert_eq!(bound.get(), 1, "the binding reaches the adapter");
            }),
        ),
        answer(
            send_file(adapters, &session, Path::new("unused"), deadline())
                .map_err(|failure| failure.into_error()),
        ),
        answer(read_screen(adapters, &session, deadline()).map(|_| ())),
        answer(
            guarded_dialog_input(adapters, directory, &session, &input(), deadline()).map(|_| ()),
        ),
        answer(verify_surface(adapters, &session, None)),
        answer(close_session_until(adapters, directory, &session, deadline()).map(|_| ())),
        answer(surface_present(adapters, directory, &session, Duration::from_secs(1)).map(|_| ())),
        answer(launch_surface(adapters, directory, &session, Duration::from_secs(1)).map(|_| ())),
    ]
}

// One lookup decides the adapter of every operation: a managed console handle reaches only
// the console adapter, a Herdr handle only the Herdr adapter, after Herdr's declared
// guarded-input refusal.
#[test]
fn each_windows_kind_reaches_only_its_own_adapter() {
    let dispatch = recording_dispatch();
    assert_eq!(
        every_operation(&dispatch, TerminalKind::WindowsConsole),
        ["ok"; 8]
    );
    assert_eq!(
        dispatch.console.calls(),
        [
            "open", "send", "screen", "dialog", "verify", "close", "present", "launch"
        ]
    );
    assert!(dispatch.herdr.calls().is_empty());

    let herdr_refusal = TerminalKind::Herdr
        .registration()
        .guarded_input_refusal
        .expect("Herdr declares its guarded-input refusal");
    assert_eq!(
        every_operation(&dispatch, TerminalKind::Herdr),
        ["ok", "ok", "ok", herdr_refusal, "ok", "ok", "ok", "ok"]
    );
    assert_eq!(
        dispatch.herdr.calls(),
        [
            "open", "send", "screen", "verify", "close", "present", "launch"
        ]
    );
    assert_eq!(
        dispatch.console.calls().len(),
        8,
        "the console was not reached again"
    );
}

#[test]
fn expired_close_reaches_no_windows_adapter() {
    let dispatch = recording_dispatch();
    let session = TerminalSession::new(TerminalKind::Herdr, "t7".into());
    let error =
        close_session_until(&dispatch, Path::new("unused"), &session, Instant::now()).unwrap_err();
    assert!(error.to_string().contains("close deadline expired"));
    assert!(dispatch.console.calls().is_empty());
    assert!(dispatch.herdr.calls().is_empty());
}

// A handle of a kind that Windows does not run (a record can name another platform) stays
// with the managed console, as before the lookup: the console's open gets the asked kind and
// refuses it (a Windows test pins that refusal), and its helper refuses the handle (no test
// runs the helper on such a handle). Only the kind's declared screen and guarded-input
// refusals answer before it.
#[test]
fn every_other_kind_stays_with_the_managed_console() {
    for kind in TerminalKind::ALL {
        if matches!(kind, TerminalKind::WindowsConsole | TerminalKind::Herdr) {
            continue;
        }
        let dispatch = recording_dispatch();
        let registration = kind.registration();
        let mut reached = vec!["open", "send"];
        let screen = registration.screen_read_refusal.unwrap_or_else(|| {
            reached.push("screen");
            "ok"
        });
        let dialog = registration.guarded_input_refusal.unwrap_or_else(|| {
            reached.push("dialog");
            "ok"
        });
        reached.extend(["verify", "close", "present", "launch"]);
        assert_eq!(
            every_operation(&dispatch, kind),
            ["ok", "ok", screen, dialog, "ok", "ok", "ok", "ok"],
            "{kind:?}"
        );
        assert_eq!(dispatch.console.calls(), reached, "{kind:?}");
        assert!(dispatch.herdr.calls().is_empty(), "{kind:?}");
    }
}

/// Hands its one adapter out for every kind and counts the lookups, so that only the
/// declared refusals can keep an operation from it.
#[derive(Default)]
struct EveryKind {
    adapter: Recording,
    lookups: Cell<usize>,
}

impl Adapters for EveryKind {
    fn adapter(&self, _: TerminalKind) -> &dyn WindowsTerminalAdapter {
        self.lookups.set(self.lookups.get() + 1);
        &self.adapter
    }
}

// A kind's declared screen-read and guarded-input refusals answer before the lookup, as in
// the POSIX dispatch: a declared refusal looks up no adapter, and a kind that declares none
// looks up its adapter once and reaches it.
#[test]
fn declared_refusals_answer_before_the_lookup() {
    for kind in TerminalKind::ALL {
        let session = TerminalSession::new(kind, "handle".into());
        let registration = kind.registration();

        let adapters = EveryKind::default();
        let screen = read_screen(&adapters, &session, deadline());
        match registration.screen_read_refusal {
            Some(refusal) => {
                assert_eq!(screen.unwrap_err().to_string(), refusal, "{kind:?}");
                assert_eq!(adapters.lookups.get(), 0, "{kind:?}");
                assert!(adapters.adapter.calls().is_empty(), "{kind:?}");
            }
            None => {
                screen.unwrap();
                assert_eq!(adapters.lookups.get(), 1, "{kind:?}");
                assert_eq!(adapters.adapter.calls(), ["screen"], "{kind:?}");
            }
        }

        let adapters = EveryKind::default();
        let dialog = guarded_dialog_input(
            &adapters,
            Path::new("unused"),
            &session,
            &input(),
            deadline(),
        );
        match registration.guarded_input_refusal {
            Some(refusal) => {
                assert_eq!(dialog.unwrap_err().to_string(), refusal, "{kind:?}");
                assert_eq!(adapters.lookups.get(), 0, "{kind:?}");
                assert!(adapters.adapter.calls().is_empty(), "{kind:?}");
            }
            None => {
                dialog.unwrap();
                assert_eq!(adapters.lookups.get(), 1, "{kind:?}");
                assert_eq!(adapters.adapter.calls(), ["dialog"], "{kind:?}");
            }
        }
    }
}

// The Herdr adapter exists only where Herdr does (macOS, Linux and Windows); the recording
// tests above also build on every other target.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod herdr_core {
    use super::*;
    use crate::native::terminal::HerdrSurface;
    use crate::native::terminal::herdr::{self, Connection, Core};
    use crate::native::terminal::record::HerdrHostBirth;
    use anyhow::anyhow;
    use std::{path::PathBuf, rc::Rc};

    /// A core that nothing answers at: it records each endpoint asked for, and every
    /// connection fails, as a core that is not running does.
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
        fn admit_launch_directory(
            &self,
            _: &Path,
            _: Option<&crate::native::terminal::bound_host::HostAccount>,
        ) -> Result<()> {
            unreachable!("nothing is created without a core")
        }
    }

    // An endpoint this platform's Herdr adapter can address: a local named pipe on Windows,
    // an absolute socket path elsewhere.
    fn endpoint(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"\\.\pipe\tabcli-dispatch-test-{name}"))
        } else {
            PathBuf::from(format!("/tabcli-dispatch-test/{name}.sock"))
        }
    }

    // The Windows dispatch over the Herdr adapter itself, on a scripted core: every operation
    // on a Herdr handle is the Herdr adapter's, on the recorded endpoint, and the console
    // adapter is never reached. Without an answering core a send is known unsent and a close
    // is unknown (never Missing); the guarded-input refusal opens no connection.
    #[test]
    fn herdr_handles_reach_their_core_through_the_windows_dispatch() {
        let created_at = endpoint("endpoint");
        let recorded = endpoint("core");
        let connects = Rc::default();
        let dispatch = Dispatch::new(
            Recording::default(),
            herdr::Adapter::with_core(UnansweredCore {
                endpoint: created_at.clone(),
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
                    socket: recorded.to_str().unwrap().into(),
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
        errors.push(
            surface_present(&dispatch, &directory, &handle, Duration::from_secs(5)).unwrap_err(),
        );
        // The launcher's probe: an unanswered core is neither absence nor an end.
        errors.push(
            launch_surface(&dispatch, &directory, &handle, Duration::from_secs(5)).unwrap_err(),
        );
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
        let mut expected = vec![created_at];
        expected.extend(vec![recorded; 6]);
        assert_eq!(*connects.borrow(), expected);
    }
}
