use super::Home;
use crate::native::consent::{self, Environment, Evidence, Trust};
use anyhow::{Context, Result, bail};
use std::path::Path;

pub(super) enum Lookup {
    StoreOnly,
    Workspace,
}

impl Lookup {
    pub(super) fn read(
        self,
        workspace: &Path,
        env: &Environment,
        store: Option<&Path>,
    ) -> Result<Trust> {
        let store = match store {
            Some(store) => store.to_path_buf(),
            None => Home::resolve(env)?.config,
        };
        let Some(text) = consent::read_store(&store)? else {
            return Ok(Trust::Absent);
        };
        let config: toml::Value = text.parse().context("invalid Codex trust config")?;
        let key = consent::native_key(workspace)?;
        let Some(projects) = config.get("projects") else {
            return Ok(Trust::Absent);
        };
        let projects = projects
            .as_table()
            .context("Codex projects is not a table")?;
        #[cfg(not(windows))]
        let entry = projects.get(&key);
        #[cfg(windows)]
        let entry = windows_entry(projects, &key, || match self {
            Self::StoreOnly => true,
            Self::Workspace => insensitive_ancestors(workspace, directory_case_sensitive),
        })?;
        let entry = entry
            .map(|v| {
                v.as_table()
                    .context("Codex project trust entry is not a table")
            })
            .transpose()?;
        match entry.and_then(|v| v.get("trust_level")) {
            None => Ok(Trust::Absent),
            Some(v) if v.as_str() == Some("trusted") => Ok(Trust::Trusted(Evidence {
                provider: "codex".into(),
                store,
                // Pins retain the evidence spelling written by 0.4.50.
                #[cfg(windows)]
                key: key.to_lowercase(),
                #[cfg(not(windows))]
                key,
            })),
            Some(v) if v.as_str() == Some("untrusted") => Ok(Trust::Declined),
            _ => bail!("unknown Codex trust level"),
        }
    }
}

#[cfg(any(test, windows))]
fn windows_entry<'a>(
    projects: &'a toml::Table,
    key: &str,
    allow_other_spelling: impl FnOnce() -> bool,
) -> Result<Option<&'a toml::Value>> {
    let mut matches = projects
        .iter()
        .filter(|(stored, _)| stored.eq_ignore_ascii_case(key));
    let Some((stored, entry)) = matches.next() else {
        let folded = key.to_lowercase();
        let decline = projects.iter().find_map(|(stored, entry)| {
            let untrusted =
                entry.get("trust_level").and_then(toml::Value::as_str) == Some("untrusted");
            (untrusted && stored.to_lowercase() == folded).then_some(entry)
        });
        return Ok(decline);
    };
    if matches.next().is_some() {
        bail!("Codex's store holds several entries for {key} that differ only in letter case");
    }
    if stored != key
        && entry.get("trust_level").and_then(toml::Value::as_str) == Some("trusted")
        && !allow_other_spelling()
    {
        return Ok(None);
    }
    Ok(Some(entry))
}

#[cfg(any(test, windows))]
fn insensitive_ancestors(
    workspace: &Path,
    mut case_sensitive: impl FnMut(&Path) -> std::io::Result<bool>,
) -> bool {
    if !workspace.is_absolute() {
        return false;
    }
    let parent = workspace.parent().unwrap_or(workspace);
    let ancestors: Vec<_> = parent.ancestors().collect();
    ancestors
        .into_iter()
        .rev()
        .all(|directory| matches!(case_sensitive(directory), Ok(false)))
}

#[cfg(windows)]
fn directory_case_sensitive(directory: &Path) -> std::io::Result<bool> {
    use std::fs::OpenOptions;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FileCaseSensitiveInfo, GetFileInformationByHandleEx,
    };

    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?;
    let mut info = FILE_CASE_SENSITIVE_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileCaseSensitiveInfo,
            (&raw mut info).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // FILE_CS_FLAG_CASE_SENSITIVE_DIR in the Windows SDK (ntifs.h).
    Ok(info.Flags & 1 != 0)
}

#[cfg(any(test, windows))]
pub(super) fn windows_override_key(key: &str) -> String {
    key.to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
