use super::*;
use crate::native::create_session_within;
use crate::native::query;
use crate::native::session::test_support::STATUS_LOCK_FILE;
use crate::native::session::{CoreRecord, close};
use std::fs;

struct Fixture {
    root: tempfile::TempDir,
    consent: Option<PathBuf>,
    unwritable_consent: bool,
    unwritable_claim: bool,
    terminal_kind: terminal::TerminalKind,
    provider: FirstPartyCli,
    // A directory at the launch receipt's path: `launch::begin` fails and the receipt stays
    // unreadable. Without it the launch can reach the terminal, so a test that clears it
    // must stop the launch another way.
    receipt_directory: bool,
    // A regular launch-host record that `launch::host::initialize` refuses to replace.
    recorded_launch_host: bool,
    // A directory at the status lock's path: every status write fails, while status reads
    // and the Claim's removal still succeed.
    status_lock_directory: bool,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            consent: None,
            unwritable_consent: false,
            unwritable_claim: false,
            terminal_kind: terminal::TerminalKind::WindowsConsole,
            provider: FirstPartyCli::Codex,
            receipt_directory: true,
            recorded_launch_host: false,
            status_lock_directory: false,
        }
    }

    // A launch whose receipt `begin` writes and whose host step then fails, before the
    // launcher fences the Claim and before any terminal is touched, on every platform.
    fn host_failure() -> Self {
        let mut fixture = Self::new();
        fixture.receipt_directory = false;
        fixture.terminal_kind = terminal::TerminalKind::Iterm2;
        fixture.recorded_launch_host = true;
        fixture
    }

    fn prepare(&self) -> Result<Prepared> {
        let resolved = Resolved::from_parts(
            SessionSpec {
                provider: self.provider,
                provider_path: PathBuf::from(self.provider.as_str()),
                provider_version: "fixture".into(),
                workspace: self.root.path().canonicalize()?,
                title: "fixture".into(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "fixture".into(),
            },
            self.terminal_kind,
            context_sources(),
            self.consent
                .as_ref()
                .map(|requested| consent::assess(self.root.path(), requested))
                .transpose()?,
        );
        let (prepared, ()) = resolved.create_with(|spec, consent| {
            let created =
                create_session_within(self.root.path(), &[self.root.path().to_owned()], spec)?;
            // Stop before any terminal or provider could be launched.
            if self.receipt_directory {
                fs::create_dir(created.directory.join(CoreRecord::Launch.name()))?;
            }
            if self.recorded_launch_host {
                fs::write(created.directory.join(CoreRecord::LaunchHost.name()), b"{}")?;
            }
            if self.unwritable_consent {
                fs::create_dir(created.directory.join("workspace-consent.json"))?;
            }
            record_consent(&created, consent)?;
            if self.unwritable_claim {
                fs::create_dir(created.directory.join("turn.claim"))?;
            }
            if self.status_lock_directory {
                let lock = created.directory.join(STATUS_LOCK_FILE);
                fs::remove_file(&lock)?;
                fs::create_dir(&lock)?;
            }
            Ok((created, ()))
        })?;
        Ok(prepared)
    }
}

fn context_sources() -> Vec<requests::ContextSource> {
    vec![requests::ContextSource {
        session: "session-source".into(),
        request_id: Some("request-source".into()),
        event_id: "event-1-1.json".into(),
        provider: "codex".into(),
        created_unix_ms: 1,
        mode: None,
        attached_chars: None,
        omitted_chars: None,
    }]
}

fn start() -> Start {
    Start::new(StartOptions {
        timeout: Duration::from_secs(30),
        detach: false,
        json: false,
    })
    .unwrap()
}

fn run(prepared: &Prepared, source: &dyn InitialSource) -> (anyhow::Error, TurnReport) {
    run_with(&start(), prepared, source)
}

fn run_with(
    start: &Start,
    prepared: &Prepared,
    source: &dyn InitialSource,
) -> (anyhow::Error, TurnReport) {
    let mut report = TurnReport::new(false, ResultExtra::new());
    let error = start.run(prepared, source, &mut report).unwrap_err();
    (error, report)
}

// Runs `run` with the records' wall clock fixed at `now`.
fn at_unix_ms<T>(now: u128, run: impl FnOnce() -> T) -> T {
    use crate::native::session::tests::FIXED_UNIX_MS;
    struct Reset(Option<u128>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIXED_UNIX_MS.set(self.0);
        }
    }
    let _reset = Reset(FIXED_UNIX_MS.replace(Some(now)));
    run()
}

struct FailedExtra;
impl InitialSource for FailedExtra {
    fn result_extra(&self) -> Result<ResultExtra> {
        bail!("injected extra")
    }
    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        panic!("a failed preparation must never reach delivery")
    }
    fn failed(&self, created: &CreatedSession, failure: Failure<'_>) -> anyhow::Error {
        assert!(matches!(failure, Failure::Preparing(_)));
        FreshSource.failed(created, failure)
    }
}

#[test]
fn invalid_deadline_stops_before_fresh_resolution() {
    let root = tempfile::tempdir().unwrap();
    let request = AskRequest {
        provider: FirstPartyCli::Codex,
        workspace: root.path().join("missing"),
        prompt: "fixture".into(),
        title: None,
        model: None,
        effort: None,
        terminal: None,
        yolo: false,
        timeout: Duration::MAX,
        detach: false,
        json: false,
        context_results: Vec::new(),
    };
    let mut report = TurnReport::new(false, ResultExtra::new());
    let error = run_ask_inner(request, &mut report).unwrap_err();
    assert!(!format!("{error:#}").contains("workspace"));
    assert!(report.address().is_none());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn failed_result_fields_are_settled_before_a_request_or_launch_exists() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare().unwrap();
    let (error, report) = run(&prepared, &FailedExtra);
    assert_eq!(error.to_string(), "injected extra");
    assert!(report.address().is_none());
    let directory = &prepared.created.directory;
    assert!(!directory.join("turn.claim").exists());
    assert!(!directory.join("requests").exists());
    assert!(!directory.join(CoreRecord::Terminal.name()).exists());
    assert_eq!(
        Reader::open_unchecked(directory).status().unwrap().state,
        SessionState::Failed
    );
}

#[test]
fn unwritable_initial_claim_is_settled_by_the_source_before_the_launch() {
    let mut fixture = Fixture::new();
    fixture.unwritable_claim = true;
    let prepared = fixture.prepare().unwrap();
    let (_, report) = run(&prepared, &FreshSource);
    assert!(report.address().is_none());
    let directory = &prepared.created.directory;
    assert!(!directory.join(CoreRecord::Terminal.name()).exists());
    assert_eq!(
        Reader::open_unchecked(directory).status().unwrap().state,
        SessionState::Failed
    );
}

