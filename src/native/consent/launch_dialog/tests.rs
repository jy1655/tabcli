use super::super::record::{SESSION_FILE, assess_with, observe, record};
use super::super::store::native_key;
use super::super::tests::{Fixture, session, setup, trust_pi};
use super::*;
use crate::native::clock::FakeClock;
use crate::native::session::primitives::{write_json_atomic, write_private};
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;

const DIALOG: &str = "the provider's workspace dialog";
const NOT_STARTED: &str = "the provider did not report its start";
const UNCONFIRMED: &str = "workspace trust was not confirmed before the deadline; inspect workspace_consent and the managed terminal";
const UNGUARDED: &str = "the workspace trust dialog cannot be answered with a guarded key in";
const UNGUARDED_ADVICE: &str =
    "no key was sent; use the provider's own workspace trust prompt, then launch again";

// A provider policy that a test scripts. It answers `Enter` for `DIALOG` only, and
// records the launch facts it received, how often its start report was read and how often
// it gave its diagnostic.
#[derive(Default)]
struct FakePolicy {
    // `None`: the provider has no start signal. Otherwise the reports in turn, the last
    // one repeated; `Err` is a read error.
    start: Option<Vec<std::result::Result<ProviderStart, &'static str>>>,
    settle: Duration,
    received: RefCell<Vec<(PathBuf, PathBuf, Bypass)>>,
    start_reads: Cell<usize>,
    diagnostics: Cell<usize>,
}

impl FakePolicy {
    fn with_start(start: &[std::result::Result<ProviderStart, &'static str>]) -> Self {
        Self {
            start: Some(start.to_vec()),
            ..Self::default()
        }
    }
}

struct FakeLaunch<'a>(&'a FakePolicy);

impl LaunchDialogAnswerer for FakePolicy {
    fn policy<'a>(&'a self, launch: LaunchFacts<'a>) -> Box<dyn LaunchDialogPolicy + 'a> {
        self.received.borrow_mut().push((
            launch.directory.to_owned(),
            launch.workspace.to_owned(),
            launch.bypass,
        ));
        Box::new(FakeLaunch(self))
    }
}

impl LaunchDialogPolicy for FakeLaunch<'_> {
    fn dialog_key(&self, screen: &str) -> Option<terminal::DialogKey> {
        (screen == DIALOG).then_some(terminal::DialogKey::Enter)
    }
    fn bypass_start(&self) -> Option<&dyn BypassStart> {
        self.0.start.as_ref().map(|_| self as &dyn BypassStart)
    }
    fn settle(&self) -> Duration {
        self.0.settle
    }
}

impl BypassStart for FakeLaunch<'_> {
    fn started(&self) -> Result<ProviderStart> {
        let reports = self.0.start.as_ref().expect("a start signal");
        let read = self.0.start_reads.get();
        self.0.start_reads.set(read + 1);
        reports[read.min(reports.len() - 1)].map_err(anyhow::Error::msg)
    }
    fn not_started(&self) -> String {
        self.0.diagnostics.set(self.0.diagnostics.get() + 1);
        NOT_STARTED.to_owned()
    }
}

// A surface that a test scripts. It may be asked once whether it takes a guarded key,
// and its screen is always read for the manifest's session.
struct Scripted<S, D> {
    guarded_key: GuardedKey,
    asked: Cell<bool>,
    screen: S,
    dispatch: D,
}

fn scripted<S, D>(guarded_key: GuardedKey, screen: S, dispatch: D) -> Scripted<S, D> {
    Scripted {
        guarded_key,
        asked: Cell::new(false),
        screen,
        dispatch,
    }
}

impl<S: FnMut() -> Result<String>, D: FnMut() -> Result<bool>> DialogSurface for Scripted<S, D> {
    fn guarded_key(&self) -> GuardedKey {
        assert!(
            !self.asked.replace(true),
            "the launch asks its surface once"
        );
        self.guarded_key
    }
    fn screen(&mut self, session_id: &str, _: Instant) -> Result<String> {
        assert_eq!(session_id, "session-trustwait");
        (self.screen)()
    }
    fn answer(&mut self, _: &terminal::GuardedDialogInput, _: Instant) -> Result<bool> {
        (self.dispatch)()
    }
}

// A surface that must not be asked anything.
struct Untouched;

impl DialogSurface for Untouched {
    fn guarded_key(&self) -> GuardedKey {
        panic!("guarded key")
    }
    fn screen(&mut self, _: &str, _: Instant) -> Result<String> {
        panic!("screen")
    }
    fn answer(&mut self, _: &terminal::GuardedDialogInput, _: Instant) -> Result<bool> {
        panic!("answer")
    }
}

fn surface_of(kind: &str) -> terminal::TerminalSession {
    serde_json::from_value(serde_json::json!({
        "terminal": kind, "session_id": "1", "managed_session_id": "session-trustwait"
    }))
    .unwrap()
}

