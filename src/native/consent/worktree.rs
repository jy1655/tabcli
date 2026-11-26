//! The linked git worktree rule of workspace consent reads a stated subset of Git's own
//! repository layout and configuration, as Git defines it, and runs no `git`. Anything
//! outside that subset, and anything that cannot be read or parsed, is no worktree, so the
//! workspace keeps the exact rule alone. See AGENTS.md, Trust Store Boundary.
//!
//! The subset, for a canonical workspace `W` (each file read is a regular file reached
//! without a link, each directory one reached without a link):
//! - `W/.git` is a file holding `gitdir: <path>` and nothing but line endings after it. The
//!   path is absolute or, as Git resolves it, relative to `W`. Lexically, before any I/O on
//!   it, it names `<main>/.git/worktrees/<id>` on `W`'s own volume (drive or UNC share).
//! - That registration directory `G` holds `commondir`, one line naming `<main>/.git`
//!   (absolute, or relative to `G`, as Git reads it), and `gitdir`, one line naming `W/.git`
//!   (absolute, or relative to `G` as Git 2.48 writes it for `--relative-paths`). Both are
//!   compared lexically, so neither causes I/O elsewhere.
//! - Both Git directories are usable as Git's `is_git_directory` checks them: `G/HEAD` and
//!   `<main>/.git/HEAD` hold `ref:` and a `refs/` name, or an object id, and
//!   `<main>/.git/objects` and `<main>/.git/refs` are directories.
//! - `<main>/.git/config` is in the configuration subset below; with
//!   `extensions.worktreeConfig` true, so are `<main>/.git/config.worktree` and
//!   `G/config.worktree` when present (only "not found" is absence).
//!
//! The configuration subset is Git's syntax (`git help config`, "Syntax"): an optional UTF-8
//! byte order mark, which Git skips; section headers `[name]`, `[name "sub"]` and the old
//! `[name.sub]`, a variable possibly following on the header's line; variables `key` (true)
//! and `key = value` with Git's quoting, escapes and comments. Refused: a value continued on
//! the next line, any `include` or `includeIf` section, `core.worktree`, a true `core.bare`
//! (last value wins, as in Git), `core.repositoryformatversion` other than 0 or 1, an
//! extension Git does not define, and anything Git's parser rejects.
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};

// Git's pointer and registration files hold one path; Git reads at most 255 bytes of HEAD.
const POINTER_LIMIT: u64 = 64 * 1024;
const HEAD_LIMIT: u64 = 255;
const CONFIG_LIMIT: u64 = 1024 * 1024;

/// The main working tree of the repository that registered `workspace` as a linked worktree.
/// `Ok(None)` is a workspace without a `.git` file: no worktree root, or a main working tree.
/// An `Err` is a `.git` file that is not a verified registration in the subset above.
/// `workspace` is canonical.
pub(super) fn main_worktree(workspace: &Path) -> Result<Option<PathBuf>> {
    let pointer = workspace.join(".git");
    match fs::symlink_metadata(&pointer) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() => return Ok(None),
        Ok(_) => {}
    }
    let here = Located::of(workspace)?;
    let text = read_git_file(&pointer, POINTER_LIMIT)?;
    let target = one_line(&text)?
        .strip_prefix("gitdir: ")
        .context("the .git file does not name a gitdir")?;
    let named = here.resolve(Path::new(target))?;
    // Plausible before any I/O on what the pointer names.
    ensure!(
        named.volume == here.volume,
        "the .git file names a directory on another volume"
    );
    ensure!(
        named.is_registration(),
        "the gitdir is not a linked worktree registration of a main working tree"
    );
    let gitdir = walk(&named)?;
    let registration = Located::of(&gitdir)?;
    ensure!(
        registration.is_registration(),
        "the gitdir is not a linked worktree registration of a main working tree"
    );
    let common = registration.ancestor(2);
    let commondir = registration.resolve(Path::new(one_line(&read_git_file(
        &gitdir.join("commondir"),
        POINTER_LIMIT,
    )?)?))?;
    ensure!(
        commondir.same(&common),
        "the worktree registration names another common directory"
    );
    let back = registration.resolve(Path::new(one_line(&read_git_file(
        &gitdir.join("gitdir"),
        POINTER_LIMIT,
    )?)?))?;
    ensure!(
        back.same(&here.child(".git")),
        "the worktree registration names another .git file"
    );
    let common_dir = gitdir
        .parent()
        .and_then(Path::parent)
        .context("the registration has no common directory")?;
    head(&gitdir.join("HEAD"))?;
    head(&common_dir.join("HEAD"))?;
    directory(&common_dir.join("objects"))?;
    directory(&common_dir.join("refs"))?;
    let worktree_config =
        repository_config(&read_git_file(&common_dir.join("config"), CONFIG_LIMIT)?)
            .context("repository config")?;
    if worktree_config {
        for (name, path) in [
            (
                "repository config.worktree",
                common_dir.join("config.worktree"),
            ),
            ("worktree config.worktree", gitdir.join("config.worktree")),
        ] {
            if let Some(text) = read_optional_git_file(&path, CONFIG_LIMIT)? {
                main_tree_config(&text).context(name)?;
            }
        }
    }
    let main = common_dir
        .parent()
        .context("the common directory has no main working tree")?;
    Ok(Some(main.to_owned()))
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

