//! The linked git worktree rule of workspace consent establishes one property, the one the
//! owner accepted (2026-11-26): someone who can write a trusted repository's `.git` can add a
//! worktree, and that worktree is then trusted. It establishes nothing else about Git: it runs
//! no `git`, does not read or evaluate Git configuration (a repository with `core.worktree` or
//! unusual extensions is judged by its registration alone), and does not inspect `HEAD`,
//! objects or refs. See AGENTS.md, Trust Store Boundary.
//!
//! The steps, for a canonical workspace `W`:
//! 1. `W/.git` is a regular file, reached without a link, holding `gitdir: <path>` and only line
//!    endings after it. The path is absolute, or relative to `W` as Git resolves it.
//! 2. Lexically, with no I/O on what it names, the path claims `<main>/.git/worktrees/<id>`.
//!    Before any I/O on `<main>`, the caller's screen must find a provider whose own store lists
//!    exactly `<main>` as trusted, answered from the store alone (no I/O on the path or its
//!    ancestors, no `git`), so a destination no store lists (a share, an automount) is never
//!    touched. The screen is a necessary condition only; consent is assessed afterwards.
//!    A path that names a component and later cancels it with `..` is refused here, before any
//!    I/O; only the leading parent steps of a relative path are taken.
//! 3. The same path is walked component by component from `W` (or from its root): each
//!    component is inspected before the next, none is a link (on Windows, no reparse point),
//!    and every one is a directory.
//! 4. `<main>/.git` is therefore a real directory of `<main>`, and it has no `commondir` of its
//!    own.
//! 5. The registration's `gitdir` names `.git` in `W` itself, and its `commondir` names
//!    `<main>/.git`; each is walked the same way from the registration and compared by file
//!    identity, never by name: the file and, for the back-pointer, its directory too, so a hard
//!    link to `W/.git` in another folder is not `W`. On Windows the identity is the volume and
//!    the full 128-bit file id; where it cannot be read, nothing is inherited.
//!
//! The limit (router's ruling, 2026-11-26): the check defends the property against whatever
//! the launch folder holds, and against any local user who cannot write the main working tree's
//! `.git` or an ancestor directory of either tree. It does not defend against someone who can
//! replace those ancestors while it runs: it inspects by path and does not hold directory
//! handles, as an exactly trusted folder is trusted by path. Pointer files are opened so that a
//! FIFO cannot block the open, and the opened handle is checked.
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

// Git's pointer and registration files hold one path.
const POINTER_LIMIT: u64 = 64 * 1024;

/// The main working tree of the repository that registered `workspace` as a linked worktree.
/// `Ok(None)` is a workspace without a `.git` file: no worktree root, or a main working tree.
/// An `Err` is a `.git` file whose registration does not verify, or whose claimed main working
/// tree the screen `trusted` refuses. `trusted` is asked with the main working tree's lexical
/// path before any I/O on it, and must itself do none. `workspace` is canonical.
pub(super) fn main_worktree(
    workspace: &Path,
    trusted: &mut dyn FnMut(&Path) -> bool,
) -> Result<Option<PathBuf>> {
    let pointer = workspace.join(".git");
    match fs::symlink_metadata(&pointer) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => return Ok(None),
        Ok(_) => {}
    }
    let text = read_git_file(&pointer)?;
    let target = Path::new(
        one_line(&text)?
            .strip_prefix("gitdir: ")
            .context("the .git file does not name a gitdir")?,
    );
    // The claim, lexically: no I/O on the destination before its screen.
    let main = claimed_main(workspace, target)?
        .context("the gitdir is not a linked worktree registration of a main working tree")?;
    ensure!(
        trusted(&main),
        "no provider's store lists the main working tree that the .git file names as trusted"
    );
    let registration = walk(workspace, target, Kind::Directory)?;
    let common = registration
        .parent()
        .and_then(Path::parent)
        .context("the registration has no common directory")?;
    ensure!(
        common.parent() == Some(main.as_path()),
        "the registration is not inside the main working tree's .git"
    );
    match fs::symlink_metadata(common.join("commondir")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => bail!("the main working tree's .git names a common directory of its own"),
    }
    let back = walk(
        &registration,
        Path::new(one_line(&read_git_file(&registration.join("gitdir"))?)?),
        Kind::File,
    )?;
    ensure!(
        object_id(&back)? == object_id(&pointer)?,
        "the worktree registration names another .git file"
    );
    ensure!(
        back.file_name() == Some(OsStr::new(".git"))
            && object_id(back.parent().context("the back-pointer names no folder")?)?
                == object_id(workspace)?,
        "the worktree registration names a .git file in another folder"
    );
    let named_common = walk(
        &registration,
        Path::new(one_line(&read_git_file(&registration.join("commondir"))?)?),
        Kind::Directory,
    )?;
    ensure!(
        object_id(&named_common)? == object_id(common)?,
        "the worktree registration names another common directory"
    );
    Ok(Some(main.canonicalize()?))
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

