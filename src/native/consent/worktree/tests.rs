use super::super::store::native_key;
use super::*;
use std::process::Command;

// A path as Git spells it in its files and takes it as an argument: without Windows' verbatim
// prefix, and with forward slashes there.
pub(in crate::native::consent) fn git_path(path: &Path) -> String {
    native_key(path).unwrap().replace('\\', "/")
}

// Git as a user runs it, isolated from the developer's environment: no repository, index,
// object or configuration override inherited from it, no system or global configuration, no
// hooks, absolute worktree paths unless a test asks for relative ones, and a fixed identity
// for the one commit a worktree needs.
pub(in crate::native::consent) struct Git {
    home: tempfile::TempDir,
}

impl Git {
    pub(in crate::native::consent) fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("gitconfig"), "").unwrap();
        fs::create_dir(home.path().join("hooks")).unwrap();
        Self { home }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        let home = self.home.path();
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .args([
                "-c",
                "user.name=TAB",
                "-c",
                "user.email=tab@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "worktree.useRelativePaths=false",
                "-c",
            ])
            .arg(format!("core.hooksPath={}", git_path(&home.join("hooks"))));
        command
    }

    pub(in crate::native::consent) fn run(&self, arguments: &[&str]) {
        let output = self.command().args(arguments).output().unwrap();
        assert!(output.status.success(), "git {arguments:?}: {output:?}");
    }

    // Whether this Git writes relative-path worktrees (Git 2.48 and newer).
    fn writes_relative_paths(&self) -> bool {
        let output = self.command().arg("--version").output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let version: Vec<u32> = text
            .split_whitespace()
            .nth(2)
            .unwrap_or_default()
            .split('.')
            .take(2)
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        version >= vec![2, 48]
    }
}

// A repository with one commit at `root/repository`, made by Git if it is not there yet.
pub(in crate::native::consent) fn git_repository(git: &Git, root: &Path) -> PathBuf {
    let repository = root.join("repository");
    if !repository.exists() {
        git.run(&["init", "--quiet", &git_path(&repository)]);
        git.run(&[
            "-C",
            &git_path(&repository),
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "init",
        ]);
    }
    repository.canonicalize().unwrap()
}

// A linked worktree that Git added at `root/<name>`; with `relative`, as
// `git worktree add --relative-paths` writes it.
pub(in crate::native::consent) fn git_worktree(
    git: &Git,
    root: &Path,
    name: &str,
    relative: bool,
) -> (PathBuf, PathBuf) {
    let repository = git_repository(git, root);
    let worktree = git_path(root) + "/" + name;
    let mut arguments = vec!["-C", &git_path(&repository)]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    arguments.extend(["worktree", "add", "--quiet", "--detach"].map(str::to_owned));
    if relative {
        arguments.push("--relative-paths".into());
    }
    arguments.push(worktree.clone());
    git.run(&arguments.iter().map(String::as_str).collect::<Vec<_>>());
    (repository, Path::new(&worktree).canonicalize().unwrap())
}

fn canonical_root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    (temp, root)
}

// A real repository and a linked worktree that Git registered at `root/worktree` with absolute
// paths; each refusal below changes one fact of it.
pub(in crate::native::consent) struct Registered {
    pub(in crate::native::consent) repository: PathBuf,
    pub(in crate::native::consent) worktree: PathBuf,
    pub(in crate::native::consent) registration: PathBuf,
    pub(in crate::native::consent) _git: Git,
}

pub(in crate::native::consent) fn registered(root: &Path) -> Registered {
    let git = Git::new();
    let (repository, worktree) = git_worktree(&git, root, "worktree", false);
    let registration = repository
        .join(".git/worktrees/worktree")
        .canonicalize()
        .unwrap();
    Registered {
        repository,
        worktree,
        registration,
        _git: git,
    }
}

// The registration that `git worktree add --relative-paths` (Git 2.48) writes, made from the
// absolute one by hand so that it does not depend on the installed Git: both pointers relative
// to their own directory, and the extension that keeps an older Git from misreading them.
fn make_relative(fixture: &Registered) {
    fs::write(
        fixture.worktree.join(".git"),
        "gitdir: ../repository/.git/worktrees/worktree\n",
    )
    .unwrap();
    fs::write(
        fixture.registration.join("gitdir"),
        "../../../../worktree/.git\n",
    )
    .unwrap();
    let config = fixture.repository.join(".git/config");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("repositoryformatversion = 0", "repositoryformatversion = 1");
    fs::write(
        &config,
        format!("{text}[extensions]\n\trelativeWorktrees = true\n"),
    )
    .unwrap();
}

