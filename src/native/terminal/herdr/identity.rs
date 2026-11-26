//! Identity of the terminal a launch requested, then of the terminal in the bound Record.
//! The launch host has a separate binding check using the address and PID it can observe.
use super::{
    Core, HOST_COMMAND, HerdrHostBirth, Linked, Observed, State, TerminalKind, TerminalSession,
    get, hc_command, link, valid_address,
};
use crate::native::terminal::HerdrSurface;
use anyhow::{Context, Result, bail};
use std::{ffi::OsStr, path::Path, time::Instant};

pub(super) const CLOSE_STOPPED: &str = "the close was stopped and the handle retained";
// The name fits hc's 32-character limit and is distinct from terminal addresses.
const NAME_PREFIX: &str = "tabcli-";
const NAME_ENTROPY: usize = 24;

/// The recorded identity of a Herdr handle. An endpoint must be local to this platform.
pub(super) fn target(session: &TerminalSession) -> Result<&HerdrSurface> {
    if session.kind != TerminalKind::Herdr || !valid_address(&session.id) {
        bail!("Herdr handle has no local terminal address");
    }
    let core = session
        .herdr_core()
        .context("Herdr handle has no recorded core")?;
    if core.core_id.is_empty() || !usable_endpoint(&core.socket) {
        bail!("Herdr handle records no usable core");
    }
    Ok(core)
}

fn usable_endpoint(endpoint: &str) -> bool {
    #[cfg(windows)]
    return super::windows_endpoint::local_pipe_name(Path::new(endpoint)).is_ok();
    #[cfg(unix)]
    return Path::new(endpoint).is_absolute();
}

/// Extra evidence each operation requires beyond the common recorded identity:
///
/// | Operations | Directory | State | Additional evidence |
/// | --- | --- | --- | --- |
/// | Send, screen, verify | Managed Session id | Live | Program still exists |
/// | Presence, close with a live owner, cleanup after binding fails | Exact path | Either | |
/// | Close with no live owner (`Created`) | Exact path | Either | A whole creation record, launch label |
///
/// A `Created` close never ends a running terminal (`still_running`).
pub(super) enum Requirement<'a> {
    Live,
    Bound(&'a Path),
    Created(&'a Path),
}

pub(super) struct Recorded<'a> {
    session: &'a TerminalSession,
    surface: &'a HerdrSurface,
    requirement: Requirement<'a>,
}

