//! The macOS and Linux transport of the Herdr adapter: the core's protected Unix socket,
//! located as `hc` locates it (herdr-core 0.1.21 `paths.rs`), and this machine's processes.
use super::{Core, Framed, Stream, core_session, not_running};
use crate::native::private_to_user::owned_and_not_writable_by_others;
use crate::native::terminal::record::HerdrHostBirth;
use anyhow::{Context, Result, bail};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        io::AsRawFd,
        net::UnixStream,
    },
    path::{Path, PathBuf},
    time::Instant,
};

// sockaddr_un.sun_path reserves one byte for the trailing NUL.
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH: usize = 103;
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH: usize = 107;

pub(in crate::native::terminal) struct Installed;

impl Core for Installed {
    fn endpoint(&self) -> Result<PathBuf> {
        resolve_socket(&EndpointInputs::from_env(), &super::read_config_socket)
    }

    fn connect(&self, socket: &Path, deadline: Instant) -> Result<Box<dyn super::Connection + '_>> {
        Ok(Box::new(open(socket, deadline)?))
    }

    fn executable(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed to resolve TAB executable")
    }

    fn caller_terminal(&self) -> Option<String> {
        super::caller_terminal_from_env()
    }

    fn current_pid(&self) -> u32 {
        std::process::id()
    }

    fn parent_pid(&self, pid: u32) -> Result<Option<u32>> {
        #[cfg(target_os = "macos")]
        return Ok(
            crate::native::terminal::macos::process::macos_process_info(pid)?
                .map(|info| info.parent_pid),
        );
        #[cfg(target_os = "linux")]
        return crate::native::terminal::linux::process::parent_pid(pid);
    }

    fn process_start(&self, pid: u32) -> Result<Option<HerdrHostBirth>> {
        #[cfg(target_os = "macos")]
        return Ok(
            crate::native::terminal::macos::process::macos_process_start(pid)?.map(
                |(host_start_seconds, host_start_microseconds)| HerdrHostBirth::Macos {
                    host_start_seconds,
                    host_start_microseconds,
                },
            ),
        );
        #[cfg(target_os = "linux")]
        return Ok(crate::native::terminal::linux::process::process_birth(pid)?
            .map(|linux_birth| HerdrHostBirth::Linux { linux_birth }));
    }

    fn launch_nonce(&self) -> Result<String> {
        let mut bytes = [0_u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

/// The environment that decides where `hc` looks for its core.
#[derive(Clone, Debug, Default)]
pub(super) struct EndpointInputs {
    pub(super) hc_socket: Option<OsString>,
    pub(super) hc_session: Option<String>,
    pub(super) home: Option<OsString>,
    pub(super) xdg_state_home: Option<OsString>,
    pub(super) xdg_config_home: Option<OsString>,
    pub(super) xdg_runtime_dir: Option<OsString>,
    /// `/run/user/<uid>` when that directory exists.
    pub(super) run_user: Option<PathBuf>,
}

impl EndpointInputs {
    fn from_env() -> Self {
        let run_user = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        Self {
            hc_socket: std::env::var_os("HC_SOCKET"),
            hc_session: std::env::var("HC_SESSION").ok(),
            home: std::env::var_os("HOME"),
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            run_user: run_user.is_dir().then_some(run_user),
        }
    }
}

fn nonempty(value: &Option<OsString>) -> Option<&OsStr> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// The socket of the core that `hc` addresses from these inputs. A session name or socket
/// that `hc` itself would refuse, and a configuration it cannot read, are refused here
/// too: TAB never guesses another endpoint.
pub(super) fn resolve_socket(
    inputs: &EndpointInputs,
    config_socket: &dyn Fn(&Path) -> Result<Option<String>>,
) -> Result<PathBuf> {
    let session = core_session(inputs.hc_session.as_deref())?;
    let home = || {
        nonempty(&inputs.home)
            .map(PathBuf::from)
            .context("HOME is not set, so the Herdr Core socket cannot be located")
    };
    let state = match nonempty(&inputs.xdg_state_home) {
        Some(state) => PathBuf::from(state),
        None => home()?.join(".local/state"),
    }
    .join("herdr-core")
    .join(session);
    let chosen = match nonempty(&inputs.hc_socket) {
        Some(socket) => Some(PathBuf::from(socket)),
        None => {
            let config = match nonempty(&inputs.xdg_config_home) {
                Some(config) => PathBuf::from(config),
                None => home()?.join(".config"),
            }
            .join("herdr-core")
            .join("config.toml");
            config_socket(&config)?.map(PathBuf::from)
        }
    };
    let socket = match chosen {
        Some(socket) => socket,
        None => {
            let run = match nonempty(&inputs.xdg_runtime_dir) {
                Some(run) => Some(PathBuf::from(run).join("herdr-core")),
                None => inputs.run_user.as_ref().map(|run| run.join("herdr-core")),
            };
            run.unwrap_or(state).join(format!("{session}.sock"))
        }
    };
    if !socket.is_absolute() {
        bail!(
            "the Herdr Core socket {} is not an absolute path",
            socket.display()
        );
    }
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        bail!(
            "the Herdr Core socket {} is longer than this system allows",
            socket.display()
        );
    }
    Ok(socket)
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<libc::uid_t> {
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<libc::uid_t> {
    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut size = size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut size,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if size as usize != size_of::<libc::ucred>() {
        return Err(std::io::Error::other(
            "incomplete Unix socket peer credentials",
        ));
    }
    Ok(unsafe { cred.assume_init() }.uid)
}

/// The connection of the installed adapter: the protected socket of a core that runs as
/// this user, with every read and write bounded by the deadline.
pub(super) fn open(socket: &Path, deadline: Instant) -> Result<Framed<UnixStream>> {
    let uid = unsafe { libc::geteuid() };
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_running(socket));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect Herdr socket {}", socket.display()));
        }
    };
    let directory = socket
        .parent()
        .context("Herdr socket has no parent directory")?;
    let directory = fs::symlink_metadata(directory)
        .with_context(|| format!("cannot inspect {}", directory.display()))?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || !directory.is_dir()
        || !owned_and_not_writable_by_others(&directory, uid)
    {
        bail!(
            "{} is not a protected Herdr Core socket of this user",
            socket.display()
        );
    }
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(not_running(socket));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot connect to {}", socket.display()));
        }
    };
    if peer_uid(&stream).context("cannot identify the Herdr Core socket's user")? != uid {
        bail!("the process at {} is not this user's", socket.display());
    }
    if Instant::now() >= deadline {
        bail!("Herdr connection timed out");
    }
    Ok(Framed::new(stream))
}

