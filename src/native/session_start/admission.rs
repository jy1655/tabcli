//! Launch admission: whether a new session may start in its workspace at all. It runs
//! before any session, record, consent or terminal surface exists, and it changes nothing.
//!
//! The only rule is the user's `codex-memory-workspace` setting. Codex's background memory
//! consolidation delegated through Bridge from the Codex memory workspace and left the
//! sessions open (issue #216), so a workspace that is that directory or lies inside it is
//! refused by default, for every target provider. The location belongs to the Codex
//! adapter; this module applies the setting to it. It is an admission policy for an
//! observed path, not a sandbox: it does not stop a launch from another directory.
use super::super::*;
use crate::native::consent::Environment;
use crate::native::settings::CodexMemoryWorkspace;
use agent_bridge::PUBLIC_COMMAND;

/// Applies the setting of the state root to a canonical workspace. An `allow` setting
/// admits without reading the caller's Codex home.
pub(in crate::native) fn check(root: &Path, canonical_workspace: &Path) -> Result<()> {
    check_from(root, canonical_workspace, Environment::current)
}

/// The self-test's isolated mode reads no settings: it uses the default, refuse.
pub(in crate::native) fn check_isolated(canonical_workspace: &Path) -> Result<()> {
    check_isolated_from(canonical_workspace, Environment::current)
}

fn check_from(
    root: &Path,
    canonical_workspace: &Path,
    env: impl FnOnce() -> Environment,
) -> Result<()> {
    let policy = settings::codex_memory_workspace(root)?;
    if policy == CodexMemoryWorkspace::Allow {
        return Ok(());
    }
    check_with(policy, canonical_workspace, &memory_workspace(env)?)
}

fn check_isolated_from(
    canonical_workspace: &Path,
    env: impl FnOnce() -> Environment,
) -> Result<()> {
    match refusal(canonical_workspace, &memory_workspace(env)?)? {
        Some(refusal) => bail!(
            "{refusal} Isolated self-test uses default settings; use ordinary self-test with the allow setting to permit this workspace."
        ),
        None => Ok(()),
    }
}

fn check_with(
    policy: CodexMemoryWorkspace,
    canonical_workspace: &Path,
    memory_workspace: &Path,
) -> Result<()> {
    if policy == CodexMemoryWorkspace::Allow {
        return Ok(());
    }
    match refusal(canonical_workspace, memory_workspace)? {
        Some(refusal) => bail!(refusal),
        None => Ok(()),
    }
}

// The memory workspace of the caller's Codex home, which the Codex adapter resolves
// exactly as workspace trust resolves it; the snapshot is taken only here, after the
// setting was read.
fn memory_workspace(env: impl FnOnce() -> Environment) -> Result<PathBuf> {
    provider::codex_memory_workspace(&env()).context("cannot locate the Codex memory workspace")
}

// The refusal detail, or None when the workspace lies outside the memory workspace. Only a
// memory workspace that does not exist protects nothing; any other failure to resolve it
// is an error, never an admission.
fn refusal(canonical_workspace: &Path, memory_workspace: &Path) -> Result<Option<String>> {
    let memory = match memory_workspace.canonicalize() {
        Ok(memory) => memory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot resolve the Codex memory workspace {}",
                    memory_workspace.display()
                )
            });
        }
    };
    let metadata = fs::metadata(&memory).with_context(|| {
        format!(
            "cannot read the Codex memory workspace {}",
            memory.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "the Codex memory workspace {} is not a directory",
            memory.display()
        );
    }
    // Path::starts_with compares whole components, so `memories-other` is outside.
    Ok(canonical_workspace.starts_with(&memory).then(|| {
        format!(
            "launch refused: workspace {} is within the Codex memory workspace {}; codex-memory-workspace is refuse. To allow launches here, run '{PUBLIC_COMMAND} settings codex-memory-workspace allow' in the same state root.",
            canonical_workspace.display(),
            memory.display()
        )
    }))
}

#[cfg(test)]
mod tests;