// The main working tree that `target`, resolved from `base` as Git resolves it with `.` and
// `..` applied lexically, claims as `<main>/.git/worktrees/<id>`; `<main>` may be a volume
// root. For the screen only: no I/O.
fn claimed_main(base: &Path, target: &Path) -> Result<Option<PathBuf>> {
    let (mut path, components) = start(base, target)?;
    for component in components {
        match component {
            Component::Normal(part) => path.push(name(part)?),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(path.pop(), "a path in Git's files leaves its volume")
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("a relative path in Git's files names a root")
            }
        }
    }
    let names: Vec<_> = path
        .components()
        .rev()
        .take(3)
        .map(|component| component.as_os_str().to_owned())
        .collect();
    if names.len() < 3 || names[1] != "worktrees" || names[2] != ".git" {
        return Ok(None);
    }
    for _ in 0..3 {
        path.pop();
    }
    Ok(Some(path))
}

// The directory a walk or a lexical resolution starts from, and the components after it: an
// absolute `target` from its own root, a relative one from `base`. On Windows an absolute root
// is spelled as `canonicalize` spells it (`\\?\C:\`, `\\?\UNC\server\share\`), the spelling the
// relative case inherits from the canonical `base`, so the screen is asked about the path an
// exact-folder lookup would key, and every walk addresses the file system without Win32's
// rewriting of the names.
fn start<'a>(base: &Path, target: &'a Path) -> Result<(PathBuf, Vec<Component<'a>>)> {
    let mut components = target.components().peekable();
    let root = if !target.is_absolute() {
        base.to_owned()
    } else {
        #[cfg(windows)]
        let root = match components.next() {
            Some(Component::Prefix(prefix)) => verbatim_root(prefix.kind())?,
            _ => bail!("an absolute path in Git's files has no volume"),
        };
        #[cfg(not(windows))]
        let root = PathBuf::from("/");
        if components.peek() == Some(&Component::RootDir) {
            components.next();
        }
        root
    };
    let components: Vec<_> = components.collect();
    // Review r3, finding 6: a name cancelled by a later `..` would make the walk touch a path
    // outside both trees that the screen never approved. Only leading parent steps are taken,
    // as Git writes them for relative paths.
    let mut named = false;
    for component in &components {
        match component {
            Component::Normal(_) => named = true,
            Component::ParentDir if named => {
                bail!("a path in Git's files steps back over a name it named")
            }
            _ => {}
        }
    }
    Ok((root, components))
}

