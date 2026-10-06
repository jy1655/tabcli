# 릴리스 게시 절차

기본 경로는 `IMMUTABLE_RELEASES_READ_TOKEN` 설정 → 직전 tag의 비게시 리허설 →
새 annotated tag push → Release workflow 게시다. secret은 해당 저장소의
fine-grained `Administration: read` 권한만 갖는다. build·publication은 workflow의
`GITHUB_TOKEN`을 쓰며 조회용 secret으로 수행하지 않는다.

Actions에 조회용 secret을 제공할 수 없는 경우 아래 수동 절차도 공식 대안이다.
0.0.6·0.0.7에서 사용한 main CI artifact 경로를 문서화한 것으로, workflow의
immutable preflight를 삭제하거나 경고로 바꾸는 절차가 아니다. 실제 게시 권한을 가진
릴리스 관리자가 수행하며, 로컬 인증으로 설정을 확인할 수 없는 경우에도 중단한다.

## 준비와 후보 고정

1. 릴리스 변경을 main에 통합하고 정확한 commit SHA를 기록한다. Cargo version,
   CI archive 이름·binary version 검사, `docs/releases/<version>.md`가 일치해야 한다.
   변경 검증은 `cargo test --all-targets --all-features -- --test-threads=1`,
   Clippy·fmt·`git diff --check`를 사용한다. Release workflow 또는 packaging을 바꿨으면
   `gh workflow run release.yml --ref <branch> -f tag=<existing-tag>`로 리허설을 먼저
   통과시킨다. 리허설은 기존 tag의 source를 빌드하므로 새 후보 binary의 검증을 대신하지 않는다.
2. 해당 **main push commit**의 CI가 모든 job에서 성공했는지 확인한다. PR merge ref,
   다른 commit, 실패한 run 또는 로컬 재빌드 결과를 대체품으로 쓰지 않는다.
   `gh run view <run-id> --json headSha,headBranch,event,conclusion,jobs`의 `headSha`가
   기록한 commit이고 `headBranch=main`, `event=push`, `conclusion=success`여야 한다.
   CI run ID와 artifact 이름·ID·digest도 기록한다.
3. `gh release verify --help`와 `gh release verify-asset --help`를 지원하는 `gh`를 준비한다.
   로컬 인증은 repository immutable 설정 조회와 draft/asset 게시 권한을 가져야 한다.
   token 자체를 로그·문서에 남기지 않는다.

다음은 Bash 명령 예시다. `<...>` 값은 이번 릴리스의 실제 값으로 바꾸고, 각 확인 결과를
검토한 후 다음 단계로 진행한다. 기존 tag나 게시 파일을 덮어쓰는 명령은 사용하지 않는다.

```sh
release_repo=jy1655/agent-bridge
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

immutable 조회 결과는 `true`여야 한다. artifact는 만료되지 않아야 하며, 각 archive와
그 `.sha256`만 있어야 한다. 두 디렉터리에서 `shasum -a 256 -c <archive>.sha256`
(Windows에서는 `Get-FileHash -Algorithm SHA256`)으로 checksum을 검증한다.
tar 구성은 `agent-bridge`, `LICENSE`, zip 구성은 `agent-bridge.exe`, `LICENSE`와
정확히 일치해야 한다. archive의 경로·파일 종류를 확인한 뒤 새 디렉터리에 해제한다.
LICENSE는 해당 commit과 대조하고, 플랫폼별 binary의 `--version`은 해당 CI job의
실행 결과와 일치해야 한다. authenticated runtime 결과는 별도이며 생략했으면 명시한다.

## Tag와 draft 검증

1. 새 tag를 만들 때는 검증한 commit에 annotated tag를 만들고 push한다. 이미 tag가
   있으면 이동하거나 다시 만들지 않는다. tag push가 자동 Release를 시작하므로, 그
   run이 게시 전 중단됐음을 확인한 뒤 수동 경로를 진행한다. 자동 게시가 진행 중이면
   동시에 draft를 만들지 않는다. 이미 게시됐으면 이 절차 대신 게시 파일 검증만 수행한다.
2. GitHub API로 `git/ref/tags/<tag>`의 `.object.type=tag`와 tag object SHA를 기록한다.
   `git/tags/<tag-object-sha>`의 `.tag`가 요청 tag, `.object.type=commit`, `.object.sha`가
   검증한 main commit인지 확인한다. fetch한 main에 commit이 포함되는지도 확인한다.
   로컬 tag ref나 Release의 `target_commitish`만으로 판정하지 않는다.
3. immutable 설정이 여전히 `enabled=true`이고 원격 tag object가 같은지 재확인한다.
   version·릴리스 노트는 검증한 commit에서 읽는다. 새로운 draft에 정확히 네 파일을
   올린다. 기존 draft가 있으면 만든 주체·commit·파일을 먼저 확인하며 자동 덮어쓰지 않는다.

```sh
gh api "repos/$release_repo/git/ref/tags/$release_tag"
gh api "repos/$release_repo/git/tags/<validated-tag-object-sha>"
gh release create "$release_tag" -R "$release_repo" --verify-tag --draft \
  --title "$release_tag" --notes-file '<notes-from-validated-commit>' \
  "$release_dir/macos/agent-bridge-<version>-aarch64-apple-darwin.tar.gz" \
  "$release_dir/macos/agent-bridge-<version>-aarch64-apple-darwin.tar.gz.sha256" \
  "$release_dir/windows/agent-bridge-<version>-x86_64-pc-windows-msvc.zip" \
  "$release_dir/windows/agent-bridge-<version>-x86_64-pc-windows-msvc.zip.sha256"