#[test]
fn real_create_stops_at_launch_begin_with_optional_consent() {
    for consent in [false, true] {
        let mut fixture = Fixture::new();
        if consent {
            fixture.consent = Some(fixture.root.path().canonicalize().unwrap());
        }
        let prepared = fixture.prepare().unwrap();
        let (error, report) = run(&prepared, &FreshSource);
        assert!(!error.to_string().contains("failed to open"), "{error:#}");
        let directory = &prepared.created.directory;
        let (session, request) = report.address().unwrap();
        assert_eq!(prepared.created.id, session);
        let index = requests::list(&Reader::open_unchecked(directory)).unwrap();
        assert_eq!(index.unreadable, 0);
        assert_eq!(index.receipts.len(), 1);
        assert_eq!(index.receipts[0].request_id, request);
        assert_eq!(index.receipts[0].context_sources, context_sources());
        assert_eq!(directory.join("workspace-consent.json").exists(), consent);
        assert!(directory.join(CoreRecord::Launch.name()).is_dir());
        assert!(!directory.join(CoreRecord::Terminal.name()).exists());
        assert!(!directory.join("turn.claim").exists());
        // A fresh launch that failed before the launcher fenced its Claim is settled as a
        // failed start; the unreadable receipt's error follows the launch error.
        let reader = Reader::open_unchecked(directory);
        let receipt_error = launch::read(&reader).unwrap_err();
        let status = reader.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(
            status.error.unwrap(),
            format!("{error:#}; {receipt_error:#}")
        );
        assert!(!directory.join(CoreRecord::ReopenRefusal.name()).exists());
    }
}

// What a settled start failure before the fence leaves for the queries and close: a failed
// Request with the recorded error, the `launch_failed` attention flag, and a close that does
// not wait for the launch deadline.
fn assert_settled_before_fence(fixture: &Fixture, prepared: &Prepared, report: &TurnReport) {
    let directory = &prepared.created.directory;
    let store = Store::open_unchecked(directory);
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    let recorded = status.error.unwrap();
    assert!(!directory.join("turn.claim").exists());
    assert!(!directory.join(CoreRecord::Terminal.name()).exists());
    let (session, request) = report.address().unwrap();
    let result = query::request_result(&store, request).unwrap();
    assert_eq!(result["request_state"], "failed", "{result}");
    assert_eq!(result["error"], recorded.as_str(), "{result}");
    assert!(
        attention(fixture, session).contains(&"launch_failed".to_owned()),
        "{:?}",
        attention(fixture, session)
    );
    close::close(&store, None, close::budget::close_deadline(), |_| {
        panic!("no terminal handle")
    })
    .unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
}

fn attention(fixture: &Fixture, session: &str) -> Vec<String> {
    let status = query::status::test_value_in(fixture.root.path());
    let row = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == session)
        .unwrap_or_else(|| panic!("{status}"));
    row["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|flag| flag.as_str().unwrap().to_owned())
        .collect()
}

// A valid Pending receipt for the Claim: the launch host step fails after `begin` and before
// the fence, and the start settles through the launch failure publication.
#[test]
fn a_host_failure_before_the_fence_settles_the_pending_receipt() {
    let fixture = Fixture::host_failure();
    let prepared = fixture.prepare().unwrap();
    let (error, report) = run(&prepared, &FreshSource);
    assert_eq!(format!("{error:#}"), "launch host is already recorded");
    let store = Store::open_unchecked(&prepared.created.directory);
    let receipt = launch::read(&store).unwrap().unwrap();
    assert_eq!(receipt.phase, launch::Phase::Pending);
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(
        status.error.as_deref(),
        Some("launch host is already recorded")
    );
    assert_settled_before_fence(&fixture, &prepared, &report);
}

// Each launch host terminal records its launch host before the fence: a WezTerm start
// reaches the same host step as an iTerm2 start.
#[test]
fn a_wezterm_start_reaches_the_launch_host_step_before_the_fence() {
    let mut fixture = Fixture::host_failure();
    fixture.terminal_kind = terminal::TerminalKind::WezTerm;
    let prepared = fixture.prepare().unwrap();
    let (error, report) = run(&prepared, &FreshSource);
    assert_eq!(format!("{error:#}"), "launch host is already recorded");
    assert_settled_before_fence(&fixture, &prepared, &report);
}

// The request deadline is already spent when the launch begins: the host step refuses the
// receipt whose deadline has passed, before the fence.
#[test]
fn an_expired_request_deadline_before_the_fence_settles_the_pending_receipt() {
    let mut fixture = Fixture::host_failure();
    fixture.recorded_launch_host = false;
    let prepared = fixture.prepare().unwrap();
    let expired = Start {
        options: StartOptions {
            timeout: Duration::from_secs(30),
            detach: false,
            json: false,
        },
        deadline: Instant::now(),
    };
    let (error, report) = at_unix_ms(1_000_000, || run_with(&expired, &prepared, &FreshSource));
    assert_eq!(format!("{error:#}"), "launch host is no longer eligible");
    let store = Store::open_unchecked(&prepared.created.directory);
    assert_eq!(
        launch::read(&store).unwrap().unwrap().deadline_unix_ms,
        1_000_000
    );
    assert!(
        !prepared
            .created
            .directory
            .join(CoreRecord::LaunchHost.name())
            .exists()
    );
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(
        status.error.as_deref(),
        Some("launch host is no longer eligible")
    );
    assert_settled_before_fence(&fixture, &prepared, &report);
}

// Repair past the receipt's deadline no longer finds a launching Session without its Claim:
// the start failure stays recorded as it was settled.
#[test]
fn repair_past_the_deadline_keeps_a_start_failure_settled_before_the_fence() {
    let fixture = Fixture::host_failure();
    let prepared = fixture.prepare().unwrap();
    let (_, report) = run(&prepared, &FreshSource);
    let store = Store::open_unchecked(&prepared.created.directory);
    let receipt = launch::read(&store).unwrap().unwrap();
    let before = store.status().unwrap();
    assert!(at_unix_ms(receipt.deadline_unix_ms + 1, || launch::repair(&store)).unwrap());
    let after = store.status().unwrap();
    assert_eq!(after.state, SessionState::Failed);
    assert_eq!(after.error, before.error);
    assert!(turn::current_claim_token(&store).unwrap().is_none());
    let (_, request) = report.address().unwrap();
    let result = query::request_result(&store, request).unwrap();
    assert_eq!(result["request_state"], "failed", "{result}");
}

// A settlement whose status write fails keeps the fenced Claim, so the Session is never left
// launching without one; repair past the receipt's deadline settles it.
#[test]
fn a_failed_settlement_before_the_fence_keeps_the_claim_for_repair() {
    let mut fixture = Fixture::host_failure();
    fixture.status_lock_directory = true;
    let prepared = fixture.prepare().unwrap();
    let (error, report) = run(&prepared, &FreshSource);
    let directory = &prepared.created.directory;
    let store = Store::open_unchecked(directory);
    let receipt = launch::read(&store).unwrap().unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Launching);
    assert_eq!(
        turn::current_claim_token(&store).unwrap().as_deref(),
        Some(receipt.claim_token.as_str())
    );
    let text = format!("{error:#}");
    assert!(
        text.starts_with("the failed launch could not be settled: "),
        "{text}"
    );
    assert!(
        text.ends_with(": launch host is already recorded"),
        "{text}"
    );
    fs::remove_dir(directory.join(STATUS_LOCK_FILE)).unwrap();
    assert!(at_unix_ms(receipt.deadline_unix_ms, || launch::repair(&store)).unwrap());
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(
        status.error.as_deref(),
        Some("provider launch timed out before startup was confirmed")
    );
    assert!(turn::current_claim_token(&store).unwrap().is_none());
    let (_, request) = report.address().unwrap();
    let result = query::request_result(&store, request).unwrap();
    assert_eq!(result["request_state"], "failed", "{result}");
}