#[test]
fn a_worktree_that_git_added_names_its_main_working_tree() {
    let (_temp, root) = canonical_root();
    let git = Git::new();
    let (repository, worktree) = git_worktree(&git, &root, "worktree", false);
    let pointer = fs::read_to_string(worktree.join(".git")).unwrap();
    let target = pointer.trim_end().strip_prefix("gitdir: ").unwrap();
    assert!(Path::new(target).is_absolute(), "{pointer}");
    assert_eq!(main_worktree(&worktree).unwrap(), Some(repository.clone()));
    let (_, second) = git_worktree(&git, &root, "second", false);
    assert_eq!(main_worktree(&second).unwrap(), Some(repository.clone()));
    // The main working tree, a directory inside a worktree and a plain directory are no
    // linked worktree: the exact rule alone applies to them.
    assert_eq!(main_worktree(&repository).unwrap(), None);
    let child = worktree.join("child");
    fs::create_dir(&child).unwrap();
    assert_eq!(main_worktree(&child).unwrap(), None);
    let plain = root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(main_worktree(&plain).unwrap(), None);
    // After Git removed a worktree, a copied `.git` file names a registration that does not
    // name it back.
    git.run(&[
        "-C",
        &git_path(&repository),
        "worktree",
        "remove",
        "--force",
        &git_path(&second),
    ]);
    fs::create_dir(&second).unwrap();
    fs::copy(worktree.join(".git"), second.join(".git")).unwrap();
    assert_eq!(
        main_worktree(&second.canonicalize().unwrap())
            .unwrap_err()
            .to_string(),
        "the worktree registration names another .git file"
    );
}

// Git resolves a relative `gitdir:` from the worktree and a relative registration `gitdir`
// from the registration, so a worktree that `--relative-paths` (or
// `worktree.useRelativePaths`) wrote is as much a worktree as an absolute one.
#[test]
fn a_relative_path_registration_names_its_main_working_tree() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    make_relative(&fixture);
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    // Relative, the registration must still name this worktree back.
    fs::write(
        fixture.registration.join("gitdir"),
        "../../../../elsewhere/.git\n",
    )
    .unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        "the worktree registration names another .git file"
    );

    // The same as Git itself writes it, where Git does.
    let git = Git::new();
    if !git.writes_relative_paths() {
        eprintln!("this Git predates --relative-paths; the written form above stands for it");
        return;
    }
    let (_temp, root) = canonical_root();
    let (repository, worktree) = git_worktree(&git, &root, "relative", true);
    let pointer = fs::read_to_string(worktree.join(".git")).unwrap();
    assert!(pointer.starts_with("gitdir: ../"), "{pointer}");
    assert_eq!(main_worktree(&worktree).unwrap(), Some(repository));
}

#[test]
fn the_registration_must_name_the_worktree_back() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    // A line ending that Git on Windows can leave.
    fs::write(
        fixture.worktree.join(".git"),
        format!("gitdir: {}\r\n", git_path(&fixture.registration)),
    )
    .unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );

    // Another worktree's registration: the forward pointer alone proves nothing.
    let other = root.join("other");
    fs::create_dir(&other).unwrap();
    fs::copy(fixture.worktree.join(".git"), other.join(".git")).unwrap();
    assert_eq!(
        main_worktree(&other).unwrap_err().to_string(),
        "the worktree registration names another .git file"
    );
    let gitdir = fixture.registration.join("gitdir");
    fs::remove_file(&gitdir).unwrap();
    assert!(main_worktree(&fixture.worktree).is_err());
}