// The volume an absolute path is on: the root on Unix, a drive or a UNC share on Windows.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Volume {
    Root,
    Disk(u8),
    Share(String, String),
}

// An absolute path as a volume and its normal components, `.` and `..` resolved lexically.
#[derive(Clone, Debug)]
struct Located {
    volume: Volume,
    parts: Vec<OsString>,
}

impl Located {
    fn of(path: &Path) -> Result<Self> {
        let mut components = path.components().peekable();
        let volume = match components.peek() {
            Some(Component::Prefix(prefix)) => {
                let volume = match prefix.kind() {
                    Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                        Volume::Disk(letter.to_ascii_uppercase())
                    }
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                        Volume::Share(
                            server.to_string_lossy().to_lowercase(),
                            share.to_string_lossy().to_lowercase(),
                        )
                    }
                    _ => bail!("{} is on an unsupported kind of volume", path.display()),
                };
                components.next();
                volume
            }
            _ => Volume::Root,
        };
        ensure!(
            matches!(components.next(), Some(Component::RootDir)),
            "{} is not an absolute path",
            path.display()
        );
        let mut located = Self {
            volume,
            parts: Vec::new(),
        };
        located.extend(components)?;
        Ok(located)
    }

    // `path` as Git resolves a path it reads in a file of this directory: an absolute path
    // as it is, a relative one from here.
    fn resolve(&self, path: &Path) -> Result<Self> {
        if path.is_absolute() {
            return Self::of(path);
        }
        let mut located = self.clone();
        located.extend(path.components())?;
        Ok(located)
    }

    fn extend<'a>(&mut self, components: impl Iterator<Item = Component<'a>>) -> Result<()> {
        for component in components {
            match component {
                Component::Normal(part) => self.parts.push(part.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    self.parts
                        .pop()
                        .context("a path in Git's files leaves its volume")?;
                }
                Component::Prefix(_) | Component::RootDir => {
                    bail!("a relative path in Git's files names a root")
                }
            }
        }
        Ok(())
    }

    // The same path on this platform: on Windows, names compare without regard to case.
    fn same(&self, other: &Self) -> bool {
        self.volume == other.volume
            && self.parts.len() == other.parts.len()
            && self.parts.iter().zip(&other.parts).all(|(a, b)| {
                if cfg!(windows) {
                    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
                } else {
                    a == b
                }
            })
    }

    // `<main>/.git/worktrees/<id>`, `<main>` not the volume root.
    fn is_registration(&self) -> bool {
        let n = self.parts.len();
        n >= 4 && self.parts[n - 2] == "worktrees" && self.parts[n - 3] == ".git"
    }

    fn ancestor(&self, levels: usize) -> Self {
        let mut located = self.clone();
        located
            .parts
            .truncate(self.parts.len().saturating_sub(levels));
        located
    }

    fn child(&self, name: &str) -> Self {
        let mut located = self.clone();
        located.parts.push(name.into());
        located
    }

    fn root(&self) -> PathBuf {
        match &self.volume {
            Volume::Root => PathBuf::from("/"),
            Volume::Disk(letter) => PathBuf::from(format!("{}:\\", char::from(*letter))),
            Volume::Share(server, share) => PathBuf::from(format!(r"\\{server}\{share}\")),
        }
    }
}

// The directory `located` names, reached from its volume's root through directories none of
// which is a link, made canonical.
fn walk(located: &Located) -> Result<PathBuf> {
    let mut path = located.root();
    for part in &located.parts {
        path.push(part);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(!is_link(&metadata), "{} is a link", path.display());
        ensure!(metadata.is_dir(), "{} is not a directory", path.display());
    }
    Ok(path.canonicalize()?)
}

fn directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !is_link(&metadata),
        "{} is not a directory",
        path.display()
    );
    Ok(())
}

