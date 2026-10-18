use super::{PendingPiTurn, install_pending_turn};
use crate::native::session::{Store, turn};
#[cfg(windows)]
use crate::native::terminal;
#[cfg(windows)]
use std::cell::RefCell;
use std::path::Path;

#[cfg(windows)]
type ConsoleSend = Box<dyn FnMut(&Path) -> terminal::TerminalSendResult>;

#[cfg(windows)]
thread_local! {
    static CONSOLE_SEND: RefCell<Option<ConsoleSend>> = RefCell::new(None);
    static BEFORE_RECEIPT_READ: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    static AFTER_RETIREMENT: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

#[cfg(windows)]
pub(super) fn before_receipt_read() {
    BEFORE_RECEIPT_READ.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

#[cfg(windows)]
pub(super) fn after_retirement() {
    AFTER_RETIREMENT.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(windows)]
pub(super) fn with_receipt_hooks<T>(
    before: Option<Box<dyn FnMut()>>,
    retired: Option<Box<dyn FnOnce()>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BEFORE_RECEIPT_READ.with(|hook| *hook.borrow_mut() = None);
            AFTER_RETIREMENT.with(|hook| *hook.borrow_mut() = None);
        }
    }
    BEFORE_RECEIPT_READ.with(|hook| *hook.borrow_mut() = before);
    AFTER_RETIREMENT.with(|hook| *hook.borrow_mut() = retired);
    let _reset = Reset;
    run()
}

#[cfg(windows)]
pub(super) fn console_send(path: &Path) -> Option<terminal::TerminalSendResult> {
    CONSOLE_SEND.with(|send| send.borrow_mut().as_mut().map(|send| send(path)))
}

#[cfg(windows)]
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
