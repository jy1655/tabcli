use super::super::store::native_key;
use super::*;
use std::cell::RefCell;
use std::process::Command;

// A path as Git spells it in its files and takes it as an argument: without Windows' verbatim
// prefix, and with forward slashes on Windows only. A backslash is an ordinary character in a
// Unix file name, so a Unix path is passed unchanged.
pub(in crate::native::consent) fn git_path(path: &Path) -> String {
    let path = native_key(path).unwrap();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path
    }
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
    let worktree = root.join(name);
    let mut arguments = vec![
        "-C".to_owned(),
        git_path(&repository),
        "worktree".to_owned(),
        "add".to_owned(),
        "--quiet".to_owned(),
        "--detach".to_owned(),
    ];
    if relative {
        arguments.push("--relative-paths".into());
    }
    arguments.push(git_path(&worktree));
    git.run(&arguments.iter().map(String::as_str).collect::<Vec<_>>());
    (repository, worktree.canonicalize().unwrap())
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
// to their own directory.
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
}

// The verifier with a screen that trusts every claimed main working tree.
fn verified(workspace: &Path) -> Result<Option<PathBuf>> {
    main_worktree(workspace, &mut |_| true)
}

fn error(workspace: &Path) -> String {
    format!("{:#}", verified(workspace).unwrap_err())
}

fn point(fixture: &Registered, target: &str) {
    fs::write(fixture.worktree.join(".git"), format!("gitdir: {target}\n")).unwrap();
}

#[test]
fn a_worktree_that_git_added_names_its_main_working_tree() {
    let (_temp, root) = canonical_root();
    let git = Git::new();
    let (repository, worktree) = git_worktree(&git, &root, "worktree", false);
    let pointer = fs::read_to_string(worktree.join(".git")).unwrap();
    let target = pointer.trim_end().strip_prefix("gitdir: ").unwrap();
    assert!(Path::new(target).is_absolute(), "{pointer}");
    assert_eq!(verified(&worktree).unwrap(), Some(repository.clone()));
    let (_, second) = git_worktree(&git, &root, "second", false);
    assert_eq!(verified(&second).unwrap(), Some(repository.clone()));
    // The main working tree, a directory inside a worktree and a plain directory are no
    // linked worktree: the exact rule alone applies to them.
    assert_eq!(verified(&repository).unwrap(), None);
    let child = worktree.join("child");
    fs::create_dir(&child).unwrap();
    assert_eq!(verified(&child).unwrap(), None);
    let plain = root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(verified(&plain).unwrap(), None);
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
        error(&second.canonicalize().unwrap()),
        "the worktree registration names a .git file in another folder"
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
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    // Relative, the registration must still name this worktree back.
    let elsewhere = root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join(".git"), "").unwrap();
    fs::write(
        fixture.registration.join("gitdir"),
        "../../../../elsewhere/.git\n",
    )
    .unwrap();
    assert_eq!(
        error(&fixture.worktree),
        "the worktree registration names a .git file in another folder"
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
    assert_eq!(verified(&worktree).unwrap(), Some(repository));
}

