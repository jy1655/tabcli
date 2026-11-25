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
/// | Presence, every close, cleanup after binding fails | Exact path | Either | Launch label |
///
/// A close with no live owner left also observes a running terminal's program with its
/// recorded birth before it ends it (`program_observed`).
pub(super) enum Requirement<'a> {
    Live,
    Bound(&'a Path),
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

    /// Applies the same proof to a fresh observation on the verified connection.
    pub(super) fn check(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        let identity = self.check_identity(core, observed);
        match self.requirement {
            Requirement::Bound(_) => {
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

    /// A close with no live owner left ends a running terminal only after observing its
    /// program with the recorded birth: a program the OS no longer reports may be one the
    /// core still lists while it winds down, and proves nothing about what would be ended.
    pub(super) fn program_observed(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        let address = &observed.address;
        match core.process_start(self.surface.host_pid)? {
            Some(birth) if birth == self.surface.birth => Ok(()),
            Some(_) => bail!(
                "the program of Herdr terminal {address} is another process than the one recorded"
            ),
            None => bail!(
                "the program of Herdr terminal {address} cannot be observed although its core reports it running; nothing was ended and the handle is retained"
            ),
        }
    }

    fn check_identity(&self, core: &dyn Core, observed: &Observed) -> Result<()> {
        let recorded = self.surface;
        let address = &observed.address;
        let directory = match self.requirement {
            Requirement::Live => None,
            Requirement::Bound(directory) => Some(directory),
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
        if observed.keep {
            if observed.state == State::Finished {
                let get = hc_command(&recorded.socket, "get", address, cfg!(windows));
                let remove = hc_command(&recorded.socket, "rm", address, cfg!(windows));
                bail!(
                    "finished Herdr terminal {address} is set to keep; verify it with `{get}`, run `{remove}`, then retry close-session"
                );
            }
            bail!(
                "Herdr terminal {address} is set to keep, so a replacement program can run in it; run `hc keep {address} --off` first"
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
            // Windows writer (0.4.47 on) saved it, so a Windows handle without one is not TAB's.
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
                socket: linked.socket.to_string_lossy().into_owned(),
                core_id: linked.core_id.clone(),
                created_ms: observed.created_ms,
                host_pid: pid,
                birth,
            },
            self.nonce,
        )
    }
}

fn launch_name(nonce: &str) -> Result<String> {
    if nonce.len() < NAME_ENTROPY || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("the Herdr launch token is not hexadecimal");
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
