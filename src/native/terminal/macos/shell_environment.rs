//! Non-TTY login/interactive exports. No shell output is retained. The supervisor is
//! outside the capture group and owns its cleanup even when the calling host dies.
use crate::native::session::launch::host::FailureReason;
use std::collections::HashSet;
use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::time::{Duration, Instant};

use super::super::shell_environment_budget::{CLEANUP, capture_budget};
use super::super::shell_environment_wire::CAP;

/// How the supervisor spends `CLEANUP` once the shell has ended or run out of time: it ends
/// the capture group within `GROUP_CLEANUP_NS`, which leaves the last tenth of `CLEANUP` for
/// its report to reach the host, and the host stops reading `CLEANUP` after the capture's
/// deadline. Nanoseconds, because the supervisor reads only `monotonic_ns` after the fork.
const GROUP_CLEANUP_NS: u64 = (CLEANUP.as_nanos() - CLEANUP.as_nanos() / 10) as u64;
/// Within `GROUP_CLEANUP_NS`, the group's time to exit after SIGTERM, before SIGKILL: a fifth
/// of `CLEANUP`, so that most of the supervisor's share remains for SIGKILL and the reaping.
const TERMINATE_GRACE_NS: u64 = (CLEANUP.as_nanos() / 5) as u64;
const _: () = assert!(TERMINATE_GRACE_NS < GROUP_CLEANUP_NS);

/// The supervisor's report: four native-endian `i32` words written once, after cleanup. Word 0
/// is the outcome below; word 1 the shell's exit status or signal; word 2 the stopped background
/// members; word 3 the outcome before cleanup, which says what word 1 holds. The supervisor
/// writes these codes and `capture` decodes them.
mod report_code {
    /// The shell exited with status zero.
    pub(super) const EXITED_ZERO: i32 = 0;
    pub(super) const SPAWN_FAILED: i32 = 1;
    pub(super) const TIMEOUT: i32 = 2;
    pub(super) const EXIT_NONZERO: i32 = 3;
    pub(super) const SIGNALED: i32 = 4;
    pub(super) const CLEANUP_FAILED: i32 = 5;
}

#[derive(Debug)]
pub(in crate::native) struct Captured {
    pub(in crate::native) variables: Vec<(OsString, OsString)>,
    pub(in crate::native) elapsed: Duration,
    pub(in crate::native) background_children_stopped: u32,
    /// The budget the capture enforced, decided when it started.
    pub(in crate::native) budget: Duration,
}
#[derive(Debug)]
pub(in crate::native) struct CaptureFailure {
    pub(in crate::native) reason: FailureReason,
    pub(in crate::native) exit_code: Option<i32>,
    pub(in crate::native) signal: Option<i32>,
    pub(in crate::native) elapsed: Duration,
    /// The budget the capture enforced, decided when it started; zero when it left no time.
    pub(in crate::native) budget: Duration,
}

fn decode(bytes: &[u8]) -> Result<Vec<(OsString, OsString)>, FailureReason> {
    super::super::shell_environment_wire::decode(bytes).map(|pairs| {
        pairs
            .into_iter()
            .map(|(key, value)| (OsString::from_vec(key), OsString::from_vec(value)))
            .collect()
    })
}

