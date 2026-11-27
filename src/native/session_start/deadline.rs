//! The request deadline during synchronous start preparation.
use std::{cell::Cell, fs, io, path::Path, time::Instant};

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
    struct Restore(Option<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXPIRE_AT.set(self.0);
        }
    }
    let _restore = Restore(EXPIRE_AT.replace(Some(step)));
    during(Instant::now() + std::time::Duration::from_secs(60), prepare)
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

/// Record creation only for the caller that won the exclusive create.
pub(in crate::native) fn open_lock(
    path: &Path,
    options: &mut fs::OpenOptions,
    fact: &'static str,
) -> io::Result<fs::File> {
    if current().is_none() {
        return options.create(true).open(path);
    }
    match options.create_new(true).open(path) {
        Ok(file) => {
            wrote(fact);
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            options.create_new(false).open(path)
        }
        Err(error) => Err(error),
    }
}

/// Permission evidence is diagnostic only. Its absence must never prevent hardening.
pub(in crate::native) fn set_permissions(
    path: &Path,
    fact: &'static str,
    apply: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let previous = current().and_then(|_| permissions(path));
    apply()?;
    if let Some(previous) = previous
        && permissions(path).is_some_and(|after| previous != after)
    {
        wrote(fact);
    }
    Ok(())
}

#[cfg(unix)]
fn permissions(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(path.metadata().ok()?.permissions().mode())
}

// The Windows setters replace a protected DACL. Compare the self-relative descriptors
// for that information alone, including their protection and inheritance flags.
#[cfg(windows)]
fn permissions(path: &Path) -> Option<Vec<u32>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetFileSecurityW};
    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut bytes = 0;
    unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        )
    };
    if bytes == 0 {
        return None;
    }
    let mut descriptor = vec![0u32; (bytes as usize).div_ceil(size_of::<u32>())];
    let ok = unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    };
    (ok != 0).then_some(descriptor)
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

#[cfg(test)]
pub(in crate::native) fn spend() {
    DEADLINE.set(Some(Instant::now()));
}

#[cfg(test)]
#[test]
fn request_deadline_injected_expiry_is_restored_on_unwind() {
    expire_at("outer", || {
        let _ = std::panic::catch_unwind(|| expire_at("inner", || panic!("injected")));
        assert!(before("outer").is_err(), "outer expiry was lost");
    });
}