fn guarded_key_of(kind: &str) -> GuardedKey {
    TerminalDialogSurface {
        directory: Path::new("unused"),
        session: &surface_of(kind),
    }
    .guarded_key()
}

fn replace_private(path: &Path, bytes: &[u8]) {
    if path.exists() {
        fs::remove_file(path).unwrap();
    }
    write_private(path, bytes).unwrap();
}

// A launching session with verified consent, as a launch that reaches the dialog has.
struct Consented {
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    directory: PathBuf,
    workspace: PathBuf,
    stores: Fixture,
}

fn consented(provider: FirstPartyCli, yolo: bool) -> Consented {
    let (home, workspace, stores) = setup();
    let (root, directory) = session(&workspace, "launching");
    trust_pi(&stores, &workspace);
    let assessment = assess_with(root.path(), &workspace, &stores.env).unwrap();
    assert_eq!(assessment.state, "verified");
    record(&directory, &assessment).unwrap();
    let mut manifest = Reader::open_unchecked(&directory).manifest().unwrap();
    manifest.provider = provider.as_str().into();
    manifest.yolo = yolo;
    write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
    Consented {
        _home: home,
        _root: root,
        directory,
        workspace,
        stores,
    }
}

impl Consented {
    fn complete(
        &self,
        target: FirstPartyCli,
        support: LaunchDialogSupport<'_>,
        clock: &mut FakeClock,
        surface: &mut dyn DialogSurface,
    ) -> Result<()> {
        LaunchDialog {
            directory: &self.directory,
            target,
            expected_claim: "1-2-3",
            deadline: clock.now() + Duration::from_secs(3),
        }
        .complete(support, &self.stores.env, clock, surface)
    }

    fn launch_complete(&self, target: FirstPartyCli) -> Result<()> {
        complete_launch(
            &self.directory,
            target,
            &surface_of("iterm2"),
            "1-2-3",
            Instant::now() + Duration::from_secs(3),
        )
    }
}

#[cfg(not(windows))]
fn claude_dialog(workspace: &Path, notice: &str) -> String {
    format!(
        "Accessing workspace:\n{}\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\nClaude Code'll be able to read, edit, and execute files here.\n{notice}Security guide\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel\n",
        native_key(workspace).unwrap()
    )
}

fn agy_dialog(workspace: &Path) -> String {
    format!(
        "Accessing workspace:\n{}\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\nNo, exit\n↑/↓ Navigate · enter Confirm",
        native_key(workspace).unwrap()
    )
}

#[cfg(target_os = "macos")]
#[test]
fn unsupported_dialog_response_preserves_consent_without_pending_input() {
    let (temp, workspace, fixture) = setup();
    let assessment = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
    let path = temp.path().join(SESSION_FILE);
    write_json_atomic(&path, &assessment).unwrap();
    let before = fs::read(&path).unwrap();
    for kind in [
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::Ghostty,
    ] {
        let session = terminal::TerminalSession::new(kind, "unused".into());
        let error = respond_to_workspace_dialog(
            temp.path(),
            TerminalDialogSurface {
                directory: temp.path(),
                session: &session,
            }
            .guarded_key(),
            None,
            &terminal::GuardedDialogInput {
                expected_claim: None,
                expires_unix_ms: None,
                screen: "verified dialog".into(),
                key: terminal::DialogKey::Enter,
            },
            |_| panic!("unsupported surface must not dispatch"),
        )
        .unwrap_err();
        assert_eq!(fs::read(&path).unwrap(), before, "{kind:?}: {error:#}");
    }
}

// The refusal names the surface, sends no key and records no pending response.
#[test]
fn the_unguarded_refusal_names_the_surface_and_records_nothing() {
    let c = consented(FirstPartyCli::Agy, false);
    let consent = c.directory.join(SESSION_FILE);
    let before = fs::read(&consent).unwrap();
    for (kind, name) in [
        ("wezterm", "WezTerm"),
        ("warp", "Warp"),
        ("ghostty", "Ghostty"),
    ] {
        let error = respond_to_workspace_dialog(
            &c.directory,
            guarded_key_of(kind),
            None,
            &terminal::GuardedDialogInput {
                expected_claim: None,
                expires_unix_ms: None,
                screen: "verified dialog".into(),
                key: terminal::DialogKey::Enter,
            },
            |_| panic!("unsupported surface must not dispatch"),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!("{UNGUARDED} {name}; {UNGUARDED_ADVICE}")
        );
        assert_eq!(fs::read(&consent).unwrap(), before, "{kind}");
    }
    for kind in ["iterm2", "apple-terminal", "windows-console"] {
        assert_eq!(guarded_key_of(kind), GuardedKey::Taken, "{kind}");
    }
}