// No receipt for the Claim (`launch::begin` failed before writing it): the status alone
// records the failure. Without a launch record there is no launch diagnostic, so the Request
// stays unresolved and no `launch_failed` flag appears.
#[test]
fn a_start_failure_without_a_receipt_records_only_the_failed_status() {
    let mut fixture = Fixture::new();
    fixture.receipt_directory = false;
    let prepared = fixture.prepare().unwrap();
    let directory = &prepared.created.directory;
    let store = Store::open_unchecked(directory);
    let mut claim = turn::claim(&store, &context_sources()).unwrap();
    let request = claim.receipt().request_id.clone();
    let error = FreshSource.failed(
        &prepared.created,
        Failure::Launch {
            claim: &mut claim,
            error: anyhow::anyhow!("injected receipt failure"),
        },
    );
    drop(claim);
    assert_eq!(format!("{error:#}"), "injected receipt failure");
    assert!(launch::read(&store).unwrap().is_none());
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some("injected receipt failure"));
    assert!(!directory.join("turn.claim").exists());
    let result = query::request_result(&store, &request).unwrap();
    assert_eq!(result["request_state"], "unresolved", "{result}");
    let flags = attention(&fixture, &prepared.created.id);
    assert!(flags.contains(&"session_failed".to_owned()), "{flags:?}");
    assert!(
        !flags.iter().any(|flag| flag.starts_with("launch_")),
        "{flags:?}"
    );
    close::close(&store, None, close::budget::close_deadline(), |_| {
        panic!("no terminal handle")
    })
    .unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
}

#[test]
fn failed_consent_assessment_stops_before_create_and_consumes_nothing() {
    let mut fixture = Fixture::new();
    fixture.consent = Some(fixture.root.path().join("missing"));
    let error = fixture.prepare().err().unwrap();
    assert_eq!(error.to_string(), "cannot identify workspace");
    assert_eq!(fs::read_dir(fixture.root.path()).unwrap().count(), 0);
}

#[test]
fn unwritable_consent_record_fails_create_and_marks_the_session_failed() {
    let mut fixture = Fixture::new();
    fixture.consent = Some(fixture.root.path().canonicalize().unwrap());
    fixture.unwritable_consent = true;
    let error = fixture.prepare().err().unwrap();
    assert!(!error.to_string().contains("injected"), "{error:#}");
    // Consent state and the Unix root durability receipt are not session directories.
    let directories: Vec<_> = fs::read_dir(fixture.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("session-"))
        })
        .collect();
    assert_eq!(directories.len(), 1);
    let directory = &directories[0];
    assert!(!directory.join("turn.claim").exists());
    assert!(!directory.join("requests").exists());
    assert_eq!(
        Reader::open_unchecked(directory).status().unwrap().state,
        SessionState::Failed
    );
}

// Both starts resolve through one step; a row is what each command supplies to it, the
// terminal kind it selected included. The rows are platform-neutral, so the table runs on
// macOS, Linux, and Windows.
mod resolution {
    use super::*;
    use crate::native::reopen::{self, ReopenGate, ReopenRequest, reopen_refusal_gate};
    use crate::native::session::primitives::STATE_DIR_ENV;
    use crate::native::session::test_support::lifecycle::link_directory;
    use std::process::Command;

    const CHILD: &str = "TABCLI_START_RESOLUTION_CHILD";
    const SOURCE: &str = "session-source";
    // The kind the command selected before the step; the step only carries it.
    const SELECTED: terminal::TerminalKind = terminal::TerminalKind::WindowsConsole;

    #[derive(Clone, Copy, Debug)]
    enum Row {
        Fresh,
        Reopen,
    }

    impl Row {
        fn noun(self) -> &'static str {
            match self {
                Self::Fresh => "workspace",
                Self::Reopen => "source workspace",
            }
        }

