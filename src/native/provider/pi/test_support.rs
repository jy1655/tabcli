use super::{PendingPiTurn, install_pending_turn};
use crate::native::session::{Store, turn};
use crate::native::terminal;
use std::cell::RefCell;
use std::path::Path;

type ConsoleSend = Box<dyn FnMut(&Path) -> terminal::TerminalSendResult>;

thread_local! {
    static CONSOLE_SEND: RefCell<Option<ConsoleSend>> = RefCell::new(None);
}

pub(super) fn console_send(path: &Path) -> Option<terminal::TerminalSendResult> {
    CONSOLE_SEND.with(|send| send.borrow_mut().as_mut().map(|send| send(path)))
}

pub(super) fn with_console_send<T>(send: ConsoleSend, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CONSOLE_SEND.with(|send| *send.borrow_mut() = None);
        }
    }
    CONSOLE_SEND.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(send);
    });
    let _reset = Reset;
    run()
}

pub(super) fn claim_pending_turn(directory: &Path) -> PendingPiTurn {
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    install_pending_turn(directory, &token).unwrap()
}

pub(super) fn marked(message: &str, pending: &PendingPiTurn) -> String {
    format!("{message}\n{}", pending.marker)
}