// An adapter that does not answer the launch dialog reads no record for it: not the
// status, the consent record or the manifest, each of which is unreadable here.
#[test]
fn an_adapter_that_does_not_answer_reads_nothing() {
    let c = consented(FirstPartyCli::Codex, true);
    crate::native::session::update_status(
        &c.directory,
        crate::native::session::SessionState::Closed,
        None,
        None,
    )
    .unwrap();
    replace_private(&c.directory.join("status.json"), b"{");
    replace_private(&c.directory.join("manifest.json"), b"{");
    replace_private(&c.directory.join(SESSION_FILE), b"{");
    let mut clock = FakeClock::new(Instant::now());
    c.complete(
        FirstPartyCli::Codex,
        LaunchDialogSupport::NotAnswered,
        &mut clock,
        &mut Untouched,
    )
    .unwrap();
    for target in [FirstPartyCli::Codex, FirstPartyCli::Pi] {
        c.launch_complete(target).unwrap();
    }
}

// The session is checked before consent: an ended or unreadable status fails the step
// with no response sent, whatever the consent record holds.
#[test]
fn complete_launch_checks_the_session_before_consent() {
    for target in [FirstPartyCli::Claude, FirstPartyCli::Agy] {
        let c = consented(target, true);
        replace_private(&c.directory.join("manifest.json"), b"{");
        replace_private(&c.directory.join(SESSION_FILE), b"{");
        crate::native::session::update_status(
            &c.directory,
            crate::native::session::SessionState::Closed,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            format!("{:#}", c.launch_complete(target).unwrap_err()),
            "the session is closed before the workspace trust response; no response sent",
            "{target:?}"
        );
        replace_private(&c.directory.join("status.json"), b"{");
        let status = Reader::open_unchecked(&c.directory).status().unwrap_err();
        assert_eq!(
            format!("{:#}", c.launch_complete(target).unwrap_err()),
            format!("{status:#}; no response sent"),
            "{target:?}"
        );
    }
}

// Without verified consent the step ends before the manifest; a consent record that
// cannot be read is the launch's error as it is, before the manifest too.
#[test]
fn complete_launch_reads_the_manifest_only_after_verified_consent() {
    for target in [FirstPartyCli::Claude, FirstPartyCli::Agy] {
        let c = consented(target, true);
        replace_private(&c.directory.join("manifest.json"), b"{");
        let consent = c.directory.join(SESSION_FILE);
        fs::remove_file(&consent).unwrap();
        c.launch_complete(target).unwrap();
        replace_private(&consent, b"{");
        let expected = crate::native::consent::authorized(&c.directory, target).unwrap_err();
        assert_eq!(
            format!("{:#}", c.launch_complete(target).unwrap_err()),
            format!("{expected:#}"),
            "{target:?}"
        );
    }
}

// The same order through the entry: an authorization error is the launch's error as it
// is, and neither the manifest, the policy, the start signal nor the surface is reached.
#[test]
fn an_authorization_error_propagates_before_the_manifest() {
    let c = consented(FirstPartyCli::Claude, true);
    replace_private(&c.directory.join("manifest.json"), b"{");
    replace_private(&c.directory.join(SESSION_FILE), b"{");
    let expected =
        super::super::authority::authority(&c.directory, FirstPartyCli::Claude, &c.stores.env)
            .unwrap_err();
    let policy = FakePolicy::with_start(&[Ok(ProviderStart::Started)]);
    let mut clock = FakeClock::new(Instant::now());
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut clock,
            &mut Untouched,
        )
        .unwrap_err();
    assert_eq!(format!("{error:#}"), format!("{expected:#}"));
    assert!(policy.received.borrow().is_empty());
    assert_eq!(policy.start_reads.get(), 0);
    // Without consent the step ends there too.
    fs::remove_file(c.directory.join(SESSION_FILE)).unwrap();
    c.complete(
        FirstPartyCli::Claude,
        LaunchDialogSupport::Answered(&policy),
        &mut clock,
        &mut Untouched,
    )
    .unwrap();
    assert!(policy.received.borrow().is_empty());
}

#[test]
fn a_manifest_error_propagates_as_it_is() {
    let c = consented(FirstPartyCli::Claude, true);
    replace_private(&c.directory.join("manifest.json"), b"{");
    let expected = format!(
        "{:#}",
        Reader::open_unchecked(&c.directory).manifest().unwrap_err()
    );
    let policy = FakePolicy::with_start(&[Ok(ProviderStart::Started)]);
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut FakeClock::new(Instant::now()),
            &mut Untouched,
        )
        .unwrap_err();
    assert_eq!(format!("{error:#}"), expected);
    assert!(policy.received.borrow().is_empty());
}

