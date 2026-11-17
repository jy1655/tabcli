//! A pidfd is opened BEFORE birth verification. Only this verified type can signal;
//! a reused PID can never redirect a later signal. No numeric-PID signalling fallback.
#![allow(dead_code)] // Phase 2 will use this for its creation-proven server, never a group.
use super::super::LinuxBirth;
use super::process::process_birth;
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
        let pid = i32::try_from(pid).context("pidfd PID out of range")?;
        anyhow::ensure!(pid > 0, "pidfd PID must be positive");
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("could not open pidfd");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(i32::try_from(fd)?) };
        if process_birth(pid as u32)? != Some(birth) {
            return Ok(None);
        }
        Ok(Some(Self(fd)))
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
}
