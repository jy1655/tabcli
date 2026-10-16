//! The in-memory observation for tests.
//!
//! Every member is a closure the test injects. A member that is not injected forwards to
//! the production adapter when the observation was built `on_records`, so a Record-backed
//! test reads the real Records at the decision's own access points while its OS and
//! surface observations stay injected; an observation built `from_values` answers its
//! Records from values and panics on an observation nobody injected. Every call, Record
//! read or observation, lands in an ordered trace with its result.
// The builders and the trace serve the macOS table; other targets use a subset.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use super::{CloseObservation, Production, TerminalCloseAuthority, decide};
#[cfg(any(target_os = "macos", windows))]
use crate::native::session::{OwnerObservation, SessionState, SessionStatus, launch};
use crate::native::terminal::TerminalSession;
#[cfg(any(target_os = "macos", windows))]
use crate::native::terminal::ownership::NativeSessionOwner;
use anyhow::Result;
use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) struct Call {
    pub(in crate::native) name: &'static str,
    pub(in crate::native) result: String,
}

impl Call {
    pub(in crate::native) fn new(name: &'static str, result: &str) -> Self {
        Self {
            name,
            result: result.to_owned(),
        }
    }
}

// The trace shows what each call answered, including a Record read that failed.
pub(in crate::native) trait Show {
    fn show(&self) -> String;
}

impl Show for bool {
    fn show(&self) -> String {
        self.to_string()
    }
}

impl Show for () {
    fn show(&self) -> String {
        "()".to_owned()
    }
}

impl<T: Show> Show for Option<T> {
    fn show(&self) -> String {
        match self {
            Some(value) => format!("Some({})", value.show()),
            None => "None".to_owned(),
        }
    }
}

impl<T: Show> Show for Result<T> {
    fn show(&self) -> String {
        match self {
            Ok(value) => format!("Ok({})", value.show()),
            Err(error) => format!("Err({error:#})"),
        }
    }
}

