# Release publication procedure

The default path is to configure `IMMUTABLE_RELEASES_READ_TOKEN`, run a non-publishing rehearsal
with the previous tag, push a new annotated tag, and publish through the Release workflow.
The secret has only fine-grained `Administration: read` permission for that repository.
Build and publication use the workflow's `GITHUB_TOKEN`, not the read-only secret.

If you cannot provide the read-only secret to Actions, the manual procedure below is also an
official alternative. It documents the main CI artifact path used for 0.0.6 and 0.0.7;
it does not remove the workflow's immutable preflight or turn it into a warning. A release
maintainer with actual publication authority runs it, and must stop if local authentication
cannot read the setting either.

## Preparation and pinning the candidate

1. Merge the release changes into main and record the exact commit SHA. The Cargo version,
   CI archive names and binary version checks, the Install sections in `README.md` and `README.ko.md`, and
   `docs/releases/<version>.md` must agree. The
   `readme_install_section_names_the_cargo_version` test in `tests/native_policy.rs` checks
   both README Install sections against the Cargo version.
   `tests/fixtures/help.txt` uses a version token and needs no per-release version edit.
   Run `python scripts/integrations.py generate` after a version bump and commit the
   generated host packages and catalogs. Run `python scripts/integrations.py check` and
   `python -m unittest discover -s scripts -p 'test_*.py' -v` (Python 3.11+). Each package's
   release version must match Cargo; the minimum compatible CLI version is independent.
   Validate changes with `cargo test --all-targets --all-features -- --test-threads=1`,
   Clippy, fmt, and `git diff --check`. If you changed the Release workflow or packaging,
   first pass a rehearsal with `gh workflow run release.yml --ref <branch> -f tag=<existing-tag>`.
   The rehearsal builds the existing tag's source; it does not replace verification of the
   new candidate binary. Its separate `integration-rehearsal` job also validates and builds
   the candidate branch's integration packages, without uploading them into the old tag's
   asset set. Tags predating integrations retain four assets. The workflow reads the Cargo
   package name from the validated commit
   and allows only `tabcli` or `agent-bridge`. For tags before 0.2.0, it puts only the executable
   and `LICENSE` in an `agent-bridge-<version>-<target>` archive and checks
   `agent-bridge <version>`. For `tabcli`, validation fails if `THIRD_PARTY_NOTICES.md` is absent.
2. Confirm that every CI job succeeded for that **main push commit**. Do not substitute a PR
   merge ref, another commit, a failed run, or a local rebuild.
   In `gh run view <run-id> --json headSha,headBranch,event,conclusion,jobs`, `headSha` must be
   the recorded commit, with `headBranch=main`, `event=push`, and `conclusion=success`.
   Record the CI run ID and artifact names, IDs, and digests too.
3. Have a version of `gh` that supports `gh release verify --help` and `gh release verify-asset --help`.
   Local authentication must have permission to read the repository's immutable setting and
   publish drafts/assets. Do not put the token itself in logs or documents.

The following examples use Bash. Replace `<...>` values with the actual values for this release,
and review each check's result before proceeding. Do not use commands that overwrite existing
tags or published files.

```sh
release_repo=jy1655/tabcli
release_tag='v<version>'
release_commit='<validated-main-commit>'
release_run='<successful-main-CI-run-id>'
release_dir='<new-empty-evidence-directory>'

gh api "repos/$release_repo/immutable-releases" \
  -H 'X-GitHub-Api-Version: 2026-03-10' --jq '.enabled'
gh run view "$release_run" -R "$release_repo" \
  --json headSha,headBranch,event,conclusion,jobs
gh api "repos/$release_repo/actions/runs/$release_run/artifacts" \
  --jq '.artifacts[] | {id,name,digest,expired,workflow_run}'
gh run download "$release_run" -R "$release_repo" \
  -n release-candidate-aarch64-apple-darwin -D "$release_dir/macos"
gh run download "$release_run" -R "$release_repo" \
  -n release-candidate-x86_64-pc-windows-msvc -D "$release_dir/windows"
gh run download "$release_run" -R "$release_repo" \
  -n release-candidate-integrations -D "$release_dir/integrations"
```

