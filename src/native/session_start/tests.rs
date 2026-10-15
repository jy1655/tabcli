use super::*;
use std::cell::RefCell;

struct Fake {
    root: tempfile::TempDir,
    stop: &'static str,
    consent: Option<PathBuf>,
    calls: RefCell<Vec<&'static str>>,
    directory: Option<PathBuf>,
}

impl Fake {
    fn new(stop: &'static str) -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            stop,
            consent: None,
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
            assess_consent: self.consent.clone(),
        })
    }
    fn create(&mut self, spec: SessionSpec) -> Result<CreatedSession> {
        self.step("create")?;
        let created =
            create_session_within(self.root.path(), &[self.root.path().to_owned()], spec)?;
        fs::create_dir(created.directory.join(CoreRecord::Launch.name()))?;
        self.directory = Some(created.directory.clone());
        Ok(created)
    }
    fn result_extra(&self) -> Result<ResultExtra> {
        self.step("extra")?;
        Ok(ResultExtra::new())
    }
    fn gate(&self, _: &Path, _: delivery::Gate, _: Instant) -> Result<()> {
        self.step("gate")
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
            vec!["resolve", "create", "extra"],
        ),
    ] {
        let mut fake = Fake::new(stop);
        // A missing path would fail prepare if create did not short circuit.
        if stop == "create" {
            fake.consent = Some(fake.root.path().join("missing"));
        }
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
        } else {
            assert_eq!(fs::read_dir(fake.root.path()).unwrap().count(), 0);
        }
    }
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
fn missing_consent_path_stops_before_extra_and_claim() {
    let mut fake = Fake::new("begin");
    fake.consent = Some(fake.root.path().join("missing"));
    let (error, report) = run(&mut fake, Duration::from_secs(30));
    assert_eq!(error.to_string(), "cannot identify workspace");
    assert_eq!(*fake.calls.borrow(), ["resolve", "create"]);
    assert!(report.address().is_none());
    assert!(!fake.directory.unwrap().join("turn.claim").exists());
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