// A layout outside the subset is refused from the pointer's text alone, before any I/O on what
// it names: the error is the layout's, not the file system's.
#[test]
fn an_implausible_pointer_is_refused_before_it_is_followed() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let pointer = fixture.worktree.join(".git");
    let point = |target: &str| fs::write(&pointer, target).unwrap();
    const LAYOUT: &str = "the gitdir is not a linked worktree registration of a main working tree";
    for target in [
        format!(
            "gitdir: {}\n",
            git_path(&root.join("tab-test-nonexistent/elsewhere"))
        ),
        // A submodule's own gitdir, and a bare repository's registration.
        format!(
            "gitdir: {}\n",
            git_path(&fixture.repository.join(".git/modules/worktree"))
        ),
        format!(
            "gitdir: {}\n",
            git_path(&root.join("bare.git/worktrees/worktree"))
        ),
        "gitdir: ../../tab-test-nonexistent/worktrees/x\n".to_owned(),
    ] {
        point(&target);
        assert_eq!(
            main_worktree(&fixture.worktree).unwrap_err().to_string(),
            LAYOUT,
            "{target:?}"
        );
    }
    for (content, error) in [
        (
            format!("{}\n", git_path(&fixture.registration)),
            "the .git file does not name a gitdir",
        ),
        (
            format!("gitdir: {}\nmore\n", git_path(&fixture.registration)),
            "a Git pointer file is not one line",
        ),
        (String::new(), "a Git pointer file is not one line"),
        (
            format!("\u{feff}gitdir: {}\n", git_path(&fixture.registration)),
            "the .git file does not name a gitdir",
        ),
    ] {
        point(&content);
        assert_eq!(
            main_worktree(&fixture.worktree).unwrap_err().to_string(),
            error,
            "{content:?}"
        );
    }
    point(&format!("gitdir: {}\n", git_path(&fixture.registration)));
    assert!(main_worktree(&fixture.worktree).unwrap().is_some());
    // A common directory named otherwise than the registration's own.
    fs::write(fixture.registration.join("commondir"), "..\n").unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        "the worktree registration names another common directory"
    );
    fs::write(
        fixture.registration.join("commondir"),
        format!("{}\n", git_path(&fixture.repository.join(".git"))),
    )
    .unwrap();
    assert!(main_worktree(&fixture.worktree).unwrap().is_some());
}

// On Windows a pointer to another drive or share is refused before any I/O on it, so a local
// workspace cannot make TAB reach a network share. Runs on the Windows CI runner only.
#[cfg(windows)]
#[test]
fn a_pointer_to_another_volume_is_refused_before_it_is_followed() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    for target in [
        "gitdir: //tab-test.invalid/share/repository/.git/worktrees/worktree\n",
        "gitdir: \\\\tab-test.invalid\\share\\repository\\.git\\worktrees\\worktree\n",
    ] {
        fs::write(fixture.worktree.join(".git"), target).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            main_worktree(&fixture.worktree).unwrap_err().to_string(),
            "the .git file names a directory on another volume",
            "{target:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}

// Both Git directories must be usable as Git checks them: the incomplete registration that
// matching pointers alone would accept is refused.
#[test]
fn matching_pointers_without_a_usable_repository_are_refused() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let common = fixture.repository.join(".git");
    let verdict = || main_worktree(&fixture.worktree).map_err(|error| format!("{error:#}"));
    let saved = fs::read(fixture.registration.join("HEAD")).unwrap();
    for head in [
        &b""[..],
        b"ref: heads/main\n",
        b"not a head\n",
        b"0123456789abcdef0123456789abcdef0123456",
    ] {
        fs::write(fixture.registration.join("HEAD"), head).unwrap();
        assert!(
            verdict().unwrap_err().ends_with("is not a Git HEAD"),
            "{head:?}"
        );
    }
    for head in [
        &b"ref:refs/heads/main"[..],
        b"ref: \t refs/heads/other\n",
        b"0123456789abcdef0123456789abcdef01234567\n",
    ] {
        fs::write(fixture.registration.join("HEAD"), head).unwrap();
        assert_eq!(verdict(), Ok(Some(fixture.repository.clone())), "{head:?}");
    }
    fs::write(fixture.registration.join("HEAD"), &saved).unwrap();
    fs::remove_file(fixture.registration.join("HEAD")).unwrap();
    assert!(verdict().is_err());
    fs::write(fixture.registration.join("HEAD"), &saved).unwrap();
    for (missing, rename) in [("objects", "objects-gone"), ("refs", "refs-gone")] {
        fs::rename(common.join(missing), common.join(rename)).unwrap();
        assert!(verdict().is_err(), "{missing}");
        fs::rename(common.join(rename), common.join(missing)).unwrap();
    }
    fs::rename(common.join("HEAD"), common.join("HEAD-gone")).unwrap();
    assert!(verdict().is_err());
    fs::rename(common.join("HEAD-gone"), common.join("HEAD")).unwrap();
    assert_eq!(verdict(), Ok(Some(fixture.repository.clone())));

    // The registration written by hand with no repository behind it.
    let (_temp, root) = canonical_root();
    let repository = root.join("repository");
    let registration = repository.join(".git/worktrees/worktree");
    fs::create_dir_all(&registration).unwrap();
    fs::write(repository.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    let worktree = root.join("worktree");
    fs::create_dir(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_path(&registration)),
    )
    .unwrap();
    fs::write(registration.join("commondir"), "../..\n").unwrap();
    fs::write(
        registration.join("gitdir"),
        format!("{}\n", git_path(&worktree.join(".git"))),
    )
    .unwrap();
    let error = format!("{:#}", main_worktree(&worktree).unwrap_err());
    assert!(error.contains("HEAD cannot be read"), "{error}");
}