        // The row's resolution exactly as its command builds it; only the probe of the
        // installed CLI is the test's.
        fn resolve(
            self,
            workspace: &Path,
            title: Option<&str>,
            probe: impl FnOnce(FirstPartyCli) -> Result<(PathBuf, String)>,
        ) -> Result<Resolved> {
            let title = title.map(str::to_owned);
            match self {
                Self::Fresh => {
                    let request = AskRequest {
                        provider: FirstPartyCli::Codex,
                        workspace: workspace.to_owned(),
                        prompt: "fixture prompt".into(),
                        title,
                        model: Some("model".into()),
                        effort: Some("effort".into()),
                        terminal: None,
                        yolo: true,
                        timeout: Duration::from_secs(30),
                        detach: false,
                        json: false,
                        context_results: Vec::new(),
                    };
                    resolve_with(
                        fresh_resolution(&request, context::resolve(&[]).unwrap(), SELECTED),
                        probe,
                    )
                }
                Self::Reopen => {
                    let request = ReopenRequest {
                        id: SOURCE.into(),
                        prompt: "fixture prompt".into(),
                        title,
                        model: Some("model".into()),
                        effort: Some("effort".into()),
                        terminal: None,
                        yolo: true,
                        timeout: Duration::from_secs(30),
                        detach: false,
                        json: false,
                    };
                    resolve_with(
                        reopen::resolution(&request, FirstPartyCli::Codex, workspace, SELECTED),
                        probe,
                    )
                }
            }
        }
    }

    fn fixture_probe(provider: FirstPartyCli) -> Result<(PathBuf, String)> {
        assert_eq!(provider, FirstPartyCli::Codex);
        Ok((PathBuf::from("codex-fixture"), "fixture".into()))
    }

    fn unreached_probe(_: FirstPartyCli) -> Result<(PathBuf, String)> {
        panic!("the provider is probed only after admission")
    }

    // Resolve symlink/junction ancestors (such as macOS `/var` -> `/private/var`) so the
    // non-alias workspace assertions do not see an alias in the temp root. On Windows, drop
    // the verbatim `\\?\` prefix that `canonicalize()` adds so `cmd /C mklink /J` accepts
    // paths under this directory.
    fn canonical_temp_root(path: &Path) -> PathBuf {
        let canonical = path.canonicalize().unwrap();
        #[cfg(windows)]
        if let Some(text) = canonical.to_str() {
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{unc}"));
            }
            if let Some(ordinary) = text.strip_prefix(r"\\?\") {
                return PathBuf::from(ordinary);
            }
        }
        canonical
    }

    // The child has its own state root and Codex home, so the admission row refuses and the
    // consent assessment reads no real provider store.
    #[test]
    fn fresh_and_reopen_rows_resolve_through_one_step() {
        if std::env::var_os(CHILD).is_none() {
            let base = tempfile::tempdir().unwrap();
            let base_path = canonical_temp_root(base.path());
            let codex_home = base_path.join("codex");
            fs::create_dir_all(codex_home.join("memories")).unwrap();
            let root = base_path.join("state");
            fs::create_dir(&root).unwrap();
            let test_name = format!(
                "{}::fresh_and_reopen_rows_resolve_through_one_step",
                module_path!()
                    .split_once("::")
                    .map_or(module_path!(), |(_, rest)| rest)
            );
            let out = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env(STATE_DIR_ENV, &root)
                .env("HOME", &base_path)
                .env("USERPROFILE", &base_path)
                .env("CODEX_HOME", &codex_home)
                .env_remove("CLAUDE_CONFIG_DIR")
                .env_remove("PI_CODING_AGENT_DIR")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success()
                    && stdout.contains("running 1 test")
                    && stdout.contains("1 passed"),
                "stdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        let root = Reader::state_root().unwrap();
        let work = root.parent().unwrap().join("work");
        let project = work.join("project");
        fs::create_dir_all(&project).unwrap();
        let canonical = project.canonicalize().unwrap();
        let file = work.join("file");
        fs::write(&file, b"").unwrap();
        let missing = work.join("missing");
        let memory = PathBuf::from(std::env::var_os("CODEX_HOME").unwrap())
            .join("memories")
            .join("project");
        fs::create_dir(&memory).unwrap();
        let no_session_was_created = || {
            assert!(fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("session-")
            }));
        };
        let refused = |outcome: Result<Resolved>| outcome.err().expect("the row must refuse");
        for row in [Row::Fresh, Row::Reopen] {
            let noun = row.noun();
            // The two workspace errors, in the row's words, before any probe.
            let error = refused(row.resolve(&missing, None, unreached_probe));
            assert_eq!(
                error.to_string(),
                format!(
                    "{noun} does not exist or cannot be resolved: {}",
                    missing.display()
                )
            );
            assert!(error.chain().count() > 1);
            assert_eq!(reopen_refusal_gate(&error), None);
            let error = refused(row.resolve(&file, None, unreached_probe));
            assert_eq!(
                error.to_string(),
                format!(
                    "{noun} is not a directory: {}",
                    file.canonicalize().unwrap().display()
                )
            );
            assert_eq!(reopen_refusal_gate(&error), None);
            // Admission refuses the memory workspace before the probe; only reopen types it.
            let error = refused(row.resolve(&memory, None, unreached_probe));
            let detail = format!("{error:#}");
            match row {
                Row::Fresh => {
                    assert_eq!(reopen_refusal_gate(&error), None);
                    assert!(detail.starts_with("launch refused: workspace "), "{detail}");
                }
                Row::Reopen => {
                    assert_eq!(
                        reopen_refusal_gate(&error),
                        Some(ReopenGate::WorkspaceRefused)
                    );
                    assert!(
                        detail.starts_with(
                            "reopen refused (workspace-refused): launch refused: workspace "
                        ),
                        "{detail}"
                    );
                }
            }
            assert!(
                detail.contains("codex-memory-workspace is refuse"),
                "{detail}"
            );
            // The probe's own failure passes through: after admission, before title and
            // consent.
            let error = refused(row.resolve(&project, None, |_| bail!("probe failed")));
            assert_eq!(error.to_string(), "probe failed");
            assert_eq!(reopen_refusal_gate(&error), None);
            no_session_was_created();
            // The default title carries the reopen suffix for reopen alone; a given title
            // is sanitized as is.
            let resolved = row.resolve(&project, None, fixture_probe).unwrap();
            let suffix = match row {
                Row::Fresh => "",
                Row::Reopen => " (reopened session-source)",
            };
            assert_eq!(resolved.spec.title, format!("codex · project{suffix}"));
            assert_eq!(resolved.spec.workspace, canonical);
            assert_eq!(resolved.spec.provider, FirstPartyCli::Codex);
            assert_eq!(resolved.spec.provider_path, PathBuf::from("codex-fixture"));
            assert_eq!(resolved.spec.provider_version, "fixture");
            assert_eq!(resolved.spec.model.as_deref(), Some("model"));
            assert_eq!(resolved.spec.effort.as_deref(), Some("effort"));
            assert!(resolved.spec.yolo);
            assert_eq!(resolved.spec.prompt, "fixture prompt");
            assert_eq!(resolved.terminal_kind, SELECTED);
            assert!(resolved.context_sources.is_empty());
            assert_ne!(resolved.consent.as_ref().unwrap().state, "alias");
            let resolved = row
                .resolve(&project, Some(" Custom\ttitle "), fixture_probe)
                .unwrap();
            assert_eq!(resolved.spec.title, "Custom title");
            no_session_was_created();
        }
        // Consent sees the requested path for a fresh start and the canonical path for a
        // reopen: through an alias (a symlink, or a junction on Windows), only the fresh
        // start's assessment records one.
        let alias = work.join("alias");
        assert!(
            link_directory(&project, &alias),
            "an alias of the workspace is needed to tell the two consent paths apart"
        );
        let fresh = Row::Fresh.resolve(&alias, None, fixture_probe).unwrap();
        assert_eq!(fresh.spec.workspace, canonical);
        assert_eq!(fresh.consent.as_ref().unwrap().state, "alias");
        let reopened = Row::Reopen.resolve(&alias, None, fixture_probe).unwrap();
        assert_eq!(reopened.spec.workspace, canonical);
        assert_ne!(reopened.consent.as_ref().unwrap().state, "alias");
        no_session_was_created();
    }
}

