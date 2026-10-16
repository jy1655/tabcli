use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    base: PathBuf,
    // The Codex home that the injected Environment names through `CODEX_HOME`.
    codex_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let codex_home = base.join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        Self {
            _directory: directory,
            base,
            codex_home,
        }
    }

    fn memories(&self) -> PathBuf {
        self.codex_home.join("memories")
    }

    fn environment(&self) -> Environment {
        environment_at(&self.codex_home)
    }

    fn directory(&self, path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        path.canonicalize().unwrap()
    }
}

fn write_setting(root: &Path, value: &str) {
    fs::create_dir_all(root).unwrap();
    write_json_atomic(
        &root.join("settings.json"),
        &serde_json::json!({ "schema": 1, "codex_memory_workspace": value }),
    )
    .unwrap();
}

// A caller whose `CODEX_HOME` is the given directory; its user home is the directory's
// parent, which the trust lookup requires but the Codex home does not use.
fn environment_at(codex_home: &Path) -> Environment {
    Environment {
        home: Some(codex_home.parent().unwrap().as_os_str().to_owned()),
        codex_home: Some(codex_home.as_os_str().to_owned()),
        ..Default::default()
    }
}

fn expected_refusal(workspace: &Path, memory: &Path) -> String {
    format!(
        "launch refused: workspace {} is within the Codex memory workspace {}; codex-memory-workspace is refuse. To allow launches here, run 'tabcli settings codex-memory-workspace allow' in the same state root.",
        workspace.display(),
        memory.display()
    )
}

#[test]
fn the_memory_workspace_and_every_directory_inside_it_are_refused() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    for workspace in [memory.clone(), fixture.directory(&memory.join("a/b"))] {
        let error = check_with(CodexMemoryWorkspace::Refuse, &workspace, &memory).unwrap_err();
        assert_eq!(error.to_string(), expected_refusal(&workspace, &memory));
    }
}

#[test]
fn a_sibling_with_the_same_prefix_and_unrelated_directories_are_admitted() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    for workspace in [
        fixture.directory(&fixture.codex_home.join("memories-other")),
        fixture.directory(&fixture.codex_home.join("memoriesx/inner")),
        fixture.codex_home.clone(),
        fixture.base.clone(),
        fixture.directory(&fixture.base.join("project")),
    ] {
        check_with(CodexMemoryWorkspace::Refuse, &workspace, &memory).unwrap();
    }
}

#[test]
fn allow_admits_without_resolving_the_codex_home() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    check_with(CodexMemoryWorkspace::Allow, &memory, &memory).unwrap();
    // Even a protected path that would be an error under refuse is not read.
    let file = fixture.base.join("file");
    fs::write(&file, b"").unwrap();
    check_with(CodexMemoryWorkspace::Allow, &memory, &file).unwrap();

    let root = fixture.base.join("state");
    write_setting(&root, "allow");
    check_from(&root, &memory, || {
        panic!("allow must not read the Codex home")
    })
    .unwrap();
}

#[test]
fn the_state_root_setting_decides_and_a_damaged_setting_is_an_error() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    // A missing state root and a missing record mean the default, refuse.
    let root = fixture.base.join("state");
    let error = check_from(&root, &memory, || fixture.environment()).unwrap_err();
    assert_eq!(error.to_string(), expected_refusal(&memory, &memory));
    assert!(!root.exists(), "admission must not create the state root");

    write_setting(&root, "refuse");
    assert!(check_from(&root, &memory, || fixture.environment()).is_err());

    write_setting(&root, "sometimes");
    let error = check_from(&root, &memory, || panic!("settings are read first")).unwrap_err();
    assert!(
        format!("{error:#}").contains("invalid settings record"),
        "{error:#}"
    );
}