fn pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [0; 2];
    // SAFETY: valid output storage; ownership is transferred exactly once.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let original = [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])];
        let duplicate = |fd: &OwnedFd| -> io::Result<OwnedFd> {
            let raw = libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10);
            if raw < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(OwnedFd::from_raw_fd(raw))
            }
        };
        // Keep sources above fd 3 even when the host has closed standard descriptors.
        Ok([duplicate(&original[0])?, duplicate(&original[1])?])
    }
}
fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on a live owned pipe descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `deadline` is the receipt/probe deadline, including the handoff and cleanup reserves.
/// Callers supply their own environment; this function applies the helper removal policy.
/// The budget is decided here, when the capture starts, and returned with either outcome.
pub(in crate::native) fn capture(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    deadline: Instant,
) -> Result<Captured, CaptureFailure> {
    let start = Instant::now();
    let budget = capture_budget(start, deadline);
    let failure = |reason, exit_code, signal| CaptureFailure {
        reason,
        exit_code,
        signal,
        elapsed: start.elapsed(),
        budget,
    };
    if budget.is_zero() {
        return Err(failure(FailureReason::LaunchBudgetExhausted, None, None));
    }
    let capture_deadline = start + budget;
    let removals: HashSet<_> = crate::native::process_env::non_provider_removals().collect();
    let mut environment = Vec::new();
    for (name, value) in base {
        if name.to_str().is_some_and(|s| removals.contains(s)) {
            continue;
        }
        let mut entry = name.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        environment.push(
            CString::new(entry).map_err(|_| failure(FailureReason::CaptureInvalid, None, None))?,
        );
    }
    let mut envp: Vec<_> = environment.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    let argv = [
        c"/bin/zsh".as_ptr(),
        c"-l".as_ptr(),
        c"-i".as_ptr(),
        c"-c".as_ptr(),
        c"/usr/bin/env -0 >&3".as_ptr(),
        std::ptr::null(),
    ];
    let setup = || -> io::Result<_> {
        let pipes = [pipe()?, pipe()?, pipe()?, pipe()?, pipe()?, pipe()?];
        // env, stdout, stderr, host lifetime, supervisor report, exec failure.
        for p in &pipes {
            nonblocking(p[0].as_raw_fd())?;
        }
        Ok(pipes)
    };
    let pipes = setup().map_err(|_| failure(FailureReason::CaptureSpawnFailed, None, None))?;
    let raw = pipes
        .each_ref()
        .map(|p| p.each_ref().map(AsRawFd::as_raw_fd));
    // Allocate before fork: the supervisor uses only fixed storage and libc calls.
    let mut pids = vec![0i32; 65536];
    let until = monotonic_ns().saturating_add(
        capture_deadline
            .saturating_duration_since(Instant::now())
            .as_nanos() as u64,
    );
    if Instant::now() >= capture_deadline {
        return Err(failure(FailureReason::LaunchBudgetExhausted, None, None));
    }
    // SAFETY: child code never returns into Rust runtime, allocates, locks or unwinds.
    let supervisor = unsafe { libc::fork() };
    if supervisor < 0 {
        return Err(failure(FailureReason::CaptureSpawnFailed, None, None));
    }
    if supervisor == 0 {
        unsafe { supervise(raw, argv.as_ptr(), envp.as_ptr(), until, &mut pids) }
    }
    let [
        [env_read, env_write],
        [out_read, out_write],
        [err_read, err_write],
        [life_read, life_write],
        [report_read, report_write],
        [exec_read, exec_write],
    ] = pipes;
    drop((
        env_write,
        out_read,
        out_write,
        err_read,
        err_write,
        life_read,
        report_write,
        exec_read,
        exec_write,
    ));
    let mut lifetime = Some(life_write);
    let mut bytes = Vec::new();
    let mut report = [0u8; 16];
    let mut report_len = 0;
    let mut early_failure = None;
    let final_deadline = capture_deadline + CLEANUP;
    loop {
        let mut chunk = [0u8; 16384];
        // A per-iteration bound prevents an endless writer from starving the clock.
        for _ in 0..16 {
            let n =
                unsafe { libc::read(env_read.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break;
            }
            if bytes.len() + n as usize > CAP {
                early_failure = Some(FailureReason::CaptureOversized);
                lifetime.take();
                break;
            }
            bytes.extend_from_slice(&chunk[..n as usize]);
        }
        let n = unsafe {
            libc::read(
                report_read.as_raw_fd(),
                report[report_len..].as_mut_ptr().cast(),
                report.len() - report_len,
            )
        };
        if n > 0 {
            report_len += n as usize;
        }
        if report_len == report.len() {
            break;
        }
        if Instant::now() >= final_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Supervisor completion precedes this final readable-only drain. A daemon holding
    // fd 3 cannot hold us open; no read waits for EOF and no thread is joined.
    for _ in 0..=CAP / 16384 {
        let mut chunk = [0u8; 16384];
        let n = unsafe { libc::read(env_read.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        if bytes.len() + n as usize > CAP {
            early_failure = Some(FailureReason::CaptureOversized);
            break;
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
    }
    drop(lifetime);
    // A report can become readable just before _exit. Reap within the same cleanup
    // bound so repeated captures do not accumulate supervisor zombies.
    loop {
        let reaped = unsafe { libc::waitpid(supervisor, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped != 0 || Instant::now() >= final_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if report_len != report.len() {
        return Err(failure(FailureReason::CleanupFailed, None, None));
    }
    let words: [i32; 4] = std::array::from_fn(|i| i32::from_ne_bytes(report.as_chunks::<4>().0[i]));
    let reason = match words[0] {
        report_code::EXITED_ZERO => None,
        report_code::SPAWN_FAILED => Some(FailureReason::CaptureSpawnFailed),
        report_code::TIMEOUT => Some(FailureReason::CaptureTimeout),
        report_code::EXIT_NONZERO => Some(FailureReason::CaptureExitNonzero),
        report_code::SIGNALED => Some(FailureReason::CaptureSignaled),
        // `report_code::CLEANUP_FAILED`, and any word the supervisor does not write.
        _ => Some(FailureReason::CleanupFailed),
    };
    let exit_code = (words[3] == report_code::EXIT_NONZERO).then_some(words[1]);
    let signal = (words[3] == report_code::SIGNALED).then_some(words[1]);
    let reason = if reason == Some(FailureReason::CleanupFailed) {
        reason
    } else {
        early_failure.or(reason)
    };
    if let Some(reason) = reason {
        return Err(failure(reason, exit_code, signal));
    }
    let variables = decode(&bytes).map_err(|r| failure(r, None, None))?;
    Ok(Captured {
        variables,
        elapsed: start.elapsed(),
        background_children_stopped: words[2] as u32,
        budget,
    })
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// Everything below runs in a post-fork process. Use only stack/preallocated storage
/// and libc operations, and terminate with _exit rather than Rust destructors.
unsafe fn supervise(
    fds: [[RawFd; 2]; 6],
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    until: u64,
    pids: &mut [i32],
) -> ! {
    unsafe {
        libc::setsid();
        // Retain wait authority even if a caller ignored SIGCHLD.
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        // Do not retain unrelated pipes or the host-lifetime writer in the supervisor.
        let keep = [
            fds[0][1], fds[1][0], fds[1][1], fds[2][0], fds[2][1], fds[3][0], fds[4][1], fds[5][0],
            fds[5][1],
        ];
        let max_fd = libc::getdtablesize();
        for fd in 0..max_fd {
            if !keep.contains(&fd) {
                libc::close(fd);
            }
        }
        let child = libc::fork();
        if child == 0 {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
            // All source descriptors were created above the standard descriptors.
            let ok = libc::setsid() >= 0
                && null >= 0
                && libc::dup2(null, 0) >= 0
                && libc::dup2(fds[1][1], 1) >= 0
                && libc::dup2(fds[2][1], 2) >= 0
                && libc::dup2(fds[0][1], 3) >= 0;
            for fd in 4..max_fd {
                if fd != fds[5][1] {
                    libc::close(fd);
                }
            }
            if ok {
                libc::execve(c"/bin/zsh".as_ptr(), argv, envp);
            }
            let marker = [1u8];
            libc::write(fds[5][1], marker.as_ptr().cast(), 1);
            libc::_exit(127);
        }
        libc::close(fds[0][1]);
        libc::close(fds[1][1]);
        libc::close(fds[2][1]);
        libc::close(fds[5][1]);
        let mut report = [report_code::EXITED_ZERO, 0, 0, 0];
        if child < 0 {
            report[0] = report_code::SPAWN_FAILED;
        } else {
            loop {
                discard(fds[1][0]);
                discard(fds[2][0]);
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let wait = libc::waitid(
                    libc::P_PID,
                    child as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                );
                if wait == 0 && info.si_pid == child {
                    let mut marker = 0u8;
                    if libc::read(fds[5][0], (&mut marker as *mut u8).cast(), 1) > 0 {
                        report[0] = report_code::SPAWN_FAILED;
                    } else if info.si_code == libc::CLD_EXITED {
                        if info.si_status != 0 {
                            report[0] = report_code::EXIT_NONZERO;
                            report[1] = info.si_status;
                        }
                    } else {
                        report[0] = report_code::SIGNALED;
                        report[1] = info.si_status;
                    }
                    break;
                }
                let mut byte = 0u8;
                let host = libc::read(fds[3][0], (&mut byte as *mut u8).cast(), 1);
                if host == 0 || monotonic_ns() >= until {
                    report[0] = report_code::TIMEOUT;
                    break;
                }
                if wait < 0 {
                    report[0] = report_code::CLEANUP_FAILED;
                    break;
                }
                libc::usleep(2000);
            }
            report[3] = report[0];
            // Keep the shell unreaped until the last signal: its PID cannot be reused
            // for an unrelated group while cleanup retains negative-PID authority.
            let cleanup_end = monotonic_ns() + GROUP_CLEANUP_NS;
            let count = group_members(child, pids);
            if count < 0 {
                report[0] = report_code::CLEANUP_FAILED;
            } else if report[0] == report_code::EXITED_ZERO {
                report[2] = count;
            }
            libc::kill(-child, libc::SIGTERM);
            let grace = monotonic_ns() + TERMINATE_GRACE_NS;
            while monotonic_ns() < grace {
                discard(fds[1][0]);
                discard(fds[2][0]);
                libc::usleep(2000);
            }
            libc::kill(-child, libc::SIGKILL);
            // The child may not yet have called setsid when the host dies. An exact
            // unreaped direct child is safe to kill separately as well.
            libc::kill(child, libc::SIGKILL);
            loop {
                discard(fds[1][0]);
                discard(fds[2][0]);
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let exited = libc::waitid(
                    libc::P_PID,
                    child as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                ) == 0
                    && info.si_pid == child;
                if exited && group_members(child, pids) == 0 {
                    break;
                }
                if monotonic_ns() >= cleanup_end {
                    report[0] = report_code::CLEANUP_FAILED;
                    break;
                }
                libc::usleep(2000);
            }
            libc::waitpid(child, std::ptr::null_mut(), libc::WNOHANG);
        }
        // SIGPIPE must not bypass cleanup; publication is deliberately last.
        libc::write(
            fds[4][1],
            report.as_ptr().cast(),
            std::mem::size_of_val(&report),
        );
        libc::_exit(0);
    }
}

unsafe fn discard(fd: RawFd) {
    let mut bytes = [0u8; 8192];
    for _ in 0..16 {
        if unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) } <= 0 {
            break;
        }
    }
}

/// Count live background group members, excluding the retained shell zombie.
unsafe fn group_members(group: i32, pids: &mut [i32]) -> i32 {
    unsafe {
        let size = std::mem::size_of_val(pids) as i32;
        // Unlike proc_listpids, this convenience API returns a PID count, not bytes.
        let n = libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), size);
        if n < 0 || n as usize >= pids.len() {
            return -1;
        }
        let mut count = 0;
        for pid in &pids[..n as usize] {
            if *pid == group || *pid <= 0 {
                continue;
            }
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let read = libc::proc_pidinfo(
                *pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                std::mem::size_of_val(&info) as i32,
            );
            if read == std::mem::size_of_val(&info) as i32 {
                if info.pbi_pgid == group as u32 && info.pbi_status != 5 {
                    count += 1;
                }
            } else if libc::kill(*pid, 0) == 0 || *libc::__error() != libc::ESRCH {
                // An unreadable live member is not proof of successful cleanup.
                return -1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn fixture(script: &[u8]) -> (tempfile::TempDir, Vec<(OsString, OsString)>) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".zshrc"), script).unwrap();
        let base = vec![
            ("HOME".into(), dir.path().into()),
            ("ZDOTDIR".into(), dir.path().into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ];
        (dir, base)
    }
    // A capture that ends by itself (an exit, a signal, too much output, or success), with
    // the whole budget a launch has. Its time is not what these cases test: with the 1.5 s
    // budget of a 4 s deadline, the 4 MiB output flood ran out of time under background QoS
    // (`taskpolicy -b`, the throttling of a loaded machine), where it took 2.9-4.7 s; idle it
    // takes about 0.3 s and every other case about 0.15 s.
    fn run(script: &[u8]) -> Result<Captured, CaptureFailure> {
        use crate::native::terminal::shell_environment_budget::{HANDOFF_RESERVE, LIMIT};
        let (_dir, base) = fixture(script);
        capture(base, Instant::now() + LIMIT + HANDOFF_RESERVE + CLEANUP)
    }
    #[test]
    fn key_read_has_no_terminal_and_export_bytes_survive_exec() {
        let value = b"newline\n'\"$\xff";
        let mut script =
            b"read -r -k 1 ignored\nexport ROUNDTRIP=$'newline\\n\\\'\"$\\xff'\n".to_vec();
        script.extend_from_slice(b"export CODEX_THREAD_ID=must_be_removed_at_handoff\n");
        let (_dir, mut base) = fixture(&script);
        base.push((
            "AGENT_BRIDGE_NATIVE_SESSION_ID".into(),
            "not_in_capture".into(),
        ));
        let captured = capture(base, Instant::now() + Duration::from_secs(4)).unwrap();
        assert!(captured.elapsed < Duration::from_secs(2));
        // The budget the capture decided when it started: 4 s less the 2.5 s of reserves.
        assert!(
            captured.budget <= Duration::from_millis(1500)
                && captured.budget > Duration::from_millis(1000),
            "{captured:?}"
        );
        assert!(
            !captured
                .variables
                .iter()
                .any(|(k, _)| k == "AGENT_BRIDGE_NATIVE_SESSION_ID")
        );
        assert_eq!(
            captured
                .variables
                .iter()
                .find(|(k, _)| k == "ROUNDTRIP")
                .unwrap()
                .1
                .as_bytes(),
            value
        );
        let output = std::process::Command::new("/usr/bin/env")
            .arg("-0")
            .env_clear()
            .envs(captured.variables)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            decode(&output.stdout)
                .unwrap()
                .iter()
                .any(|(k, v)| k == "ROUNDTRIP" && v.as_bytes() == value)
        );
    }
    #[test]
    fn non_identifier_names_allow_capture_and_survive_direct_exec() {
        let (_dir, mut base) = fixture(b"");
        let names = ["A-B", "a.b", "BASH_FUNC_x%%", "1X"];
        for name in names {
            base.push((name.into(), "newline\n'\"$".into()));
        }
        capture(base, Instant::now() + Duration::from_secs(4)).unwrap();
        // zsh drops non-identifier names when exporting to children, as the old
        // launch shell did too. Verify decoded handoff bytes with a direct exec.
        let variables = decode(b"A-B=newline\n'\"$\0a.b=newline\n'\"$\0BASH_FUNC_x%%=newline\n'\"$\x001X=newline\n'\"$\0").unwrap();
        let output = std::process::Command::new("/usr/bin/env")
            .arg("-0")
            .env_clear()
            .envs(variables)
            .output()
            .unwrap();
        assert!(output.status.success());
        let variables = decode(&output.stdout).unwrap();
        for name in names {
            assert!(
                variables
                    .iter()
                    .any(|(k, v)| k == name && v == "newline\n'\"$"),
                "missing {name}"
            );
        }
    }
    #[test]
    fn timeout_kills_sleep_and_is_bounded() {
        let (dir, base) = fixture(b"sleep 100 &\necho $! > $HOME/child\nwait\n");
        let start = Instant::now();
        let result = capture(base, start + Duration::from_secs(3)).unwrap_err();
        assert_eq!(result.reason, FailureReason::CaptureTimeout);
        // The deadline less the 2.5 s of reserves leaves at most 500 ms, which the capture
        // runs out before it reports the timeout.
        assert!(result.elapsed >= Duration::from_millis(400), "{result:?}");
        assert!(
            result.budget <= Duration::from_millis(500) && result.elapsed >= result.budget,
            "{result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_gone(&dir.path().join("child"));
    }
    fn assert_gone(path: &std::path::Path) {
        let pid: i32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "surviving fixture pid {pid}"
        );
    }
    #[test]
    fn background_fd_holder_is_stopped_without_waiting_for_eof() {
        let (dir, base) = fixture(b"sleep 100 &!\necho $! > $HOME/child\n");
        let result = capture(base, Instant::now() + Duration::from_secs(4)).unwrap();
        assert!(result.background_children_stopped >= 1, "{result:?}");
        assert!(result.elapsed < Duration::from_secs(2));
        assert_gone(&dir.path().join("child"));
    }
    #[test]
    fn exit_signal_empty_oversize_and_output_flood_are_classified() {
        let error = run(b"exit 3\n").unwrap_err();
        assert_eq!(error.reason, FailureReason::CaptureExitNonzero);
        assert_eq!(error.exit_code, Some(3));
        let error = run(b"kill -KILL $$\n").unwrap_err();
        assert_eq!(error.reason, FailureReason::CaptureSignaled);
        assert_eq!(error.signal, Some(9));
        assert_eq!(
            run(b"exit 0\n").unwrap_err().reason,
            FailureReason::CaptureNoOutput
        );
        assert_eq!(
            run(b"/usr/bin/head -c 1048577 /dev/zero >&3\n")
                .unwrap_err()
                .reason,
            FailureReason::CaptureOversized
        );
        run(b"/usr/bin/head -c 2097152 /dev/zero\n/usr/bin/head -c 2097152 /dev/zero >&2\n")
            .unwrap();
    }
    #[test]
    fn exhausted_budget_starts_no_shell() {
        let (dir, base) = fixture(b"touch $HOME/started\n");
        // A deadline that leaves only the reserves: no budget remains before the spawn.
        let failure = capture(base, Instant::now() + Duration::from_millis(2500)).unwrap_err();
        assert_eq!(failure.reason, FailureReason::LaunchBudgetExhausted);
        assert_eq!(failure.budget, Duration::ZERO);
        assert_eq!((failure.exit_code, failure.signal), (None, None));
        assert!(!dir.path().join("started").exists());
    }
    // The supervisor's cleanup timings are shares of `CLEANUP`.
    #[test]
    fn supervisor_cleanup_fits_inside_the_cleanup_reserve() {
        assert_eq!(CLEANUP, Duration::from_millis(500));
        assert_eq!(
            Duration::from_nanos(GROUP_CLEANUP_NS),
            Duration::from_millis(450)
        );
        assert_eq!(
            Duration::from_nanos(TERMINATE_GRACE_NS),
            Duration::from_millis(100)
        );
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;
    #[test]
    fn host_death_still_cleans_capture_group() {
        const CHILD: &str = "BRIDGE_CAPTURE_LIFETIME_FIXTURE";
        if let Some(path) = std::env::var_os(CHILD) {
            let base = vec![
                ("HOME".into(), path.clone()),
                ("ZDOTDIR".into(), path),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ];
            let _ = capture(base, Instant::now() + Duration::from_secs(12));
            panic!("fixture host should have been killed");
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".zshrc"),
            b"sleep 100 &\necho $! > $HOME/child\nwait\n",
        )
        .unwrap();
        let mut host = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native::terminal::macos::shell_environment::lifetime_tests::host_death_still_cleans_capture_group", "--test-threads=1"])
            .env_clear().env(CHILD, dir.path()).env("HOME", dir.path()).env("ZDOTDIR", dir.path())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        let child_file = dir.path().join("child");
        while !child_file.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        host.kill().unwrap();
        host.wait().unwrap();
        let pid: i32 = std::fs::read_to_string(child_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "capture survived host death"
        );
    }
}