#[cfg(unix)]
#[test]
fn consent_receives_original_alias_not_canonical_spec_workspace() {
    let mut fixture = Fixture::new();
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(fixture.root.path(), &alias).unwrap();
    fixture.consent = Some(alias);
    let prepared = fixture.prepare().unwrap();
    let (_, report) = run(&prepared, &FreshSource);
    assert!(report.address().is_some());
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(prepared.created.directory.join("workspace-consent.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["state"], "alias");
}

// A consent failure after a confirmed launch, as `Start::run` meets it: the launcher has
// fenced the initial Claim and the wrapper has recorded the state the adapter's launch plan
// selected. Nothing here sends a prompt, so only that recorded state decides the Request.
mod consent_failure {
    use super::*;
    use crate::native::query;
    use crate::native::session::test_support::{
        injected_fault, with_fault_budget, write_test_manifest,
    };
    use crate::native::session::{close, launch};

    // The text keeps the dialog evidence: a key may have reached the dialog.
    const CONSENT_FAILURE: &str = "workspace trust was not confirmed before the deadline; inspect workspace_consent and the managed terminal; possible response; trust unconfirmed; no input was retried";

    // `awaiting-initial-input` keeps the preserved prompt for delivery after launch;
    // `running` is an argument-delivered prompt the wrapper already removed.
    fn launched(state: SessionState) -> (tempfile::TempDir, Store, turn::Claim) {
        let directory = tempfile::Builder::new()
            .prefix("session-")
            .tempdir()
            .unwrap();
        let store = Store::open_unchecked(directory.path());
        fs::create_dir(store.record(CoreRecord::Events).path()).unwrap();
        write_test_manifest(directory.path(), "claude");
        update_status(store.directory(), SessionState::Launching, None, None).unwrap();
        let mut claim = turn::claim(&store, &[]).unwrap();
        claim.fence_launch();
        if state == SessionState::AwaitingInitialInput {
            store
                .record(CoreRecord::InitialPrompt)
                .write_private(b"prompt")
                .unwrap();
        }
        update_status(store.directory(), state, None, None).unwrap();
        (directory, store, claim)
    }

    fn settle(store: &Store, claim: &mut turn::Claim) -> anyhow::Error {
        settle_consent_failure(store.directory(), claim, anyhow::anyhow!(CONSENT_FAILURE))
    }

    fn assert_not_sent(store: &Store, token: &str, request: &str) {
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(status.error.as_deref(), Some(CONSENT_FAILURE));
        assert!(turn::current_claim_token(store).unwrap().is_none());
        assert_eq!(store.events().unwrap().len(), 1);
        let receipt = requests::for_claim(store, token).unwrap().unwrap();
        let event = store.event_strict(&receipt.event_file).unwrap();
        assert_eq!(
            event.provenance.as_deref(),
            Some(turn::DELIVERY_NOT_SENT_PROVENANCE)
        );
        assert_eq!(event.error.as_deref(), Some(CONSENT_FAILURE));
        let result = query::request_result(store, request).unwrap();
        assert_eq!(result["request_state"], "failed");
        assert_eq!(result["error"], CONSENT_FAILURE);
    }

    #[test]
    fn an_undelivered_initial_prompt_ends_as_not_sent() {
        let (_directory, store, mut claim) = launched(SessionState::AwaitingInitialInput);
        let token = claim.token().to_owned();
        let request = claim.receipt().request_id.clone();
        let error = settle(&store, &mut claim);
        drop(claim);
        assert_eq!(format!("{error:#}"), CONSENT_FAILURE);
        assert_not_sent(&store, &token, &request);
        assert_eq!(
            store.record(CoreRecord::InitialPrompt).bytes().unwrap(),
            Some(b"prompt".to_vec())
        );
    }

    #[test]
    fn an_argument_delivered_claim_is_kept() {
        let (_directory, store, mut claim) = launched(SessionState::Running);
        let token = claim.token().to_owned();
        let request = claim.receipt().request_id.clone();
        let records = || {
            [
                store.record(CoreRecord::Status).bytes().unwrap(),
                store.record(CoreRecord::TurnClaim).bytes().unwrap(),
                fs::read(
                    store
                        .record(CoreRecord::Requests)
                        .path()
                        .join(format!("{token}.json")),
                )
                .ok(),
            ]
        };
        let before = records();
        let error = settle(&store, &mut claim);
        drop(claim);
        assert_eq!(format!("{error:#}"), CONSENT_FAILURE);
        assert_eq!(records(), before);
        assert!(store.events().unwrap().is_empty());
        assert_eq!(
            turn::current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
        let result = query::request_result(&store, &request).unwrap();
        assert_eq!(result["request_state"], "pending");
    }

    // Every fault point of the settlement, up to the budget at which it completes. Each
    // interruption leaves a prefix of the settlement order on disk (journal, event, status,
    // claim release, journal removal); the error claims nothing about which, and recovery
    // converges every prefix that has a journal to the same failed event.
    #[test]
    fn an_interrupted_refusal_claims_nothing_the_records_contradict() {
        const PHASES: [&str; 6] = [
            "nothing recorded",
            "journal",
            "event",
            "status",
            "claim released",
            "journal removed",
        ];
        let mut phases = Vec::new();
        let mut budget = 0;
        loop {
            assert!(budget < 64, "the settlement never completed");
            let (_directory, store, mut claim) = launched(SessionState::AwaitingInitialInput);
            let token = claim.token().to_owned();
            let request = claim.receipt().request_id.clone();
            let error = with_fault_budget(budget, || settle(&store, &mut claim));
            drop(claim);
            let text = format!("{error:#}");
            if !injected_fault(&error) {
                assert_eq!(text, CONSENT_FAILURE, "budget {budget}");
                assert_not_sent(&store, &token, &request);
                break;
            }
            let claimed = turn::current_claim_token(&store).unwrap().is_some();
            let journal = store.record(CoreRecord::Completion).path().exists();
            let event = !store.events().unwrap().is_empty();
            let failed = store.status().unwrap().state == SessionState::Failed;
            let phase = match (claimed, journal, event, failed) {
                (true, false, false, false) => 0,
                (true, true, false, false) => 1,
                (true, true, true, false) => 2,
                (true, true, true, true) => 3,
                (false, true, true, true) => 4,
                (false, false, true, true) => 5,
                other => panic!("budget {budget}: records outside the settlement order: {other:?}"),
            };
            // The text asserts nothing the records contradict.
            if text.contains("stays claimed") {
                assert!(claimed, "budget {budget} ({}): {text}", PHASES[phase]);
            }
            if text.contains("not recorded") {
                assert!(
                    !journal && !event,
                    "budget {budget} ({}): {text}",
                    PHASES[phase]
                );
            }
            assert!(
                text.starts_with(
                    "the not-sent settlement did not complete and may or may not have recorded the refusal; read this request's recorded outcome with `tabcli result` or `tabcli inspect`: "
                ) && text.ends_with(&format!(": {CONSENT_FAILURE}")),
                "budget {budget}: {text}"
            );
            phases.push(phase);
            if phase == 0 {
                assert_eq!(turn::current_claim_token(&store).unwrap(), Some(token));
                let result = query::request_result(&store, &request).unwrap();
                assert_eq!(result["request_state"], "pending", "budget {budget}");
            } else {
                store.converge().unwrap();
                assert!(!store.record(CoreRecord::Completion).path().exists());
                assert_not_sent(&store, &token, &request);
            }
            budget += 1;
        }
        assert!(
            phases.windows(2).all(|pair| pair[0] <= pair[1]),
            "{phases:?}"
        );
        phases.dedup();
        assert_eq!(phases, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_provider_exit_or_close_before_the_settlement_records_nothing() {
        for closed in [false, true] {
            let (_directory, store, mut claim) = launched(SessionState::AwaitingInitialInput);
            let request = claim.receipt().request_id.clone();
            if closed {
                close::close(&store, None, close::budget::close_deadline(), |_| {
                    panic!("no terminal handle")
                })
                .unwrap();
            } else {
                launch::finalize(&store, &Ok(())).unwrap();
            }
            let status = store.record(CoreRecord::Status).bytes().unwrap();
            let error = settle(&store, &mut claim);
            drop(claim);
            assert_eq!(format!("{error:#}"), CONSENT_FAILURE);
            assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), status);
            assert!(store.events().unwrap().is_empty(), "closed: {closed}");
            assert!(turn::current_claim_token(&store).unwrap().is_none());
            let result = query::request_result(&store, &request).unwrap();
            assert_eq!(result["request_state"], "unresolved", "closed: {closed}");
        }
    }
}

// The version note rides in the sentence of the initial Delivery failure, after the session
// and before the cause, and leaves the uncertain marker and the cause as they were.
#[test]
fn the_initial_delivery_failure_carries_the_version_note_beside_the_session() {
    for uncertain in [false, true] {
        let error = initial_delivery_failure(
            anyhow::anyhow!("cause"),
            uncertain,
            FirstPartyCli::Claude,
            "session-note",
            Some("this session runs claude 2".to_owned()),
        );
        assert_eq!(
            format!("{error:#}"),
            "failed to deliver the initial prompt to claude session session-note (this session runs claude 2): cause"
        );
        assert_eq!(
            error.is::<crate::native::failure_kind::DeliveryUncertain>(),
            uncertain
        );
    }
    let error = initial_delivery_failure(
        anyhow::anyhow!("cause"),
        false,
        FirstPartyCli::Claude,
        "session-note",
        None,
    );
    assert_eq!(
        format!("{error:#}"),
        "failed to deliver the initial prompt to claude session session-note: cause"
    );
}

// An unreadable receipt stays unreadable to the queries, whatever the status records. Close
// reports it as its pre-close repair failure and, the start being settled as failed, no
// longer stops at the receipt that a launching Session's deadline check reads.
#[test]
fn an_unreadable_launch_receipt_stays_unreadable_to_queries() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare().unwrap();
    let (_, report) = run(&prepared, &FreshSource);
    let store = Store::open_unchecked(&prepared.created.directory);
    let receipt_error = format!("{:#}", launch::read(&store).unwrap_err());
    assert!(
        receipt_error.starts_with("refusing non-regular session file: "),
        "{receipt_error}"
    );
    let (session, request) = report.address().unwrap();
    let error = query::request_result(&store, request).unwrap_err();
    assert_eq!(format!("{error:#}"), receipt_error);
    let status = query::status::test_value_in(fixture.root.path());
    assert_eq!(status["incomplete"], true, "{status}");
    assert_eq!(
        status["incomplete_reasons"],
        serde_json::json!([{"session": session, "reason": receipt_error}]),
        "{status}"
    );
    assert_eq!(status["sessions"], serde_json::json!([]), "{status}");
    close::close(&store, None, close::budget::close_deadline(), |_| {
        panic!("no terminal handle")
    })
    .unwrap();
    let closed = store.closed_if_present().unwrap().unwrap();
    assert_eq!(closed.state, SessionState::Closed);
    assert_eq!(
        closed.error.unwrap(),
        format!("pre-close session repair failed: {receipt_error}")
    );
}

// A launch failure after the launcher fenced the Claim keeps the terminal failure handoff's
// settlement: the recorded wording, the released Claim and the failed Request. The surface
// fails without a terminal on every platform: macOS and Linux refuse Windows Console, and
// Windows takes the injected failure before it opens one.
#[test]
fn a_surface_failure_after_the_fence_keeps_its_terminal_failure_settlement() {
    let mut fixture = Fixture::new();
    fixture.receipt_directory = false;
    let prepared = fixture.prepare().unwrap();
    #[cfg(windows)]
    terminal::launch_failure_test_support::inject(|_| anyhow::anyhow!("injected surface failure"));
    let (error, report) = run(&prepared, &FreshSource);
    let text = format!("{error:#}");
    let cause = text
        .strip_prefix(&format!(
            "failed to open Windows Console surface for session {}: ",
            prepared.created.id
        ))
        .unwrap_or_else(|| panic!("{text}"));
    #[cfg(windows)]
    assert_eq!(cause, "injected surface failure");
    let store = Store::open_unchecked(&prepared.created.directory);
    assert_eq!(
        launch::read(&store).unwrap().unwrap().phase,
        launch::Phase::Pending
    );
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    let recorded = format!(
        "terminal launch failed: {cause}; no surface handle was recorded and no prompt was sent"
    );
    assert_eq!(status.error.as_deref(), Some(recorded.as_str()));
    assert!(turn::current_claim_token(&store).unwrap().is_none());
    let (_, request) = report.address().unwrap();
    let result = query::request_result(&store, request).unwrap();
    assert_eq!(result["request_state"], "failed", "{result}");
    assert_eq!(result["error"], recorded.as_str(), "{result}");
}

// A Herdr start whose session directory another account can change, which the core's launch
// host refuses, prepared in `fixture`. These Herdr starts use the scripted core, which exists
// where TAB runs Herdr.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn refused_herdr_start(fixture: &mut Fixture) -> Prepared {
    fixture.receipt_directory = false;
    fixture.terminal_kind = terminal::TerminalKind::Herdr;
    fixture.provider = FirstPartyCli::Pi;
    let prepared = fixture.prepare().unwrap();
    let directory = &prepared.created.directory;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
    }
    #[cfg(windows)]
    {
        let system = std::env::var_os("SystemRoot").expect("SystemRoot");
        let status = std::process::Command::new(PathBuf::from(system).join(r"System32\icacls.exe"))
            .arg(directory)
            .args(["/grant", "*S-1-1-0:(W)"])
            .status()
            .unwrap();
        assert!(status.success(), "icacls /grant");
    }
    prepared
}

