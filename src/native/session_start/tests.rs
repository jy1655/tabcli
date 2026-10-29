use super::*;
use crate::native::create_session_within;
use crate::native::session::CoreRecord;
use std::fs;

struct Fixture {
    root: tempfile::TempDir,
    consent: Option<PathBuf>,
    unwritable_consent: bool,
    unwritable_claim: bool,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            consent: None,
            unwritable_consent: false,
            unwritable_claim: false,
        }
    }

    fn prepare(&self) -> Result<Prepared> {
        let resolved = Resolved {
            spec: SessionSpec {
                provider: FirstPartyCli::Codex,
                provider_path: PathBuf::from("codex"),
                provider_version: "fixture".into(),
                workspace: self.root.path().canonicalize()?,
                title: "fixture".into(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "fixture".into(),
            },
            terminal_kind: terminal::TerminalKind::WindowsConsole,
            context_sources: context_sources(),
            consent: self
                .consent
                .as_ref()
                .map(|requested| consent::assess(self.root.path(), requested))
                .transpose()?,
        };
        let (prepared, ()) = resolved.create_with(|spec, consent| {
            let created =
                create_session_within(self.root.path(), &[self.root.path().to_owned()], spec)?;
            // Stop before any terminal or provider could be launched.
            fs::create_dir(created.directory.join(CoreRecord::Launch.name()))?;
            if self.unwritable_consent {
                fs::create_dir(created.directory.join("workspace-consent.json"))?;
            }
            record_consent(&created, consent)?;
            if self.unwritable_claim {
                fs::create_dir(created.directory.join("turn.claim"))?;
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
    let mut report = TurnReport::new(false, ResultExtra::new());
    let error = start().run(prepared, source, &mut report).unwrap_err();
    (error, report)
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
        // A fresh launch that failed before a durable receipt keeps ordinary Drop behavior.
        assert_eq!(
            Reader::open_unchecked(directory).status().unwrap().state,
            SessionState::Launching
        );
        assert!(!directory.join(CoreRecord::ReopenRefusal.name()).exists());
    }
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
// terminal kind it selected included. The rows are platform-neutral; the table is kept to
// the platforms where the whole start path, terminal selection included, runs, and it has
// not run on Linux.
#[cfg(any(target_os = "macos", windows))]
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

    // The child has its own state root and Codex home, so the admission row refuses and the
    // consent assessment reads no real provider store.
    #[test]
    fn fresh_and_reopen_rows_resolve_through_one_step() {
        if std::env::var_os(CHILD).is_none() {
            let base = tempfile::tempdir().unwrap();
            let codex_home = base.path().join("codex");
            fs::create_dir_all(codex_home.join("memories")).unwrap();
            let root = base.path().join("state");
            fs::create_dir(&root).unwrap();
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::fresh_and_reopen_rows_resolve_through_one_step"
                    ),
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .env(STATE_DIR_ENV, &root)
                .env("HOME", base.path())
                .env("CODEX_HOME", &codex_home)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
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
            link_directory(&canonical, &alias),
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
