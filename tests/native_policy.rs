use agent_bridge::{
    FirstPartyCli, cli_version_is_supported, confirm_explicit_close, provider_adapter,
    provider_launch_args, provider_model_args, supported_clis, terminal_safe_text,
    validate_terminal_input,
};

#[test]
fn claude_message_guard_control_fails_closed_without_managed_state() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["native-provider-control", "claude", "message-guard"])
        .env_remove("AGENT_BRIDGE_NATIVE_SESSION_DIR")
        .env_remove("AGENT_BRIDGE_NATIVE_STATE_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success());
    let decision: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn claude_message_receipt_control_records_nothing_without_managed_state() {
    let state = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args([
            "native-provider-control",
            "claude",
            "message-receipt",
            "claude-turn-safe123",
        ])
        .env_remove("AGENT_BRIDGE_NATIVE_SESSION_DIR")
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", state.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    // A PostToolUse hook cannot undo a send; failing leaves the delivery unconfirmed.
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

#[test]
fn supported_cli_versions_use_a_minimum_not_an_exact_pin() {
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.147.0").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.148.3").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Codex, "codex-cli 0.146.9").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.1.234 (Claude Code)").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Claude, "2.2.0 (Claude Code)").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Claude, "2.1.233 (Claude Code)").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Agy, "agy 1.1.12").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Agy, "agy 1.2.0").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Agy, "agy 1.1.11").unwrap());

    assert!(cli_version_is_supported(FirstPartyCli::Pi, "0.84.1").unwrap());
    assert!(cli_version_is_supported(FirstPartyCli::Pi, "pi 0.85.0").unwrap());
    assert!(!cli_version_is_supported(FirstPartyCli::Pi, "pi 0.84.0").unwrap());
}

#[test]
fn yolo_is_forwarded_only_when_the_new_session_explicitly_requests_it() {
    assert!(provider_launch_args(FirstPartyCli::Codex, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Claude, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Agy, false).is_empty());
    assert!(provider_launch_args(FirstPartyCli::Pi, false).is_empty());

    assert_eq!(
        provider_launch_args(FirstPartyCli::Codex, true),
        ["--dangerously-bypass-approvals-and-sandbox"]
    );
    assert_eq!(
        provider_launch_args(FirstPartyCli::Claude, true),
        ["--dangerously-skip-permissions"]
    );
    assert_eq!(
        provider_launch_args(FirstPartyCli::Agy, true),
        ["--dangerously-skip-permissions"]
    );
    assert_eq!(provider_launch_args(FirstPartyCli::Pi, true), ["--approve"]);
}

#[test]
fn close_requires_an_explicit_confirmation_flag() {
    assert!(confirm_explicit_close(false).is_err());
    assert!(confirm_explicit_close(true).is_ok());
}

#[test]
fn the_native_bridge_accepts_each_supported_visible_cli() {
    assert_eq!("codex".parse(), Ok(FirstPartyCli::Codex));
    assert_eq!("claude".parse(), Ok(FirstPartyCli::Claude));
    assert_eq!("agy".parse(), Ok(FirstPartyCli::Agy));
    assert_eq!("pi".parse(), Ok(FirstPartyCli::Pi));
    assert!("unknown".parse::<FirstPartyCli>().is_err());
}

#[test]
fn supported_clis_are_backed_by_one_complete_adapter_registry() {
    assert_eq!(
        supported_clis(),
        &[
            FirstPartyCli::Codex,
            FirstPartyCli::Claude,
            FirstPartyCli::Agy,
            FirstPartyCli::Pi,
        ]
    );

    for &cli in supported_clis() {
        let adapter = provider_adapter(cli);
        assert_eq!(adapter.cli(), cli);
        assert_eq!(adapter.command(), cli.as_str());
        assert_eq!(adapter.minimum_version(), cli.minimum_version());
    }
}

#[test]
fn each_provider_adapter_owns_its_native_effort_policy() {
    assert_eq!(
        provider_adapter(FirstPartyCli::Codex)
            .effort_args("xhigh")
            .unwrap(),
        ["-c", "model_reasoning_effort=\"xhigh\""]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Claude)
            .effort_args("max")
            .unwrap(),
        ["--effort", "max"]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Agy)
            .effort_args("high")
            .unwrap(),
        ["--effort", "high"]
    );
    assert_eq!(
        provider_adapter(FirstPartyCli::Pi)
            .effort_args("minimal")
            .unwrap(),
        ["--thinking", "minimal"]
    );
}