// The launch host's reason for `refused_herdr_start`, as the caller is given it after the
// launcher's context.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn herdr_refusal_cause<'a>(prepared: &Prepared, text: &'a str) -> &'a str {
    let cause = text
        .strip_prefix(&format!(
            "failed to open Herdr surface for session {}: ",
            prepared.created.id
        ))
        .unwrap_or_else(|| panic!("{text}"));
    // Permission bits that let another account in are judged before the core is reached on
    // macOS and Linux; a Windows access rule is judged on the core's connection, for the
    // core's account, because who may change a directory depends on whose it is.
    let refusal = format!(
        "the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user",
        prepared.created.directory.display()
    );
    assert!(
        cause.starts_with(&refusal)
            || cause.starts_with(&format!("the Herdr terminal was not created: {refusal}")),
        "{cause}"
    );
    cause
}

// What the Session records for `refused_herdr_start`: a fixed reason that names no path.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
const HERDR_REFUSAL_RECORDED: &str = "terminal launch failed: the Herdr launch host would refuse the session directory; no surface handle was recorded and no prompt was sent";

// The case of 2026-11-26 on a supported route: a Herdr start whose session directory the
// core's launch host would refuse (here: another account can change it). Before, the
// terminal was created and its host refused the directory in the launch log only, so the
// start waited for its deadline. Now the adapter judges the directory for the core's account
// on the connection that would create the terminal, and `terminal.run` is never sent. The
// scripted core refuses every `terminal.run`, so the code before this check failed at once
// too, after sending it. The start settles as a refusal before a surface: the receipt stays
// pending, the failure is recorded with the released Claim and the failed Request, the
// launch log keeps the recorded reason, and the session closes without a terminal and is
// pruned. The caller is given the host's reason with the directory; the Session records a
// fixed reason, so a state root whose path contains the legacy residual-surface text does not
// make the failure report a residual surface.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_herdr_directory_its_launch_host_would_refuse_never_reaches_terminal_run() {
    let core = crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore::start();
    let mut fixture = Fixture::new();
    fixture.root = tempfile::Builder::new()
        .prefix(&format!("{} ", launch::RESIDUAL_SURFACE_MARKER))
        .tempdir()
        .unwrap();
    let prepared = refused_herdr_start(&mut fixture);
    let directory = &prepared.created.directory;
    let (error, report) = core.reached_by(|| run(&prepared, &FreshSource));
    let text = format!("{error:#}");
    assert_eq!(core.runs(), 0, "{text}");
    herdr_refusal_cause(&prepared, &text);
    let store = Store::open_unchecked(directory);
    assert_eq!(
        launch::read(&store).unwrap().unwrap().phase,
        launch::Phase::Pending
    );
    let recorded = HERDR_REFUSAL_RECORDED.to_owned();
    let status = store.status().unwrap();
    assert_eq!(status.error, Some(recorded.clone()));
    assert_eq!(status.residual_surface(), None);
    assert!(
        fs::read_to_string(directory.join(launch::LOG))
            .unwrap()
            .contains(&recorded)
    );
    assert!(
        store
            .record(CoreRecord::ProviderProcess)
            .bytes()
            .unwrap()
            .is_none()
    );
    // Only the macOS capture host's stage is recorded before the surface opens.
    assert_eq!(
        store
            .record(CoreRecord::LaunchHost)
            .bytes()
            .unwrap()
            .is_some(),
        cfg!(target_os = "macos")
    );
    assert_settled_before_fence(&fixture, &prepared, &report);
    assert_eq!(
        crate::native::prune::prune_closed_sessions(fixture.root.path(), u128::MAX).unwrap(),
        std::slice::from_ref(&prepared.created.id)
    );
    assert!(!directory.exists());
}

// A reader that holds the lifecycle lock when the refusal settles is waited out within the
// launch's own budget: the start still fails with the recorded refusal and the released Claim,
// as an uncontended one does.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_refusal_settles_once_a_reader_releases_the_lifecycle_lock() {
    let core = crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore::start();
    let mut fixture = Fixture::new();
    let prepared = refused_herdr_start(&mut fixture);
    let held = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = std::rc::Rc::clone(&held);
    launch::launcher::open_failure_hook::set(move |directory| {
        let reader = Store::open_unchecked(directory)
            .lock_shared()
            .unwrap()
            .unwrap();
        seen.set(true);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(reader);
        });
    });
    let (error, report) = core.reached_by(|| run(&prepared, &FreshSource));
    assert!(held.get());
    let text = format!("{error:#}");
    assert_eq!(core.runs(), 0, "{text}");
    let cause = herdr_refusal_cause(&prepared, &text);
    assert!(
        !cause.contains("provider startup is in progress"),
        "{cause}"
    );
    let store = Store::open_unchecked(&prepared.created.directory);
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(HERDR_REFUSAL_RECORDED)
    );
    assert_settled_before_fence(&fixture, &prepared, &report);
}