impl<'a> Recorded<'a> {
    /// Connects to the recorded core and proves the addressed terminal meets the operation's
    /// requirement. Only `not_found` from that core returns `None`.
    pub(super) fn open<'core>(
        core: &'core dyn Core,
        session: &'a TerminalSession,
        requirement: Requirement<'a>,
        deadline: Instant,
    ) -> Result<Option<(Self, Linked<'core>)>> {
        let surface = target(session)?;
        let recorded = Self {
            session,
            surface,
            requirement,
        };
        // Absence from the core settles a close on the creation record only for a handle whose
        // creation record is whole, so it is checked before the core is asked.
        if let Requirement::Created(_) = recorded.requirement {
            recorded.check_creation_record_whole()?;
        }
        let mut linked = link(
            core,
            Path::new(&surface.socket),
            Some(&surface.core_id),
            deadline,
        )?;
        let Some(observed) = get(&mut linked, &session.id, deadline)? else {
            return Ok(None);
        };
        recorded.check(core, &observed)?;
        Ok(Some((recorded, linked)))
    }

    /// The permanent id of the core the handle records.
    pub(super) fn core_id(&self) -> &str {
        &self.surface.core_id
    }

    /// The terminal address the handle records.
    pub(super) fn address(&self) -> &str {
        &self.session.id
    }

    /// Applies the same proof to a fresh observation on the verified connection.
    pub(super) fn check(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        let identity = self.check_identity(core, observed);
        match self.requirement {
            Requirement::Bound(_) => identity,
            Requirement::Created(_) => {
                identity?;
                self.check_launch(observed)
            }
            Requirement::Live => {
                identity?;
                let address = &observed.address;
                if observed.state != State::Live {
                    bail!("Herdr terminal {address} has finished");
                }
                if core.process_start(self.surface.host_pid)?.is_none() {
                    bail!("the program of Herdr terminal {address} has ended");
                }
                Ok(())
            }
        }
    }

    /// The proof `authority::grant` asks of a close on the creation record: the handle was
    /// opened for one, and the observation matches its identity and launch label.
    pub(super) fn check_creation_record(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        if !matches!(self.requirement, Requirement::Created(_)) {
            bail!("a close on the creation record needs a handle opened for one");
        }
        self.check(core, observed)
    }

    /// The handle's own creation record is whole when its decoded values pass three checks,
    /// each derived from the released writers (0.4.45 to 0.4.49): a host PID that this
    /// platform's process reader reports (`host_pid_reported`), a host birth of this platform's
    /// kind with values its writers record (`host_birth_written`), and a launch label as the
    /// generators wrote it, 32 lowercase hexadecimal digits (on Windows, where every writer saved
    /// it, present; never an explicit `null`). The macOS and Linux writers of 0.4.45 and 0.4.46
    /// omitted the field; their label is checked on the terminal and compared with its name
    /// (`check_launch`). The wire spelling is not examined (the shared decoder's aliases,
    /// unknown fields, case): this path ends nothing and removes only a terminal the verified
    /// core identifies, so settling an oddly spelled handle on `not_found` can at worst leave a
    /// terminal the core still lists, while refusing it would strand the session.
    fn check_creation_record_whole(&self) -> Result<()> {
        let address = &self.session.id;
        let damaged = |what: &str| {
            anyhow::anyhow!(
                "Herdr handle for terminal {address} records {what}; the handle is retained"
            )
        };
        if !host_pid_reported(self.surface.host_pid) {
            return Err(damaged(
                "a host PID that this platform's process reader never reports",
            ));
        }
        if !host_birth_written(&self.surface.birth) {
            return Err(damaged(
                "a host birth that no writer on this platform records",
            ));
        }
        if self.session.herdr_launch_nonce_is_null() {
            return Err(damaged("a null launch label, which no writer saved"));
        }
        match self.session.herdr_launch_nonce() {
            Some(nonce) if written_launch_label(nonce) => Ok(()),
            Some(_) => Err(damaged("a damaged launch label")),
            #[cfg(unix)]
            None => Ok(()),
            #[cfg(windows)]
            None => Err(damaged(
                "no launch label, which every Windows Herdr handle TAB wrote has",
            )),
        }
    }

    /// The refusal of a close on the creation record for a running terminal: TAB does not end
    /// it (the owner's decision of 2026-11-26). It says what TAB established (it could not
    /// verify a live owner of this session, and why when the owner record or its process could
    /// not be read), names the recorded core, and names what works, in this platform's `hc`
    /// form on the recorded endpoint: look at the terminal, check the core and the session
    /// directory as the guide says, end the terminal there or wait for it to finish, then close
    /// the session again, which removes the finished terminal.
    pub(super) fn still_running(&self, address: &str, unverified: Option<&str>) -> anyhow::Error {
        let socket = &self.surface.socket;
        let core_id = &self.surface.core_id;
        let session = self
            .session
            .managed_session_id
            .as_deref()
            .unwrap_or("<session>");
        let get = hc_command(socket, "get", address, cfg!(windows));
        let kill = hc_command(socket, "kill", address, cfg!(windows));
        let why = unverified
            .map(|why| format!(" ({why})"))
            .unwrap_or_default();
        anyhow::anyhow!(
            "Herdr terminal {address} on the recorded Herdr Core {core_id} is still running, and TAB could not verify a live owner for this session{why}, so it does not end the terminal; nothing was sent and the handle is retained. Look at it with `{get}`. Before ending it yourself, run `tabcli doctor {session} --probe` to check that the recorded core answers at its endpoint, and check that the program arguments `hc get` shows name this session's directory. Then end it with `{kill}`, or wait for it to finish, and run close-session again: it removes the finished terminal and closes the session"
        )
    }

    fn check_identity(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        let recorded = self.surface;
        let address = &observed.address;
        let directory = match self.requirement {
            Requirement::Live => None,
            Requirement::Bound(directory) | Requirement::Created(directory) => Some(directory),
        };
        let argv_directory = observed.argv.get(2);
        let session_directory = match directory {
            Some(directory) => {
                argv_directory.map(String::as_str) == Some(&*directory.to_string_lossy())
            }
            None => self
                .session
                .managed_session_id
                .as_deref()
                .is_some_and(|id| {
                    argv_directory.and_then(|path| Path::new(path).file_name())
                        == Some(OsStr::new(id))
                }),
        };
        // The Record contains neither the original executable nor the launch script.
        if observed.argv.len() != 5
            || observed.argv[1] != HOST_COMMAND
            || observed.argv[3] != crate::native::terminal::host::LAUNCH_COMMAND_FLAG
            || !session_directory
            || observed.created_ms != recorded.created_ms
            || observed.pid != Some(recorded.host_pid)
        {
            bail!("Herdr terminal {address} is not the terminal this session created");
        }
        // Every check runs on a connection whose `hello` named the recorded core id, so a refusal
        // from here on says that the recorded core answered at its endpoint.
        let core_id = &recorded.core_id;
        if observed.keep {
            if observed.state == State::Finished {
                let get = hc_command(&recorded.socket, "get", address, cfg!(windows));
                let remove = hc_command(&recorded.socket, "rm", address, cfg!(windows));
                bail!(
                    "finished Herdr terminal {address} is set to keep on the recorded Herdr Core {core_id}, which answered at its endpoint; verify it with `{get}`, run `{remove}`, then retry close-session"
                );
            }
            let keep_off = hc_command(
                &recorded.socket,
                "keep",
                &format!("{address} --off"),
                cfg!(windows),
            );
            bail!(
                "Herdr terminal {address} is set to keep on the recorded Herdr Core {core_id}, which answered at its endpoint, so a replacement program can run in it; run `{keep_off}` first"
            );
        }
        if observed.state == State::Live
            && let Some(birth) = core.process_start(recorded.host_pid)?
            && birth != recorded.birth
        {
            bail!(
                "the program of Herdr terminal {address} is another process than the one recorded"
            );
        }
        Ok(())
    }

    /// The launch label TAB set at creation, part of the creation record every close checks in
    /// either terminal state.
    fn check_launch(&self, observed: &Observed) -> Result<()> {
        let address = &observed.address;
        let nonce = observed
            .launch
            .as_deref()
            .with_context(|| format!("Herdr terminal {address} is missing its launch label"))?;
        let matches_launch = match self.session.herdr_launch_nonce() {
            Some(expected) => nonce == expected,
            // Records from 0.4.45 and 0.4.46, on macOS and Linux only, omitted the nonce. Every
            // Windows writer (0.4.47 on) saved it, so a Windows handle without one is not TAB's
            // (`check_creation_record_whole` refuses it first).
            // The observed label must itself be one the generators wrote before its name is
            // compared (`launch_name` refuses any other form).
            #[cfg(unix)]
            None => observed.name.as_deref() == Some(launch_name(nonce)?.as_str()),
            #[cfg(windows)]
            None => bail!(
                "Herdr handle for terminal {address} records no launch label, which every Windows Herdr handle TAB wrote has; the handle is retained"
            ),
        };
        if !matches_launch {
            bail!("Herdr terminal {address} has a different launch label; the handle is retained");
        }
        Ok(())
    }
}