#[test]
fn codex_normalizes_only_pi_qualified_openai_codex_models() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "openai-codex/gpt-5.6-sol"),
        ["--model", "gpt-5.6-sol"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "gpt-5.6-sol"),
        ["--model", "gpt-5.6-sol"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Codex, "openai-codex/"),
        ["--model", "openai-codex/"]
    );

    for provider in [FirstPartyCli::Claude, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(
            provider_model_args(provider, "openai-codex/gpt-5.6-sol"),
            ["--model", "openai-codex/gpt-5.6-sol"]
        );
    }
}

#[test]
fn claude_normalizes_only_the_observed_fable5_model_alias() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "Fable5"),
        ["--model", "Fable"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "Fable"),
        ["--model", "Fable"]
    );
    assert_eq!(
        provider_model_args(FirstPartyCli::Claude, "fable5"),
        ["--model", "fable5"]
    );

    for provider in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(
            provider_model_args(provider, "Fable5"),
            ["--model", "Fable5"]
        );
    }
}

#[test]
fn pi_qualifies_only_the_exact_fable_model_alias() {
    assert_eq!(
        provider_model_args(FirstPartyCli::Pi, "Fable"),
        ["--model", "anthropic/claude-fable-5"]
    );
    for model in [
        "fable",
        "Fable5",
        "anthropic/claude-fable-5",
        "provider/model",
    ] {
        assert_eq!(
            provider_model_args(FirstPartyCli::Pi, model),
            ["--model", model]
        );
    }
}

#[test]
fn terminal_input_rejects_submission_and_escape_controls() {
    assert!(validate_terminal_input("line one\nline two\tindented", "prompt").is_ok());
    for control in ['\0', '\r', '\u{1b}', '\u{7f}'] {
        assert!(
            validate_terminal_input(&format!("before{control}after"), "prompt").is_err(),
            "accepted terminal control U+{:04X}",
            u32::from(control)
        );
    }
}

#[test]
fn terminal_output_renders_controls_as_visible_text() {
    let escaped = terminal_safe_text("ok\u{1b}]52;clipboard\u{7}\nnext", true);

    assert_eq!(escaped, "ok\\u{1b}]52;clipboard\\u{7}\nnext");
    assert!(!escaped.contains('\u{1b}'));
    assert!(!escaped.contains('\u{7}'));
}

#[test]
fn release_publication_requires_repository_immutable_releases() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let guard = workflow
        .find("- name: Require immutable releases before publication")
        .expect("release workflow has no immutable-release preflight");
    let publish = workflow
        .find("- name: Publish verified release assets")
        .expect("release workflow has no publication step");
    let guard_step = &workflow[guard..publish];

    assert!(
        guard < publish,
        "immutable-release preflight runs after publish"
    );
    assert!(guard_step.contains("repos/${GH_REPO}/immutable-releases"));
    assert!(guard_step.contains(".enabled == true"));
    assert!(guard_step.contains("GH_TOKEN: ${{ secrets.IMMUTABLE_RELEASES_READ_TOKEN }}"));
    assert!(!guard_step.contains("GH_TOKEN: ${{ github.token }}"));
}

fn release_workflow_job<'a>(workflow: &'a str, job: &str, next_job: &str) -> &'a str {
    let start = workflow
        .find(&format!("\n  {job}:\n"))
        .unwrap_or_else(|| panic!("release workflow has no {job} job"));
    let end = workflow
        .find(&format!("\n  {next_job}:\n"))
        .unwrap_or_else(|| panic!("release workflow has no {next_job} job"));
    &workflow[start..end]
}