// Review rulings, round 3: the claimed main working tree is screened, lexically and before any
// I/O on it, against the providers' stores alone; a destination no store lists is never touched.
#[test]
fn the_claimed_main_working_tree_is_screened_before_any_io_on_it() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let asked = RefCell::new(Vec::new());
    let screen = |answer: bool| {
        let asked = &asked;
        move |main: &Path| {
            asked.borrow_mut().push(main.to_owned());
            answer
        }
    };
    // A destination that does not exist: refused by the screen, not by the file system.
    let unknown = root.join("unknown/repository");
    point(
        &fixture,
        &git_path(&unknown.join(".git/worktrees/worktree")),
    );
    assert_eq!(
        main_worktree(&fixture.worktree, &mut screen(false))
            .unwrap_err()
            .to_string(),
        "no provider's store lists the main working tree that the .git file names as trusted"
    );
    assert_eq!(asked.borrow().as_slice(), std::slice::from_ref(&unknown));
    // On Windows the screen is asked with the spelling `canonicalize` gives (`\\?\C:\...`, the
    // drive letter in upper case whatever case Git's file gives it), so the providers' stores
    // are keyed for it exactly as for an exact folder (Windows CI, 2026-11-26).
    #[cfg(windows)]
    {
        assert!(
            unknown.to_str().unwrap().starts_with(r"\\?\"),
            "{unknown:?}"
        );
        asked.borrow_mut().clear();
        let pointer = git_path(&unknown.join(".git/worktrees/worktree"));
        point(
            &fixture,
            &(pointer[..1].to_ascii_lowercase() + &pointer[1..]),
        );
        main_worktree(&fixture.worktree, &mut screen(false)).unwrap_err();
        assert_eq!(asked.borrow().as_slice(), std::slice::from_ref(&unknown));
        point(
            &fixture,
            &git_path(&unknown.join(".git/worktrees/worktree")),
        );
    }
    // Only past the screen is it touched.
    assert!(
        format!(
            "{:#}",
            main_worktree(&fixture.worktree, &mut screen(true)).unwrap_err()
        )
        .contains("cannot be read")
    );
    // The real registration, relative too: the screen is asked with the main working tree.
    asked.borrow_mut().clear();
    make_relative(&fixture);
    assert_eq!(
        main_worktree(&fixture.worktree, &mut screen(true)).unwrap(),
        Some(fixture.repository.clone())
    );
    assert_eq!(
        asked.borrow().as_slice(),
        std::slice::from_ref(&fixture.repository)
    );
    // A layout that is no registration of a main working tree is not even screened: a
    // submodule's gitdir and a bare repository's registration.
    asked.borrow_mut().clear();
    for target in [
        fixture.repository.join(".git/modules/worktree"),
        root.join("bare.git/worktrees/worktree"),
    ] {
        point(&fixture, &git_path(&target));
        assert_eq!(
            main_worktree(&fixture.worktree, &mut screen(true))
                .unwrap_err()
                .to_string(),
            "the gitdir is not a linked worktree registration of a main working tree"
        );
    }
    assert!(asked.borrow().is_empty());
}

