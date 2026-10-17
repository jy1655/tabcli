use super::*;
use crate::native::session::{CoreRecord, Reader, SessionState, requests};
use crate::native::turn_report::TurnReport;
use crate::native::{CreatedSession, SessionSpec, create_session_within, delivery, terminal};
use agent_bridge::FirstPartyCli;
use anyhow::{Result, bail};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Fake {
    root: tempfile::TempDir,
    stop: &'static str,
    consent: Option<PathBuf>,
    // Make the consent record unwritable: a directory where the record file must go.
    unwritable_consent_record: bool,
    // Make the initial claim unwritable the same way.
    unwritable_claim: bool,
    calls: RefCell<Vec<&'static str>>,
    directory: Option<PathBuf>,
}

impl Fake {
    fn new(stop: &'static str) -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            stop,
            consent: None,
            unwritable_consent_record: false,
            unwritable_claim: false,
            calls: RefCell::new(Vec::new()),
            directory: None,
        }
    }
    fn step(&self, name: &'static str) -> Result<()> {
        self.calls.borrow_mut().push(name);
        if self.stop == name {
            bail!("injected {name}")
        }
        Ok(())
    }
}

fn context_sources() -> Vec<requests::ContextSource> {
    vec![requests::ContextSource {
        session: "session-source".into(),
        request_id: Some("request-source".into()),
        event_id: "event-1-1.json".into(),
        provider: "codex".into(),
        created_unix_ms: 1,
    }]
}

impl StartSource for Fake {
    fn resolve(&mut self, _: Instant) -> Result<Resolved> {
        self.step("resolve")?;
        Ok(Resolved {
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
        })
    }
    fn create(
        &mut self,
        spec: SessionSpec,
        consent: Option<&consent::Assessment>,
    ) -> Result<CreatedSession> {
        self.step("create")?;
        let created =
            create_session_within(self.root.path(), &[self.root.path().to_owned()], spec)?;
        fs::create_dir(created.directory.join(CoreRecord::Launch.name()))?;
        self.directory = Some(created.directory.clone());
        if self.unwritable_consent_record {
            fs::create_dir(created.directory.join("workspace-consent.json"))?;
        }
        record_consent(&created, consent)?;
        if self.unwritable_claim {
            fs::create_dir(created.directory.join("turn.claim"))?;
        }
        Ok(created)
    }
    fn result_extra(&self) -> Result<ResultExtra> {
        self.step("extra")?;
        Ok(ResultExtra::new())
    }
    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        self.step("gate")
    }
    fn start_failed(&self, created: &CreatedSession, error: anyhow::Error) -> anyhow::Error {
        self.calls.borrow_mut().push("start_failed");
        mark_start_failed(created, &error);
        error
    }
    fn delivery_failed(
        &self,
        _: &Path,
        _: &str,
        error: anyhow::Error,
        _: Instant,
    ) -> anyhow::Error {
        self.calls.borrow_mut().push("delivery_failed");
        error
    }
}

fn run(fake: &mut Fake, timeout: Duration) -> (anyhow::Error, TurnReport) {
    let mut report = TurnReport::new(false, ResultExtra::new());
    let error = start(
        StartOptions {
            timeout,
            detach: false,
            json: false,
        },
        fake,
        &mut report,
    )
    .unwrap_err();
    (error, report)
}

#[test]
fn pre_open_short_circuits() {
    for (stop, timeout, expected) in [
        ("deadline", Duration::MAX, vec![]),
        ("resolve", Duration::from_secs(30), vec!["resolve"]),
        ("create", Duration::from_secs(30), vec!["resolve", "create"]),
        (
            "extra",
            Duration::from_secs(30),
            vec!["resolve", "create", "extra", "start_failed"],
        ),
    ] {
        let mut fake = Fake::new(stop);
        let (error, report) = run(&mut fake, timeout);
        if stop != "deadline" {
            assert_eq!(error.to_string(), format!("injected {stop}"));
        }
        assert_eq!(*fake.calls.borrow(), expected, "{stop}");
        assert!(report.address().is_none());
        if let Some(directory) = fake.directory {
            assert!(!directory.join("turn.claim").exists());
            assert!(!directory.join("requests").exists());
            assert!(!directory.join("workspace-consent.json").exists());
            // A failure after create is settled by the source: the session is failed.
            assert_eq!(
                Reader::open_unchecked(&directory).status().unwrap().state,
                SessionState::Failed
            );
        } else {
            assert_eq!(fs::read_dir(fake.root.path()).unwrap().count(), 0);
        }
    }
}