/// The program, label and local name from the request, available before a handle exists.
pub(super) struct Launch<'a> {
    argv: &'a [String],
    pub(super) nonce: &'a str,
    pub(super) name: String,
}

impl<'a> Launch<'a> {
    pub(super) fn new(argv: &'a [String], nonce: &'a str) -> Result<Self> {
        Ok(Self {
            argv,
            nonce,
            name: launch_name(nonce)?,
        })
    }

    fn matches(&self, observed: &Observed) -> bool {
        observed.argv == self.argv
            && observed.launch.as_deref() == Some(self.nonce)
            && observed.name.as_deref() == Some(self.name.as_str())
    }

    pub(super) fn matches_unkept(&self, observed: &Observed) -> bool {
        self.matches(observed) && !observed.keep
    }

    /// Read back on the creating connection, with the program still reported after its
    /// birth was read. The first observation supplies the recorded creation time.
    pub(super) fn confirm(
        &self,
        core: &dyn Core,
        linked: &mut Linked<'_>,
        address: &str,
        deadline: Instant,
    ) -> Result<TerminalSession> {
        let observed = get(linked, address, deadline)?
            .with_context(|| format!("created Herdr terminal {address} is not listed"))?;
        if !self.matches(&observed) {
            bail!(
                "created Herdr terminal {address} does not carry this launch's program and label"
            );
        }
        if observed.keep || observed.state != State::Live {
            bail!("created Herdr terminal {address} is not a live terminal without keep");
        }
        let pid = observed
            .pid
            .with_context(|| format!("created Herdr terminal {address} reports no program yet"))?;
        let birth = core
            .process_start(pid)?
            .with_context(|| format!("the program of Herdr terminal {address} already ended"))?;
        let again = get(linked, address, deadline)?
            .with_context(|| format!("created Herdr terminal {address} disappeared"))?;
        if !self.matches_unkept(&again) || again.state != State::Live || again.pid != Some(pid) {
            bail!("created Herdr terminal {address} changed while it was read");
        }
        Ok(self.handle(linked, &observed, pid, birth))
    }

