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

pub fn provider_effort_args(cli: FirstPartyCli, effort: &str) -> Result<Vec<String>> {
    provider_adapter(cli).effort_args(effort)
}

pub fn confirm_explicit_close(explicit: bool) -> Result<()> {
    if !explicit {
        bail!("closing a visible terminal session requires --explicit");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentId {
    Codex,
    Claude,
    Agy,
}

impl AgentId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Agy => "Agy",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AgentDefinition {
    pub id: AgentId,
    pub command: &'static str,
    pub role: &'static str,
}

pub const fn agents() -> [AgentDefinition; 3] {
    [
        AgentDefinition {
            id: AgentId::Codex,
            command: "codex",
            role: "builder",
        },
        AgentDefinition {
            id: AgentId::Claude,
            command: "claude",
            role: "reviewer",
        },
        AgentDefinition {
            id: AgentId::Agy,
            command: "agy",
            role: "specialist",
        },
    ]
}

pub fn handoff_text_from(from: &str, request: &str, context: &str) -> Result<String> {
    let request = request.trim();
    if request.is_empty() {
        bail!("handoff request cannot be empty");
    }
    let context = context.trim();
    Ok(format!(
        "[Agent Bridge handoff]\nSource: {}\nRequest: {}\n\nRecent source terminal:\n---\n{}\n---",
        from.trim(),
        request,
        if context.is_empty() {
            "(no visible source context)"
        } else {
            context
        }
    ))
}

pub fn session_title(agent: AgentId, ordinal: u32) -> String {
    format!("{} {ordinal}", agent.name())
}

#[derive(Debug, Default)]
pub struct TabSet<T> {
    items: Vec<T>,
    active: Option<usize>,
}

impl<T> TabSet<T> {
    pub const fn new() -> Self {
        Self {
            items: Vec::new(),
            active: None,
        }
    }

    pub fn push(&mut self, item: T) {
        self.items.push(item);
        self.active = Some(self.items.len() - 1);
    }

    pub fn remove_active(&mut self) -> Option<T> {
        let index = self.active?;
        self.remove(index)
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        self.active = if self.items.is_empty() {
            None
        } else {
            self.active.map(|active| {
                if index < active {
                    active - 1
                } else {
                    active.min(self.items.len() - 1)
                }
            })
        };
        Some(removed)
    }

    pub fn replace_active(&mut self, item: T) -> Option<T> {
        let index = self.active?;
        Some(std::mem::replace(&mut self.items[index], item))
    }

    pub fn move_active(&mut self, delta: isize) {
        if let Some(active) = self.active {
            self.active =
                Some((active as isize + delta).rem_euclid(self.items.len() as isize) as usize);
        }
    }

    pub const fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn set_active(&mut self, index: usize) -> bool {
        if index >= self.items.len() {
            return false;
        }
        self.active = Some(index);
        true
    }

    pub fn active(&self) -> Option<&T> {
        self.active.and_then(|index| self.items.get(index))
    }

    pub fn active_mut(&mut self) -> Option<&mut T> {
        self.active.and_then(|index| self.items.get_mut(index))
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