// A `HEAD` as Git's `validate_headref` accepts it, read as Git reads it: `ref:`, optional
// whitespace and a `refs/` name, or a hexadecimal object id. A symbolic-link `HEAD` is refused.
fn head(path: &Path) -> Result<()> {
    let bytes = read_git_bytes(path, HEAD_LIMIT, false)?;
    let symbolic = bytes
        .strip_prefix(b"ref:")
        .map(|rest| rest.trim_ascii_start().starts_with(b"refs/"));
    let detached = bytes.len() >= 40 && bytes[..40].iter().all(u8::is_ascii_hexdigit);
    ensure!(
        symbolic.unwrap_or(detached),
        "{} is not a Git HEAD",
        path.display()
    );
    Ok(())
}

// One of Git's files: a regular file reached without a link, within `limit`, strict UTF-8.
fn read_git_file(path: &Path, limit: u64) -> Result<String> {
    String::from_utf8(read_git_bytes(path, limit, true)?)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

// An optional one: only "not found" is absence; any other failure is an error.
fn read_optional_git_file(path: &Path, limit: u64) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => read_git_file(path, limit).map(Some),
    }
}

// The bytes of a regular file reached without a link, checked again on the handle that is
// read: up to `limit`, and with `whole`, refused beyond it.
fn read_git_bytes(path: &Path, limit: u64, whole: bool) -> Result<Vec<u8>> {
    let before =
        fs::symlink_metadata(path).with_context(|| format!("{} cannot be read", path.display()))?;
    ensure!(
        before.is_file() && !is_link(&before),
        "{} is not a regular file",
        path.display()
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && !is_link(&opened),
        "{} is not a regular file",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (opened.dev(), opened.ino()) == (before.dev(), before.ino()),
            "{} changed while it was opened",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        ensure!(!whole, "{} is larger than expected", path.display());
        bytes.truncate(limit as usize);
    }
    Ok(bytes)
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

// The extensions Git defines (`git help config`, `extensions.*`), lowercase.
const EXTENSIONS: [&str; 9] = [
    "noop",
    "noop-v1",
    "preciousobjects",
    "partialclone",
    "worktreeconfig",
    "objectformat",
    "compatobjectformat",
    "refstorage",
    "relativeworktrees",
];

// `<main>/.git/config`: the main working tree's own subset, a repository format Git reads,
// and whether `extensions.worktreeConfig` brings the `config.worktree` files in.
fn repository_config(text: &str) -> Result<bool> {
    let variables = variables(text)?;
    main_tree(&variables)?;
    let mut version = 0;
    let mut worktree_config = false;
    for variable in &variables {
        match (variable.section.as_str(), variable.subsection) {
            ("core", false) if variable.key == "repositoryformatversion" => {
                version = integer(variable.value.as_deref())?;
            }
            ("extensions", false) => {
                ensure!(
                    EXTENSIONS.contains(&variable.key.as_str()),
                    "extensions.{} is not an extension Git defines",
                    variable.key
                );
                if variable.key == "worktreeconfig" {
                    worktree_config = boolean(variable.value.as_deref())?;
                }
            }
            ("extensions", true) => bail!("an extension Git does not define"),
            _ => {}
        }
    }
    ensure!(
        version == 0 || version == 1,
        "core.repositoryformatversion {version} is not one Git reads"
    );
    Ok(worktree_config)
}

// A `config.worktree` file: the main working tree's own subset only.
fn main_tree_config(text: &str) -> Result<()> {
    main_tree(&variables(text)?)
}

// The working tree stays where the common directory is: no `core.worktree`, and a `core.bare`
// whose last value is false.
fn main_tree(variables: &[Variable]) -> Result<()> {
    let mut bare = false;
    for variable in variables {
        if variable.section != "core" || variable.subsection {
            continue;
        }
        match variable.key.as_str() {
            "worktree" => bail!("core.worktree moves the working tree"),
            "bare" => bare = boolean(variable.value.as_deref())?,
            _ => {}
        }
    }
    ensure!(!bare, "the repository is bare");
    Ok(())
}

// A Git boolean: `true`, `yes`, `on`, `false`, `no`, `off`, empty, or a decimal integer;
// a key without `=` is true.
fn boolean(value: Option<&str>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(true);
    };
    Ok(match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => true,
        "false" | "no" | "off" | "" => false,
        other => integer(Some(other))? != 0,
    })
}

fn integer(value: Option<&str>) -> Result<i64> {
    let value = value.context("a number without a value")?;
    value
        .parse()
        .with_context(|| format!("{value:?} is not a decimal number Git reads here"))
}