#[test]
fn the_configuration_is_read_as_git_reads_it() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let config = fixture.repository.join(".git/config");
    let original = fs::read_to_string(&config).unwrap();
    let verdict = |text: &str| {
        fs::write(&config, text).unwrap();
        main_worktree(&fixture.worktree).map_err(|error| format!("{error:#}"))
    };
    for text in [
        original.as_str(),
        "[core]\n\tbare = false\n",
        "\u{feff}[core]\n\tbare = false\n",
        "[Core]\n\tBare=No ; a comment\n",
        "[core] bare = off\n",
        "[core]\n\tbare = \"0\"\n",
        "[core]\n\tbare = true\n\tbare = false\n",
        "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha1\n",
        "[remote \"origin\"]\n\turl = https://example.invalid/x # c\n\tbare = true\n",
        "[core \"x\"]\n\tbare = true\n",
        "[core.x]\n\tworktree = /elsewhere\n",
        "# [core] bare = true\n",
        "[alias]\n\tst = \"status \\\"--short\\\"\"\n",
        "",
    ] {
        assert_eq!(
            verdict(text),
            Ok(Some(fixture.repository.clone())),
            "{text:?}"
        );
    }
    const BARE: &str = "repository config: the repository is bare";
    for (text, error) in [
        ("[core]\n\tbare = true\n", BARE),
        ("[core]\n\tbare\n", BARE),
        ("[core]\n\tbare = 2\n", BARE),
        ("[core]\n\tbare = false\n\tbare = yes\n", BARE),
        ("\u{feff}[core]\n\tbare = true\n", BARE),
        (
            "[core]\n\tbare = maybe\n",
            "repository config: \"maybe\" is not a decimal number Git reads here: invalid digit found in string",
        ),
        (
            "[core]\n\tworktree = /elsewhere\n",
            "repository config: core.worktree moves the working tree",
        ),
        // Git reads this first section past its byte order mark.
        (
            "\u{feff}[core]\n\tworktree = /elsewhere\n",
            "repository config: core.worktree moves the working tree",
        ),
        (
            "[include]\n\tpath = other\n",
            "repository config: the configuration includes another file",
        ),
        (
            "[includeIf \"gitdir:/x/\"]\n\tpath = other\n",
            "repository config: the configuration includes another file",
        ),
        (
            "[core]\n\tbare = false \\\n",
            "repository config: a configuration value continues on the next line",
        ),
        (
            "[core\n",
            "repository config: unterminated configuration section",
        ),
        (
            "[ core ]\n",
            "repository config: an empty configuration section name",
        ),
        (
            "bare = false\n",
            "repository config: a configuration variable outside any section",
        ),
        (
            "[core]\n\t-bare = false\n",
            "repository config: a malformed configuration variable",
        ),
        (
            "[core]\n\tname = \"open\n",
            "repository config: a configuration value with an unterminated quote",
        ),
        (
            "[core]\n\tname = a\\qb\n",
            "repository config: a configuration value with an escape Git rejects",
        ),
        (
            "[core]\n\trepositoryformatversion = 2\n",
            "repository config: core.repositoryformatversion 2 is not one Git reads",
        ),
        (
            "[extensions]\n\tsomethingNew = true\n",
            "repository config: extensions.somethingnew is not an extension Git defines",
        ),
    ] {
        assert_eq!(verdict(text), Err(error.to_owned()), "{text:?}");
    }
    // A repository without a configuration is not one Git made.
    fs::remove_file(&config).unwrap();
    assert!(main_worktree(&fixture.worktree).is_err());
}

// With `extensions.worktreeConfig`, Git also reads `config.worktree` of the main working tree
// and of the registration; without it, Git reads neither, and neither does TAB.
#[test]
fn per_worktree_configuration_counts_exactly_when_git_reads_it() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let config = fixture.repository.join(".git/config");
    let common = fixture.repository.join(".git/config.worktree");
    let own = fixture.registration.join("config.worktree");
    let moved = "[core]\n\tworktree = /elsewhere\n";
    let verdict = || main_worktree(&fixture.worktree).map_err(|error| format!("{error:#}"));
    fs::write(&common, moved).unwrap();
    fs::write(&own, moved).unwrap();
    assert_eq!(verdict(), Ok(Some(fixture.repository.clone())));
    fs::write(
        &config,
        "[core]\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n",
    )
    .unwrap();
    assert_eq!(
        verdict(),
        Err("repository config.worktree: core.worktree moves the working tree".to_owned())
    );
    fs::remove_file(&common).unwrap();
    assert_eq!(
        verdict(),
        Err("worktree config.worktree: core.worktree moves the working tree".to_owned())
    );
    fs::write(&own, "[core]\n\tbare = true\n").unwrap();
    assert_eq!(
        verdict(),
        Err("worktree config.worktree: the repository is bare".to_owned())
    );
    fs::write(&own, "[core]\n\tsparseCheckout = true\n").unwrap();
    assert_eq!(verdict(), Ok(Some(fixture.repository.clone())));
    // Only "not found" is absence: a `config.worktree` that cannot be read refuses.
    fs::remove_file(&own).unwrap();
    fs::create_dir(&own).unwrap();
    assert!(verdict().unwrap_err().ends_with("is not a regular file"));
}

