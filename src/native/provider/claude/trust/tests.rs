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
    // The workspace's own .git is skipped, including a nested repository.
    std::fs::create_dir(workspace.join(".git")).unwrap();
    require_exact_scope_with(&workspace, &mut git).unwrap();
    assert_eq!(git.calls, 0);
    std::fs::create_dir(root.join(".git")).unwrap();
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