#[test]
fn a_missing_memory_workspace_protects_nothing_and_a_missing_home_is_an_error() {
    let fixture = Fixture::new();
    let workspace = fixture.directory(&fixture.base.join("project"));
    check_from(&fixture.base.join("state"), &workspace, || {
        fixture.environment()
    })
    .unwrap();
    check_isolated_from(&workspace, || environment_at(&fixture.base.join("absent"))).unwrap();
    // Without HOME and USERPROFILE the Codex home cannot be resolved, even with CODEX_HOME.
    let error = check_isolated_from(&workspace, || Environment {
        codex_home: Some(fixture.codex_home.as_os_str().to_owned()),
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "cannot locate the Codex memory workspace: no user home for workspace trust lookup"
    );
}

#[test]
fn a_protected_path_that_is_not_a_directory_is_an_error() {
    let fixture = Fixture::new();
    fs::write(fixture.memories(), b"not a directory").unwrap();
    let workspace = fixture.directory(&fixture.base.join("project"));
    let error = check_with(
        CodexMemoryWorkspace::Refuse,
        &workspace,
        &fixture.memories(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("is not a directory"),
        "{error:#}"
    );
}

#[test]
fn the_isolated_check_refuses_by_default_and_says_how_to_permit_the_workspace() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    let error = check_isolated_from(&memory, || fixture.environment()).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "{} Isolated self-test uses default settings; use ordinary self-test with the allow setting to permit this workspace.",
            expected_refusal(&memory, &memory)
        )
    );
}

#[cfg(unix)]
#[test]
fn aliases_resolve_before_comparison_and_an_escaping_link_is_admitted() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    let inside = fixture.directory(&memory.join("inside"));
    // A workspace alias outside the memory workspace that resolves into it is refused.
    let alias = fixture.base.join("alias");
    symlink(&inside, &alias).unwrap();
    let workspace = alias.canonicalize().unwrap();
    let error = check_with(CodexMemoryWorkspace::Refuse, &workspace, &memory).unwrap_err();
    assert_eq!(error.to_string(), expected_refusal(&inside, &memory));
    // A Codex home reached through a link names the same memory workspace.
    let linked_home = fixture.base.join("linked-home");
    symlink(&fixture.codex_home, &linked_home).unwrap();
    let error = check_isolated_from(&inside, || environment_at(&linked_home)).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with(&expected_refusal(&inside, &memory))
    );
    // A link inside the memory workspace that resolves outside it is not within it.
    let outside = fixture.directory(&fixture.base.join("outside"));
    symlink(&outside, memory.join("escape")).unwrap();
    let workspace = memory.join("escape").canonicalize().unwrap();
    check_with(CodexMemoryWorkspace::Refuse, &workspace, &memory).unwrap();
}

#[cfg(unix)]
#[test]
fn only_a_missing_memory_workspace_admits_a_failed_resolution() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let workspace = fixture.directory(&fixture.base.join("project"));
    // A link loop cannot be resolved; it is not evidence that nothing is protected.
    symlink(fixture.memories(), fixture.memories()).unwrap();
    let error = check_with(
        CodexMemoryWorkspace::Refuse,
        &workspace,
        &fixture.memories(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").starts_with("cannot resolve the Codex memory workspace"),
        "{error:#}"
    );
    // A dangling link is a missing memory workspace.
    let dangling = fixture.base.join("dangling-home");
    fs::create_dir(&dangling).unwrap();
    symlink(fixture.base.join("absent"), dangling.join("memories")).unwrap();
    check_with(
        CodexMemoryWorkspace::Refuse,
        &workspace,
        &dangling.join("memories"),
    )
    .unwrap();
    // A Codex home that cannot be searched hides the memory workspace: an error.
    let closed = fixture.base.join("closed-home");
    fs::create_dir_all(closed.join("memories")).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let result = check_with(
        CodexMemoryWorkspace::Refuse,
        &workspace,
        &closed.join("memories"),
    );
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();
    assert!(
        format!("{error:#}").starts_with("cannot resolve the Codex memory workspace"),
        "{error:#}"
    );
}

#[cfg(windows)]
#[test]
fn windows_paths_compare_after_canonical_resolution() {
    let fixture = Fixture::new();
    let memory = fixture.directory(&fixture.memories());
    let workspace = fixture.directory(&memory.join("Inner"));
    // The protected path as a caller may spell it: other letter case, no verbatim prefix.
    let spelled = PathBuf::from(
        fixture
            .memories()
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_uppercase(),
    );
    let error = check_with(CodexMemoryWorkspace::Refuse, &workspace, &spelled).unwrap_err();
    assert_eq!(error.to_string(), expected_refusal(&workspace, &memory));
    let sibling = fixture.directory(&fixture.codex_home.join("memories-other"));
    check_with(CodexMemoryWorkspace::Refuse, &sibling, &spelled).unwrap();
}