```

draft의 REST API asset 목록은 이름 기준으로 네 파일과 정확히 일치해야 한다. 각
`digest=sha256:<local-hash>`와 `size`를 로컬 파일과 대조한다. draft의 모든 asset을
**새 디렉터리**에 내려받아 원본 CI artifact와 bytes가 같은지도 확인한다.
`gh release download "$release_tag" -R "$release_repo" -D <draft-readback-dir>`를
사용할 수 있다. 차이가 하나라도 있으면 게시하지 않고 원인을 조사한다.

## 게시와 최종 검증

1. 게시 직전 immutable 설정과 tag object·target commit을 다시 확인한다. draft ID와
   네 asset의 이름·digest·size가 검증한 그대로인지 확인한다.
2. `gh release edit "$release_tag" -R "$release_repo" --draft=false --latest`로 게시한다.
3. REST API에서 `draft=false`, `prerelease=false`, `immutable=true`, 의도한 latest 상태를
   확인한다. 원격 tag object·target commit이 같은지 다시 확인하고 게시 파일 네 개를
   새 디렉터리로 내려받아 CI artifact와 bytes를 대조한다.
4. release attestation과 **archive 두 개뿐 아니라 checksum 파일 두 개도** 검증한다.

```sh
gh release verify "$release_tag" -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-macos-checksum> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-archive> -R "$release_repo"
gh release verify-asset "$release_tag" <downloaded-windows-checksum> -R "$release_repo"
```

검증 실패를 성공으로 기록하지 않는다. 게시 후 확인은 이미 일어난 게시를 되돌릴 수 없고,
각 tag 재확인은 다른 작성자의 동시 변경을 잠그지 않는다. 게시된 immutable release의
asset 교체·삭제나 tag 이동으로 복구하지 말고, 결함과 확인 범위를 기록한 뒤 수정 버전을
준비한다. 최종 기록에는 source commit, tag object, CI run, 네 파일 digest, 게시 전후
검증 결과, attestation 결과, 설치·runtime 검증 범위를 구분해 남긴다.

참고: [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases),
[release attestation 검증](https://cli.github.com/manual/gh_release_verify),
[asset attestation 검증](https://cli.github.com/manual/gh_release_verify-asset).

의존성을 변경하면 저장소 루트에서 `AGENT_BRIDGE_UPDATE_NOTICES=1 cargo test --test third_party_notices third_party_notices_match_lock -- --exact`를 실행해 `THIRD_PARTY_NOTICES.md`를 갱신한다. 갱신 모드는 `cargo metadata --locked`로 찾은 crate의 라이선스 원문을 사용하며 필요한 registry source를 내려받을 수 있다. 새 라이선스 표현이나 누락된 원문은 자동 대체하지 않고 검토 후 생성기를 갱신한다. crate에 포함된 `COPYRIGHT`, `AUTHORS`, `NOTICE`도 함께 보존하며 `r-efi`의 MIT 원문은 `AUTHORS`에서 읽는다. `CI`가 설정된 환경에서는 갱신 모드를 거부한다. 일반 테스트는 네트워크나 registry cache 없이 `Cargo.lock`과 고지 파일의 패키지·버전 일치를 검사한다.