#[test]
fn release_validation_checks_the_annotated_tag_on_the_remote() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let validate = release_workflow_job(&workflow, "validate", "test");

    // actions/checkout rewrites the local tag ref to the commit, so only the remote still
    // holds the tag object, and the check must not depend on a second fetch.
    assert!(validate.contains("persist-credentials: false"));
    assert!(validate.contains("git/ref/tags/${RELEASE_TAG}"));
    assert!(validate.contains("git/tags/${ref_sha}"));
    assert!(validate.contains("is not an annotated tag"));
    assert!(!validate.contains("git cat-file"));
    assert!(!validate.contains("git fetch"));
    assert!(
        validate.contains("git merge-base --is-ancestor \"$tag_commit\" refs/remotes/origin/main")
    );
}

#[test]
fn release_jobs_build_and_publish_the_validated_commit() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");

    // A tag name can be moved while the run is in progress; only validation resolves it.
    assert_eq!(
        workflow
            .matches("ref: refs/tags/${{ env.RELEASE_TAG }}")
            .count(),
        1
    );
    for (job, next_job) in [("test", "build"), ("build", "publish")] {
        assert!(
            release_workflow_job(&workflow, job, next_job)
                .contains("ref: ${{ needs.validate.outputs.commit }}"),
            "{job} does not check out the validated commit"
        );
    }
    let publish = &workflow[workflow.find("\n  publish:\n").unwrap()..];
    assert!(publish.contains("ref: ${{ needs.validate.outputs.commit }}"));
    assert_eq!(
        publish
            .matches("VALIDATED_TAG_OBJECT: ${{ needs.validate.outputs.tag_object }}")
            .count(),
        2
    );
}

#[test]
fn release_stops_before_building_when_immutable_releases_cannot_be_confirmed() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let validate = release_workflow_job(&workflow, "validate", "test");
    let gate = validate
        .find("- name: Require immutable releases before building")
        .expect("release validation has no early immutable-release gate");
    let gate_step = &validate[gate..];

    assert!(gate_step.contains("repos/${GH_REPO}/immutable-releases"));
    assert!(gate_step.contains(".enabled == true"));
    assert!(gate_step.contains("GH_TOKEN: ${{ secrets.IMMUTABLE_RELEASES_READ_TOKEN }}"));
    assert!(!gate_step.contains("GH_TOKEN: ${{ github.token }}"));
    assert!(
        release_workflow_job(&workflow, "test", "build").contains("\n    needs: validate\n"),
        "tests must not spend runners before the gate has passed"
    );
    assert!(
        release_workflow_job(&workflow, "build", "publish").contains("needs: [validate, test]"),
        "artifacts must not be built before the gate has passed"
    );
}

#[test]
fn release_rehearsal_never_publishes() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    assert!(workflow.contains("\n  workflow_dispatch:\n"));

    let release_steps = workflow
        .split("\n      - ")
        .filter(|step| step.contains("gh release create") || step.contains("gh release edit"))
        .collect::<Vec<_>>();
    assert_eq!(release_steps.len(), 2);
    for step in release_steps {
        assert!(
            step.contains("\n        if: github.event_name == 'push'\n"),
            "a rehearsal could reach: {step}"
        );
    }

    // Only a rehearsal may continue without the immutable-release token.
    for tolerated in workflow.match_indices("exit 0") {
        let before = &workflow[..tolerated.0];
        let condition = before
            .rfind("if [ \"$GITHUB_EVENT_NAME\" = \"workflow_dispatch\" ]; then")
            .expect("a missing token is tolerated outside a rehearsal");
        assert!(!before[condition..].contains("\n            fi\n"));
    }
}

#[test]
fn release_artifact_job_is_isolated_from_mutable_terminal_app_installs() {
    let workflow = include_str!("../.github/workflows/release.yml").replace("\r\n", "\n");
    let build = workflow
        .find("\n  build:\n")
        .expect("release workflow has no build job");
    let publish = workflow
        .find("\n  publish:\n")
        .expect("release workflow has no publish job");
    let build_job = &workflow[build..publish];

    assert!(
        build_job.contains("needs: [validate, test]"),
        "artifact build must wait for the isolated release test job"
    );
    assert!(
        !build_job.contains("brew install --cask"),
        "mutable terminal app installers must not run in the artifact-producing job"
    );
}