    /// Cleanup before binding fixes the creation time and PID from the first address read.
    /// This also applies when the host never ran long enough to record a birth.
    pub(super) fn check_cleanup(&self, first: &Observed, observed: &Observed) -> Result<()> {
        let address = &observed.address;
        if !self.matches(observed)
            || observed.created_ms != first.created_ms
            || observed.pid != first.pid
        {
            bail!("Herdr terminal {address} is no longer this launch's terminal");
        }
        if observed.keep {
            bail!(
                "Herdr terminal {address} is set to keep, so a replacement program can run in it"
            );
        }
        Ok(())
    }

    /// Cleanup can hand a handle back to the launcher only after observing a birth.
    pub(super) fn recover_handle(
        &self,
        core: &dyn Core,
        linked: &Linked<'_>,
        observed: &Observed,
    ) -> Option<TerminalSession> {
        let pid = observed.pid?;
        let birth = core.process_start(pid).ok().flatten()?;
        Some(self.handle(linked, observed, pid, birth))
    }

    fn handle(
        &self,
        linked: &Linked<'_>,
        observed: &Observed,
        pid: u32,
        birth: HerdrHostBirth,
    ) -> TerminalSession {
        TerminalSession::new(TerminalKind::Herdr, observed.address.clone()).with_herdr_core(
            HerdrSurface {
                socket: linked.socket().to_string_lossy().into_owned(),
                core_id: linked.core_id().to_owned(),
                created_ms: observed.created_ms,
                host_pid: pid,
                birth,
            },
            self.nonce,
        )
    }
}

/// The form every launch-token generator wrote (0.4.45 on, `unix.rs` and `pipe.rs` today): 16
/// random bytes as 32 lowercase hexadecimal digits.
fn written_launch_label(nonce: &str) -> bool {
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// A PID that this platform's process reader reports for the program a core started. Every
/// writer took the PID the core reported (above zero) and recorded a handle only after that
/// reader returned the program's birth: on macOS (`macos_process_info`) and Linux (procfs
/// `read_stat`) the reader refuses anything above `i32::MAX`; on Windows (`OpenProcess`) it
/// takes any nonzero `DWORD`.
fn host_pid_reported(pid: u32) -> bool {
    #[cfg(unix)]
    return pid > 0 && pid <= i32::MAX as u32;
    #[cfg(windows)]
    return pid > 0;
}

/// A host birth of this platform's kind with values its writers record: on macOS the kernel's
/// start `timeval`, whose microseconds are below a second; on Linux the kernel's boot identity,
/// a random UUID that is never all zero, and any procfs start tick, zero included (a program
/// started in its time namespace's first tick); on Windows any `GetProcessTimes` creation time.
fn host_birth_written(birth: &HerdrHostBirth) -> bool {
    match *birth {
        HerdrHostBirth::Macos {
            host_start_microseconds,
            ..
        } => cfg!(target_os = "macos") && host_start_microseconds < 1_000_000,
        HerdrHostBirth::Linux { linux_birth } => {
            cfg!(target_os = "linux") && !linux_birth.boot_id.is_nil()
        }
        HerdrHostBirth::Windows { .. } => cfg!(windows),
    }
}

/// The local name TAB gives the terminal of a launch label, which must be one the generators
/// wrote.
fn launch_name(nonce: &str) -> Result<String> {
    if !written_launch_label(nonce) {
        bail!("the Herdr launch label is not 32 lowercase hexadecimal digits");
    }
    Ok(format!("{NAME_PREFIX}{}", &nonce[..NAME_ENTROPY]))
}

/// The launch host recognizes its binding by the local marker and its own PID.
pub(super) fn binding_names_host(
    surface: &TerminalSession,
    terminal: Option<&str>,
    pid: u32,
) -> bool {
    surface.kind == TerminalKind::Herdr
        && terminal == Some(surface.id.as_str())
        && surface
            .herdr_core()
            .is_some_and(|core| core.host_pid == pid)
}