// A refusal settles only this launch: when the Claim at the session directory's path is not
// this launch's (the directory replaced for a moment), nothing is written there, and the
// caller is still given the host's reason.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_refusal_writes_nothing_to_another_launch_at_its_path() {
    let core = crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore::start();
    let mut fixture = Fixture::new();
    let prepared = refused_herdr_start(&mut fixture);
    launch::launcher::open_failure_hook::set(|directory| {
        launch::test_support::apply(
            &Store::open_unchecked(directory),
            &[
                launch::test_support::Change::ReceiptToken,
                launch::test_support::Change::ClaimOther,
            ],
        );
    });
    let (error, _) = core.reached_by(|| run(&prepared, &FreshSource));
    let text = format!("{error:#}");
    assert_eq!(core.runs(), 0, "{text}");
    let cause = herdr_refusal_cause(&prepared, &text);
    assert!(!cause.contains("handoff"), "{cause}");
    let store = Store::open_unchecked(&prepared.created.directory);
    let status = store.status().unwrap();
    assert_eq!(
        (status.state, status.error),
        (SessionState::Launching, None)
    );
    assert_eq!(
        turn::current_claim_token(&store).unwrap().as_deref(),
        Some("successor")
    );
    let receipt = launch::read(&store).unwrap().unwrap();
    assert_eq!(
        (receipt.claim_token.as_str(), receipt.phase),
        ("successor", launch::Phase::Pending)
    );
}

// A refusal creates nothing in a directory that is not this launch's: a replacement with
// another launch's records and no lifecycle lock file gets no lock file, and nothing is
// written; the caller is still given the host's reason.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_refusal_creates_no_lock_file_in_a_replacement_directory() {
    let core = crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore::start();
    let mut fixture = Fixture::new();
    let prepared = refused_herdr_start(&mut fixture);
    launch::launcher::open_failure_hook::set(|directory| {
        launch::test_support::apply(
            &Store::open_unchecked(directory),
            &[
                launch::test_support::Change::ReceiptToken,
                launch::test_support::Change::ClaimOther,
            ],
        );
        fs::remove_file(directory.join(crate::native::session::test_support::TURN_CLAIM_LOCK_FILE))
            .unwrap();
    });
    let (error, _) = core.reached_by(|| run(&prepared, &FreshSource));
    let text = format!("{error:#}");
    assert_eq!(core.runs(), 0, "{text}");
    let cause = herdr_refusal_cause(&prepared, &text);
    assert!(!cause.contains("handoff"), "{cause}");
    let directory = &prepared.created.directory;
    assert!(
        !directory
            .join(crate::native::session::test_support::TURN_CLAIM_LOCK_FILE)
            .exists()
    );
    let store = Store::open_unchecked(directory);
    let status = store.status().unwrap();
    assert_eq!(
        (status.state, status.error),
        (SessionState::Launching, None)
    );
    assert_eq!(
        turn::current_claim_token(&store).unwrap().as_deref(),
        Some("successor")
    );
}

// A Herdr start whose session directory any account would refuse fails with that refusal
// even when no core answers: the facts that need no account are judged before the core is
// reached, so the refusal is a known refusal before any Surface, not a core failure.
#[cfg(unix)]
#[test]
fn a_herdr_directory_refusal_needs_no_core() {
    let absent = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new();
    fixture.receipt_directory = false;
    fixture.terminal_kind = terminal::TerminalKind::Herdr;
    fixture.provider = FirstPartyCli::Pi;
    let prepared = fixture.prepare().unwrap();
    let directory = &prepared.created.directory;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
    }
    let saved = ["HC_SOCKET", "HC_TERMINAL_ID"].map(|name| (name, std::env::var_os(name)));
    // SAFETY: the test harness runs serially (`--test-threads=1`); the values are restored
    // right after the start.
    unsafe {
        std::env::set_var("HC_SOCKET", absent.path().join("core.sock"));
        std::env::remove_var("HC_TERMINAL_ID");
    }
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&prepared, &FreshSource)
    }));
    for (name, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
    let (error, report) = started.unwrap();
    let text = format!("{error:#}");
    assert_eq!(
        text,
        format!(
            "failed to open Herdr surface for session {}: the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user",
            prepared.created.id,
            directory.display()
        )
    );
    assert_settled_before_fence(&fixture, &prepared, &report);
}

// A launch failure whose Claim the launcher already fenced belongs to the launcher's own
// settlement (the terminal failure handoff or the startup wait); the fresh source adds none,
// even while the Session is still launching.
#[test]
fn a_fenced_launch_failure_is_left_to_the_launchers_settlement() {
    let mut fixture = Fixture::new();
    fixture.receipt_directory = false;
    let prepared = fixture.prepare().unwrap();
    let directory = &prepared.created.directory;
    let store = Store::open_unchecked(directory);
    let mut claim = turn::claim(&store, &context_sources()).unwrap();
    let token = claim.token().to_owned();
    let request = claim.receipt().request_id.clone();
    launch::begin(&store, &token, Instant::now() + Duration::from_secs(30)).unwrap();
    claim.fence_launch();
    let records = || {
        [
            CoreRecord::Status,
            CoreRecord::TurnClaim,
            CoreRecord::Launch,
        ]
        .map(|record| store.record(record).bytes().unwrap())
    };
    let before = records();
    let error = FreshSource.failed(
        &prepared.created,
        Failure::Launch {
            claim: &mut claim,
            error: anyhow::anyhow!("startup is still holding the lifecycle lock"),
        },
    );
    drop(claim);
    assert_eq!(
        format!("{error:#}"),
        "startup is still holding the lifecycle lock"
    );
    assert_eq!(records(), before);
    assert_eq!(store.status().unwrap().state, SessionState::Launching);
    assert_eq!(
        turn::current_claim_token(&store).unwrap().as_deref(),
        Some(token.as_str())
    );
    let result = query::request_result(&store, &request).unwrap();
    assert_eq!(result["request_state"], "pending", "{result}");
}

