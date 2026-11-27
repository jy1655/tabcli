use super::*;

struct FakeGitScope {
    calls: usize,
    answer: Option<Result<std::process::Output>>,
}

impl GitScope for FakeGitScope {
    fn output(&mut self, _: &Path) -> Result<std::process::Output> {
        self.calls += 1;
        self.answer.take().expect("unexpected Git invocation")
    }
}

fn output(root: &Path, success: bool) -> std::process::Output {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout: format!("{}\r\n", root.display()).into_bytes(),
        stderr: Vec::new(),
    }
}

#[test]
fn exact_scope_preserves_no_spawn_matching_refusal_and_runner_errors() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut git = FakeGitScope {
        calls: 0,
        answer: None,
    };
    require_exact_scope_with(&workspace, &mut git).unwrap();
    std::fs::create_dir(workspace.join(".git")).unwrap();
    require_exact_scope_with(&workspace, &mut git).unwrap();
    assert_eq!(git.calls, 0);
    std::fs::create_dir(root.join(".git")).unwrap();
    require_exact_scope_with(&workspace, &mut git).unwrap();
    assert_eq!(git.calls, 0);
    std::fs::remove_dir(workspace.join(".git")).unwrap();
    git.answer = Some(Ok(output(&workspace, true)));
    require_exact_scope_with(&workspace, &mut git).unwrap();
    assert_eq!(git.calls, 1);
    git.answer = Some(Ok(output(&root, true)));
    assert_eq!(
        require_exact_scope_with(&workspace, &mut git)
            .unwrap_err()
            .to_string(),
        "Claude would approve the parent Git repository, not this exact workspace; use its own trust dialog"
    );
    git.answer = Some(Ok(output(&workspace, false)));
    assert_eq!(
        require_exact_scope_with(&workspace, &mut git)
            .unwrap_err()
            .to_string(),
        "cannot verify Claude's Git workspace trust scope; use its own trust dialog"
    );
    for reason in [
        "Git runner failed",
        "Claude workspace trust scope timed out",
    ] {
        git.answer = Some(Err(anyhow::anyhow!(reason)));
        assert_eq!(
            require_exact_scope_with(&workspace, &mut git)
                .unwrap_err()
                .to_string(),
            reason
        );
    }
    assert_eq!(git.calls, 5);
}

fn nested_workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    (directory, workspace)
}

// Only the injected Git runner can be called. Its result is independent of the file's contents.
fn write_git_file(workspace: &Path) {
    let pointer = if cfg!(windows) {
        r"gitdir: \\localhost\unused\registration"
    } else {
        "gitdir: unused/registration"
    };
    std::fs::write(workspace.join(".git"), pointer).unwrap();
}

#[test]
fn absent_claude_entry_skips_git_with_or_without_a_workspace_git_file() {
    let (directory, workspace) = nested_workspace();
    let env = consent::Environment::at_home(directory.path());
    crate::native::session::primitives::write_json_atomic(
        &trust_store(&env).unwrap(),
        &serde_json::json!({"projects": {}}),
    )
    .unwrap();
    for own_git in [true, false] {
        if own_git {
            write_git_file(&workspace);
        } else {
            std::fs::remove_file(workspace.join(".git")).unwrap();
        }
        let mut git = FakeGitScope {
            calls: 0,
            answer: Some(Ok(output(&workspace, true))),
        };
        assert_eq!(
            workspace_trust_with(&workspace, &env, None, &mut git).unwrap(),
            Trust::Absent
        );
        assert_eq!(git.calls, 0, "own .git file: {own_git}");
    }
}

#[test]
fn listed_claude_workspace_with_its_own_git_file_uses_its_exact_entry_without_git() {
    let (directory, workspace) = nested_workspace();
    write_git_file(&workspace);
    let env = consent::Environment::at_home(directory.path());
    let store = trust_store(&env).unwrap();
    let native = consent::native_key(&workspace).unwrap();
    let key = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        native
    };
    crate::native::session::primitives::write_json_atomic(
        &store,
        &serde_json::json!({"projects": {&key: {"hasTrustDialogAccepted": true}}}),
    )
    .unwrap();
    let mut git = FakeGitScope {
        calls: 0,
        answer: Some(Ok(output(&workspace, true))),
    };
    let found = workspace_trust_with(&workspace, &env, None, &mut git).unwrap();
    assert_eq!(git.calls, 0);
    assert_eq!(
        found,
        Trust::Trusted(Evidence {
            provider: "claude".into(),
            store,
            key
        })
    );
}