// Under bypass the provider's own start report decides: no report for this launch or a
// started provider leaves the dialog alone, and a provider that has not started yet
// reaches the dialog with the signal, so a dialog that is there is answered (Claude
// Code 2.1.321 on macOS shows it under bypass; 2.1.319 did not). Without bypass the
// signal is not read and the dialog is checked as before it existed.
#[test]
fn the_bypass_start_report_decides_before_the_loop() {
    for (yolo, start, error) in [
        (true, ProviderStart::NoSignal, None),
        (true, ProviderStart::Started, None),
        (true, ProviderStart::NotStarted, Some(NOT_STARTED)),
        (false, ProviderStart::NotStarted, Some(UNCONFIRMED)),
    ] {
        let c = consented(FirstPartyCli::Claude, yolo);
        let policy = FakePolicy::with_start(&[Ok(start)]);
        let reads = Cell::new(0);
        let mut clock = FakeClock::new(Instant::now());
        let result = c.complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut clock,
            &mut scripted(
                GuardedKey::Taken,
                || {
                    reads.set(reads.get() + 1);
                    Ok("starting".to_owned())
                },
                || panic!("answer"),
            ),
        );
        let case = format!("yolo {yolo}, {start:?}");
        assert_eq!(
            *policy.received.borrow(),
            [(
                c.directory.clone(),
                c.workspace.clone(),
                if yolo { Bypass::On } else { Bypass::Off }
            )],
            "{case}"
        );
        match error {
            None => {
                result.unwrap();
                assert_eq!((policy.start_reads.get(), reads.get()), (1, 0), "{case}");
                assert_eq!(policy.diagnostics.get(), 0, "{case}");
            }
            Some(error) => {
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    format!("{error}; no response sent"),
                    "{case}"
                );
                assert!(reads.get() > 1, "{case}");
                if yolo {
                    assert_eq!(policy.start_reads.get(), reads.get() + 1, "{case}");
                    // The adapter's diagnostic is produced only at the deadline.
                    assert_eq!(policy.diagnostics.get(), 1, "{case}");
                } else {
                    assert_eq!(policy.start_reads.get(), 0, "{case}");
                    assert_eq!(policy.diagnostics.get(), 0, "{case}");
                }
            }
        }
    }
}

// A surface that takes no guarded key cannot answer the dialog, and reading it is
// refused: a launch under bypass there reads no start signal and leaves the step.
#[test]
fn a_surface_without_a_guarded_key_reads_no_start_signal() {
    let c = consented(FirstPartyCli::Claude, true);
    let policy = FakePolicy::with_start(&[Ok(ProviderStart::NotStarted)]);
    c.complete(
        FirstPartyCli::Claude,
        LaunchDialogSupport::Answered(&policy),
        &mut FakeClock::new(Instant::now()),
        &mut scripted(
            guarded_key_of("wezterm"),
            || panic!("screen"),
            || panic!("answer"),
        ),
    )
    .unwrap();
    assert_eq!(policy.received.borrow().len(), 1);
    assert_eq!(policy.start_reads.get(), 0);
}

// Before the loop a start-signal read error is the launch's error as it is; inside the
// loop the same error says that no response was sent.
#[test]
fn a_start_signal_read_error_before_the_loop_propagates_unwrapped() {
    let c = consented(FirstPartyCli::Claude, true);
    let policy = FakePolicy::with_start(&[Err("signal unreadable")]);
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut FakeClock::new(Instant::now()),
            &mut scripted(GuardedKey::Taken, || panic!("screen"), || panic!("answer")),
        )
        .unwrap_err();
    assert_eq!(format!("{error:#}"), "signal unreadable");
    assert_eq!(policy.start_reads.get(), 1);
}

#[test]
fn a_start_signal_read_error_inside_the_loop_reports_no_response_sent() {
    let c = consented(FirstPartyCli::Claude, true);
    let policy = FakePolicy::with_start(&[Ok(ProviderStart::NotStarted), Err("signal unreadable")]);
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut FakeClock::new(Instant::now()),
            &mut scripted(GuardedKey::Taken, || panic!("screen"), || panic!("answer")),
        )
        .unwrap_err();
    assert_eq!(format!("{error:#}"), "signal unreadable; no response sent");
    assert_eq!(policy.start_reads.get(), 2);
}

// Inside the loop only a started provider ends the wait: a launch whose signal turns
// absent keeps waiting and ends with the adapter's diagnostic.
#[test]
fn an_absent_start_signal_inside_the_loop_does_not_end_the_wait() {
    let c = consented(FirstPartyCli::Claude, true);
    let policy =
        FakePolicy::with_start(&[Ok(ProviderStart::NotStarted), Ok(ProviderStart::NoSignal)]);
    let reads = Cell::new(0);
    let mut clock = FakeClock::new(Instant::now());
    let deadline = clock.now() + Duration::from_secs(3);
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut clock,
            &mut scripted(
                GuardedKey::Taken,
                || {
                    reads.set(reads.get() + 1);
                    Ok("starting".to_owned())
                },
                || panic!("answer"),
            ),
        )
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!("{NOT_STARTED}; no response sent")
    );
    assert!(reads.get() > 1);
    assert!(clock.now() >= deadline);
}