// The release scripts are executed exactly as the workflow holds them, against a throwaway
// repository and stand-ins for `gh` and `sha256sum`.
#[cfg(unix)]
mod release_workflow_behavior {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
    const TAG_OBJECT: &str = "1111111111111111111111111111111111111111";
    const VALIDATED_TAG: &str = "tag\t1111111111111111111111111111111111111111";
    const MOVED_TAG: &str = "tag\t2222222222222222222222222222222222222222";
    const LIGHTWEIGHT_TAG: &str = "commit\t1111111111111111111111111111111111111111";

    fn step_script(marker: &str) -> String {
        let step = &WORKFLOW[WORKFLOW
            .find(marker)
            .unwrap_or_else(|| panic!("release workflow has no step {marker}"))..];
        let run = "        run: |\n";
        let body = &step[step.find(run).expect("step has no script") + run.len()..];
        let script = body
            .lines()
            .take_while(|line| line.is_empty() || line.starts_with("          "))
            .map(|line| line.strip_prefix("          ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !script.contains("${{"),
            "workflow expressions must reach release scripts through env"
        );
        script
    }

    fn git(repository: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "release test")
            .env("GIT_AUTHOR_EMAIL", "release@example.invalid")
            .env("GIT_COMMITTER_NAME", "release test")
            .env("GIT_COMMITTER_EMAIL", "release@example.invalid")
            .output()
            .expect("git is required for the release workflow tests");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        repository: PathBuf,
        bin: PathBuf,
        log: PathBuf,
        main_commit: String,
        side_commit: String,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let bin = root.path().join("bin");
        fs::create_dir_all(repository.join("docs/releases")).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(
            repository.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        fs::write(repository.join("docs/releases/1.2.3.md"), "notes\n").unwrap();
        git(&repository, &["init", "-q"]);
        git(&repository, &["add", "-A"]);
        git(&repository, &["commit", "-q", "-m", "main"]);
        let main_commit = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(&repository, &["checkout", "-q", "--detach"]);
        fs::write(repository.join("outside-main"), "x\n").unwrap();
        git(&repository, &["add", "-A"]);
        git(&repository, &["commit", "-q", "-m", "outside main"]);
        let side_commit = git(&repository, &["rev-parse", "HEAD"]);
        git(&repository, &["checkout", "-q", "--detach", &main_commit]);

        // `gh` answers with what the workflow's --jq expressions would print.
        let gh = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_LOG"
case "$1 $2" in
  "api "*/git/ref/tags/*) [ -n "$FAKE_REF" ] || exit 1; printf '%s\n' "$FAKE_REF" ;;
  "api "*/git/tags/*) [ -n "$FAKE_TAG" ] || exit 1; printf '%s\n' "$FAKE_TAG" ;;
  "release view") case "$*" in
      *isImmutable*) printf '%s\n' "$FAKE_PUBLISHED" ;;
      *) printf '%s\n' "$FAKE_ASSETS" ;;
    esac ;;
  "release create"|"release edit") ;;
  *) exit 1 ;;
esac
"#;
        let sha256sum = "#!/bin/sh\nprintf 'feedface  %s\\n' \"$1\"\n";
        for (name, body) in [("gh", gh), ("sha256sum", sha256sum)] {
            fs::write(bin.join(name), body).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        Fixture {
            log: root.path().join("gh.log"),
            _root: root,
            repository,
            bin,
            main_commit,
            side_commit,
        }
    }

    impl Fixture {
        fn run(&self, script: &str, environment: &[(&str, &str)]) -> Output {
            let _ = fs::remove_file(&self.log);
            let mut command = Command::new("bash");
            command
                .arg("-c")
                .arg(script)
                .current_dir(&self.repository)
                .env(
                    "PATH",
                    format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
                )
                .env("FAKE_GH_LOG", &self.log)
                .env("GH_REPO", "owner/repository")
                .env("RELEASE_TAG", "v1.2.3")
                .env("GITHUB_OUTPUT", self.repository.join("github-output"));
            for (name, value) in environment {
                command.env(name, value);
            }
            command
                .output()
                .expect("bash is required for the release workflow tests")
        }

        fn gh_calls(&self) -> String {
            fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn validation_accepts_only_the_checked_out_annotated_tag_in_main() {
        let fixture = fixture();
        let script = step_script("      - id: version\n");
        let main_tag = format!("v1.2.3\tcommit\t{}", fixture.main_commit);
        let side_tag = format!("v1.2.3\tcommit\t{}", fixture.side_commit);
        let output_file = fixture.repository.join("github-output");

        for event in ["push", "workflow_dispatch"] {
            let _ = fs::remove_file(&output_file);
            let accepted = fixture.run(
                &script,
                &[
                    ("FAKE_REF", VALIDATED_TAG),
                    ("FAKE_TAG", &main_tag),
                    ("GITHUB_EVENT_NAME", event),
                    ("GITHUB_SHA", &fixture.main_commit),
                ],
            );
            assert!(accepted.status.success(), "{event}: {}", stdout(&accepted));
            assert_eq!(
                fs::read_to_string(&output_file).unwrap(),
                format!(
                    "version=1.2.3\nnotes=docs/releases/1.2.3.md\ncommit={}\ntag_object={TAG_OBJECT}\n",
                    fixture.main_commit
                )
            );
        }

        let other_name = format!("v1.2.4\tcommit\t{}", fixture.main_commit);
        let tree_target = format!("v1.2.3\ttree\t{}", fixture.main_commit);
        let rejected = [
            (
                "a lightweight tag",
                vec![("FAKE_REF", LIGHTWEIGHT_TAG), ("FAKE_TAG", &main_tag)],
                "is not an annotated tag",
            ),
            (
                "a tag that is not on the remote",
                vec![("FAKE_REF", ""), ("FAKE_TAG", &main_tag)],
                "does not exist on the remote",
            ),
            (
                "a tag object with another name",
                vec![("FAKE_REF", VALIDATED_TAG), ("FAKE_TAG", &other_name)],
                "tag object names",
            ),
            (
                "a tag that does not point at a commit",
                vec![("FAKE_REF", VALIDATED_TAG), ("FAKE_TAG", &tree_target)],
                "points at a tree",
            ),
            (
                "a remote tag on another commit than the checkout",
                vec![("FAKE_REF", VALIDATED_TAG), ("FAKE_TAG", &side_tag)],
                "checked-out commit is not the commit",
            ),
            (
                "a push of another commit",
                vec![
                    ("FAKE_REF", VALIDATED_TAG),
                    ("FAKE_TAG", &main_tag),
                    ("GITHUB_SHA", &fixture.side_commit),
                ],
                "pushed ref is not the commit",
            ),
            (
                "a version other than Cargo's",
                vec![
                    ("FAKE_REF", VALIDATED_TAG),
                    ("FAKE_TAG", &main_tag),
                    ("RELEASE_TAG", "v1.2.4"),
                ],
                "does not match Cargo version",
            ),
            (
                "a malformed tag",
                vec![
                    ("FAKE_REF", VALIDATED_TAG),
                    ("FAKE_TAG", &main_tag),
                    ("RELEASE_TAG", "v1.2.3; echo injected"),
                ],
                "is not vMAJOR.MINOR.PATCH",
            ),
        ];
        for (case, overrides, reason) in rejected {
            let mut environment = vec![
                ("GITHUB_EVENT_NAME", "push"),
                ("GITHUB_SHA", fixture.main_commit.as_str()),
            ];
            environment.extend(overrides);
            let output = fixture.run(&script, &environment);
            assert!(!output.status.success(), "accepted {case}");
            assert!(
                stdout(&output).contains(reason),
                "{case}: {}",
                stdout(&output)
            );
        }

        let accepted_environment = [
            ("FAKE_REF", VALIDATED_TAG),
            ("FAKE_TAG", main_tag.as_str()),
            ("GITHUB_EVENT_NAME", "push"),
            ("GITHUB_SHA", fixture.main_commit.as_str()),
        ];
        let notes = fixture.repository.join("docs/releases/1.2.3.md");
        fs::remove_file(&notes).unwrap();
        let without_notes = fixture.run(&script, &accepted_environment);
        assert!(!without_notes.status.success());
        assert!(stdout(&without_notes).contains("is missing"));
        fs::write(&notes, "notes\n").unwrap();

        // The commit of the tag must be contained in main.
        git(
            &fixture.repository,
            &["checkout", "-q", "--detach", &fixture.side_commit],
        );
        let outside = fixture.run(
            &script,
            &[
                ("FAKE_REF", VALIDATED_TAG),
                ("FAKE_TAG", &side_tag),
                ("GITHUB_EVENT_NAME", "push"),
                ("GITHUB_SHA", &fixture.side_commit),
            ],
        );
        assert!(!outside.status.success());
        assert!(stdout(&outside).contains("is not contained in main"));
    }

    #[test]
    fn a_moved_tag_is_never_given_a_release() {
        let fixture = fixture();
        let script = step_script("      - name: Publish verified release assets\n");
        fs::create_dir(fixture.repository.join("dist")).unwrap();
        fs::write(fixture.repository.join("dist/asset.tar.gz"), "asset").unwrap();
        let run = |remote_tag: &str| {
            fixture.run(
                &script,
                &[
                    ("NOTES_FILE", "docs/releases/1.2.3.md"),
                    ("VALIDATED_TAG_OBJECT", TAG_OBJECT),
                    ("FAKE_REF", remote_tag),
                ],
            )
        };

        for changed in [MOVED_TAG, LIGHTWEIGHT_TAG, ""] {
            let output = run(changed);
            assert!(
                !output.status.success(),
                "created a release for {changed:?}"
            );
            assert!(!fixture.gh_calls().contains("release create"));
        }
        assert!(stdout(&run(MOVED_TAG)).contains("changed after validation"));

        let unchanged = run(VALIDATED_TAG);
        assert!(unchanged.status.success(), "{}", stdout(&unchanged));
        assert!(fixture.gh_calls().contains(
            "release create v1.2.3 dist/asset.tar.gz --verify-tag --generate-notes --notes-file docs/releases/1.2.3.md --title v1.2.3 --draft"
        ));
    }

    #[test]
    fn publication_needs_matching_assets_the_validated_tag_and_immutability() {
        let fixture = fixture();
        let script = step_script("      - name: Verify draft assets and publish\n");
        fs::create_dir(fixture.repository.join("dist")).unwrap();
        for asset in ["a.tar.gz", "a.tar.gz.sha256"] {
            fs::write(fixture.repository.join("dist").join(asset), asset).unwrap();
        }
        let assets = "a.tar.gz sha256:feedface\na.tar.gz.sha256 sha256:feedface";
        let run = |assets: &str, remote_tag: &str, published: &str| {
            fixture.run(
                &script,
                &[
                    ("VALIDATED_TAG_OBJECT", TAG_OBJECT),
                    ("FAKE_ASSETS", assets),
                    ("FAKE_REF", remote_tag),
                    ("FAKE_PUBLISHED", published),
                ],
            )
        };

        let tampered = run(
            "a.tar.gz sha256:0badf00d\na.tar.gz.sha256 sha256:feedface",
            VALIDATED_TAG,
            "false\ttrue",
        );
        assert!(!tampered.status.success());
        assert!(stdout(&tampered).contains("draft assets differ"));
        assert!(!fixture.gh_calls().contains("release edit"));

        let moved = run(assets, MOVED_TAG, "false\ttrue");
        assert!(!moved.status.success());
        assert!(stdout(&moved).contains("changed after validation"));
        assert!(!fixture.gh_calls().contains("release edit"));

        let mutable = run(assets, VALIDATED_TAG, "false\tfalse");
        assert!(!mutable.status.success());
        assert!(stdout(&mutable).contains("is not published as immutable"));

        let published = run(assets, VALIDATED_TAG, "false\ttrue");
        assert!(published.status.success(), "{}", stdout(&published));
        let calls = fixture.gh_calls();
        let mut position = 0;
        for call in [
            "release view v1.2.3 --json assets",
            "git/ref/tags/v1.2.3",
            "release edit v1.2.3 --draft=false",
            "isDraft,isImmutable",
            "git/ref/tags/v1.2.3",
        ] {
            position += calls[position..]
                .find(call)
                .unwrap_or_else(|| panic!("{call} is missing or out of order in:\n{calls}"))
                + call.len();
        }
    }
}
