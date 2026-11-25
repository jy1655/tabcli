//! The linked git worktree rule of workspace consent: a workspace that Git's own files prove,
//! in both directions, to be a linked worktree of a repository whose main working tree is
//! trusted. Git's files are read directly and no `git` runs. Anything that cannot be verified
//! here is no worktree, so the workspace keeps the exact rule alone. See AGENTS.md, Trust
//! Store Boundary.
use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

// Git's pointer and registration files hold one path; a repository's config is larger.
const POINTER_LIMIT: u64 = 64 * 1024;
const CONFIG_LIMIT: u64 = 1024 * 1024;

/// The main working tree of the repository that registered `workspace` as a linked worktree.
/// `Ok(None)` is a workspace without a `.git` file: no worktree root, or a main working tree.
/// An `Err` is a `.git` file whose registration does not verify. `workspace` is canonical.
///
/// Verified, in order: `.git` is a regular file (no link) naming, by an absolute path without a
/// link in it, a registration directory `<common>/worktrees/<id>`; that directory's `commondir`
/// is Git's own `../..` and its `gitdir` names this workspace's `.git` file back, by an
/// absolute path without a link in it; the common directory is the `.git` directory of its
/// parent, the main working tree (a submodule's or a bare repository's is named otherwise);
/// and its configuration neither declares it bare nor moves the main working tree elsewhere.
pub(super) fn main_worktree(workspace: &Path) -> Result<Option<PathBuf>> {
    let pointer = workspace.join(".git");
    match fs::symlink_metadata(&pointer) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() => return Ok(None),
        Ok(_) => {}
    }
    let target = read_git_file(&pointer, POINTER_LIMIT)?;
    let gitdir = unlinked(Path::new(
        one_line(&target)?
            .strip_prefix("gitdir: ")
            .context("the .git file does not name a gitdir")?,
    ))?;
    let worktrees = gitdir
        .parent()
        .filter(|parent| parent.file_name() == Some("worktrees".as_ref()))
        .context("the gitdir is not a linked worktree registration")?;
    let common = worktrees
        .parent()
        .context("the worktree registration has no common directory")?;
    ensure!(
        one_line(&read_git_file(&gitdir.join("commondir"), POINTER_LIMIT)?)? == "../..",
        "the worktree registration does not name its common directory as Git does"
    );
    let back = unlinked(Path::new(one_line(&read_git_file(
        &gitdir.join("gitdir"),
        POINTER_LIMIT,
    )?)?))?;
    ensure!(
        back == pointer,
        "the worktree registration names another .git file"
    );
    ensure!(
        common.file_name() == Some(".git".as_ref()),
        "the common directory is not the .git directory of a main working tree"
    );
    let root = common
        .parent()
        .context("the common directory has no main working tree")?;
    for config in ["config", "config.worktree"] {
        let path = common.join(config);
        if config == "config" || fs::symlink_metadata(&path).is_ok() {
            main_tree_config(&read_git_file(&path, CONFIG_LIMIT)?)
                .with_context(|| format!("repository {config}"))?;
        }
    }
    Ok(Some(root.to_owned()))
}

/// Whether `metadata`, read without following links, is a link: a symbolic link, or on
/// Windows any reparse point (a junction as well).
pub(super) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

// An absolute path that no link on the way resolves elsewhere, made canonical. Git writes
// real paths unless the repository asks for relative ones, which are not followed here.
fn unlinked(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Git names {} by a relative path",
        path.display()
    );
    for ancestor in path.ancestors() {
        ensure!(
            !is_link(&fs::symlink_metadata(ancestor)?),
            "{} is a link",
            ancestor.display()
        );
    }
    Ok(path.canonicalize()?)
}

// One of Git's files: a regular file reached without a link, within `limit`, strict UTF-8.
fn read_git_file(path: &Path, limit: u64) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || is_link(&metadata) {
        bail!("{} is not a regular file", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{} is larger than expected",
        path.display()
    );
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

// The single line of a pointer file, without the line ending Git writes.
fn one_line(text: &str) -> Result<&str> {
    let line = text.trim_end_matches(['\n', '\r']);
    ensure!(
        !line.is_empty() && !line.contains(['\n', '\r']),
        "a Git pointer file is not one line"
    );
    Ok(line)
}

// A repository configuration that keeps its main working tree where the common directory is:
// `core.bare` is not true and `core.worktree` is absent. A file that another file is included
// into, or that continues a line, cannot be evaluated here and is refused.
fn main_tree_config(text: &str) -> Result<()> {
    let mut core = false;
    for raw in text.lines() {
        let line = raw.trim();
        ensure!(
            line.chars().rev().take_while(|c| *c == '\\').count() % 2 == 0,
            "a configuration line continues on the next"
        );
        let entry = if let Some(header) = line.strip_prefix('[') {
            let (header, rest) = header
                .split_once(']')
                .context("unterminated configuration section")?;
            let name_end = header
                .find(|c: char| c.is_whitespace() || c == '.' || c == '"')
                .unwrap_or(header.len());
            let name = header[..name_end].to_ascii_lowercase();
            ensure!(
                name != "include" && name != "includeif",
                "the configuration includes another file"
            );
            core = name == "core" && name_end == header.len();
            rest.trim()
        } else {
            line
        };
        if !core || entry.is_empty() || entry.starts_with(['#', ';']) {
            continue;
        }
        let (key, value) = entry.split_once('=').unwrap_or((entry, "true"));
        match key.trim().to_ascii_lowercase().as_str() {
            "worktree" => bail!("core.worktree moves the main working tree"),
            "bare" => {
                let value = value
                    .split(['#', ';'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('"')
                    .to_ascii_lowercase();
                match value.as_str() {
                    "false" | "no" | "off" | "0" | "" => {}
                    _ => bail!("the repository is bare or its core.bare cannot be read"),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// The consent tests build their worktrees with this module's fixtures.
#[cfg(test)]
pub(super) mod tests;