// The policy's settle window holds the one response until the same screen has been
// seen that long.
#[test]
fn the_policy_settle_window_holds_the_response() {
    let c = consented(FirstPartyCli::Claude, false);
    let policy = FakePolicy {
        settle: Duration::from_millis(600),
        ..FakePolicy::default()
    };
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let now = clock.shared();
    let answered = Cell::new(None);
    let error = c
        .complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&policy),
            &mut clock,
            &mut scripted(
                GuardedKey::Taken,
                || Ok(DIALOG.to_owned()),
                || {
                    answered.set(Some(now.get() - start));
                    Ok(true)
                },
            ),
        )
        .unwrap_err();
    assert_eq!(answered.get(), Some(Duration::from_millis(600)));
    assert!(
        format!("{error:#}").contains("trust unconfirmed"),
        "{error:#}"
    );
    assert_eq!(
        observe(&c.directory).applied_marker(),
        Some("workspace-dialog-response-pending")
    );
}

// A launching session whose consent is inherited: its workspace is a verified linked worktree
// of a repository whose main working tree Pi trusts exactly.
fn inherited(provider: FirstPartyCli, yolo: bool) -> Consented {
    let (home, _, stores) = setup();
    let fixture = super::super::worktree::tests::registered(&home.path().canonicalize().unwrap());
    trust_pi(&stores, &fixture.repository);
    let (root, directory) = session(&fixture.worktree, "launching");
    let assessment = assess_with(root.path(), &fixture.worktree, &stores.env).unwrap();
    assert_eq!(assessment.state, "verified");
    assert!(assessment.repository_root.is_some());
    record(&directory, &assessment).unwrap();
    let mut manifest = Reader::open_unchecked(&directory).manifest().unwrap();
    manifest.provider = provider.as_str().into();
    manifest.yolo = yolo;
    write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
    Consented {
        _home: home,
        _root: root,
        directory,
        workspace: fixture.worktree,
        stores,
    }
}

// Consent that the launch already applied through the target's own option, and consent
// inherited from a repository's main working tree, take no key on any surface: neither the
// policy, the provider's store nor the surface is consulted. Before, a verified Agy launch
// on a surface without a guarded key (Herdr, tmux, WezTerm) failed with the unguarded refusal,
// and on one whose screen cannot be read (Ghostty, Warp) at the screen read.
#[test]
fn applied_or_inherited_consent_takes_no_key() {
    for target in [FirstPartyCli::Claude, FirstPartyCli::Agy] {
        for yolo in [false, true] {
            let label = format!("{target:?} yolo={yolo}");
            let c = consented(target, yolo);
            super::super::record::applied(&c.directory, "agy-settings-override").unwrap();
            let policy = FakePolicy::with_start(&[Ok(ProviderStart::NotStarted)]);
            let mut clock = FakeClock::new(Instant::now());
            let result = c.complete(
                target,
                LaunchDialogSupport::Answered(&policy),
                &mut clock,
                &mut Untouched,
            );
            // The target's own launch option skips the dialog; another adapter's option in
            // this session's record is neither a reason to skip nor to answer.
            if target == FirstPartyCli::Agy {
                result.unwrap();
            } else {
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    "the session's consent record names another provider's launch option (agy-settings-override); no response sent",
                    "{label}"
                );
            }
            assert!(policy.received.borrow().is_empty(), "{label}");
            assert_eq!(policy.start_reads.get(), 0, "{label}");
            assert_eq!(clock.slept, Duration::ZERO, "{label}");
            assert_eq!(
                observe(&c.directory).applied_marker(),
                Some("agy-settings-override"),
                "{label}"
            );

            let c = inherited(target, yolo);
            let consent = fs::read(c.directory.join(SESSION_FILE)).unwrap();
            let store = fs::read(&c.stores.pi).unwrap();
            for support in [
                LaunchDialogSupport::Answered(&policy),
                provider::launch_dialog(target),
            ] {
                c.complete(
                    target,
                    support,
                    &mut FakeClock::new(Instant::now()),
                    &mut Untouched,
                )
                .unwrap();
            }
            assert!(policy.received.borrow().is_empty(), "{label}");
            assert_eq!(
                fs::read(c.directory.join(SESSION_FILE)).unwrap(),
                consent,
                "{label}"
            );
            assert_eq!(fs::read(&c.stores.pi).unwrap(), store, "{label}");
            for store in [&c.stores.codex, &c.stores.claude, &c.stores.agy] {
                assert!(!store.exists(), "{label}: {}", store.display());
            }
        }
    }
}

// A launch of an Agy positively identified as older than its session-only setting: the one
// Agy launch that keeps the guarded dialog answer for exact consent.
fn older_agy(yolo: bool) -> Consented {
    let c = consented(FirstPartyCli::Agy, yolo);
    super::super::record::option_unavailable(&c.directory).unwrap();
    c
}

// Review round 3, finding 1: an Agy launch that did not apply its setting and did not record
// it as unavailable (an inconclusive version, a refused or unreadable check) never falls back
// to the guarded key, on any surface. Only a positively older Agy takes it.
#[test]
fn an_inconclusive_agy_launch_never_falls_back_to_the_key() {
    for yolo in [false, true] {
        let c = consented(FirstPartyCli::Agy, yolo);
        c.complete(
            FirstPartyCli::Agy,
            provider::launch_dialog(FirstPartyCli::Agy),
            &mut FakeClock::new(Instant::now()),
            &mut Untouched,
        )
        .unwrap();
        assert_eq!(observe(&c.directory).applied_marker(), None);
    }
}