The integrations download applies to source commits containing `scripts/integrations.py`.
For those releases, require the `release-candidate-integrations` artifact from the same CI
commit as both binaries. From that exact source checkout run
`python scripts/integrations.py verify --output <release-dir>/integrations`. It checks the
four `agent-bridge-<host>-<version>.zip` files (`claude`, `codex`, `agy`, `pi`) and their
four checksums, including every bundled file against source. Do not substitute locally
regenerated files for the recorded CI artifact. These are platform-independent instruction
packages and contain no `tabcli` binary. The twelve-asset set is the two CLI archives and
their two checksums plus these eight files; older source commits have only the four CLI files.

The immutable query must return `true`. Artifacts must not be expired. Each must contain only
its archive and the corresponding `.sha256`. Verify checksums in both directories with
`shasum -a 256 -c <archive>.sha256` (or `Get-FileHash -Algorithm SHA256` on Windows).
From 0.2.0 onward, archive names are `tabcli-<version>-aarch64-apple-darwin.tar.gz` and
`tabcli-<version>-x86_64-pc-windows-msvc.zip`.
Check the contents with `tar -tzf <archive>` and `unzip -Z1 <archive>`.
The tar contents must be exactly `tabcli`, `LICENSE`, and `THIRD_PARTY_NOTICES.md`; the zip
contents must be exactly `tabcli.exe`, `LICENSE`, and `THIRD_PARTY_NOTICES.md`.
Check archive paths and file types before extracting into a new directory.
Compare LICENSE and THIRD_PARTY_NOTICES.md with the commit. Each platform's binary
`--version` must match `tabcli <version>` and the output from that CI job.
Authenticated runtime results are separate; state explicitly if they were omitted.

## Tag and draft verification

1. When creating a new tag, create an annotated tag on the validated commit and push it. If the
   tag already exists, do not move or recreate it. A tag push starts the automatic Release;
   confirm that its run stopped before publication before proceeding with the manual path.
   Do not create a draft concurrently with automatic publication. If publication has already
   happened, verify the published files only instead of following this procedure.
2. Use the GitHub API to record `.object.type=tag` and the tag object SHA from `git/ref/tags/<tag>`.
   Check that `.tag` in `git/tags/<tag-object-sha>` is the requested tag, `.object.type=commit`,
   and `.object.sha` is the validated main commit. Also check that the fetched main contains
   the commit. Do not rely only on a local tag ref or the Release's `target_commitish`.
3. Recheck that the immutable setting is still `enabled=true` and the remote tag object is
   unchanged. Read the version and release notes from the validated commit. Upload exactly
   twelve files (four for source commits predating integrations) to a new draft. If a draft
   exists, first check its creator, commit, and files;
   do not overwrite it automatically.

```sh
gh api "repos/$release_repo/git/ref/tags/$release_tag"
gh api "repos/$release_repo/git/tags/<validated-tag-object-sha>"
gh release create "$release_tag" -R "$release_repo" --verify-tag --draft \
  --title "$release_tag" --notes-file '<notes-from-validated-commit>' \
  "$release_dir/macos/tabcli-<version>-aarch64-apple-darwin.tar.gz" \
  "$release_dir/macos/tabcli-<version>-aarch64-apple-darwin.tar.gz.sha256" \
  "$release_dir/windows/tabcli-<version>-x86_64-pc-windows-msvc.zip" \
  "$release_dir/windows/tabcli-<version>-x86_64-pc-windows-msvc.zip.sha256"
```

For a source commit with integrations, include the eight previously verified files from
`"$release_dir"/integrations/*` in that same `gh release create` command. The command above
shows the historical four-asset CLI set. Never publish an integration-capable source with
only that subset.

The draft's REST API asset list must match the complete twelve-file set (or historical
four-file set) exactly by name. Compare each
`digest=sha256:<local-hash>` and `size` with the local file. Download every draft asset into
**a new directory** and also check that its bytes match the original CI artifact.
You can use `gh release download "$release_tag" -R "$release_repo" -D <draft-readback-dir>`.
If anything differs, do not publish; investigate the cause.

## Publication and final verification

1. Immediately before publication, recheck the immutable setting, tag object, and target commit.
   Confirm that the draft ID and all assets' names, digests, and sizes remain as validated.
