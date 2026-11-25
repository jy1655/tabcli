//! The macOS and Linux transport of the Herdr adapter: the core's protected Unix socket,
//! located as `hc` locates it (herdr-core 0.1.21 `paths.rs`), and this machine's processes.
use super::{Core, Framed, Stream, core_session, not_running};
use crate::native::private_to_user::owned_and_not_writable_by_others;
use crate::native::terminal::TerminalKind;
use crate::native::terminal::bound_host::HostAccount;
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

    fn admit_launch_directory(
        &self,
        directory: &Path,
        account: Option<&HostAccount>,
    ) -> Result<()> {
        crate::native::terminal::bound_host::refuse_before_core_launch(
            directory,
            TerminalKind::Herdr,
            account,
        )
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

impl Stream for UnixStream {
    fn write_before(&mut self, bytes: &[u8], deadline: Instant) -> std::io::Result<usize> {
        self.set_write_timeout(Some(super::remaining(deadline)?))?;
        Write::write(self, bytes)
    }

    fn read_before(&mut self, buffer: &mut [u8], deadline: Instant) -> std::io::Result<usize> {
        self.set_read_timeout(Some(super::remaining(deadline)?))?;
        Read::read(self, buffer)
    }

    /// The peer's effective uid, which `open` already required to be this process's.
    fn peer_account(&self) -> Result<HostAccount> {
        Ok(HostAccount::of_uid(
            peer_uid(self).context("cannot identify the Herdr Core socket's user")?,
        ))
    }
}