// The verbatim root of a volume as `canonicalize` spells it: the drive letter in upper case, a
// share by its server and share names. A device namespace or a volume named by its object name
// is no root Git writes for a working tree.
#[cfg(windows)]
fn verbatim_root(prefix: std::path::Prefix<'_>) -> Result<PathBuf> {
    use std::path::Prefix::{Disk, UNC, VerbatimDisk, VerbatimUNC};
    Ok(PathBuf::from(match prefix {
        Disk(letter) | VerbatimDisk(letter) => {
            format!(r"\\?\{}:\", char::from(letter.to_ascii_uppercase()))
        }
        UNC(server, share) | VerbatimUNC(server, share) => format!(
            r"\\?\UNC\{}\{}\",
            server.to_str().context("a share name is not UTF-8")?,
            share.to_str().context("a share name is not UTF-8")?
        ),
        _ => bail!("a path in Git's files names a device or a volume by its object name"),
    }))
}

// One name of a path in Git's files. A verbatim Windows path does not split at `/`, but pushing
// such a name would add components that were never inspected one by one.
fn name(part: &OsStr) -> Result<&OsStr> {
    ensure!(
        !part.as_encoded_bytes().contains(&b'/'),
        "a name in a path in Git's files holds a separator"
    );
    Ok(part)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Directory,
    File,
}

// `target`, resolved from `base` as Git resolves it, reached component by component: each
// component is inspected before the next is taken, none is a link, every one but the last is
// a directory, and the last is of `kind`. A leading `..` steps back over an ancestor of `base`,
// a directory reached without a link.
fn walk(base: &Path, target: &Path, kind: Kind) -> Result<PathBuf> {
    let (mut path, components) = start(base, target)?;
    ensure!(
        !components.is_empty(),
        "a path in Git's files names nothing"
    );
    let last = components.len() - 1;
    for (index, component) in components.into_iter().enumerate() {
        match component {
            Component::Normal(part) => {
                path.push(name(part)?);
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("{} cannot be read", path.display()))?;
                ensure!(!is_link(&metadata), "{} is a link", path.display());
                let directory = index < last || kind == Kind::Directory;
                ensure!(
                    if directory {
                        metadata.is_dir()
                    } else {
                        metadata.is_file()
                    },
                    "{} is not a {}",
                    path.display(),
                    if directory { "directory" } else { "file" }
                );
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(path.pop(), "a path in Git's files leaves its volume");
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("a relative path in Git's files names a root")
            }
        }
    }
    Ok(path)
}

// The identity of what `path` names, its last component not followed: the device and inode on
// Unix; on Windows the volume and the full 128-bit file id (`FileIdInfo`), because the 64-bit
// file index can repeat on ReFS (review r3, finding 11). An identity that cannot be read is an
// error, so nothing is inherited.
fn object_id(path: &Path) -> Result<(u64, u128)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        Ok((metadata.dev(), u128::from(metadata.ino())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx,
        };
        let handle = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot identify {}", path.display()));
        }
        ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "{} is a link",
            path.display()
        );
        let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileIdInfo,
                (&raw mut id).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot read the full file id of {}", path.display()));
        }
        Ok((
            id.VolumeSerialNumber,
            u128::from_le_bytes(id.FileId.Identifier),
        ))
    }
}

// One of Git's pointer files: a regular file reached without a link, checked again on the
// handle that is read, within its limit, strict UTF-8.
fn read_git_file(path: &Path) -> Result<String> {
    let before =
        fs::symlink_metadata(path).with_context(|| format!("{} cannot be read", path.display()))?;
    ensure!(
        before.is_file() && !is_link(&before),
        "{} is not a regular file",
        path.display()
    );
    let file = open_pointer(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        ensure!(
            (opened.dev(), opened.ino()) == (before.dev(), before.ino()),
            "{} changed while it was opened",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(POINTER_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= POINTER_LIMIT,
        "{} is larger than expected",
        path.display()
    );
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

// Opens one of Git's pointer files without following a link and, on Unix, without blocking
// on a FIFO put there after it was inspected (review r3, finding 8), then checks the handle it
// opened: a regular file. A regular file reads the same with `O_NONBLOCK`.
fn open_pointer(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .with_context(|| format!("{} cannot be read", path.display()))?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && !is_link(&opened),
        "{} is not a regular file",
        path.display()
    );
    Ok(file)
}

// The single line of a pointer file, without the line endings Git strips.
fn one_line(text: &str) -> Result<&str> {
    let line = text.trim_end_matches(['\n', '\r']);
    ensure!(
        !line.is_empty() && !line.contains(['\n', '\r', '\0']),
        "a Git pointer file is not one line"
    );
    Ok(line)
}

// The consent tests build their worktrees with this module's fixtures.
#[cfg(test)]
pub(super) mod tests;
