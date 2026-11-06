//! `screen`: the visible text of a session's own terminal surface at one moment. The surface
//! binding is read under the Snapshot's shared lifecycle lock, which is released before
//! anything reaches the terminal. The surface is then proven with the ownership check that
//! `tell` and the launch dialog make before they address it and read once through the shared
//! dispatch. The text is returned only if the binding is still the same handle and a second
//! check verifies the same owner record. No input, key or selection request is sent and
//! nothing is repaired or written. The text reaches no record, log or diagnostic, only each
//! helper call's temporary output file.
use super::*;
use crate::native::delivery;
use crate::native::failure_kind::Unsupported;
use crate::native::terminal::ownership::{NativeSessionOwner, verified_terminal_surface_owner};
use observation::SurfaceRecord;
use observation::judgment::{self, ActiveHandle, Surface};

/// The default budget of both ownership proofs and the read: three bounded terminal calls
/// that a responsive terminal answers in about a second, with room for a slow one, short
/// enough that a caller observing a pending request is not kept waiting. On macOS a call
/// that times out is followed by the automation's screen-lock probe of up to 0.5 s, so the
/// command can return that much after the deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest screen returned, in UTF-8 bytes, checked after the capture: a limit on what is
/// returned, not on memory. No adapter truncates its read, and the Windows console refuses a
/// window over 131,072 cells, which is at most 512 KiB with its row breaks. A larger answer
/// fails instead of being cut, so that no partial screen is presented as the screen.
const MAX_SCREEN_BYTES: usize = 512 * 1024;

#[derive(Debug)]
pub(crate) struct ScreenRequest {
    id: String,
    timeout: Duration,
    json: bool,
}

pub(in crate::native) fn parse(args: &[String]) -> Result<NativeCommand> {
    let parsed = (|| {
        let (id, options) = args
            .split_first()
            .context("screen requires one session id")?;
        require_valid_session_id(id)?;
        let mut timeout = None;
        let mut json = false;
        let mut index = 0;
        while index < options.len() {
            match options[index].as_str() {
                "--timeout-secs" => {
                    let value = option_value(options, &mut index, "--timeout-secs")?;
                    set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
                }
                "--json" => set_flag_once(&mut json, "--json")?,
                other => bail!("unknown screen option: {other}"),
            }
            index += 1;
        }
        Ok(ScreenRequest {
            id: id.clone(),
            timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
            json,
        })
    })();
    parsed.map(NativeCommand::Screen)
}

/// The two terminal operations of a read. Production uses the installed terminals; tests
/// replace both and observe what reaches them.
trait SurfaceAccess {
    /// The ownership check `tell` and the launch dialog make before they address a surface,
    /// returning the owner record whose live incarnation it verified (`None` where no
    /// terminal is supported).
    fn verify(
        &self,
        directory: &Path,
        id: &str,
        surface: &terminal::TerminalSession,
        deadline: Instant,
        requested: Duration,
    ) -> Result<Option<NativeSessionOwner>>;
    /// One read through the shared dispatch, which answers a declared refusal itself.
    fn read(&self, surface: &terminal::TerminalSession, deadline: Instant) -> Result<String>;
}

struct Installed;

impl SurfaceAccess for Installed {
    fn verify(
        &self,
        directory: &Path,
        id: &str,
        surface: &terminal::TerminalSession,
        deadline: Instant,
        requested: Duration,
    ) -> Result<Option<NativeSessionOwner>> {
        // As `delivery::verify_terminal_surface_ownership_until`, keeping the owner.
        let timeout = delivery::remaining_turn_timeout(deadline, requested)?;
        verified_terminal_surface_owner(directory, id, surface, Some(timeout))
    }

    fn read(&self, surface: &terminal::TerminalSession, deadline: Instant) -> Result<String> {
        terminal::read_screen(surface, deadline)
    }
}

struct Screen {
    kind: terminal::TerminalKind,
    text: String,
    observed_unix_ms: u128,
}

pub(in crate::native) fn run(request: ScreenRequest) -> Result<()> {
    let outcome = (|| {
        let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
        let directory = Reader::session_directory(&request.id)?;
        observe(
            &directory,
            &request.id,
            deadline,
            request.timeout,
            &Installed,
        )
    })();
    match outcome {
        Ok(screen) => envelope::emit(request.json, &value(&request.id, &screen), || {
            // The text alone, so that it can be piped; controls other than LF and tab are
            // escaped as in every other human rendering of provider output.
            print!("{}", terminal_safe_text(&screen.text, true));
            Ok(())
        }),
        Err(error) => {
            if request.json {
                envelope::print(&crate::native::failure_policy::runtime_error(
                    "screen",
                    &error,
                    serde_json::Map::from_iter([("session".into(), json!(request.id))]),
                ))?;
            }
            Err(error)
        }
    }
}

fn value(id: &str, screen: &Screen) -> Value {
    envelope::ok(serde_json::Map::from_iter([
        ("session".into(), json!(id)),
        ("terminal".into(), json!(screen.kind)),
        ("observed_unix_ms".into(), json!(screen.observed_unix_ms)),
        ("screen".into(), json!(screen.text)),
    ]))
}