#[cfg(target_os = "macos")]
impl Show for crate::native::session::close::TerminalTeardownIntent {
    fn show(&self) -> String {
        format!("teardown(group {}, {:?})", self.process_group, self.reason)
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Show for launch::Record {
    fn show(&self) -> String {
        format!("launch({:?})", self.phase)
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Show for SessionStatus {
    fn show(&self) -> String {
        format!("status({})", self.state)
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Show for NativeSessionOwner {
    fn show(&self) -> String {
        format!(
            "owner(pid {}, session {:?})",
            self.pid,
            self.managed_session_id.as_deref()
        )
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Show for OwnerObservation {
    fn show(&self) -> String {
        format!(
            "observed(alive {:?}, matches {:?})",
            self.process_alive, self.identity_matches
        )
    }
}

#[cfg(target_os = "macos")]
impl Show for crate::native::session::owner::OwnerAttestation {
    fn show(&self) -> String {
        format!("{:?}", self.process)
    }
}

type Member<'a, F> = RefCell<Option<Box<F>>>;

// What `from_values` answers for the Records.
#[cfg(any(target_os = "macos", windows))]
pub(in crate::native) struct RecordValues {
    pub(in crate::native) owner: Option<NativeSessionOwner>,
    pub(in crate::native) state: SessionState,
    pub(in crate::native) launch: Option<launch::Record>,
}

// A test may share the trace so that its own callbacks land between the seam calls.
enum Trace<'a> {
    Owned(RefCell<Vec<Call>>),
    Shared(&'a RefCell<Vec<Call>>),
}

impl Trace<'_> {
    fn cell(&self) -> &RefCell<Vec<Call>> {
        match self {
            Trace::Owned(cell) => cell,
            Trace::Shared(cell) => cell,
        }
    }
}

pub(in crate::native) struct TestObservation<'a> {
    trace: Trace<'a>,
    production: Option<Production<'a>>,
    #[cfg(target_os = "macos")]
    teardown_intent: Member<
        'a,
        dyn FnMut() -> Result<Option<crate::native::session::close::TerminalTeardownIntent>> + 'a,
    >,
    #[cfg(any(target_os = "macos", windows))]
    owner_present: Member<'a, dyn FnMut() -> Result<bool> + 'a>,
    #[cfg(any(target_os = "macos", windows))]
    launch: Member<'a, dyn FnMut() -> Result<Option<launch::Record>> + 'a>,
    #[cfg(any(target_os = "macos", windows))]
    status: Member<'a, dyn FnMut() -> Result<SessionStatus> + 'a>,
    #[cfg(any(target_os = "macos", windows))]
    owner: Member<'a, dyn FnMut() -> Result<NativeSessionOwner> + 'a>,
    #[cfg(target_os = "macos")]
    resumable: Member<'a, dyn FnMut() -> Result<bool> + 'a>,
    surface_present: Member<'a, dyn FnMut() -> Result<bool> + 'a>,
    ownership_proof: Member<'a, dyn FnMut() -> Result<()> + 'a>,
    #[cfg(any(target_os = "macos", windows))]
    owner_record: Member<'a, dyn FnMut(&NativeSessionOwner) -> OwnerObservation + 'a>,
    #[cfg(target_os = "macos")]
    pid_alive: Member<'a, dyn FnMut(u32) -> bool + 'a>,
    #[cfg(target_os = "macos")]
    mac_owner_live: Member<
        'a,
        dyn FnMut(&NativeSessionOwner) -> crate::native::session::owner::OwnerAttestation + 'a,
    >,
    #[cfg(target_os = "macos")]
    attest_owner: Member<'a, dyn FnMut() -> Result<()> + 'a>,
    #[cfg(target_os = "macos")]
    group_gone: Member<'a, dyn FnMut(u32) -> Result<bool> + 'a>,
    #[cfg(target_os = "macos")]
    evidence: Option<&'a dyn crate::native::terminal::close_evidence::CloseEvidence>,
}

impl<'a> TestObservation<'a> {
    fn empty(production: Option<Production<'a>>) -> Self {
        Self {
            trace: Trace::Owned(RefCell::new(Vec::new())),
            production,
            #[cfg(target_os = "macos")]
            teardown_intent: RefCell::new(None),
            #[cfg(any(target_os = "macos", windows))]
            owner_present: RefCell::new(None),
            #[cfg(any(target_os = "macos", windows))]
            launch: RefCell::new(None),
            #[cfg(any(target_os = "macos", windows))]
            status: RefCell::new(None),
            #[cfg(any(target_os = "macos", windows))]
            owner: RefCell::new(None),
            #[cfg(target_os = "macos")]
            resumable: RefCell::new(None),
            surface_present: RefCell::new(None),
            ownership_proof: RefCell::new(None),
            #[cfg(any(target_os = "macos", windows))]
            owner_record: RefCell::new(None),
            #[cfg(target_os = "macos")]
            pid_alive: RefCell::new(None),
            #[cfg(target_os = "macos")]
            mac_owner_live: RefCell::new(None),
            #[cfg(target_os = "macos")]
            attest_owner: RefCell::new(None),
            #[cfg(target_os = "macos")]
            group_gone: RefCell::new(None),
            #[cfg(target_os = "macos")]
            evidence: None,
        }
    }

    /// Record-backed: every member that the test does not inject reads the session
    /// directory or observes the system exactly as production does.
    pub(in crate::native) fn on_records(
        directory: &'a Path,
        expected_session_id: &'a str,
        session: &'a TerminalSession,
        deadline: Instant,
    ) -> Self {
        Self::empty(Some(Production::new(
            directory,
            expected_session_id,
            session,
            deadline,
        )))
    }

    /// In memory: the Records answer from values, no teardown intent exists, and
    /// resumability is false unless injected. Every observation must be injected.
    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn from_values(values: RecordValues) -> Self {
        let RecordValues {
            owner,
            state,
            launch,
        } = values;
        let present = owner.is_some();
        let observation = Self::empty(None)
            .with_owner_present(move || Ok(present))
            .with_launch(move || Ok(launch.clone()))
            .with_status(move || {
                Ok(SessionStatus {
                    state: state.clone(),
                    generation: 0,
                    updated_unix_ms: 1,
                    exit_code: None,
                    error: None,
                    residual_surface: None,
                })
            })
            .with_owner(move || {
                owner
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("the in-memory observation has no owner record"))
            });
        #[cfg(target_os = "macos")]
        let observation = observation
            .with_teardown_intent(|| Ok(None))
            .with_resumable(|| Ok(false));
        observation
    }

    fn production(&self, name: &str) -> &Production<'a> {
        self.production
            .as_ref()
            .unwrap_or_else(|| panic!("{name} was not injected and the observation has no records"))
    }

    // A call is traced when it is made, so that a callback it runs lands after it;
    // its result is filled in when it returns.
    fn begin(&self, name: &'static str) -> usize {
        let mut trace = self.trace.cell().borrow_mut();
        trace.push(Call {
            name,
            result: String::new(),
        });
        trace.len() - 1
    }

    fn finish(&self, call: usize, result: &impl Show) {
        self.trace.cell().borrow_mut()[call].result = result.show();
    }

    /// Records every seam call in `sink`, which the test's own callbacks may also write.
    pub(in crate::native) fn with_trace(mut self, sink: &'a RefCell<Vec<Call>>) -> Self {
        self.trace = Trace::Shared(sink);
        self
    }

    pub(in crate::native) fn trace(&self) -> Vec<Call> {
        self.trace.cell().borrow().clone()
    }

    pub(in crate::native) fn calls(&self) -> Vec<&'static str> {
        self.trace
            .cell()
            .borrow()
            .iter()
            .map(|call| call.name)
            .collect()
    }

    pub(in crate::native) fn decide(
        &self,
        expected_session_id: &str,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<TerminalCloseAuthority> {
        decide(self, expected_session_id, session, deadline)
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_teardown_intent(
        self,
        f: impl FnMut() -> Result<Option<crate::native::session::close::TerminalTeardownIntent>> + 'a,
    ) -> Self {
        *self.teardown_intent.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn with_owner_present(
        self,
        f: impl FnMut() -> Result<bool> + 'a,
    ) -> Self {
        *self.owner_present.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn with_launch(
        self,
        f: impl FnMut() -> Result<Option<launch::Record>> + 'a,
    ) -> Self {
        *self.launch.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn with_status(
        self,
        f: impl FnMut() -> Result<SessionStatus> + 'a,
    ) -> Self {
        *self.status.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn with_owner(
        self,
        f: impl FnMut() -> Result<NativeSessionOwner> + 'a,
    ) -> Self {
        *self.owner.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_resumable(self, f: impl FnMut() -> Result<bool> + 'a) -> Self {
        *self.resumable.borrow_mut() = Some(Box::new(f));
        self
    }

    pub(in crate::native) fn with_presence(self, f: impl FnMut() -> Result<bool> + 'a) -> Self {
        *self.surface_present.borrow_mut() = Some(Box::new(f));
        self
    }

    pub(in crate::native) fn with_proof(self, f: impl FnMut() -> Result<()> + 'a) -> Self {
        *self.ownership_proof.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn with_owner_record(
        self,
        f: impl FnMut(&NativeSessionOwner) -> OwnerObservation + 'a,
    ) -> Self {
        *self.owner_record.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_pid_alive(self, f: impl FnMut(u32) -> bool + 'a) -> Self {
        *self.pid_alive.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_mac_owner_live(
        self,
        f: impl FnMut(&NativeSessionOwner) -> crate::native::session::owner::OwnerAttestation + 'a,
    ) -> Self {
        *self.mac_owner_live.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_attest(self, f: impl FnMut() -> Result<()> + 'a) -> Self {
        *self.attest_owner.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_group_gone(
        self,
        f: impl FnMut(u32) -> Result<bool> + 'a,
    ) -> Self {
        *self.group_gone.borrow_mut() = Some(Box::new(f));
        self
    }

    #[cfg(target_os = "macos")]
    pub(in crate::native) fn with_evidence(
        mut self,
        evidence: &'a dyn crate::native::terminal::close_evidence::CloseEvidence,
    ) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl CloseObservation for TestObservation<'_> {
    #[cfg(target_os = "macos")]
    fn teardown_intent(
        &self,
    ) -> Result<Option<crate::native::session::close::TerminalTeardownIntent>> {
        let call = self.begin("teardown-intent");
        let result = match self.teardown_intent.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("teardown-intent").teardown_intent(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(any(target_os = "macos", windows))]
    fn owner_present(&self) -> Result<bool> {
        let call = self.begin("owner-present");
        let result = match self.owner_present.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("owner-present").owner_present(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(any(target_os = "macos", windows))]
    fn launch(&self) -> Result<Option<launch::Record>> {
        let call = self.begin("launch");
        let result = match self.launch.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("launch").launch(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(any(target_os = "macos", windows))]
    fn status(&self) -> Result<SessionStatus> {
        let call = self.begin("status");
        let result = match self.status.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("status").status(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(any(target_os = "macos", windows))]
    fn owner(&self) -> Result<NativeSessionOwner> {
        let call = self.begin("owner");
        let result = match self.owner.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("owner").owner(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn resumable(&self) -> Result<bool> {
        let call = self.begin("resumable");
        let result = match self.resumable.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("resumable").resumable(),
        };
        self.finish(call, &result);
        result
    }

    fn surface_present(&self) -> Result<bool> {
        let call = self.begin("presence");
        let result = match self.surface_present.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("presence").surface_present(),
        };
        self.finish(call, &result);
        result
    }

    fn ownership_proof(&self) -> Result<()> {
        let call = self.begin("proof");
        let result = match self.ownership_proof.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("proof").ownership_proof(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(any(target_os = "macos", windows))]
    fn observe_owner_record(&self, owner: &NativeSessionOwner) -> OwnerObservation {
        let call = self.begin("owner-record");
        let result = match self.owner_record.borrow_mut().as_mut() {
            Some(f) => f(owner),
            None => self.production("owner-record").observe_owner_record(owner),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn pid_alive(&self, pid: u32) -> bool {
        let call = self.begin("pid-alive");
        let result = match self.pid_alive.borrow_mut().as_mut() {
            Some(f) => f(pid),
            None => self.production("pid-alive").pid_alive(pid),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn mac_owner_live(
        &self,
        owner: &NativeSessionOwner,
    ) -> crate::native::session::owner::OwnerAttestation {
        let call = self.begin("mac-live");
        let result = match self.mac_owner_live.borrow_mut().as_mut() {
            Some(f) => f(owner),
            None => self.production("mac-live").mac_owner_live(owner),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn attest_owner(&self) -> Result<()> {
        let call = self.begin("attest");
        let result = match self.attest_owner.borrow_mut().as_mut() {
            Some(f) => f(),
            None => self.production("attest").attest_owner(),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn group_gone(&self, group: u32) -> Result<bool> {
        let call = self.begin("group-gone");
        let result = match self.group_gone.borrow_mut().as_mut() {
            Some(f) => f(group),
            None => self.production("group-gone").group_gone(group),
        };
        self.finish(call, &result);
        result
    }

    #[cfg(target_os = "macos")]
    fn evidence(&self) -> &dyn crate::native::terminal::close_evidence::CloseEvidence {
        match self.evidence {
            Some(evidence) => evidence,
            None => self.production("evidence").evidence(),
        }
    }
}
