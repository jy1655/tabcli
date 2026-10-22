use super::*;

fn lock_for_close_or_block(file: &File, name: &'static str) -> Result<()> {
    #[cfg(test)]
    close_io_test::at(close_io_test::Point::BeforeLock(name));
    let Some(deadline) = close::budget::deadline() else {
        #[cfg(test)]
        close_io_test::at(close_io_test::Point::BlockingLock);
        return file
            .lock()
            .with_context(|| format!("failed to lock native {name}"));
    };
    let mut contended = false;
    loop {
        let remaining = crate::native::session::close::budget::close_remaining(deadline).map_err(|error| {
            if contended {
                error.context(format!(
                    "another close or turn holds the session's {name} lock; nothing was changed while waiting for this lock; earlier close work is not rolled back"
                ))
            } else {
                error
            }
        })?;
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                contended = true;
                #[cfg(test)]
                close_io_test::at(close_io_test::Point::WouldBlock(name));
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("failed to lock native {name}"));
            }
        }
    }
}

pub(in crate::native) struct StatusLock {
    _file: File,
}

pub(in crate::native) fn lock_status(directory: &Path) -> Result<StatusLock> {
    let path = directory.join(STATUS_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    set_private_file_permissions(&file)?;
    lock_for_close_or_block(&file, "status")?;
    Ok(StatusLock { _file: file })
}

pub(in crate::native) struct TurnClaimLock {
    _file: File,
}

pub(in crate::native) fn lock_turn_claim(path: &Path) -> Result<TurnClaimLock> {
    let lock_path = path.with_file_name(TURN_CLAIM_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    set_private_file_permissions(&file)?;
    lock_for_close_or_block(&file, "turn claim lifecycle")?;
    Ok(TurnClaimLock { _file: file })
}
pub(in crate::native) fn try_lock(directory: &Path) -> Result<Option<TurnClaimLock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(TURN_CLAIM_LOCK_FILE))?;
    set_private_file_permissions(&file)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(TurnClaimLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
