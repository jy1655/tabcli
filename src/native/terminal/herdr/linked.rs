//! The verified connection to a core, and the only way a request reaches one. Every
//! `Connection::call` takes a `Permit` that only this module constructs, so no other code can
//! send a request on a raw connection. `Linked::call` refuses `terminal.kill` and `terminal.rm`,
//! which are sent only by `kill` and `remove`, each consuming a grant that `authority::grant`
//! minted for the terminal it observed on this core.
use super::authority::{Kill, Remove};
use super::{CallError, Connection, Core, GENERATION, validate_hello};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

/// The capability to send one request on a raw connection. Its field is private to this
/// module; a `Connection` implementation receives it and cannot make one.
pub(in crate::native::terminal) struct Permit(());

#[cfg(test)]
impl Permit {
    /// For the transport tests, which exercise a raw connection's framing.
    pub(in crate::native::terminal) fn for_test() -> Self {
        Self(())
    }
}

/// The calls that end or remove a terminal.
const MUTATIONS: [&str; 2] = ["terminal.kill", "terminal.rm"];

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

    /// The one `terminal.kill` that `grant` permitted, for the terminal it observed.
    pub(super) fn kill(&mut self, kill: Kill, deadline: Instant) -> Result<Value, CallError> {
        self.mutate("terminal.kill", kill.core_id(), kill.address(), deadline)
    }

    /// The one `terminal.rm` that `grant` permitted, for the terminal it observed.
    pub(super) fn remove(&mut self, remove: Remove, deadline: Instant) -> Result<Value, CallError> {
        self.mutate("terminal.rm", remove.core_id(), remove.address(), deadline)
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