#[cfg(unix)]
#[test]
fn no_link_on_the_way_counts() {
    use std::os::unix::fs::symlink;
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    // `.git` itself as a link to the real pointer file.
    let pointer = fixture.worktree.join(".git");
    let moved = root.join("pointer");
    fs::rename(&pointer, &moved).unwrap();
    symlink(&moved, &pointer).unwrap();
    assert!(
        main_worktree(&fixture.worktree)
            .unwrap_err()
            .to_string()
            .ends_with("is not a regular file")
    );
    fs::remove_file(&pointer).unwrap();
    fs::rename(&moved, &pointer).unwrap();
    assert!(main_worktree(&fixture.worktree).unwrap().is_some());
    // The registration reached through a linked directory.
    let alias = root.join("alias");
    symlink(&fixture.repository, &alias).unwrap();
    fs::write(
        &pointer,
        format!(
            "gitdir: {}\n",
            git_path(&alias.join(".git/worktrees/worktree"))
        ),
    )
    .unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        format!("{} is a link", alias.display())
    );
    fs::write(
        &pointer,
        format!("gitdir: {}\n", git_path(&fixture.registration)),
    )
    .unwrap();
    // The registration naming this worktree back through a linked directory.
    let worktree_alias = root.join("worktree-alias");
    symlink(&fixture.worktree, &worktree_alias).unwrap();
    fs::write(
        fixture.registration.join("gitdir"),
        format!("{}\n", git_path(&worktree_alias.join(".git"))),
    )
    .unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        "the worktree registration names another .git file"
    );
    fs::write(
        fixture.registration.join("gitdir"),
        format!("{}\n", git_path(&pointer)),
    )
    .unwrap();
    // A HEAD or configuration that is a link.
    let head = fixture.registration.join("HEAD");
    let real_head = root.join("real-head");
    fs::rename(&head, &real_head).unwrap();
    symlink(&real_head, &head).unwrap();
    assert!(
        main_worktree(&fixture.worktree)
            .unwrap_err()
            .to_string()
            .ends_with("is not a regular file")
    );
    fs::remove_file(&head).unwrap();
    fs::rename(&real_head, &head).unwrap();
    // A `.git` directory that is a link is no main working tree's own.
    let real = root.join("real-git");
    fs::rename(fixture.repository.join(".git"), &real).unwrap();
    symlink(&real, fixture.repository.join(".git")).unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        format!("{} is a link", fixture.repository.join(".git").display())
    );
}

// On Windows a junction is a link too: a registration reached through one, and a `.git`
// directory that is one, do not verify. Junctions need no privilege, unlike symbolic links.
// Runs on the Windows CI runner only.
#[cfg(windows)]
#[test]
fn a_junction_on_the_way_is_a_link() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let junction = |link: &Path, target: &Path| {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(native_key(link).unwrap())
            .arg(native_key(target).unwrap())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J");
    };
    let pointer = fixture.worktree.join(".git");
    let alias = root.join("alias");
    junction(&alias, &fixture.repository);
    fs::write(
        &pointer,
        format!(
            "gitdir: {}\n",
            git_path(&alias.join(".git/worktrees/worktree"))
        ),
    )
    .unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        format!("{} is a link", native_key(&alias).unwrap())
    );
    fs::write(
        &pointer,
        format!("gitdir: {}\n", git_path(&fixture.registration)),
    )
    .unwrap();
    assert!(main_worktree(&fixture.worktree).unwrap().is_some());
    let real = root.join("real-git");
    fs::rename(fixture.repository.join(".git"), &real).unwrap();
    junction(&fixture.repository.join(".git"), &real);
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        format!(
            "{} is a link",
            native_key(&fixture.repository.join(".git")).unwrap()
        )
    );
}