fn observe(
    directory: &Path,
    id: &str,
    deadline: Instant,
    requested: Duration,
    access: &dyn SurfaceAccess,
) -> Result<Screen> {
    let surface = bound_surface(&Reader::open_unchecked(directory), id, deadline)?;
    // Declared before any adapter, as the shared dispatch answers it: the ownership proof
    // below would otherwise reach the adapter of a kind that cannot read its screen.
    if let Some(refusal) = surface.kind.registration().screen_read_refusal {
        return Err(anyhow::Error::new(Unsupported::new(format!(
            "{} screen reads are unsupported: {refusal}",
            surface.kind.display_name()
        ))));
    }
    let owner = access
        .verify(directory, id, &surface, deadline, requested)
        .context(
            "the recorded terminal surface was not verified as this session's; nothing was read",
        )?;
    let text = access.read(&surface, deadline).with_context(|| {
        format!(
            "{} did not return the session's screen",
            surface.kind.display_name()
        )
    })?;
    let observed_unix_ms = crate::native::session::primitives::unix_ms();
    if text.len() > MAX_SCREEN_BYTES {
        bail!(
            "the screen is {} bytes, over the {MAX_SCREEN_BYTES}-byte limit; nothing is returned",
            text.len()
        );
    }
    // The adapters address the recorded ids again for the read, without the owner, and a
    // close can begin after the first Snapshot. The same handle, and a second check of the
    // same owner record, show that one owner incarnation held the surface at both checks;
    // not that it held it throughout the read, and not a close begun after the second check.
    confirm(directory, id, &surface, &owner, deadline, requested, access).context(
        "the terminal surface could not be confirmed as this session's after the read; the text is withheld",
    )?;
    Ok(Screen {
        kind: surface.kind,
        text,
        observed_unix_ms,
    })
}

/// The binding read again under a fresh Snapshot's shared lock, required to be the handle
/// that was read, then the same ownership check within the same deadline, required to have
/// verified the owner record the first check verified.
fn confirm(
    directory: &Path,
    id: &str,
    surface: &terminal::TerminalSession,
    owner: &Option<NativeSessionOwner>,
    deadline: Instant,
    requested: Duration,
    access: &dyn SurfaceAccess,
) -> Result<()> {
    if bound_surface(&Reader::open_unchecked(directory), id, deadline)? != *surface {
        bail!("the recorded terminal handle changed during the read");
    }
    let again = access.verify(directory, id, surface, deadline, requested)?;
    // Each check verified the live process its record names; a replaced record can name
    // another incarnation that passes too. Whole-record equality, as the owner teardown's
    // second attestation requires, covers the pid and its birth.
    if serde_json::to_value(&again)? != serde_json::to_value(owner)? {
        bail!("the owner verified after the read is not the owner verified before it");
    }
    Ok(())
}

/// The active handle bound to this session, read with its status under the Snapshot's
/// shared lifecycle lock. A busy Snapshot is retried no later than the deadline, and the lock
/// is released when this returns, before any terminal call.
fn bound_surface(
    reader: &Reader,
    id: &str,
    deadline: Instant,
) -> Result<terminal::TerminalSession> {
    let snapshot = observe_snapshot_until(
        reader,
        PublicationRead::Within(EVENT_READ_LIMIT),
        deadline,
        &mut SystemClock,
    )?;
    let state = snapshot.status.state.clone();
    // An interrupted close can leave its tombstone before status.json catches up.
    if state == SessionState::Closed || reader.closed_if_present()?.is_some() {
        bail!("session {id} is closed; its terminal surface is not read");
    }
    let record = SurfaceRecord::read(reader);
    drop(snapshot);
    match (judgment::surface(&record, id), record) {
        (
            Surface::Active {
                kind,
                supported,
                bound,
                empty_id,
            },
            SurfaceRecord::Active(Ok(Some(handle))),
        ) => match ActiveHandle::of(supported, bound, empty_id) {
            ActiveHandle::Bound => Ok(handle),
            ActiveHandle::Unsupported => Err(anyhow::Error::new(Unsupported::new(format!(
                "the recorded terminal, {}, is not supported on this platform",
                kind.display_name()
            )))),
            ActiveHandle::BindingUnverified => {
                bail!("the recorded terminal handle is not bound to session {id}")
            }
        },
        (Surface::Missing, _) => bail!(
            "session {id} ({state}) has no active terminal handle; a launching session records it once its surface is bound"
        ),
        (Surface::Closed { .. } | Surface::Closing { .. }, _) => {
            bail!("the terminal surface of session {id} is closed or being closed")
        }
        (Surface::Unreadable { reason, .. }, _) => {
            bail!("the terminal surface record of session {id} is unreadable: {reason}")
        }
        (Surface::Active { .. }, _) => unreachable!("only an active handle is judged active"),
    }
}

#[cfg(test)]
mod tests;
