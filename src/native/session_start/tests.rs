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
