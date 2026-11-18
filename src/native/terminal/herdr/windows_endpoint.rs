//! Where `hc` on Windows looks for its core (herdr-core 0.1.21 `paths.rs` with
//! `platform/windows.rs`): `HC_SOCKET`, else `socket` in `config.toml` under
//! `XDG_CONFIG_HOME`, `APPDATA` or the profile's `AppData\Roaming`, else the named pipe
//! `\\.\pipe\herdr-core-<user>-<session>.sock`, whose user part keeps only the ASCII
//! letters and digits of `USERNAME`. Pure, so that every test platform runs it.
use super::core_session;
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The local named-pipe namespace. hc creates its pipes there with
/// `PIPE_REJECT_REMOTE_CLIENTS`; Bridge connects to nothing else, so another machine's
/// pipe (`\\host\pipe\...`) and a file path are never a core.
pub(super) const LOCAL_PIPE_PREFIX: &str = r"\\.\pipe\";
/// hc's limit on a pipe name (`MAX_SOCKET_PATH` on Windows).
const MAX_PIPE_NAME: usize = 256;

/// The environment that decides where `hc` looks for its core on Windows.
#[derive(Clone, Debug, Default)]
pub(super) struct WindowsEndpointInputs {
    pub(super) hc_socket: Option<OsString>,
    pub(super) hc_session: Option<String>,
    pub(super) xdg_config_home: Option<OsString>,
    pub(super) appdata: Option<OsString>,
    pub(super) userprofile: Option<OsString>,
    pub(super) username: Option<String>,
}

impl WindowsEndpointInputs {
    #[cfg(windows)]
    pub(super) fn from_env() -> Self {
        Self {
            hc_socket: std::env::var_os("HC_SOCKET"),
            hc_session: std::env::var("HC_SESSION").ok(),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            appdata: std::env::var_os("APPDATA"),
            userprofile: std::env::var_os("USERPROFILE"),
            username: std::env::var("USERNAME").ok(),
        }
    }
}

fn nonempty(value: &Option<OsString>) -> Option<&OsString> {
    value.as_ref().filter(|value| !value.is_empty())
}

/// The pipe of the core that `hc` addresses from these inputs. A session name `hc` would
/// refuse, a configuration it cannot read and a name that is not a local pipe are refused:
/// Bridge never guesses another endpoint. hc falls back to `C:\` for a missing profile;
/// Bridge refuses instead of reading a configuration from there.
pub(super) fn resolve_pipe(
    inputs: &WindowsEndpointInputs,
    config_socket: &dyn Fn(&Path) -> Result<Option<String>>,
) -> Result<PathBuf> {
    let session = core_session(inputs.hc_session.as_deref())?;
    let chosen = match nonempty(&inputs.hc_socket) {
        Some(socket) => Some(PathBuf::from(socket)),
        None => {
            let config_home = match (
                nonempty(&inputs.xdg_config_home),
                nonempty(&inputs.appdata),
                nonempty(&inputs.userprofile),
            ) {
                (Some(config), _, _) => PathBuf::from(config),
                (None, Some(appdata), _) => PathBuf::from(appdata),
                (None, None, Some(profile)) => {
                    PathBuf::from(profile).join("AppData").join("Roaming")
                }
                (None, None, None) => bail!(
                    "neither APPDATA nor USERPROFILE is set, so the Herdr Core configuration cannot be located"
                ),
            };
            config_socket(&config_home.join("herdr-core").join("config.toml"))?.map(PathBuf::from)
        }
    };
    let pipe = match chosen {
        Some(pipe) => pipe,
        None => {
            let user: String = inputs
                .username
                .as_deref()
                .unwrap_or_default()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect();
            PathBuf::from(format!(
                "{LOCAL_PIPE_PREFIX}herdr-core-{user}-{session}.sock"
            ))
        }
    };
    local_pipe_name(&pipe)?;
    Ok(pipe)
}

/// The name after `\\.\pipe\` of a local pipe; anything else is refused.
pub(super) fn local_pipe_name(pipe: &Path) -> Result<&str> {
    let text = pipe
        .to_str()
        .with_context(|| format!("the Herdr Core pipe {} is not text", pipe.display()))?;
    let name = text
        .get(..LOCAL_PIPE_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(LOCAL_PIPE_PREFIX))
        .map(|_| &text[LOCAL_PIPE_PREFIX.len()..])
        .filter(|name| !name.is_empty() && !name.contains(['\\', '/', '\0']))
        .with_context(|| {
            format!(
                "the Herdr Core endpoint {text} is not a local named pipe ({LOCAL_PIPE_PREFIX}<name>); Bridge connects to no other endpoint"
            )
        })?;
    if text.len() > MAX_PIPE_NAME {
        bail!("the Herdr Core pipe {text} is longer than Windows allows");
    }
    Ok(name)
}