// Review round 3, finding 11: a value the step does not know refuses the step.
#[test]
fn an_unknown_launch_value_refuses_the_step() {
    for target in [FirstPartyCli::Claude, FirstPartyCli::Agy] {
        let c = consented(target, false);
        super::super::record::applied(&c.directory, "some-future-option").unwrap();
        let error = c
            .complete(
                target,
                provider::launch_dialog(target),
                &mut FakeClock::new(Instant::now()),
                &mut Untouched,
            )
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "the session's consent record holds an unknown launch value (some-future-option); no response sent"
        );
    }
}

// A clock that runs a hook at one chosen reading after the screen was read: the test places a
// change of the session's consent at an exact point of the step.
struct Hooked<'a> {
    clock: FakeClock,
    armed: &'a Cell<bool>,
    readings: usize,
    at: usize,
    hook: Option<Box<dyn FnOnce() + 'a>>,
}

impl Clock for Hooked<'_> {
    fn now(&mut self) -> Instant {
        if self.armed.get() {
            self.readings += 1;
            if self.readings == self.at
                && let Some(hook) = self.hook.take()
            {
                hook();
            }
        }
        self.clock.now()
    }
    fn sleep(&mut self, duration: Duration) {
        self.clock.sleep(duration)
    }
}

// Review round 3, finding 4: the pending transition is written only while the record still
// holds what the step assessed, and the dispatch callback asks the authority after that write.
// A launch option recorded just before the write keeps its record and gets no key; consent
// withdrawn just after it gets no key either, and the record returns to what was assessed.
#[test]
fn the_authority_is_consumed_inside_the_dispatch_after_the_pending_write() {
    // Readings after the screen: 1 when it was observed, 2 at the deadline check right before
    // the pending write, 3 at the deadline check inside the dispatch callback.
    for (at, change) in [(2, "launch option"), (3, "withdrawn")] {
        let c = older_agy(false);
        let armed = Cell::new(false);
        let directory = c.directory.clone();
        let pi = c.stores.pi.clone();
        let mut clock = Hooked {
            clock: FakeClock::new(Instant::now()),
            armed: &armed,
            readings: 0,
            at,
            hook: Some(Box::new(move || {
                if change == "withdrawn" {
                    fs::remove_file(&pi).unwrap();
                } else {
                    super::super::record::applied(&directory, "agy-settings-override").unwrap();
                }
            })),
        };
        let deadline = clock.clock.now() + Duration::from_secs(3);
        let error = LaunchDialog {
            directory: &c.directory,
            target: FirstPartyCli::Agy,
            expected_claim: "1-2-3",
            deadline,
        }
        .complete(
            provider::launch_dialog(FirstPartyCli::Agy),
            &c.stores.env,
            &mut clock,
            &mut scripted(
                GuardedKey::Taken,
                || {
                    armed.set(true);
                    Ok(agy_dialog(&c.workspace))
                },
                || panic!("{change}: no key"),
            ),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "workspace consent changed before trust dialog response; no response sent",
            "{change}"
        );
        assert_eq!(
            observe(&c.directory).applied_marker(),
            Some(if change == "withdrawn" {
                "launch-option-unavailable"
            } else {
                "agy-settings-override"
            }),
            "{change}"
        );
    }
}

// Review finding 6: the step asks the one authority again before the screen read and
// immediately before the one dispatch. Consent withdrawn, or a launch option recorded, after
// the screen was read sends no key.
#[test]
fn consent_is_asked_again_immediately_before_the_dispatch() {
    for change in ["withdrawn", "applied"] {
        let c = older_agy(false);
        let reads = Cell::new(0);
        let error = c
            .complete(
                FirstPartyCli::Agy,
                provider::launch_dialog(FirstPartyCli::Agy),
                &mut FakeClock::new(Instant::now()),
                &mut scripted(
                    GuardedKey::Taken,
                    || {
                        reads.set(reads.get() + 1);
                        if change == "withdrawn" {
                            let _ = fs::remove_file(&c.stores.pi);
                        } else {
                            super::super::record::applied(&c.directory, "agy-settings-override")
                                .unwrap();
                        }
                        Ok(agy_dialog(&c.workspace))
                    },
                    || panic!("{change}: no key after consent changed"),
                ),
            )
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "workspace consent changed before trust dialog response; no response sent",
            "{change}"
        );
        assert_eq!(reads.get(), 1, "{change}");
    }
}