// Surface admission runs before workspace/probe for platform refusals (Windows Herdr) and
// again right after the version probe so a surface version floor (Codex 0.169.0+ on Herdr)
// refuses before title, consent, session creation, or terminal creation.
#[test]
fn resolve_with_refuses_unsupported_herdr_codex_versions_before_session_creation() {
    use crate::native::failure_kind::{FailureKind, classify};
    use crate::native::session::primitives::STATE_DIR_ENV;
    use std::process::Command;

    const CHILD: &str = "TABCLI_START_HERDR_SURFACE_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let base = tempfile::tempdir().unwrap();
        let codex_home = base.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let root = base.path().join("state");
        fs::create_dir(&root).unwrap();
        let test_name = format!(
            "{}::resolve_with_refuses_unsupported_herdr_codex_versions_before_session_creation",
            module_path!()
                .split_once("::")
                .map_or(module_path!(), |(_, rest)| rest)
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env(STATE_DIR_ENV, &root)
            .env("HOME", base.path())
            .env("USERPROFILE", base.path())
            .env("CODEX_HOME", &codex_home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }

    let root = Reader::state_root().unwrap();
    let project = root.parent().unwrap().join("project");
    fs::create_dir_all(&project).unwrap();
    let missing = root.parent().unwrap().join("missing");
    let request = |provider, workspace: &Path, title: Option<&str>| AskRequest {
        provider,
        workspace: workspace.to_owned(),
        prompt: "fixture prompt".into(),
        title: title.map(str::to_owned),
        model: None,
        effort: None,
        terminal: None,
        yolo: false,
        timeout: Duration::from_secs(30),
        detach: false,
        json: false,
        context_results: Vec::new(),
    };
    let resolve_surface =
        |provider, workspace: &Path, title: Option<&str>, surface, version: Option<&str>| {
            let ask = request(provider, workspace, title);
            resolve_with(
                fresh_resolution(&ask, context::resolve(&[]).unwrap(), surface),
                |_| match version {
                    Some(version) => Ok((PathBuf::from("provider-fixture"), version.to_owned())),
                    None => panic!("probe must not run before early refusal"),
                },
            )
        };
    let assert_no_sessions = || {
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("session-")
        }));
    };

    if cfg!(windows) {
        // Native Windows refuses Codex on Herdr before workspace resolution or probing.
        let error = resolve_surface(
            FirstPartyCli::Codex,
            &missing,
            None,
            terminal::TerminalKind::Herdr,
            None,
        )
        .err()
        .expect("Windows Codex on Herdr must refuse before resolution");
        assert_eq!(classify(&error), Some(FailureKind::Unsupported));
        assert!(
            format!("{error:#}").starts_with("codex on Herdr is unsupported on this platform:"),
            "{error:#}"
        );
        // Managed console still admits Codex below 0.169.0.
        resolve_surface(
            FirstPartyCli::Codex,
            &project,
            None,
            terminal::TerminalKind::WindowsConsole,
            Some("codex-cli 0.168.0"),
        )
        .unwrap();
    } else {
        // Workspace errors still precede probing on Herdr.
        let error = resolve_surface(
            FirstPartyCli::Codex,
            &missing,
            None,
            terminal::TerminalKind::Herdr,
            None,
        )
        .err()
        .expect("missing workspace refuses before probing");
        assert!(error.to_string().contains("workspace does not exist"));

        // Probed Codex versions below stable 0.169.0 (including prereleases above the
        // floor and malformed SemVer suffixes) refuse before creation with a valid title.
        for version in [
            "codex-cli 0.168.0",
            "codex-cli 0.169.0-alpha.1",
            "codex-cli 0.170.0-alpha.1",
            "codex-cli 0.170.0--",
            "codex-cli 0.170.0+",
            "codex-cli 0.168.0+ (runtime 22.0.0)",
            "codex-cli 0.168.0- (protocol 1.0.0)",
            "unknown",
        ] {
            let error = resolve_surface(
                FirstPartyCli::Codex,
                &project,
                None,
                terminal::TerminalKind::Herdr,
                Some(version),
            )
            .err()
            .unwrap_or_else(|| panic!("Herdr must refuse Codex {version} before creation"));
            let detail = format!("{error:#}");
            assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{detail}");
            assert!(
                detail.starts_with(
                    "codex on Herdr is unsupported: Herdr requires stable Codex 0.169.0 or newer with --no-daemon;"
                ),
                "{detail}"
            );
        }

        // Precedence: post-probe surface admission runs before title validation.
        let error = resolve_surface(
            FirstPartyCli::Codex,
            &project,
            Some("   "),
            terminal::TerminalKind::Herdr,
            Some("codex-cli 0.168.0"),
        )
        .err()
        .expect("Herdr version refusal must precede blank-title validation");
        let detail = format!("{error:#}");
        assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{detail}");
        assert!(
            detail.starts_with(
                "codex on Herdr is unsupported: Herdr requires stable Codex 0.169.0 or newer with --no-daemon;"
            ),
            "{detail}"
        );

        // Stable 0.169.0+ is admitted on Herdr; older Codex stays admitted on non-Herdr surfaces.
        resolve_surface(
            FirstPartyCli::Codex,
            &project,
            None,
            terminal::TerminalKind::Herdr,
            Some("codex-cli 0.169.0"),
        )
        .unwrap();
        resolve_surface(
            FirstPartyCli::Codex,
            &project,
            None,
            terminal::TerminalKind::Iterm2,
            Some("codex-cli 0.168.0"),
        )
        .unwrap();
    }

    // Other providers are unaffected by Codex's Herdr rules and admit Herdr for any version.
    for provider in [FirstPartyCli::Claude, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        resolve_surface(
            provider,
            &project,
            None,
            terminal::TerminalKind::Herdr,
            Some("fixture"),
        )
        .unwrap();
    }
    assert_no_sessions();
}

// Post-probe surface admission hands its selected initial Transport straight into Resolved
// and Prepared so initial delivery cannot re-select a transport independently.
#[test]
fn resolve_with_carries_the_admitted_initial_transport_into_prepared() {
    use crate::native::delivery::Readiness;
    use crate::native::session::primitives::STATE_DIR_ENV;
    use std::process::Command;

    const CHILD: &str = "TABCLI_START_ADMITTED_TRANSPORT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let base = tempfile::tempdir().unwrap();
        let codex_home = base.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let root = base.path().join("state");
        fs::create_dir(&root).unwrap();
        let test_name = format!(
            "{}::resolve_with_carries_the_admitted_initial_transport_into_prepared",
            module_path!()
                .split_once("::")
                .map_or(module_path!(), |(_, rest)| rest)
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env(STATE_DIR_ENV, &root)
            .env("HOME", base.path())
            .env("USERPROFILE", base.path())
            .env("CODEX_HOME", &codex_home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }

    let root = Reader::state_root().unwrap();
    let project = root.parent().unwrap().join("project");
    fs::create_dir_all(&project).unwrap();
    let resolve_case = |provider, surface, version: &str| {
        let ask = AskRequest {
            provider,
            workspace: project.clone(),
            prompt: "fixture prompt".into(),
            title: None,
            model: None,
            effort: None,
            terminal: None,
            yolo: false,
            timeout: Duration::from_secs(30),
            detach: false,
            json: false,
            context_results: Vec::new(),
        };
        resolve_with(
            fresh_resolution(&ask, context::resolve(&[]).unwrap(), surface),
            |_| Ok((PathBuf::from("provider-fixture"), version.to_owned())),
        )
        .unwrap()
    };

    let surface = if cfg!(windows) {
        terminal::TerminalKind::WindowsConsole
    } else {
        terminal::TerminalKind::Herdr
    };
    let resolved = resolve_case(FirstPartyCli::Codex, surface, "codex-cli 0.169.0");
    let expected_name = if cfg!(windows) {
        "terminal-paste-fallback"
    } else {
        "launch-argument"
    };
    assert_eq!(resolved.initial_transport.facts().name, expected_name);
    let (prepared, ()) = resolved
        .create_with(|spec, consent| {
            let created = create_session_within(&root, std::slice::from_ref(&root), spec)?;
            record_consent(&created, consent)?;
            Ok((created, ()))
        })
        .unwrap();
    let facts = prepared.initial_transport.facts();
    assert_eq!(facts.name, expected_name);
    if cfg!(windows) {
        assert!(facts.surface.is_some());
    } else {
        assert_eq!(facts.readiness, Readiness::LaunchArgument);
        assert!(facts.surface.is_none());
    }

    let claude = resolve_case(
        FirstPartyCli::Claude,
        terminal::TerminalKind::Herdr,
        "2.1.302",
    );
    assert_eq!(
        claude.initial_transport.facts().name,
        "provider-cross-session-message"
    );
}
