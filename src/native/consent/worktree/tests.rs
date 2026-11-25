use super::super::store::native_key;
use super::*;
use std::process::Command;

// A path as Git spells it in its files and takes it as an argument: without Windows' verbatim
// prefix, and with forward slashes there.
pub(in crate::native::consent) fn git_path(path: &Path) -> String {
    native_key(path).unwrap().replace('\\', "/")
}

// Git itself, as a user runs it, with an identity for the one commit a worktree needs.
pub(in crate::native::consent) fn git(arguments: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=TAB",
            "-c",
            "user.email=tab@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
}

// A repository with one commit at `root/repository`, and a linked worktree that Git added at
// `root/<name>`; both canonical.
pub(in crate::native::consent) fn git_worktree(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let repository = root.join("repository");
    if !repository.exists() {
        git(&["init", "--quiet", &git_path(&repository)]);
        let repository = git_path(&repository);
        git(&[
            "-C",
            &repository,
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "init",
        ]);
    }
    let worktree = git_path(root) + "/" + name;
    git(&[
        "-C",
        &git_path(&repository),
        "worktree",
        "add",
        "--quiet",
        "--detach",
        &worktree,
    ]);
    (
        repository.canonicalize().unwrap(),
        Path::new(&worktree).canonicalize().unwrap(),
    )
}

fn canonical_root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    (temp, root)
}

// The registration Git writes for `git worktree add <root>/worktree`, written by hand so that
// each refusal below changes one fact of it.
pub(in crate::native::consent) struct Registered {
    pub(in crate::native::consent) repository: PathBuf,
    pub(in crate::native::consent) worktree: PathBuf,
    pub(in crate::native::consent) registration: PathBuf,
}

pub(in crate::native::consent) fn registered(root: &Path) -> Registered {
    let repository = root.join("repository");
    let registration = repository.join(".git/worktrees/worktree");
    fs::create_dir_all(&registration).unwrap();
    fs::write(
        repository.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n",
    )
    .unwrap();
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
    Registered {
        repository: repository.canonicalize().unwrap(),
        worktree: worktree.canonicalize().unwrap(),
        registration: registration.canonicalize().unwrap(),
    }
}

#[test]
fn a_worktree_that_git_added_names_its_main_working_tree() {
    let (_temp, root) = canonical_root();
    let (repository, worktree) = git_worktree(&root, "worktree");
    assert_eq!(main_worktree(&worktree).unwrap(), Some(repository.clone()));
    let (_, second) = git_worktree(&root, "second");
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
    git(&[
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
    fs::write(&gitdir, "worktree/.git\n").unwrap();
    assert!(
        main_worktree(&fixture.worktree)
            .unwrap_err()
            .to_string()
            .contains("by a relative path")
    );
}

#[test]
fn only_gits_own_registration_layout_verifies() {
    let (_temp, root) = canonical_root();
    let fixture = registered(&root);
    let pointer = fixture.worktree.join(".git");
    let point = |target: &str| fs::write(&pointer, target).unwrap();
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
    ] {
        point(&content);
        assert_eq!(
            main_worktree(&fixture.worktree).unwrap_err().to_string(),
            error,
            "{content:?}"
        );
    }
    // Git follows a relative pointer when a repository asks for relative paths; TAB does not.
    point("gitdir: ../repository/.git/worktrees/worktree\n");
    assert!(
        main_worktree(&fixture.worktree)
            .unwrap_err()
            .to_string()
            .contains("by a relative path")
    );
    // A submodule's gitdir is not a linked worktree registration.
    let module = fixture.repository.join(".git/modules/worktree");
    fs::create_dir_all(&module).unwrap();
    point(&format!("gitdir: {}\n", git_path(&module)));
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        "the gitdir is not a linked worktree registration"
    );
    point(&format!("gitdir: {}\n", git_path(&fixture.registration)));
    assert!(main_worktree(&fixture.worktree).is_ok());
    // A common directory named otherwise than in Git's own registration.
    fs::write(fixture.registration.join("commondir"), "..\n").unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        "the worktree registration does not name its common directory as Git does"
    );
}

#[test]
fn a_bare_repository_or_a_moved_main_working_tree_has_no_main_working_tree_here() {
    let (_temp, root) = canonical_root();
    // A bare repository's own worktrees: the common directory is not a `.git` directory.
    let bare = root.join("bare.git");
    let registration = bare.join("worktrees/worktree");
    fs::create_dir_all(&registration).unwrap();
    fs::write(bare.join("config"), "[core]\n\tbare = true\n").unwrap();
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
    assert_eq!(
        main_worktree(&worktree).unwrap_err().to_string(),
        "the common directory is not the .git directory of a main working tree"
    );

    let named = root.join("named");
    fs::create_dir(&named).unwrap();
    let fixture = registered(&named);
    let config = fixture.repository.join(".git/config");
    let verdict = |text: &str| {
        fs::write(&config, text).unwrap();
        main_worktree(&fixture.worktree).map_err(|error| format!("{error:#}"))
    };
    for text in [
        "[core]\n\tbare = false\n",
        "[Core]\n\tBare=No ; a comment\n",
        "[core] bare = off\n",
        "[core]\n\tbare = \"0\"\n",
        "[remote \"origin\"]\n\turl = https://example.invalid/x\n\tbare = true\n",
        "[core \"x\"]\n\tbare = true\n",
        "# [core] bare = true\n",
        "",
    ] {
        assert_eq!(
            verdict(text),
            Ok(Some(fixture.repository.clone())),
            "{text:?}"
        );
    }
    const BARE: &str = "repository config: the repository is bare or its core.bare cannot be read";
    for (text, error) in [
        ("[core]\n\tbare = true\n", BARE),
        ("[core]\n\tbare\n", BARE),
        ("[core]\n\tbare = 2\n", BARE),
        (
            "[core]\n\tworktree = /elsewhere\n",
            "repository config: core.worktree moves the main working tree",
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
            "repository config: a configuration line continues on the next",
        ),
        (
            "[core\n",
            "repository config: unterminated configuration section",
        ),
    ] {
        assert_eq!(verdict(text), Err(error.to_owned()), "{text:?}");
    }
    // The main working tree's own configuration under `extensions.worktreeConfig` counts too.
    verdict("[core]\n\tbare = false\n").unwrap();
    fs::write(
        fixture.repository.join(".git/config.worktree"),
        "[core]\n\tworktree = /elsewhere\n",
    )
    .unwrap();
    assert_eq!(
        verdict("[core]\n\tbare = false\n"),
        Err("repository config.worktree: core.worktree moves the main working tree".to_owned())
    );
    // A repository without a configuration is not one Git made.
    fs::remove_file(fixture.repository.join(".git/config.worktree")).unwrap();
    fs::remove_file(&config).unwrap();
    assert!(main_worktree(&fixture.worktree).is_err());
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
        format!("{} is a link", worktree_alias.display())
    );
    // A `.git` directory that is a link is no main working tree's own.
    fs::write(
        fixture.registration.join("gitdir"),
        format!("{}\n", git_path(&pointer)),
    )
    .unwrap();
    let real = root.join("real-git");
    fs::rename(fixture.repository.join(".git"), &real).unwrap();
    symlink(&real, fixture.repository.join(".git")).unwrap();
    assert_eq!(
        main_worktree(&fixture.worktree).unwrap_err().to_string(),
        format!("{} is a link", fixture.repository.join(".git").display())
    );
}
