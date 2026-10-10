pub(in crate::native) const HOLD_FILE: &str = "hold.json";
pub(in crate::native) const TURN_CLAIM_FILE: &str = "turn.claim";
pub(in crate::native) const TURN_CLAIM_LOCK_FILE: &str = "turn.claim.lock";
pub(in crate::native) const TURN_COMPLETION_FILE: &str = "turn.completion.json";
pub(in crate::native) const STATUS_LOCK_FILE: &str = "status.lock";
pub(in crate::native) const SESSION_OWNER_FILE: &str = "native-session.json";
pub(in crate::native) const CLOSED_STATUS_FILE: &str = "closed.json";
pub(in crate::native) const TERMINAL_HANDLE_FILE: &str = "terminal.json";
pub(in crate::native) const TERMINAL_CLOSING_FILE: &str = "terminal.closing.json";
pub(in crate::native) const TERMINAL_TOMBSTONE_FILE: &str = "terminal.closed.json";
// An explicit close of a surface whose owner must be verified first writes it after
// verifying the live owner and surface, before teardown/close; see
// `terminal_close_intent_owner`.
#[cfg(target_os = "macos")]
pub(in crate::native) const TERMINAL_CLOSE_INTENT_FILE: &str = "terminal.close-intent.json";
// v0.0.2 native-Windows Claude sessions may still carry these files. New sessions never
// create them; explicit close and prune consume them so an upgrade cannot strand state.
pub(in crate::native) const LEGACY_RESUME_PENDING_FILE: &str = "resume.pending.json";
pub(in crate::native) const LEGACY_RESUME_RUNNING_FILE: &str = "resume.running.json";
pub(in crate::native) const UNPUBLISHED_EVENT_PREFIX: &str = "unpublished-";
/// The state root's durability receipt. It exists only after some creator synced the
/// directory entry of the root and of every ancestor up to the filesystem root or the
/// user's home directory, so a root that lacks it is not assumed durable merely because
/// it exists: the creator that made it may have stopped before those syncs.
pub(in crate::native) const STATE_ROOT_DURABLE_FILE: &str = "state-root.durable";
/// Upper bound on the directory entries the state-root ancestry walk makes durable.
pub(in crate::native) const STATE_ROOT_ANCESTRY_SYNC_LIMIT: usize = 16;
// Written into a closed source session by the one reopen that won its turn-claim lock. It is
// the only file a reopen ever adds to the source; the source's tombstone, events, and
// requests stay byte-for-byte intact.
pub(in crate::native) const REOPEN_MARKER_FILE: &str = "reopen.marker.json";
// Written into the NEW session by a reopen gate that fails after the session exists: the
// launch wrapper's pre-spawn ownership recheck or the post-launch holder check. It carries
// the gate name across the process boundary so the reopen response can still report it,
// and it is the durable evidence from which a later reopen reconciles a source marker
// that its parent never settled (`verify_reopen_source_is_closed`).
pub(in crate::native) const REOPEN_REFUSAL_FILE: &str = "reopen.refusal.json";
// Written by the launch wrapper immediately after it spawns the provider process and before
// the session leaves its launch state: the provider's pid and, on Windows, its creation time
// and executable path. The provider process is the only process that can hold a resumed
// conversation (Windows does not end children with their parent, and the wrapper exits
// after the provider), so a reopen refused after this point releases the source's marker
// only once the process this record names is verified gone (`refused_launch_cleanup`).
pub(in crate::native) const PROVIDER_PROCESS_FILE: &str = "provider-process.json";

pub(in crate::native) const STATUS_FILE: &str = "status.json";
pub(in crate::native) const MANIFEST_FILE: &str = "manifest.json";
pub(in crate::native) const INITIAL_PROMPT_FILE: &str = "initial-prompt.txt";
pub(in crate::native) const EVENTS_DIRECTORY: &str = "events";
pub(in crate::native) const REQUESTS_DIRECTORY: &str = "requests";
pub(in crate::native) const LAUNCH_FILE: &str = "launch.json";