#[test]
fn unwritable_initial_claim_is_settled_by_the_source_before_the_launch() {
    // The claim is the last record before the launch; its failure reaches the source's
    // start_failed, which marks the session failed, and the launch never begins (#245).
    let mut fake = Fake::new("begin");
    fake.unwritable_claim = true;
    let (error, report) = run(&mut fake, Duration::from_secs(30));
    assert!(!error.to_string().contains("injected"), "{error:#}");
    assert_eq!(
        *fake.calls.borrow(),
        ["resolve", "create", "extra", "start_failed"]
    );
    assert!(report.address().is_none());
    let directory = fake.directory.unwrap();
    assert!(!directory.join(CoreRecord::Terminal.name()).exists());
    assert_eq!(
        Reader::open_unchecked(&directory).status().unwrap().state,
        SessionState::Failed
    );
}

#[test]
fn real_create_stops_at_launch_begin_with_optional_consent() {
    for consent in [false, true] {
        let mut fake = Fake::new("begin");
        if consent {
            fake.consent = Some(fake.root.path().canonicalize().unwrap());
        }
        let (error, report) = run(&mut fake, Duration::from_secs(30));
        assert!(!error.to_string().contains("failed to open"), "{error:#}");
        assert_eq!(*fake.calls.borrow(), ["resolve", "create", "extra"]);
        let directory = fake.directory.as_ref().unwrap();
        let (session, request) = report.address().unwrap();
        assert_eq!(directory.file_name().unwrap().to_str().unwrap(), session);
        let index = requests::list(&Reader::open_unchecked(directory)).unwrap();
        assert_eq!(index.unreadable, 0);
        assert_eq!(index.receipts.len(), 1);
        let receipt = &index.receipts[0];
        assert_eq!(receipt.request_id, request);
        assert_eq!(receipt.context_sources, context_sources());
        assert_eq!(directory.join("workspace-consent.json").exists(), consent);
        assert!(directory.join(CoreRecord::Launch.name()).is_dir());
        assert!(!directory.join(CoreRecord::Terminal.name()).exists());
        assert!(!directory.join("turn.claim").exists());
    }
}

#[test]
fn failed_consent_assessment_stops_before_create_and_consumes_nothing() {
    // The assessment precedes create, so a failure consumes nothing: no session, no shared
    // record, and for a reopen no source marker, which only create claims (#202).
    let mut fake = Fake::new("begin");
    fake.consent = Some(fake.root.path().join("missing"));
    let (error, report) = run(&mut fake, Duration::from_secs(30));
    assert_eq!(error.to_string(), "cannot identify workspace");
    assert_eq!(*fake.calls.borrow(), ["resolve"]);
    assert!(report.address().is_none());
    assert!(fake.directory.is_none());
    assert_eq!(fs::read_dir(fake.root.path()).unwrap().count(), 0);
}

#[test]
fn unwritable_consent_record_fails_create_and_marks_the_session_failed() {
    // The record is part of create: no extra, claim or, for a reopen, provenance and source
    // marker binds to a session whose assessment could not be recorded (#202 review).
    let mut fake = Fake::new("begin");
    fake.consent = Some(fake.root.path().canonicalize().unwrap());
    fake.unwritable_consent_record = true;
    let (error, report) = run(&mut fake, Duration::from_secs(30));
    assert!(!error.to_string().contains("injected"), "{error:#}");
    assert_eq!(*fake.calls.borrow(), ["resolve", "create"]);
    assert!(report.address().is_none());
    let directory = fake.directory.unwrap();
    assert!(!directory.join("turn.claim").exists());
    assert!(!directory.join("requests").exists());
    assert_eq!(
        Reader::open_unchecked(&directory).status().unwrap().state,
        SessionState::Failed
    );
}

#[cfg(unix)]
#[test]
fn consent_receives_original_alias_not_canonical_spec_workspace() {
    let mut fake = Fake::new("begin");
    let alias = fake.root.path().join("alias");
    std::os::unix::fs::symlink(fake.root.path(), &alias).unwrap();
    fake.consent = Some(alias);
    let (_, report) = run(&mut fake, Duration::from_secs(30));
    assert!(report.address().is_some());
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(fake.directory.unwrap().join("workspace-consent.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["state"], "alias");
    assert_eq!(*fake.calls.borrow(), ["resolve", "create", "extra"]);
}
