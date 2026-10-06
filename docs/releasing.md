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
   CI archive names and binary version checks, and `docs/releases/<version>.md` must agree.
   Validate changes with `cargo test --all-targets --all-features -- --test-threads=1`,
   Clippy, fmt, and `git diff --check`. If you changed the Release workflow or packaging,
   first pass a rehearsal with `gh workflow run release.yml --ref <branch> -f tag=<existing-tag>`.
   The rehearsal builds the existing tag's source; it does not replace verification of the
   new candidate binary. The workflow reads the Cargo package name from the validated commit
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
```

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
   four files to a new draft. If a draft exists, first check its creator, commit, and files;
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

The draft's REST API asset list must match the four files exactly by name. Compare each
`digest=sha256:<local-hash>` and `size` with the local file. Download every draft asset into
**a new directory** and also check that its bytes match the original CI artifact.
You can use `gh release download "$release_tag" -R "$release_repo" -D <draft-readback-dir>`.
If anything differs, do not publish; investigate the cause.

## Publication and final verification

1. Immediately before publication, recheck the immutable setting, tag object, and target commit.
   Confirm that the draft ID and the four assets' names, digests, and sizes remain as validated.
2. Publish with `gh release edit "$release_tag" -R "$release_repo" --draft=false --latest`.
3. Check `draft=false`, `prerelease=false`, `immutable=true`, and the intended latest status
   through the REST API. Recheck that the remote tag object and target commit are unchanged,
   download the four published files into a new directory, and compare their bytes with the CI artifact.
4. Verify the release attestation and **both checksum files as well as both archives**.

```sh
gh release verify "$release_tag" -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-checksum> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-checksum> -R "$release_repo"
```

Do not record failed verification as success. Checks after publication cannot undo publication,
and rechecking the tag does not lock out concurrent changes by another writer. Do not recover by
replacing or deleting assets of a published immutable release or moving its tag. Record the defect
and the scope of verification, then prepare a fixed version. In the final record, distinguish the
source commit, tag object, CI run, four file digests, verification results before and after
publication, attestation results, and the scope of installation and runtime verification.

References: [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases),
[release attestation verification](https://cli.github.com/manual/gh_release_verify),
[asset attestation verification](https://cli.github.com/manual/gh_release_verify-asset).

When changing dependencies, run `AGENT_BRIDGE_UPDATE_NOTICES=1 cargo test --test third_party_notices third_party_notices_match_lock -- --exact` from the repository root to update `THIRD_PARTY_NOTICES.md`. Update mode uses the license texts of crates found by `cargo metadata --locked` and can download the required registry sources. For a new license expression or a missing license text, update mode stops rather than choosing a substitute. Review the crate and update the generator. It also preserves `COPYRIGHT`, `AUTHORS`, and `NOTICE` included in crates, and reads the MIT text for `r-efi` from `AUTHORS`. Update mode is refused when `CI` is set. The normal test checks package and version agreement between `Cargo.lock` and the notices file without the network or registry cache.
