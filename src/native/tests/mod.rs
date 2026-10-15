use super::*;
#[cfg(not(any(target_os = "macos", windows)))]
use crate::native::session::close::budget::close_deadline;
use crate::native::session::primitives::{SESSION_DIR_ENV, unix_ms, valid_session_id};
use crate::native::session::schema::{
    EVENT_READ_LIMIT, ProviderProcessRecord, SESSION_SCHEMA, SessionEvent, SessionManifest,
};
use crate::native::session::{Reader, Store, turn};
#[cfg(unix)]
use crate::native::terminal::shell_quote;
use agent_bridge::FirstPartyCli;
use std::process::Command;

#[cfg(target_os = "macos")]
pub(super) use support::write_attested_apple_terminal_state;
pub(super) use support::{
    seed_close_fixture, spawn_surviving_process, test_windows_process_identity,
    write_provider_process_record,
};

mod args;
mod cancel;
mod close;
mod delivery_contracts;
mod elapsed;
mod launch_orchestration;
mod lifecycle;
#[cfg(target_os = "macos")]
mod macos;
mod output;
mod provider_launch;
mod prune;
mod publication;
mod queries;
mod reopen;
mod state_root;
mod support;
#[cfg(windows)]
mod windows;