/// The read of `Stream::read_before`. It waits with poll(2), not a receive timeout: macOS
/// refuses SO_RCVTIMEO with EINVAL once the core has closed its end, and a reply the core
/// sent before it closed is still in the socket to be read. Readiness does not promise
/// ordinary bytes (Linux reports a queue that holds only an urgent byte as readable), so the
/// receive itself never blocks: with nothing to read it waits again, against the deadline.
fn receive_before(
    stream: &UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    loop {
        wait_readable(stream, deadline)?;
        let received = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if let Ok(count) = usize::try_from(received) {
            return Ok(count);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            // Readable with nothing to receive: wait again at once, so that a reply the core
            // sends before the deadline is read. This cannot spin. On Linux the ordinary
            // receive drops a lone urgent byte from the queue (`manage_oob` in af_unix.c),
            // so the next poll waits; where Unix sockets keep no out-of-band data (macOS,
            // Linux before 5.15 or without CONFIG_AF_UNIX_OOB), readable means bytes or the
            // end of the stream, which the receive returns.
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => {}
            _ => return Err(error),
        }
    }
}

/// Returns once `stream` is readable (bytes, the end of the stream or an error is waiting, or,
/// on Linux, an urgent byte), or `TimedOut` at the deadline.
fn wait_readable(stream: &UnixStream, deadline: Instant) -> std::io::Result<()> {
    loop {
        // Rounded up to whole milliseconds, so that no wait ends before the deadline.
        let timeout = super::remaining(deadline)?.as_micros().div_ceil(1000);
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut poll, 1, i32::try_from(timeout).unwrap_or(i32::MAX)) } {
            0 => {}
            ready if ready > 0 => return Ok(()),
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

impl Stream for UnixStream {
    fn write_before(&mut self, bytes: &[u8], deadline: Instant) -> std::io::Result<usize> {
        self.set_write_timeout(Some(super::remaining(deadline)?))?;
        Write::write(self, bytes)
    }

    fn read_before(&mut self, buffer: &mut [u8], deadline: Instant) -> std::io::Result<usize> {
        receive_before(self, buffer, deadline)
    }
}