// Review r3, finding 6: a pointer that names a component and later cancels it with `..` is
// refused lexically, before any I/O and before the screen, so the walk never touches a path
// outside both trees (an automount, say). Leading parent steps of a relative pointer stay
// accepted (`a_relative_path_registration_names_its_main_working_tree`), and so does the
// registration's own relative `commondir`. The pointers are spelled as text: on Windows
// `Path::join` onto the canonical (verbatim) root would remove `x/..` before the file is
// written (Windows CI, 2026-11-26).
#[test]
fn a_pointer_that_steps_back_over_a_name_is_refused_before_any_io() {
    const DETOUR: &str = "a path in Git's files steps back over a name it named";
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let screened = RefCell::new(0);
    let mut screen = |_: &Path| {
        *screened.borrow_mut() += 1;
        true
    };
    let spelled = |rest: &str| format!("{}/{rest}", git_path(&root));
    fs::write(root.join("file"), "").unwrap();
    for detour in [
        "missing/../repository/.git/worktrees/worktree",
        "file/../repository/.git/worktrees/worktree",
        "worktree/../repository/.git/worktrees/worktree",
        "repository/.git/worktrees/../worktrees/worktree",
    ] {
        point(&fixture, &spelled(detour));
        assert_eq!(
            main_worktree(&fixture.worktree, &mut screen)
                .unwrap_err()
                .to_string(),
            DETOUR,
            "{detour}"
        );
    }
    // The same for a relative pointer after its leading parent steps.
    point(&fixture, "../missing/../repository/.git/worktrees/worktree");
    assert_eq!(error(&fixture.worktree), DETOUR);
    assert_eq!(*screened.borrow(), 0);
    // A Git-written registration still verifies, absolute or relative, with the relative
    // `commondir` (`../..`) that Git writes into the registration.
    point(&fixture, &git_path(&fixture.registration));
    assert_eq!(
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    assert_eq!(
        fs::read_to_string(fixture.registration.join("commondir"))
            .unwrap()
            .trim_end(),
        "../.."
    );
    make_relative(&fixture);
    assert_eq!(
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    // A verbatim spelling is refused the same way, and a name in it that holds `/` (which a
    // verbatim path does not split at) is refused instead of being pushed as two names.
    #[cfg(windows)]
    {
        let verbatim = |rest: &str| format!(r"{}\{rest}", root.display());
        point(
            &fixture,
            &verbatim(r"missing\..\repository\.git\worktrees\worktree"),
        );
        assert_eq!(error(&fixture.worktree), DETOUR);
        point(
            &fixture,
            &verbatim(r"worktree/..\repository\.git\worktrees\worktree"),
        );
        assert_eq!(
            error(&fixture.worktree),
            "a name in a path in Git's files holds a separator"
        );
    }
}

#[test]
fn the_registration_must_name_this_worktree_and_its_common_directory_by_identity() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    // A line ending that Git on Windows can leave.
    fs::write(
        fixture.worktree.join(".git"),
        format!("gitdir: {}\r\n", git_path(&fixture.registration)),
    )
    .unwrap();
    assert_eq!(
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
    // Another worktree's registration: the forward pointer alone proves nothing.
    let other = root.join("other");
    fs::create_dir(&other).unwrap();
    fs::copy(fixture.worktree.join(".git"), other.join(".git")).unwrap();
    assert_eq!(
        error(&other.canonicalize().unwrap()),
        "the worktree registration names a .git file in another folder"
    );
    // The common directory named by the registration must be the main working tree's `.git`.
    let commondir = fixture.registration.join("commondir");
    let saved = fs::read(&commondir).unwrap();
    fs::write(&commondir, "..\n").unwrap();
    assert_eq!(
        error(&fixture.worktree),
        "the worktree registration names another common directory"
    );
    fs::write(
        &commondir,
        format!("{}\n", git_path(&fixture.repository.join(".git"))),
    )
    .unwrap();
    assert!(verified(&fixture.worktree).unwrap().is_some());
    fs::write(&commondir, &saved).unwrap();
    // A `.git` of the main working tree that points to a common directory of its own.
    fs::write(fixture.repository.join(".git/commondir"), "../other\n").unwrap();
    assert_eq!(
        error(&fixture.worktree),
        "the main working tree's .git names a common directory of its own"
    );
    fs::remove_file(fixture.repository.join(".git/commondir")).unwrap();
    // A missing back-pointer refuses.
    fs::remove_file(fixture.registration.join("gitdir")).unwrap();
    assert!(error(&fixture.worktree).contains("cannot be read"));
}

// Review r3, finding 7: the back-pointer must name `.git` in the assessed worktree directory. A
// folder whose `.git` is a hard link to the registered worktree's `.git` shares that file's
// identity but is not the folder Git registered.
#[test]
fn a_hard_linked_git_file_in_another_folder_is_not_the_registered_worktree() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let other = root.join("other");
    fs::create_dir(&other).unwrap();
    fs::hard_link(fixture.worktree.join(".git"), other.join(".git")).unwrap();
    assert_eq!(
        error(&other.canonicalize().unwrap()),
        "the worktree registration names a .git file in another folder"
    );
    assert_eq!(
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
}

// TAB does not read Git's configuration: a repository that Git itself would treat differently
// (here one whose `core.worktree` names another directory) is judged by its registration
// alone. Only a writer of its `.git` can register a worktree there either way.
#[test]
fn the_configuration_is_not_read() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    for config in [
        "[core]\n\tworktree = /elsewhere\n",
        "\u{feff}[core]\n\tbare = true\n",
        "[include]\n\tpath = other\n",
        "not a configuration at all",
    ] {
        fs::write(fixture.repository.join(".git/config"), config).unwrap();
        assert_eq!(
            verified(&fixture.worktree).unwrap(),
            Some(fixture.repository.clone()),
            "{config:?}"
        );
    }
    fs::remove_file(fixture.repository.join(".git/HEAD")).unwrap();
    assert_eq!(
        verified(&fixture.worktree).unwrap(),
        Some(fixture.repository.clone())
    );
}

#[test]
fn only_gits_pointer_form_is_read() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let pointer = fixture.worktree.join(".git");
    for (content, expected) in [
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
        fs::write(&pointer, &content).unwrap();
        assert_eq!(error(&fixture.worktree), expected, "{content:?}");
    }
}

