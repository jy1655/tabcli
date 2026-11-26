//! The verified connection to a core. In production builds the installed code sends a request
//! to a core only through `Linked`: by visibility, `Connection::call` takes a `Permit` that only
//! this module constructs, and the framed stream is private to `framed`. In production code
//! only this module names `terminal.kill` and `terminal.rm` (a source scan checks the literal
//! names; it does not catch a name assembled from parts). `Linked::call` refuses both, and
//! `end_program` and `remove_confirmed` send them only by consuming a grant that
//! `authority::grant` minted from an observation a verified connection produced (`Seen`). The
//! grant names the core that answered that connection and the observed terminal; the
//! connection that consumes it need not be the one that observed the terminal, only a verified
//! connection to the same core (it compares the permanent core id). What this does not cover:
//! test builds can reach a raw stream, and the type system does not stop a new `Connection`
//! implementation from forwarding the permit it receives to another request.
use super::authority::{Kill, Remove};
use super::{CallError, CloseOutcome, Connection, Core, GENERATION, Observed, get};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

/// The capability to send one request on a raw connection. Its field is private to this
/// module, so only this module makes one; a `Connection` implementation receives it, and
/// nothing in the type system stops it from passing it on.
pub(in crate::native::terminal) struct Permit(());

const KILL: &str = "terminal.kill";
const REMOVE: &str = "terminal.rm";
/// The calls that end or remove a terminal.
const MUTATIONS: [&str; 2] = [KILL, REMOVE];
/// The methods a core must offer in `hello`.
pub(super) const REQUIRED_METHODS: [&str; 6] = [
    "terminal.run",
    "terminal.get",
    KILL,
    REMOVE,
    "terminal.send",
    "terminal.read",
];

/// A connection whose `hello` named this core: generation 1, the methods TAB uses, and,
/// when one is expected, the recorded core id.
pub(super) struct Linked<'a> {
    connection: Box<dyn Connection + 'a>,
    socket: PathBuf,
    core_id: String,
}

pub(super) fn link<'a>(
    core: &'a dyn Core,
    socket: &Path,
    expected_core: Option<&str>,
    deadline: Instant,
) -> Result<Linked<'a>> {
    let mut connection = core.connect(socket, deadline)?;
    let hello = connection
        .call(
            Permit(()),
            "hello",
            json!({"generation": GENERATION}),
            deadline,
        )
        .map_err(|error| error.into_error("hello"))?;
    let core_id = validate_hello(&hello)?;
    if let Some(expected) = expected_core
        && expected != core_id
    {
        bail!(
            "a different Herdr Core (id {core_id}) answers at {}; the recorded core {expected} is not reachable there, so the state of this terminal is unknown",
            socket.display()
        );
    }
    Ok(Linked {
        connection,
        socket: socket.to_path_buf(),
        core_id,
    })
}

impl Linked<'_> {
    pub(super) fn socket(&self) -> &Path {
        &self.socket
    }

    /// The permanent id of the core that answered `hello` on this connection.
    pub(super) fn core_id(&self) -> &str {
        &self.core_id
    }

    /// Any call that neither ends nor removes a terminal.
    pub(super) fn call(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        if MUTATIONS.contains(&method) {
            return Err(CallError::NotSent(anyhow!(
                "{method} is sent only for a grant over its verified terminal"
            )));
        }
        self.connection.call(Permit(()), method, params, deadline)
    }

    /// The terminal at `address` as this connection observes it, bound to this connection's core.
    /// The only constructor of `Seen`.
    pub(super) fn observe(&mut self, address: &str, deadline: Instant) -> Result<Option<Seen>> {
        let core_id = self.core_id.clone();
        Ok(get(self, address, deadline)?.map(|observed| Seen { core_id, observed }))
    }

    /// The one `terminal.kill` that `grant` permitted, for the terminal it observed.
    pub(super) fn end_program(&mut self, kill: Kill, deadline: Instant) -> Result<()> {
        match self.mutate(KILL, kill.core_id(), kill.address(), deadline) {
            Ok(_) => Ok(()),
            Err(CallError::Refused { kind, .. }) if kind == "not_found" || kind == "finished" => {
                Ok(())
            }
            Err(error) => Err(error.into_error(KILL)),
        }
    }

    /// The one `terminal.rm` that `grant` permitted, for the terminal it observed, and one read
    /// to confirm it. `not_found` from the recorded core answers the removal itself: another
    /// client removed the terminal after the last read. A removal the core acknowledged while it
    /// still lists the terminal is not sent again; the handle is kept for the next close.
    pub(super) fn remove_confirmed(
        &mut self,
        remove: Remove,
        deadline: Instant,
        outcome: CloseOutcome,
    ) -> Result<CloseOutcome> {
        let address = remove.address().to_owned();
        match self.mutate(REMOVE, remove.core_id(), &address, deadline) {
            Ok(_) => {}
            Err(CallError::Refused { kind, .. }) if kind == "not_found" => return Ok(outcome),
            Err(error) => return Err(error.into_error(REMOVE)),
        }
        if get(self, &address, deadline)?.is_some() {
            bail!(
                "Herdr terminal {address} is still held by its core after {REMOVE}; the handle is retained"
            );
        }
        Ok(CloseOutcome::Closed)
    }

    fn mutate(
        &mut self,
        method: &str,
        core_id: &str,
        address: &str,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        if core_id != self.core_id {
            return Err(CallError::NotSent(anyhow!(
                "the grant for {method} names core {core_id}, not the core {} on this connection",
                self.core_id
            )));
        }
        self.connection
            .call(Permit(()), method, json!({"terminal": address}), deadline)
    }
}

pub(super) fn validate_hello(hello: &Value) -> Result<String> {
    if hello.get("generation").and_then(Value::as_u64) != Some(GENERATION) {
        bail!("Herdr Core does not speak protocol generation {GENERATION}");
    }
    hello
        .get("version")
        .and_then(Value::as_str)
        .context("Herdr hello has no version")?;
    let methods: Vec<&str> = hello
        .get("methods")
        .and_then(Value::as_array)
        .context("Herdr hello has no method list")?
        .iter()
        .map(|method| method.as_str().context("Herdr method name is not text"))
        .collect::<Result<_>>()?;
    if let Some(missing) = REQUIRED_METHODS
        .iter()
        .find(|method| !methods.contains(method))
    {
        bail!("Herdr Core does not offer {missing}");
    }
    let id = hello
        .pointer("/core/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Herdr hello has no core id")?;
    Ok(id.to_owned())
}

/// A terminal as a verified connection observed it: that connection's core and the reply for
/// the terminal, inseparable. Only `Linked::observe` constructs it, so a grant minted from it
/// names the core that actually reported the terminal.
pub(super) struct Seen {
    core_id: String,
    observed: Observed,
}

impl Seen {
    /// The permanent id of the core that reported the terminal.
    pub(super) fn core_id(&self) -> &str {
        &self.core_id
    }

    pub(super) fn observed(&self) -> &Observed {
        &self.observed
    }
}