// Agy's own policy has no start signal: under `--yolo` its dialog is checked and answered
// as without bypass, and a surface without a guarded key refuses it in the loop.
#[test]
fn agy_under_bypass_still_checks_the_dialog() {
    for yolo in [false, true] {
        let c = older_agy(yolo);
        let answers = Cell::new(0);
        c.complete(
            FirstPartyCli::Agy,
            provider::launch_dialog(FirstPartyCli::Agy),
            &mut FakeClock::new(Instant::now()),
            &mut scripted(
                GuardedKey::Taken,
                || Ok(agy_dialog(&c.workspace)),
                || {
                    answers.set(answers.get() + 1);
                    write_json_atomic(
                        &c.stores.agy,
                        &serde_json::json!({"trustedWorkspaces":[native_key(&c.workspace).unwrap()]}),
                    )
                    .unwrap();
                    Ok(true)
                },
            ),
        )
        .unwrap();
        assert_eq!(answers.get(), 1, "yolo {yolo}");
        assert_eq!(
            observe(&c.directory).applied_marker(),
            Some("provider-workspace-dialog")
        );
    }
}

// A dialog on a surface that takes no guarded key fails the step with the refusal and
// no response sent, with or without bypass when the adapter has no start signal.
#[test]
fn a_surface_without_a_guarded_key_refuses_the_dialog_in_the_loop() {
    for yolo in [false, true] {
        let c = older_agy(yolo);
        let reads = Cell::new(0);
        let error = c
            .complete(
                FirstPartyCli::Agy,
                provider::launch_dialog(FirstPartyCli::Agy),
                &mut FakeClock::new(Instant::now()),
                &mut scripted(
                    guarded_key_of("wezterm"),
                    || {
                        reads.set(reads.get() + 1);
                        Ok(agy_dialog(&c.workspace))
                    },
                    || panic!("answer"),
                ),
            )
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!("{UNGUARDED} WezTerm; {UNGUARDED_ADVICE}; no response sent")
        );
        assert_eq!(reads.get(), 1);
        // The record still holds what the launch recorded: no pending response.
        assert_eq!(
            observe(&c.directory).applied_marker(),
            Some("launch-option-unavailable")
        );
    }
}

// Claude's pre-approval variant is answered under bypass only: the key match receives
// the session's bypass fact. Claude's own start signal is not started here.
#[cfg(not(windows))]
#[test]
fn the_pre_approval_dialog_is_answered_only_under_bypass() {
    let notice = "⚠ This folder pre-approves 2 tool permissions in .claude/settings.local.json:\nRead(//Users/example/.tool/pending/**) and\nRead(//Users/example/.tool/pending/code/**)\nThese will apply without asking. Only proceed if you trust this configuration.\n";
    for yolo in [true, false] {
        let c = consented(FirstPartyCli::Claude, yolo);
        crate::native::provider::ClaudeCompletionRecordFixture::NATIVE_MODE.write(&c.directory);
        let key = native_key(&c.workspace).unwrap();
        let answers = Cell::new(0);
        let result = c.complete(
            FirstPartyCli::Claude,
            provider::launch_dialog(FirstPartyCli::Claude),
            &mut FakeClock::new(Instant::now()),
            &mut scripted(
                GuardedKey::Taken,
                || Ok(claude_dialog(&c.workspace, notice)),
                || {
                    answers.set(answers.get() + 1);
                    write_json_atomic(
                        &c.stores.claude,
                        &serde_json::json!({"projects": {&key: {"hasTrustDialogAccepted": true}}}),
                    )
                    .unwrap();
                    Ok(true)
                },
            ),
        );
        let marker = observe(&c.directory);
        if yolo {
            result.unwrap();
            assert_eq!(answers.get(), 1);
            assert_eq!(marker.applied_marker(), Some("provider-workspace-dialog"));
        } else {
            assert_eq!(
                format!("{:#}", result.unwrap_err()),
                format!("{UNCONFIRMED}; no response sent")
            );
            assert_eq!(answers.get(), 0);
            assert_eq!(marker.applied_marker(), None);
        }
    }
}

// Claude's own policy with a start signal that a test scripts through a marker file.
#[cfg(target_os = "macos")]
struct ClaudeWithStartMarker;

#[cfg(target_os = "macos")]
struct StartMarker<'a> {
    claude: Box<dyn LaunchDialogPolicy + 'a>,
    directory: &'a Path,
}

#[cfg(target_os = "macos")]
impl LaunchDialogAnswerer for ClaudeWithStartMarker {
    fn policy<'a>(&'a self, launch: LaunchFacts<'a>) -> Box<dyn LaunchDialogPolicy + 'a> {
        let LaunchDialogSupport::Answered(claude) = provider::launch_dialog(FirstPartyCli::Claude)
        else {
            panic!("Claude answers its launch dialog");
        };
        Box::new(StartMarker {
            claude: claude.policy(launch),
            directory: launch.directory,
        })
    }
}

#[cfg(target_os = "macos")]
impl LaunchDialogPolicy for StartMarker<'_> {
    fn dialog_key(&self, screen: &str) -> Option<terminal::DialogKey> {
        self.claude.dialog_key(screen)
    }
    fn bypass_start(&self) -> Option<&dyn BypassStart> {
        Some(self)
    }
    fn settle(&self) -> Duration {
        self.claude.settle()
    }
}