// Review r3, finding 8: a pointer file is opened so that a FIFO put in its place after it was
// inspected cannot block the open, and the opened handle is then checked. The replacement is a
// race; the open itself is tested here on a FIFO.
#[cfg(unix)]
#[test]
fn a_fifo_in_place_of_a_pointer_file_does_not_block_its_open() {
    use std::os::unix::ffi::OsStrExt;
    let (_temp, root) = canonical_root();
    let fifo = root.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (sent, received) = std::sync::mpsc::channel();
    let path = fifo.clone();
    std::thread::spawn(move || {
        let _ = sent.send(open_pointer(&path).map(drop).map_err(|e| e.to_string()));
    });
    let outcome = received
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the open does not block on a FIFO");
    assert_eq!(
        outcome.unwrap_err(),
        format!("{} is not a regular file", fifo.display())
    );
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
    assert!(error(&fixture.worktree).ends_with("is not a regular file"));
    fs::remove_file(&pointer).unwrap();
    fs::rename(&moved, &pointer).unwrap();
    assert!(verified(&fixture.worktree).unwrap().is_some());
    // The registration reached through a linked directory.
    let alias = root.join("alias");
    symlink(&fixture.repository, &alias).unwrap();
    point(&fixture, &git_path(&alias.join(".git/worktrees/worktree")));
    assert_eq!(
        error(&fixture.worktree),
        format!("{} is a link", alias.display())
    );
    point(&fixture, &git_path(&fixture.registration));
    // The registration naming this worktree back through a linked directory: not a spelling of
    // this worktree's `.git`, so refused before the link is reached (review r4, finding 4).
    let worktree_alias = root.join("worktree-alias");
    symlink(&fixture.worktree, &worktree_alias).unwrap();
    fs::write(
        fixture.registration.join("gitdir"),
        format!("{}\n", git_path(&worktree_alias.join(".git"))),
    )
    .unwrap();
    assert_eq!(
        error(&fixture.worktree),
        "the worktree registration names a .git file in another folder"
    );
    fs::write(
        fixture.registration.join("gitdir"),
        format!("{}\n", git_path(&pointer)),
    )
    .unwrap();
    // A `.git` directory that is a link is no main working tree's own.
    let real = root.join("real-git");
    fs::rename(fixture.repository.join(".git"), &real).unwrap();
    symlink(&real, fixture.repository.join(".git")).unwrap();
    assert_eq!(
        error(&fixture.worktree),
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
    let alias = root.join("alias");
    junction(&alias, &fixture.repository);
    point(&fixture, &git_path(&alias.join(".git/worktrees/worktree")));
    assert!(error(&fixture.worktree).ends_with("alias is a link"));
    point(&fixture, &git_path(&fixture.registration));
    assert!(verified(&fixture.worktree).unwrap().is_some());
    let real = root.join("real-git");
    fs::rename(fixture.repository.join(".git"), &real).unwrap();
    junction(&fixture.repository.join(".git"), &real);
    assert!(error(&fixture.worktree).ends_with(".git is a link"));
}

// On Windows the claimed main working tree on another drive or share is screened like any
// other: refused unless a store lists it, before any I/O on it, so a local workspace
// cannot make TAB reach an untrusted network share. Runs on the Windows CI runner only.
#[cfg(windows)]
#[test]
fn a_share_without_consent_evidence_is_never_touched() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    point(
        &fixture,
        "//tab-test.invalid/share/repository/.git/worktrees/worktree",
    );
    let started = std::time::Instant::now();
    let mut asked = Vec::new();
    let error = main_worktree(&fixture.worktree, &mut |main| {
        asked.push(main.to_owned());
        false
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no provider's store lists the main working tree that the .git file names as trusted"
    );
    // Asked with the share spelled as `canonicalize` spells it, whose key is `\\server\share\...`
    // as for an exact folder there.
    assert_eq!(
        asked,
        [PathBuf::from(r"\\?\UNC\tab-test.invalid\share\repository")]
    );
    assert_eq!(
        native_key(&asked[0]).unwrap(),
        r"\\tab-test.invalid\share\repository"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

// Review r4, finding 4: the back-pointer is resolved lexically first and must designate `.git`
// in the assessed workspace before anything on its path is walked. A stale registration that
// names a former worktree in a third tree is refused before any I/O there (that tree does not
// even exist here: a walk would report it unreadable), absolute or relative.
#[test]
fn a_back_pointer_into_a_third_tree_is_refused_before_any_io_there() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let gitdir = fixture.registration.join("gitdir");
    for back in [
        format!("{}\n", git_path(&root.join("third/former/.git"))),
        "../../../../third/former/.git\n".to_owned(),
    ] {
        fs::write(&gitdir, &back).unwrap();
        assert_eq!(
            error(&fixture.worktree),
            "the worktree registration names a .git file in another folder",
            "{back}"
        );
    }
    // Git's own spellings of this worktree's `.git` still verify.
    for back in [
        format!("{}\n", git_path(&fixture.worktree.join(".git"))),
        "../../../../worktree/.git\n".to_owned(),
    ] {
        fs::write(&gitdir, &back).unwrap();
        assert_eq!(
            verified(&fixture.worktree).unwrap(),
            Some(fixture.repository.clone()),
            "{back}"
        );
    }
}
