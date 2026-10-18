use std::cell::RefCell;
use std::path::Path;

type Failure = Box<dyn FnOnce(&Path) -> anyhow::Error>;

thread_local! {
    static OPEN_FAILURE: RefCell<Option<Failure>> = const { RefCell::new(None) };
}

pub(in crate::native) fn inject(failure: impl FnOnce(&Path) -> anyhow::Error + 'static) {
    OPEN_FAILURE.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(failure)).is_none());
    });
}

pub(super) fn take_failure(directory: &Path) -> Option<anyhow::Error> {
    let failure = OPEN_FAILURE.with(|slot| slot.borrow_mut().take());
    failure.map(|failure| failure(directory))
}
