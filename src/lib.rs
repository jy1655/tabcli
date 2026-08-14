use anyhow::{Result, bail};
use semver::Version;
use std::time::{Duration, Instant};

pub mod providers;

pub use providers::{FirstPartyCli, ProviderAdapter, provider_adapter, supported_clis};

#[cfg(unix)]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut exit_code = 0;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    let _ = unsafe { CloseHandle(handle) };
    queried != 0 && exit_code == STILL_ACTIVE as u32
}

#[cfg(not(any(unix, windows)))]
pub fn process_is_alive(pid: u32) -> bool {
    pid == std::process::id()
}

pub fn checked_deadline_from(start: Instant, timeout: Duration) -> Result<Instant> {
    start.checked_add(timeout).ok_or_else(|| {
        anyhow::anyhow!(
            "timeout of {} seconds cannot be represented by this platform's monotonic clock",
            timeout.as_secs()
        )
    })
}

pub fn validate_terminal_input(value: &str, field: &str) -> Result<()> {
    if let Some(character) = value
        .chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        bail!(
            "{field} contains terminal control U+{:04X}; only newline and tab are allowed",
            u32::from(character)
        );
    }
    Ok(())
}

pub fn terminal_safe_text(value: &str, multiline: bool) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        if multiline && matches!(character, '\n' | '\t') {
            safe.push(character);
        } else if character.is_control() {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

pub fn cli_version_is_supported(cli: FirstPartyCli, output: &str) -> Result<bool> {
    let installed = output
        .split_whitespace()
        .filter_map(|token| {
            let candidate = token
                .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .strip_prefix('v')
                .unwrap_or(
                    token.trim_matches(|character: char| !character.is_ascii_alphanumeric()),
                );
            Version::parse(candidate).ok()
        })
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not parse a semantic version from {output:?}"))?;

    Ok(installed >= cli.minimum_version())
}

pub fn provider_launch_args(cli: FirstPartyCli, yolo: bool) -> Vec<&'static str> {
    if !yolo {
        return Vec::new();
    }

    provider_adapter(cli).yolo_args().to_vec()
}

pub fn provider_model_args(cli: FirstPartyCli, model: &str) -> Vec<String> {
    provider_adapter(cli).model_args(model)
}

pub fn provider_effort_args(cli: FirstPartyCli, effort: &str) -> Result<Vec<String>> {
    provider_adapter(cli).effort_args(effort)
}

pub fn confirm_explicit_close(explicit: bool) -> Result<()> {
    if !explicit {
        bail!("closing a visible terminal session requires --explicit");
    }
    Ok(())
}
