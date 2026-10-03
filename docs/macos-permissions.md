# macOS 초기 승인과 실앱 검증

Claude를 처음 실행할 때의 폴더 신뢰·도구 권한, macOS 권한, Codex/ChatGPT의 도구 접근 정책은 서로 다른 승인입니다. 하나를 승인했다고 나머지가 허용되지는 않습니다. 특히 `Computer Use is not allowed to use the app '…' for safety reasons.`는 Claude 로그인이나 workspace 신뢰 실패를 뜻하지 않습니다.

## 오류가 발생한 계층부터 확인

| 관측 | 의미 | 다음 조치 |
| --- | --- | --- |
| Computer Use가 위 문구로 터미널 앱 접근을 거절 | Computer Use의 앱 안전 정책 | Computer Use로 터미널 GUI를 조작할 수 없습니다. 별도로 허용된 shell 실행 환경에서는 아래 CLI 진단과 `self-test`를 사용할 수 있습니다. |
| Codex의 실제 Auto-review `Denied`와 판단 사유 | 특정 도구 호출의 승인 심사 | 정확한 동작·대상·사유를 확인합니다. 지원되는 Codex TUI의 `/approve`는 해당 거절 동작을 한 번 재심사하는 경로이며, Computer Use의 터미널 앱 제한을 해제하는 명령이 아닙니다. |
| macOS가 다른 앱 제어를 허용할지 묻거나 Apple Events 권한 오류 발생 | 호출 앱에서 대상 앱으로의 Automation 권한 | 사용자가 시스템 설정 → 개인정보 보호 및 보안 → 자동화에서 실제 호출 앱과 제어 대상의 조합을 확인합니다. |
| Claude의 폴더 신뢰 또는 도구 실행 허용 화면 | Claude의 workspace 신뢰·도구 정책 | Bridge에 넘길 workspace에서 직접 Claude를 실행해 내용을 확인하고 승인합니다. `/permissions`로 규칙을 확인합니다. 터미널 앱 전체에 대한 제어 승인으로 기록하지 않습니다. |
| 키체인 접근 창, `CSSMERR_CSP_OPERATION_AUTH_DENIED`, 로그인 불가 표시 | 자격증명 접근 또는 인증 문제의 단서 | 사용자가 요청 앱과 키체인 항목을 확인합니다. 인증 실패만으로 키체인 문제나 토큰 폐기를 단정하지 않습니다. |
| Warp Control 접근 실패 또는 생성 기능 미제공 | Warp의 endpoint·Scripting·TabConfigs 가용성 | [지원 범위](../README.md#지원-범위)의 조건과 실제 오류를 확인합니다. Claude 승인으로 Warp 기능이 활성화되지는 않습니다. |

2026-10-03에 확인한 [Computer Use 공식 문서](https://learn.chatgpt.com/docs/computer-use)는 터미널 앱 조작을 통한 보안 정책 우회를 제한합니다. 같은 날 Warp(`dev.warp.Warp-Stable`)와 Terminal.app(`com.apple.Terminal`) 접근에서 위 오류를 재확인했습니다. 따라서 이 오류를 단순한 “자동 승인 검토 거절”로 보고하지 않습니다. 일반 앱의 Always allow 설정, Full Access, Bridge의 `--yolo`로 이 제한을 해제한다고 안내하지 않습니다. 거절된 앱 조작을 다른 UI 기술로 재시도하지 않습니다.

같은 공식 문서는 파일 편집과 shell 명령이 별도의 승인·sandbox 정책을 따른다고 구분합니다. 따라서 Computer Use 거절만으로 모든 CLI 통합 검사를 사용자 수동 실행으로 제한하지 않습니다. 허용된 shell 도구에서 제품의 기존 CLI/API를 검사할 수 있습니다. 실제 shell 호출이 별도로 거절되면 그 사유를 따라야 하며, 이 설명은 해당 거절의 우회를 허용하지 않습니다.

[Auto-review](https://learn.chatgpt.com/docs/sandboxing/auto-review)는 승인 요청의 검토자를 바꾸는 기능입니다. Computer Use의 앱 승인과 별개이며, 자동 검토가 모든 동작을 허용하거나 OS 권한을 대신 부여하지 않습니다. `--yolo` 역시 [각 provider의 권한 옵션](../README.md#권한과-세션-경계)을 전달할 뿐입니다.

## Claude를 처음 사용할 때

1. 사용할 터미널을 직접 열고 Bridge의 `--workspace`에 지정할 디렉터리로 이동합니다. `command -v claude`와 `claude --version`으로 실제 실행 파일·버전을 확인한 뒤 `claude`를 실행합니다.
2. 표시된 로그인·폴더 신뢰·도구 권한을 각각 확인합니다. 이미 로그인되어 있고 신뢰가 유효하면 다시 승인할 필요가 없습니다. 승인 범위는 Claude의 설정과 workspace를 따릅니다. “각 터미널을 한 번 승인하면 모든 프로젝트가 영구 허용된다”는 뜻이 아닙니다. 도구의 일회성 허용과 저장되는 규칙도 구분합니다. [Claude 권한 문서](https://code.claude.com/docs/en/permissions)
3. 키체인 창이 실제로 나타난 경우에만 사용자가 처리합니다. Apple의 Allow Once는 이번 접근만, Always Allow는 해당 접근을 이후에도 허용하는 선택입니다. 폴더 신뢰 창을 키체인 승인으로 기록하지 않습니다. [Apple 설명](https://support.apple.com/en-euro/guide/keychain-access/kyca1243/mac)
4. 도구 실행이 필요 없는 짧은 marker 응답을 받아 CLI 자체가 작동하는지 확인합니다. 이 성공은 Bridge의 후속 전달·탭 생성·정리까지 증명하지 않습니다.

터미널마다 shell 환경이 다르면 다른 Claude 실행 파일이나 설정 디렉터리를 사용할 수 있습니다. `CLAUDE_CONFIG_DIR`이 다르면 인증 저장소도 달라질 수 있으므로 필요할 때 경로만 비교합니다. 인증 파일·키체인 비밀값을 출력하거나 복사할 필요는 없습니다. [Claude 인증 문서](https://code.claude.com/docs/en/authentication)

Automation 권한은 Claude 안의 도구 승인과 다릅니다. 실제 제어를 요청한 앱과 대상 앱을 사용자가 확인해야 합니다. [Apple Automation 안내](https://support.apple.com/en-hk/guide/mac-help/mchl108e1718/mac)

## Computer Use 없이 하는 CLI 진단과 실앱 검사

`doctor --provider claude --probe --json`은 CLI 경로·버전·workspace 등 가용성을 조회합니다. Claude의 `--probe`는 모델이나 messenger를 호출하지 않으므로 실제 전달 성공을 증명하지 않습니다. 기존 요청은 `inspect <session> --timeline --json`과 `result <session> --request <request-id> --json`으로 조회할 수 있습니다. 대기·전달 불확실 상태에서 동일 요청을 재전송하지 않습니다.

아래 `self-test`는 Computer Use를 호출하지 않고 Agent Bridge의 실제 `ask`·`result`·`tell`·`close-session` 경로를 검사합니다. 허용된 shell 실행 환경에서는 에이전트도 수행할 수 있습니다. 실제 모델 호출과 새 터미널 surface 생성·종료가 발생하며, 화면 없는 provider 검사와는 다릅니다. 승인 대화상자가 나타나면 사용자가 내용을 확인하고 처리합니다.

먼저 검증할 소스의 commit과 바이너리를 고정합니다. Warp·WezTerm·`macos-open-mode`가 추가된 `3648cc65abf1d2a76f28ba6e17d78fbabafe25ff`의 개발 빌드는 버전 문자열도 `0.0.10`이므로, `--version`만으로 정식 v0.0.10 설치본과 구별할 수 없습니다. 설치본을 교체하지 않고 빌드 결과의 절대 경로를 사용합니다.

```sh
git rev-parse HEAD
git status --short
cargo build --locked
shasum -a 256 target/debug/agent-bridge

ab_binary="$PWD/target/debug/agent-bridge"
ab_workspace="$PWD"
ab_report_dir="$(mktemp -d "${TMPDIR:-/tmp}/agent-bridge-check.XXXXXX")"
```

변경된 source가 있으면 그 diff도 보존해야 합니다. 아래 예제는 provider의 기본 권한 모드를 사용합니다. 이미 승인된 별도 실행 정책에 따라 `--yolo`를 쓰더라도 그 사실을 기록하고, 정상 승인 모드 검증으로 보고하지 않습니다. 키체인·workspace·Automation 창은 사용자가 직접 처리합니다.

```sh
"$ab_binary" self-test claude \
  --workspace "$ab_workspace" --terminal terminal \
  --timeout-secs 120 --isolated --json \
  > "$ab_report_dir/terminal-claude.json" \
  2> "$ab_report_dir/terminal-claude.stderr"
ab_terminal_exit=$?
printf 'Terminal exit=%s; report=%s\n' "$ab_terminal_exit" "$ab_report_dir/terminal-claude.json"
```

Terminal 검사와 정리가 끝난 뒤 Warp의 실행 조건이 갖춰졌으면 별도로 검사합니다. Warp Stable의 공식 Control CLI는 다음과 같이 인스턴스 목록만 조회할 수 있습니다. 이 경로는 설치된 Stable 앱의 제어 모드이며 Preview 등 다른 채널의 경로로 추정해서 쓰지 않습니다.

```sh
/Applications/Warp.app/Contents/MacOS/stable --warpctrl --output-format json instance list
```

`instances: []`이면 제어 가능한 endpoint가 발견되지 않은 상태입니다. Warp 실행 여부·빌드 지원·Settings → Scripting을 확인하고, 활성화가 필요한 경우 사용자가 직접 선택합니다. 빈 목록만으로 Scripting이 꺼졌다고 단정하지 않습니다. 2026-10-03 Stable `0.2026.09.30.08.29.01`에서 위 명령은 exit 0과 빈 목록을 반환했습니다. 이 결과는 Claude 폴더 신뢰 문제도, Warp 탭 생성 성공도 아닙니다. endpoint가 있어도 필요한 action과 TabConfigs/Launch Configuration 가용성은 별도로 충족해야 합니다.

```sh
"$ab_binary" self-test claude \
  --workspace "$ab_workspace" --terminal warp \
  --timeout-secs 120 --isolated --json \
  > "$ab_report_dir/warp-claude.json" \
  2> "$ab_report_dir/warp-claude.stderr"
ab_warp_exit=$?
printf 'Warp exit=%s; report=%s\n' "$ab_warp_exit" "$ab_report_dir/warp-claude.json"
```

`--isolated`는 Bridge 상태를 분리하며 provider의 로그인·설정까지 격리하지 않습니다. 평소 Bridge state root의 settings·consent는 적용되지 않으므로 위 검사는 기본 `tab-first` 정책을 사용합니다. 별도의 `new-window` 설정 검증으로 보고하지 않습니다. timeout은 명령별 예산이며 전체 실행 시간 제한이 아닙니다. 실패하면 같은 명령을 반복하기 전에 JSON·stderr와 실제 화면을 읽습니다.

격리된 실행의 기록은 report의 `state_root`와 `session`을 사용해 조회합니다.

```sh
AGENT_BRIDGE_NATIVE_STATE_DIR="<report.state_root>" "$ab_binary" inspect "<report.session>" --timeline --json
AGENT_BRIDGE_NATIVE_STATE_DIR="<report.state_root>" "$ab_binary" doctor "<report.session>" --probe --json
```

다음 항목을 별도로 남깁니다.

- CLI 종료 코드 0과 JSON의 `outcome: "passed"`; `ask`, `initial_result`, `tell`, `follow_up_result`, `cleanup` 모두 `passed`.
- 검사한 commit·binary SHA-256·provider 버전·terminal·workspace·session·request/event·state root.
- 사용자가 실제로 본 새 tab/window 생성과 소멸, 기존 tab/window 보존, 포커스·키 입력 영향. JSON의 `session_state: "closed"`와 `cleanup: passed`만으로 화면 소멸까지 확인했다고 쓰지 않습니다.
- 승인 화면이 있었다면 정확한 종류, 요청 앱·workspace와 사용자가 선택한 범위. 암호·토큰은 기록하지 않습니다.

Warp에서 Agy·Pi의 terminal 기반 후속 제출은 이 후보가 지원하지 않습니다. 최초 실행 승인으로 해결되는 실패가 아니며 Claude의 왕복 성공을 네 provider 전체의 성공으로 확대하지 않습니다.

## Terminal.app 종료 결과를 읽을 때

2026-10-03의 `3648cc65` CLI self-test는 Claude 최초 응답·공식 후속 전달까지 통과했지만 cleanup에서 `window no longer holds its tab`으로 실패했습니다. 해당 창은 Terminal API의 목록에 남아 있으면서 `tabs=0`, `visible=false`였다가 나중에 `visible=true`로 관측됐고, 사용자가 빈 창이 남았음을 확인했습니다. **빈 탭 목록·비가시 상태만으로 닫혔다고 판단했던 중간 수정과 그 self-test PASS는 철회했습니다.**

실제 `close` 이후에만 그 상태를 인정하는 두 번째 후보도 검토했으나 제품 변경으로 채택하지 않았습니다. 사용자가 원래 창의 소멸을 한 번 확인한 뒤 **같은 창 ID·제목의 재등장**을 보고했습니다. 공식 `close`와 `close saving no` 응답 및 `visible=false`만으로 영구 정리를 증명할 수 없었습니다. 제품의 종료·부재 판정은 기존의 엄격한 동작으로 복원했고, 숨겨진 빈 창과 보이는 빈 창을 종료 성공으로 바꾸지 않는 회귀 사례를 남겼습니다. 실제 빈 창을 확실히 정리하는 수정은 미해결입니다.

이미 handle을 소비한 실험 세션은 `close-session`이 성공을 반환해도 그 창을 다시 제어하지 않을 수 있습니다. 새 검사를 반복하지 말고 정확한 창 ID·원래 요청 기록을 보존합니다. native API로 정리가 증명되지 않는 잔여 창은 사용자가 해당 창의 닫기 버튼으로 확인해야 합니다. 앱 전체 종료·추측한 TTY 종료로 다른 사용자 세션을 정리하지 않습니다.

실패한 실행의 정확한 window·tab ID까지 보존해야 한다면 `self-test` 대신 `ask --detach --json`부터 `result`·`tell`·`close-session`까지 각각 실행하고 `ask` 응답을 보존합니다. 현재 self-test report에는 surface ID가 없고, 닫힌 세션의 terminal handle은 소비되므로 사후 복원에 의존하지 않습니다. 화면 관찰이 JSON과 다르면 PASS를 철회하고 사람이 본 사실을 먼저 기록합니다.