#[cfg(target_os = "macos")]
impl BypassStart for StartMarker<'_> {
    fn started(&self) -> Result<ProviderStart> {
        Ok(if self.directory.join("started").exists() {
            ProviderStart::Started
        } else {
            ProviderStart::NotStarted
        })
    }
    fn not_started(&self) -> String {
        NOT_STARTED.to_owned()
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Debug)]
enum BypassCase {
    Started,
    StartsAfterTheFirstPoll,
    NeverReports,
    Dialog,
    DialogAnswerUnconfirmed,
}
// The loop under bypass. A provider that reports its start, at once or later, ends the
// wait without a key although its store never shows trust. One that never reports fails
// with the adapter's own diagnostic, not as a trust failure. A dialog that
// is there is answered once, and after the answer only the store confirms trust: the
// start report alone does not.
#[cfg(target_os = "macos")]
#[test]
fn bypass_dialog_loop_follows_the_start_signal_until_it_answers() {
    for case in [
        BypassCase::Started,
        BypassCase::StartsAfterTheFirstPoll,
        BypassCase::NeverReports,
        BypassCase::Dialog,
        BypassCase::DialogAnswerUnconfirmed,
    ] {
        let c = consented(FirstPartyCli::Claude, true);
        let (directory, stores) = (&c.directory, &c.stores);
        let key = native_key(&c.workspace).unwrap();
        let dialog = claude_dialog(&c.workspace, "");
        let shows_dialog = matches!(
            case,
            BypassCase::Dialog | BypassCase::DialogAnswerUnconfirmed
        );
        let (reads, answers) = (Cell::new(0), Cell::new(0));
        let mut clock = FakeClock::new(Instant::now());
        let deadline = clock.now() + Duration::from_secs(3);
        let started = directory.join("started");
        if case == BypassCase::Started {
            fs::write(&started, b"").unwrap();
        }
        let result = c.complete(
            FirstPartyCli::Claude,
            LaunchDialogSupport::Answered(&ClaudeWithStartMarker),
            &mut clock,
            &mut scripted(
                GuardedKey::Taken,
                || {
                    reads.set(reads.get() + 1);
                    if case == BypassCase::StartsAfterTheFirstPoll {
                        fs::write(&started, b"").unwrap();
                    }
                    Ok(if shows_dialog {
                        dialog.clone()
                    } else {
                        "starting".to_owned()
                    })
                },
                || {
                    answers.set(answers.get() + 1);
                    if case == BypassCase::Dialog {
                        write_json_atomic(
                            &stores.claude,
                            &serde_json::json!({"projects": {&key: {"hasTrustDialogAccepted": true}}}),
                        )
                        .unwrap();
                    }
                    fs::write(&started, b"").unwrap();
                    Ok(true)
                },
            ),
        );
        let marker = observe(directory);
        let marker = marker.applied_marker();
        match case {
            BypassCase::Started => {
                result.unwrap();
                assert_eq!((reads.get(), answers.get(), marker), (0, 0, None));
            }
            BypassCase::StartsAfterTheFirstPoll => {
                result.unwrap();
                assert_eq!((reads.get(), answers.get(), marker), (1, 0, None));
            }
            BypassCase::NeverReports => {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "the provider did not report its start; no response sent"
                );
                assert!(reads.get() > 1, "{case:?}");
                assert_eq!((answers.get(), marker), (0, None));
                assert!(clock.now() >= deadline);
            }
            BypassCase::Dialog => {
                result.unwrap();
                assert_eq!((reads.get(), answers.get()), (1, 1));
                assert_eq!(marker, Some("provider-workspace-dialog"));
            }
            BypassCase::DialogAnswerUnconfirmed => {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains("trust unconfirmed"), "{error}");
                assert_eq!((reads.get(), answers.get()), (1, 1));
                assert_eq!(marker, Some("workspace-dialog-response-pending"));
            }
        }
    }
}

// Claude on native Windows has no start signal: under bypass an authorized launch reaches
// its dialog, and one without verified consent reads neither the manifest nor the screen.
#[cfg(windows)]
#[test]
fn windows_claude_under_bypass_reaches_only_an_authorized_dialog() {
    for authorized in [false, true] {
        let c = consented(FirstPartyCli::Claude, true);
        if !authorized {
            fs::remove_file(c.directory.join(SESSION_FILE)).unwrap();
            replace_private(&c.directory.join("manifest.json"), b"{");
        }
        let dialogs = Cell::new(0);
        let result = c.complete(
            FirstPartyCli::Claude,
            provider::launch_dialog(FirstPartyCli::Claude),
            &mut FakeClock::new(Instant::now()),
            &mut scripted(
                GuardedKey::Taken,
                || {
                    dialogs.set(dialogs.get() + 1);
                    bail!("guarded dialog refused")
                },
                || panic!("answer"),
            ),
        );
        assert_eq!(dialogs.get(), usize::from(authorized));
        if authorized {
            assert_eq!(
                result.unwrap_err().to_string(),
                "guarded dialog refused; no response sent"
            );
        } else {
            result.unwrap();
        }
    }
}