2. Publish with `gh release edit "$release_tag" -R "$release_repo" --draft=false --latest`.
3. Check `draft=false`, `prerelease=false`, `immutable=true`, and the intended latest status
   through the REST API. Recheck that the remote tag object and target commit are unchanged,
   download every published file into a new directory, and compare its bytes with the CI artifact.
4. Verify the release attestation and **every checksum file as well as every archive**.

```sh
gh release verify "$release_tag" -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-checksum> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-checksum> -R "$release_repo"
```

Also run `gh release verify-asset` on each of the eight downloaded integration files when
present, and rerun the integration verifier on their readback directory. Record all twelve
digests. Installing/updating a host integration is a separate operation from publishing these
assets; it must not update the engine, change consent, or remove a binary used by an open session.

Do not record failed verification as success. Checks after publication cannot undo publication,
and rechecking the tag does not lock out concurrent changes by another writer. Do not recover by
replacing or deleting assets of a published immutable release or moving its tag. Record the defect
and the scope of verification, then prepare a fixed version. In the final record, distinguish the
source commit, tag object, CI run, all file digests, verification results before and after
publication, attestation results, and the scope of installation and runtime verification.

## Local build cleanup

Finish every release by cleaning the local checkout's build output. Do this after publication
verification, any requested installation, and all local checks: a later build, test or Clippy
run that targets this directory recreates its build artifacts. GitHub Actions runs on separate
machines and cannot clean the maintainer's local checkout.

1. Keep release evidence, logs and handwritten probes outside `target/`. Move any such files
   already there to an evidence directory before cleaning: a full Cargo clean removes the
   entire directory, including files Cargo did not create.
2. Confirm that `target/` is this checkout's own directory, not a symlink or shared build
   directory. Wait for builds and tests to finish. Check that no live managed session depends
   on an executable under it, including provider hooks that will invoke that path later: a
   completed request and the session manifest, which records the provider executable, do not
   prove that. For each relevant state root, list the sessions that may still be live with
   `tabcli status --all --all-workspaces` and `inspect`, including the current session, and
   read their launch executable (`launch.sh` on macOS, the process command line on Windows),
   their provider hook configuration such as `claude-settings.json`, and
   `AGENT_BRIDGE_EXECUTABLE`, which Pi invokes again at settlement. Finish task-owned sessions
   first. If another session still needs the binary, or the dependency cannot be established,
   defer cleanup and report that session and path, or the missing evidence, instead of closing
   the session or deleting its executable.
3. From the repository root, measure the directory and run:

   ```sh
   du -sh ./target
   cargo --config 'build.build-dir="./target"' clean --target-dir ./target
   ```

   The explicit directory selects `./target` for the final artifacts and the `--config` value
   selects it for the intermediate ones, so a shared `CARGO_TARGET_DIR` and a separately
   configured `build.build-dir` are left untouched: a full clean deletes both directories when
   they differ. Use a full clean, without `--release` or `--package`, to remove every profile
   and target triple within this checkout's verified `target/`, including debug, incremental
   and cross-target build caches as well as release builds. Leave installed binaries, session
   records, other worktrees and global Cargo caches intact.
4. Confirm that the build output was removed and report the reclaimed space in the release
   handoff. The next local build will rebuild its dependencies.

References: [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases),
[release attestation verification](https://cli.github.com/manual/gh_release_verify),
[asset attestation verification](https://cli.github.com/manual/gh_release_verify-asset).

When changing dependencies, run `AGENT_BRIDGE_UPDATE_NOTICES=1 cargo test --test third_party_notices third_party_notices_match_lock -- --exact` from the repository root to update `THIRD_PARTY_NOTICES.md`. Update mode uses the license texts of crates found by `cargo metadata --locked` and can download the required registry sources. For a new license expression or a missing license text, update mode stops rather than choosing a substitute. Review the crate and update the generator. It also preserves `COPYRIGHT`, `AUTHORS`, and `NOTICE` included in crates, and reads the MIT text for `r-efi` from `AUTHORS`. Update mode is refused when `CI` is set. The normal test checks package and version agreement between `Cargo.lock` and the notices file without the network or registry cache.
