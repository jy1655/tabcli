//! The request deadline during synchronous start preparation.
use std::{cell::Cell, time::Instant};

thread_local! {
    static WRITES: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(in crate::native) fn current() -> Option<Instant> {
    DEADLINE.get()
}

pub(in crate::native) fn during<T>(deadline: Instant, prepare: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>, Vec<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.set(self.0);
            WRITES.replace(std::mem::take(&mut self.1));
        }
    }
    let _restore = Restore(DEADLINE.replace(Some(deadline)), WRITES.take());
    prepare()
}

#[cfg(test)]
thread_local! {
    static EXPIRE_AT: Cell<Option<&'static str>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(in crate::native) fn expire_at<T>(step: &'static str, prepare: impl FnOnce() -> T) -> T {
    let previous = EXPIRE_AT.replace(Some(step));
    let result = during(Instant::now() + std::time::Duration::from_secs(60), prepare);
    EXPIRE_AT.set(previous);
    result
}

pub(in crate::native) fn wrote(fact: &'static str) {
    if current().is_some() {
        WRITES.with_borrow_mut(|writes| {
            if !writes.contains(&fact) {
                writes.push(fact);
            }
        });
    }
}

pub(in crate::native) fn before(step: &'static str) -> anyhow::Result<()> {
    let Some(deadline) = current() else {
        return Ok(());
    };
    #[cfg(test)]
    let now = if EXPIRE_AT.get() == Some(step) {
        deadline
    } else {
        Instant::now()
    };
    #[cfg(not(test))]
    let now = Instant::now();
    if now >= deadline {
        let earlier = WRITES.with_borrow(|writes| writes.join("; "));
        let mut message =
            format!("the request's deadline passed before {step}; no Session was created");
        if !earlier.is_empty() {
            message.push_str("; ");
            message.push_str(&earlier);
        }
        anyhow::bail!(crate::native::failure_kind::TimedOut::new(message));
    }
    Ok(())
}

pub(in crate::native) fn lock(
    file: &std::fs::File,
    end: Instant,
    step: &'static str,
) -> anyhow::Result<()> {
    loop {
        before(step)?;
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(
                    end.saturating_duration_since(Instant::now())
                        .min(std::time::Duration::from_millis(5)),
                );
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}