// One variable of a Git configuration file: its section (lowercase), whether a subsection
// follows, its key (lowercase) and its value (`None` for a key without `=`).
#[derive(Debug)]
struct Variable {
    section: String,
    subsection: bool,
    key: String,
    value: Option<String>,
}

fn variables(text: &str) -> Result<Vec<Variable>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut section: Option<(String, bool)> = None;
    let mut variables = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        ensure!(
            !line.contains(['\r', '\0']),
            "a configuration line holds a carriage return or NUL"
        );
        ensure!(
            line.bytes().rev().take_while(|byte| *byte == b'\\').count() % 2 == 0,
            "a configuration value continues on the next line"
        );
        let mut rest = line.trim_start_matches(is_space);
        if let Some(header) = rest.strip_prefix('[') {
            let (name, subsection, after) = section_header(header)?;
            ensure!(
                name != "include" && name != "includeif",
                "the configuration includes another file"
            );
            section = Some((name, subsection));
            rest = after.trim_start_matches(is_space);
        }
        if rest.is_empty() || rest.starts_with(['#', ';']) {
            continue;
        }
        let (name, subsection) = section
            .clone()
            .context("a configuration variable outside any section")?;
        let (key, value) = variable(rest)?;
        variables.push(Variable {
            section: name,
            subsection,
            key,
            value,
        });
    }
    Ok(variables)
}

fn is_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\x0b' | '\x0c')
}

// After `[`: the lowercase section name, whether a subsection follows, and the rest after `]`.
fn section_header(header: &str) -> Result<(String, bool, &str)> {
    let end = header
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
        .context("unterminated configuration section")?;
    let name = &header[..end];
    ensure!(!name.is_empty(), "an empty configuration section name");
    let rest = &header[end..];
    if let Some(after) = rest.strip_prefix(']') {
        // `[section.subsection]`, Git's older form.
        return Ok(match name.split_once('.') {
            Some((section, _)) => (section.to_ascii_lowercase(), true, after),
            None => (name.to_ascii_lowercase(), false, after),
        });
    }
    ensure!(
        !name.contains('.'),
        "a configuration section with both subsection forms"
    );
    let quoted = rest
        .trim_start_matches(is_space)
        .strip_prefix('"')
        .context("a malformed configuration section")?;
    let mut characters = quoted.char_indices();
    loop {
        match characters.next() {
            None => bail!("unterminated configuration subsection"),
            Some((_, '\\')) => {
                characters
                    .next()
                    .context("unterminated configuration subsection")?;
            }
            Some((index, '"')) => {
                let after = quoted[index + 1..]
                    .strip_prefix(']')
                    .context("a malformed configuration section")?;
                return Ok((name.to_ascii_lowercase(), true, after));
            }
            Some(_) => {}
        }
    }
}

// A variable line: its lowercase key and its value as Git reads it.
fn variable(line: &str) -> Result<(String, Option<String>)> {
    let end = line
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(line.len());
    let key = &line[..end];
    ensure!(
        key.starts_with(|c: char| c.is_ascii_alphabetic()),
        "a malformed configuration variable"
    );
    let rest = line[end..].trim_start_matches(is_space);
    if rest.is_empty() || rest.starts_with(['#', ';']) {
        return Ok((key.to_ascii_lowercase(), None));
    }
    let value = rest
        .strip_prefix('=')
        .context("a malformed configuration variable")?;
    Ok((key.to_ascii_lowercase(), Some(value_text(value)?)))
}

// Git's `parse_value`: leading space skipped, inner unquoted space kept, trailing unquoted
// space dropped, `"` toggling quotes, the escapes `\n`, `\t`, `\b`, `\\`, `\"`, and an
// unquoted `#` or `;` starting a comment.
fn value_text(text: &str) -> Result<String> {
    let mut value = String::new();
    let mut pending_space = 0;
    let mut quoted = false;
    let mut characters = text.trim_start_matches(is_space).chars();
    while let Some(character) = characters.next() {
        if !quoted && is_space(character) {
            if !value.is_empty() {
                pending_space += 1;
            }
            continue;
        }
        if !quoted && matches!(character, '#' | ';') {
            break;
        }
        value.extend(std::iter::repeat_n(' ', pending_space));
        pending_space = 0;
        match character {
            '"' => quoted = !quoted,
            '\\' => value.push(match characters.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some('b') => '\u{8}',
                Some('\\') => '\\',
                Some('"') => '"',
                _ => bail!("a configuration value with an escape Git rejects"),
            }),
            other => value.push(other),
        }
    }
    ensure!(!quoted, "a configuration value with an unterminated quote");
    Ok(value)
}

// The consent tests build their worktrees with this module's fixtures.
#[cfg(test)]
pub(super) mod tests;
