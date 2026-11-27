//! A pidfd is opened BEFORE birth verification. Only this verified type can signal;
//! a reused PID can never redirect a later signal. No numeric-PID signalling fallback.
use super::super::LinuxBirth;
use super::process::{ReleasedTaskEntry, process_birth};
use anyhow::{Context, Result};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Instant,
};

pub(in crate::native) struct VerifiedPidfd(OwnedFd);
impl VerifiedPidfd {
    /// None proves the recorded incarnation absent (exit, reuse or another boot).
    /// Any other observation/open failure refuses authority.
    pub(in crate::native) fn open(pid: u32, birth: LinuxBirth) -> Result<Option<Self>> {
        Self::open_with(pid, birth, process_birth)
    }
    fn open_with(
        pid: u32,
        birth: LinuxBirth,
        read_birth: impl Fn(u32) -> Result<Option<LinuxBirth>>,
    ) -> Result<Option<Self>> {
        let raw = i32::try_from(pid).context("pidfd PID out of range")?;
        anyhow::ensure!(raw > 0, "pidfd PID must be positive");
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw, 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("could not open pidfd");
        }
        let held = Self(unsafe { OwnedFd::from_raw_fd(i32::try_from(fd)?) });
        let current = match read_birth(pid) {
            // The read met a task whose signal state was released: either the process exited
            // and its reaper is releasing it, or another of its threads ran exec and took over
            // its PID and start time. The pidfd, held on the PID's process before the read,
            // tells which: an exit proves the recorded incarnation gone (the repeated close of a
            // real tmux server once failed on this read); otherwise the process lives on, and
            // a second read shows its birth or refuses.
            Err(error) if error.downcast_ref::<ReleasedTaskEntry>().is_some() => {
                if held.wait_until(Instant::now())? {
                    return Ok(None);
                }
                read_birth(pid)?
            }
            read => read?,
        };
        if current != Some(birth) {
            return Ok(None);
        }
        Ok(Some(held))
    }
    pub(in crate::native) fn signal(&self, signal: i32) -> Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("could not signal verified pidfd");
        }
        Ok(())
    }
    /// Poll until the deadline; true means exit was observed, not when it happened.
    /// An already readable pidfd can prove exit even at an expired deadline. Callers
    /// enforce their operation budget separately. Zombies are readable here while
    /// the birth-only observer deliberately still sees their incarnation live.
    pub(in crate::native) fn wait_until(&self, deadline: Instant) -> Result<bool> {
        loop {
            let mut poll = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(error).context("could not poll verified pidfd");
            }
            anyhow::ensure!(
                poll.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
                "invalid pidfd poll result"
            );
            if poll.revents & libc::POLLIN != 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_verified_incarnation_can_signal_and_exit_wait_is_bounded() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = process_birth(child.id()).unwrap().unwrap();
        let mut wrong = birth;
        wrong.starttime_ticks = birth.starttime_ticks + 1;
        assert!(VerifiedPidfd::open(child.id(), wrong).unwrap().is_none());
        let fd = VerifiedPidfd::open(child.id(), birth).unwrap().unwrap();
        assert!(!fd.wait_until(Instant::now()).unwrap());
        fd.signal(libc::SIGTERM).unwrap();
        assert!(
            fd.wait_until(Instant::now() + std::time::Duration::from_secs(2))
                .unwrap()
        );
        // Zombie is the same live procfs incarnation until reaped.
        assert_eq!(process_birth(child.id()).unwrap(), Some(birth));
        child.wait().unwrap();
        assert!(VerifiedPidfd::open(child.id(), birth).unwrap().is_none());
    }
    fn released() -> Result<Option<LinuxBirth>> {
        Err(anyhow::Error::new(ReleasedTaskEntry))
    }
    // A read that meets a task being released while the process lives on (after an exec in
    // another of its threads, the PID and start time live on in the new leader): the held
    // pidfd shows no exit, so the birth is read again, and a second such read refuses.
    #[test]
    fn a_released_read_of_a_running_process_reads_its_birth_again() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = process_birth(child.id()).unwrap().unwrap();
        let reads = std::cell::Cell::new(0);
        let held = VerifiedPidfd::open_with(child.id(), birth, |pid| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                released()
            } else {
                process_birth(pid)
            }
        })
        .unwrap();
        assert!(held.is_some(), "a running process was judged gone");
        assert_eq!(reads.get(), 2);
        let error = VerifiedPidfd::open_with(child.id(), birth, |_| released())
            .err()
            .expect("a running process that reads as released twice is refused");
        assert!(
            error.downcast_ref::<ReleasedTaskEntry>().is_some(),
            "{error:#}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
    // The same read of a process that exited (here a zombie its parent has not reaped yet):
    // the held pidfd shows the exit, which proves the recorded incarnation gone.
    #[test]
    fn a_released_read_of_an_exited_process_is_its_absence() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = process_birth(child.id()).unwrap().unwrap();
        let watcher = VerifiedPidfd::open(child.id(), birth).unwrap().unwrap();
        watcher.signal(libc::SIGKILL).unwrap();
        assert!(
            watcher
                .wait_until(Instant::now() + std::time::Duration::from_secs(5))
                .unwrap()
        );
        let reads = std::cell::Cell::new(0);
        let absent = VerifiedPidfd::open_with(child.id(), birth, |_| {
            reads.set(reads.get() + 1);
            released()
        })
        .unwrap();
        assert!(absent.is_none());
        assert_eq!(reads.get(), 1);
        child.wait().unwrap();
    }
}
