# Agent Bridge

로컬에 설치되고 로그인된 `codex`, `claude`, `agy`, `pi` CLI를 사용자가 볼 수 있는 실제 터미널 세션에서 연결하는 브리지입니다. API 키나 로그인 토큰을 대신 소유하지 않고 각 CLI의 기존 인증·설정·대화형 UI를 그대로 사용합니다.

첫 설치 가능 릴리스는 **v0.0.1**입니다. macOS에서 Ghostty 1.3 이상, iTerm2, 내장 Terminal.app을 지원합니다. Agent Bridge는 자신을 실행한 터미널을 감지해 같은 앱의 새 surface에서 세션을 시작하고, 자신이 만든 surface만 제어합니다. Terminal.app에서는 기존 tab/window를 사용하지 않고 항상 전용 새 window를 엽니다. 감지할 수 없는 호스트에서는 Terminal.app으로 안전하게 fallback합니다. 이미 독립적으로 실행 중인 임의의 CLI에는 사후 attach하지 않습니다.

## 지원 범위

| 환경 | 상태 | transport |
| --- | --- | --- |
| macOS + Ghostty 1.3+ | 지원 | Ghostty AppleScript 직접 제어 |
| macOS + iTerm2 | 지원 | iTerm2 AppleScript 직접 제어 |
| macOS + Terminal.app | 지원 | Terminal AppleScript 직접 제어 |
| macOS의 다른 터미널 | fallback | 별도 adapter가 없으면 Terminal.app에서 시작 |
| Windows PowerShell / cmd | 지원 | PowerShell 7(`pwsh.exe`) 기반 전용 visible console; `ask`/`tell`/`sessions`/explicit prune·close |
| Linux 터미널 | 미지원 | [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)에서 별도 구현 |
| VS Code 통합 터미널 | 현재 비범위 | 전용 adapter가 필요하면 별도로 판단 |

macOS에서는 `TERM_PROGRAM`, `TERM`, `ITERM_SESSION_ID`, `TERM_SESSION_ID` 순으로 현재 호스트를 식별합니다. `--terminal ghostty|iterm2|terminal`로 명시 선택할 수 있고, 선택을 생략한 상태에서 호스트를 식별하지 못하면 내장 Terminal.app을 엽니다. Terminal.app은 복원되거나 기존에 열린 surface를 채택하지 않고 항상 전용 새 window를 만듭니다. 명시 선택한 adapter가 실패하면 다른 앱으로 조용히 우회하지 않고 오류를 반환합니다.

Ghostty의 AppleScript는 1.3에서 추가된 preview API이며 macOS Automation 권한이 필요합니다. Ghostty 설정에서 `macos-applescript = false`이면 adapter를 사용할 수 없습니다. Terminal.app은 기존 tab이나 UI scripting을 사용하지 않고 native AppleScript로 항상 전용 새 window를 만듭니다.

Windows는 PowerShell 또는 cmd에서 호출할 수 있으며 PowerShell 7(`pwsh.exe`)이 설치되어 있어야 합니다. bridge는 absolute PATH entry에서 찾은 `pwsh.exe`의 절대 경로를 `CreateProcessW`에 전달하고, `CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP`로 전용 visible console을 만듭니다. 후속 입력과 explicit close는 managed session ID, PID 생성 시각, 실행 파일 identity가 모두 일치할 때만 전달합니다. npm provider shim은 `.exe`, `.ps1`, `.cmd`, `.bat` 순으로 찾고 PowerShell shim을 우선해 `%NAME%`의 `cmd.exe` 확장을 피합니다. Linux는 아직 미지원입니다. provider/session 계약은 공유하되 OS와 terminal transport는 각각 독립 모듈로 유지합니다. provider 간에도 transport 구현을 억지로 공통화하지 않습니다. 각 provider adapter가 공식 session messaging·follow-up·result identity를 우선 사용하고, upstream에서 제공하지 않는 플랫폼·버전에만 같은 의미론의 fallback을 소유합니다. upstream 지원이 추가되면 공통층을 늘리는 대신 해당 fallback을 삭제·교체합니다.

의미를 이해하고 완료 결과를 회수하는 provider는 다음 네 가지입니다.

- Codex: 세션별 `notify`
- Claude Code: 세션별 `Stop` hook
- Agy: 세션 로그와 완료 transcript
- Pi: 세션 전용 lifecycle 확장

Windows와 macOS의 사용자 기능은 같지만 provider transport는 공식 기능의 플랫폼 지원 여부에 따라 다릅니다.

| Provider | macOS | native Windows |
| --- | --- | --- |
| Codex | provider session notify + provider-owned terminal follow-up | 동일한 provider adapter의 notify/result correlation + Windows console follow-up |
| Claude Code | 지원 버전·backend·설정 gate를 모두 통과할 때 공식 cross-session `ListAgents`/`SendMessage` + `Stop` hook | Claude Code 2.1.234+의 공식 named-pipe cross-session `ListAgents`/`SendMessage` + `Stop` hook |
| Agy | transcript/result monitor + provider-owned terminal follow-up | 동일한 provider adapter의 transcript/result monitor + Windows console follow-up |
| Pi | session lifecycle extension + provider-owned terminal follow-up | 동일한 provider adapter의 lifecycle extension + Windows console follow-up |

다른 provider의 console follow-up은 각 adapter 내부에 격리되어 있으며, bridge 공통층이 provider payload나 결과 identity를 추측하지 않습니다. Claude의 공식 cross-session 기능을 runtime gate 때문에 사용할 수 없으면 terminal injection으로 우회하지 않고 실패합니다.

## 설치

소스에서 설치할 때는 Rust 1.97.1 이상이 필요합니다. macOS에서는 지원하는 터미널 하나가 필요하고 Ghostty를 사용하면 1.3 이상이어야 합니다. Windows에서는 PowerShell 7이 필요합니다. 두 OS 모두 사용할 provider CLI를 먼저 직접 실행해 로그인과 초기 설정을 완료해야 합니다.

```sh
git clone --branch v0.0.3 --depth 1 https://github.com/jy1655/agent-bridge.git
cd agent-bridge
cargo install --path . --locked
agent-bridge --version
```

마지막 명령은 `agent-bridge 0.0.3`을 출력해야 합니다. 개발 중인 `main`이 아니라 릴리스 tag에서 설치해야 설치본과 소스의 경계가 명확합니다.

Windows 명령줄 한도를 넘는 요청은 `--prompt-file`로 전달합니다. 파일은 UTF-8 텍스트로 읽으며 Agent Bridge가 원본을 삭제하거나 수정하지 않습니다.

기존 설치를 교체할 때는 `cargo install --path . --locked --force`를 사용합니다. 기본 설치 위치인 `~/.cargo/bin`이 `PATH`에 없다면 추가하거나 빌드한 바이너리를 절대 경로로 실행합니다.

GitHub Release에는 Apple Silicon macOS용 `agent-bridge-<version>-aarch64-apple-darwin.tar.gz`와 64비트 Windows용 `agent-bridge-<version>-x86_64-pc-windows-msvc.zip`을 게시하며, 각 archive와 같은 이름의 `.sha256` 파일을 함께 제공합니다. prebuilt archive 설치에는 Rust가 필요하지 않습니다. archive를 푼 뒤 macOS에서는 `agent-bridge`, Windows에서는 `agent-bridge.exe`를 `PATH`에 있는 디렉터리로 옮깁니다. 다운로드한 파일은 실행 전에 체크섬을 검증하세요.

```sh
shasum -a 256 -c agent-bridge-0.0.3-aarch64-apple-darwin.tar.gz.sha256
tar -xzf agent-bridge-0.0.3-aarch64-apple-darwin.tar.gz
./agent-bridge --version
```

```powershell
$archive = "agent-bridge-0.0.3-x86_64-pc-windows-msvc.zip"
$expected = (Get-Content "$archive.sha256").Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
Expand-Archive $archive -DestinationPath .\agent-bridge
.\agent-bridge\agent-bridge.exe --version
```

Windows에서 Agent Bridge를 호출하는 shell은 `cmd.exe`, Windows PowerShell 5.1, PowerShell 7을 지원합니다. 어느 shell에서 호출하더라도 실제 managed console은 `PATH`의 절대 항목에서 찾은 PowerShell 7(`pwsh.exe`)로 실행되므로 PowerShell 7 설치는 필수입니다. shell별 quoting에 의존하지 않도록 긴 입력, Unicode 입력, 공백이 있는 경로는 `--prompt-file` 사용을 권장합니다.

## 사용법

새 세션을 열고 첫 결과를 기다립니다.

```sh
agent-bridge ask claude \
  --workspace ~/Dev/project \
  --title "Claude reviewer" \
  --model Fable5 \
  --effort max \
  --prompt "이 변경을 검토하고 결과만 요약해줘"
```

Codex provider에 Pi 형식의 `openai-codex/<model>`이 들어오면 비어 있지 않은 `<model>`을 Codex가 요구하는 bare model ID로 변환합니다. Claude provider에서 정확한 모델 입력값 `Fable5`는 Claude Code가 요구하는 `Fable`로 변환됩니다. Pi provider에서는 정확한 입력값 `Fable`만 `anthropic/claude-fable-5`로 변환합니다. 그 밖의 모델 값은 각 provider에 그대로 전달합니다.

`--workspace`를 생략하면 현재 디렉터리를 사용합니다. `--title`은 사람이 읽는 세션 metadata이며 terminal surface의 제목을 설정하거나 고정하지 않습니다. Claude Code에는 정확한 cross-session 주소를 보장하기 위해 Agent Bridge의 고유 session ID를 native session name으로 전달합니다. 다른 provider는 지원 범위에 따라 title을 native name으로 사용할 수 있습니다. `--title`을 생략하면 provider와 workspace 이름으로 metadata를 만듭니다. `--model`과 `--effort`를 생략하면 각 CLI의 기존 기본값을 유지합니다. `--terminal`을 생략하면 호출 환경을 자동 감지하고, 감지 불가 시 Terminal.app을 사용합니다.

기계 판독이 필요하면 `--json`을 사용합니다. 반환된 `session` id로 같은 탭에 후속 프롬프트를 전달할 수 있습니다. 응답에는 `terminal`, `terminal_session_id`와 adapter가 제공하는 `terminal_tab_id`·`terminal_window_id`가 포함됩니다. 호환성을 위해 iTerm2 세션에만 기존 `iterm_session_id`도 함께 제공합니다.

```sh
agent-bridge ask codex \
  --workspace ~/Dev/project \
  --model gpt-daybreak-blue-latest \
  --effort xhigh \
  --prompt "원인을 진단해줘" \
  --json

agent-bridge tell session-XXXXXXXX \
  --prompt "그중 2번만 수정해줘" \
  --json

agent-bridge sessions --json
agent-bridge prune-sessions --closed-before-days 30 --explicit
agent-bridge close-session session-XXXXXXXX --explicit
```

`ask`와 `tell`은 기본적으로 다음 provider 결과를 최대 900초 기다립니다. `ask --timeout-secs`는 준비 검사, terminal 시작, provider별 초기 입력 준비 지연, 결과 대기를 합친 전체 시간 제한이고, `tell --timeout-secs`는 provider-native 전송과 결과 대기를 합친 전체 시간 제한입니다. 제한을 바꾸거나, 결과를 기다리지 않고 탭만 열려면 `--detach`를 사용합니다. 대기 실패나 timeout은 이미 열린 탭을 자동으로 닫지 않습니다.

전체 명령은 다음과 같습니다.

```text
agent-bridge ask <codex|claude|agy|pi> [--workspace PATH] (--prompt TEXT | --prompt-file PATH) [--title NAME]
    [--model MODEL] [--effort EFFORT] [--terminal <ghostty|iterm2|terminal|windows-console>]
    [--yolo] [--timeout-secs N] [--detach] [--json]
agent-bridge tell <session> (--prompt TEXT | --prompt-file PATH) [--timeout-secs N] [--detach] [--json]
agent-bridge sessions [--json]
agent-bridge prune-sessions --closed-before-days N --explicit [--json]
agent-bridge close-session <session> --explicit [--json]
agent-bridge --help | --version
```

## 권한과 세션 경계

- Agent Bridge는 같은 `ask` 작업에서 새로 만든 surface만 기록합니다. 새 handle은 managed session ID와 host가 제공하는 stable ID를 결합하며 `tell`과 `close-session` 직전에 live wrapper와 함께 다시 검증합니다. macOS의 세 adapter 모두 target `native-session` owner의 managed session ID·PID·controlling TTY device·process start fingerprint·foreground process group과 전용 login shell identity를 검증합니다. iTerm2와 Terminal.app은 surface가 보고하는 TTY까지 owner와 일치시켜야 하고, Ghostty는 terminal·tab·window ID 복합체와 live owner를 함께 확인합니다. Windows는 console root와 `native-session` owner의 PID 생성 시각·실행 파일 identity를 함께 검증해 PID 재사용을 fail-closed하며, suspended console의 identity-bound handle을 private state에 내구성 있게 기록한 뒤에만 실행을 재개합니다. 호출 당시 터미널을 재감지하거나 복원된 front/current/selected surface를 채택하지 않습니다.
- 새 세션의 `--model`, `--effort`, `--yolo`는 부모 CLI에서 추측하거나 상속하지 않습니다. 해당 `ask` 요청에 명시된 값만 사용합니다.
- `--yolo`는 Codex의 `--dangerously-bypass-approvals-and-sandbox`, Claude와 Agy의 `--dangerously-skip-permissions`를 전달합니다. Pi에서는 해당 실행의 project-local files를 신뢰하는 `--approve`를 전달하며 Pi 자체 tool 정책은 유지합니다.
- `tell`은 세션별 한 턴만 허용합니다. Claude Code 2.1.234 이상은 macOS와 native Windows 모두 별도의 비영속·격리 설정 print-mode Claude 프로세스에서 공식 `ListAgents`로 고유 managed session name을 찾고 `SendMessage`로 전달합니다. Claude의 공식 cross-session 기능은 macOS·Linux에서 2.1.224부터, native Windows에서 per-session named pipe를 사용하는 2.1.234부터 제공됩니다. 메시지는 stdin JSON으로만 전달하고, 임시 `PreToolUse` hook이 정확한 local session name·요약·본문을 실행 전에 검증하며 `isolatePeerMachines`로 cross-machine 전송을 막고 실제 tool call과 성공 결과도 다시 일치해야 전송 성공으로 인정합니다. 대상 세션의 `Stop` hook은 요청별 고유 Claude turn ID와 정확한 최종 마커가 일치한 응답만 결과로 기록하고 반환값에서는 마커를 제거합니다. 마커가 다른 수동·비상관 턴에는 개입하지 않고 pending claim을 유지합니다. 이 provider 전용 상관관계 프로토콜은 Claude가 대상 turn identity를 공식 결과로 제공하면 교체할 경계입니다. 이 provider-native 전송에는 `tell`마다 별도의 Claude transport turn이 한 번 필요합니다. provider·feature-flag·정책 설정 때문에 공식 기능을 사용할 수 없으면 terminal injection으로 자동 전환하지 않고 전송 전에 실패합니다. 전송을 시도한 뒤 성공 여부를 확인할 수 없으면 중복 재전송을 막기 위해 turn claim을 유지하며, 대상 결과가 도착하거나 `close-session --explicit`으로 닫을 때 해제됩니다. Codex·Pi·Agy는 각 provider adapter가 소유한 terminal paste fallback을 사용합니다. 각 fallback turn은 claim token과 정확한 final marker가 일치한 결과만 수락합니다. Enter·ESC 같은 별도 터미널 동작을 만들 수 있는 제어문자는 거부합니다.
- 모든 bridge 프롬프트에는 source provenance가 붙습니다. 사람이 읽는 결과의 터미널 제어문자는 가시적인 문자열로 이스케이프합니다.
- 결과가 돌아온 뒤 탭은 열린 채 유지되어 사용자가 직접 이어서 작업할 수 있습니다. terminal paste fallback을 쓰는 provider에서는 진행 중인 bridge 요청과 같은 탭의 수동 입력을 겹치면 수동 턴 결과가 bridge 요청의 결과로 먼저 인식될 수 있으므로 동시에 입력하지 않아야 합니다.
- `close-session`은 `--explicit`이 있어야 합니다. Terminal.app은 live `native-session` owner attestation과 전용 window ID·TTY가 모두 일치하고 owner가 현재 terminal foreground process group의 leader임을 확인합니다. 이어 같은 TTY의 실제 parent login shell이 별도 process-group leader이고 owner를 foreground group으로 보고하는지도 검증한 뒤, managed group에는 `SIGTERM`, 전용 shell group에는 `SIGKILL`을 보내 Terminal.app이 idle 전이를 관찰한 경우에만 전용 window를 닫습니다. 과거 owner record에 process-group·shell 필드가 없어도 PID·시작시각·parent 관계·TTY가 일치하는 live identity에서 같은 관계를 모두 증명해야 하며, terminal control character나 UI scripting은 사용하지 않습니다. close finality에서는 `terminal.json` handle을 `terminal.closed.json` tombstone으로 소진하며, 이미 `closed`인 세션의 반복 close는 terminal adapter를 호출하지 않습니다.
- `prune-sessions`도 `--explicit`이 있어야 합니다. `closed.json` 시각이 보존 기간보다 오래됐고 현재 status도 `closed`이며 terminal handle, pending resume, turn claim, live owner가 없는 관리 디렉터리만 삭제합니다. 열린 세션이나 판별할 수 없는 owner는 유지하며 자동 보존 기간이나 암묵적 삭제는 없습니다.
- 세션별 상태와 결과는 권한을 제한한 `~/.agent-bridge/native-sessions` 아래에 저장합니다. 상태·event·turn claim은 파일과 상위 디렉터리까지 동기화하고, 중간 완료 journal을 복구한 뒤 event·ready 상태·claim 해제를 한 lifecycle lock 아래에서 수렴시킵니다. Windows는 사용자 지정 state root에서도 ACL 상속을 제거하고 현재 사용자 전용 ACL을 적용합니다. provider의 전역 설정이나 workspace hook 파일은 수정하지 않습니다. Agent Bridge가 만든 Claude 세션의 private `--settings` 파일에는 모든 지원 OS에서 `crossSessionInbound: "accept"`와 `Stop`·`StopFailure` hook을 기록합니다. Messenger의 `PreToolUse` guard 설정과 기대 payload 파일은 해당 `tell` 동안만 같은 private 세션 디렉터리에 존재하고 종료 시 제거합니다. 요청별 pending turn 레코드는 상관된 결과에서 제거하며, 상관관계를 확인할 수 없는 오류에서는 보존합니다.

최소 지원 버전은 다음과 같습니다. 더 새로운 버전은 허용합니다.

| Provider | 최소 버전 |
| --- | --- |
| Codex | 0.147.0 |
| Claude Code | 2.1.234 |
| Agy | 1.1.12 |
| Pi | 0.84.1 |

> [!WARNING]
> Codex, Claude, Agy에서 `--yolo`는 해당 CLI의 승인·sandbox 보호를 우회합니다. 신뢰하는 코드와 workspace에서만 명시적으로 사용하세요. Agent Bridge는 그 세션 내부의 명령을 다시 sandbox하지 않습니다.

> [!WARNING]
> Agent Bridge가 만든 Claude 세션은 같은 계정의 다른 Claude 세션에서 오는 공식 cross-session 메시지를 자동 수락합니다. 고유 session ID를 주소로 사용하지만, `--yolo`와 결합하면 inbound 작업도 해당 세션의 우회 권한 경계 안에서 실행될 수 있으므로 관리 세션 ID와 로그인 계정을 신뢰 경계로 취급하세요.

## 구조와 확장

코드는 provider 의미와 terminal transport를 분리합니다.

```text
src/providers/                 공통 provider 정책: 명령, 버전, model/effort/yolo 인자
src/native/provider/          provider별 실행, follow-up transport, 완료 monitor 선택
src/native/provider_process.rs provider process 실행과 Windows shim 경계
src/native/terminal/mod.rs     공통 terminal kind, session record, OS dispatch
src/native/terminal/macos/     iTerm2, Terminal.app, Ghostty adapter
src/native/terminal/linux/     Linux transport 경계(현재 미지원)
src/native/terminal/windows/   Windows console transport, process identity, ACL/security
src/native.rs                  세션 상태, lifecycle, 명령 및 provider-neutral orchestration
src/native/tests.rs            provider-neutral native orchestration 단위 테스트
```

새 CLI를 추가할 때는 provider registry와 두 provider adapter를 추가하고, model/effort/권한 및 실제 결과 회수 계약을 각각 테스트합니다. 새 터미널은 해당 OS 디렉터리에 adapter를 추가하고 OS dispatcher에 등록합니다. 새 OS는 독립 디렉터리에서 같은 `detect/open_tab/send_file/close_session` 계약을 구현합니다. launch command quoting은 POSIX shell과 Windows PowerShell을 분리해 유지합니다. 공통화가 플랫폼의 native 동작을 약화한다면 플랫폼별 구현을 우선합니다.

기존 `{"iterm_session_id":"..."}` 형식의 `terminal.json`은 iTerm2 세션으로 계속 읽습니다. 새 세션은 terminal-neutral한 `terminal`, `session_id`, 선택적 `tab_id`·`window_id`와 내부 `managed_session_id` binding을 기록합니다. 가시적인 terminal title은 설정하거나 ownership record에 저장하지 않습니다. Terminal.app의 추가 owner attestation은 target `native-session`이 별도 private record에 기록합니다.

## 0.0.3 업데이트

0.0.3은 native Windows를 macOS와 같은 관리형 세션 계약으로 배포하는 첫 릴리스입니다. `cmd.exe`, Windows PowerShell 5.1, PowerShell 7에서 Codex·Claude·Agy·Pi의 `ask → result → tell → result → sessions → explicit close` 흐름을 지원하며, 호출 shell과 분리된 PowerShell 7 visible console을 사용합니다. Claude Code 2.1.234+의 공식 native-Windows cross-session messaging으로 구형 resume supervisor를 대체했고, 모든 provider의 turn correlation, crash-recovery journal, 단조 상태 전이, 전체 timeout 예산, PID/TTY 소유권 검증을 보강했습니다.

GitHub Release는 Apple Silicon macOS와 64비트 Windows archive 및 SHA-256 checksum을 함께 게시합니다. Linux transport, 독립 실행 중인 CLI에 대한 사후 attach, provider 간 workspace trust 공유는 이번 릴리스 범위에 포함되지 않습니다.

## 0.0.2 업데이트

0.0.2는 Pi 형식의 `openai-codex/<model>`이 Codex session에 전달됐을 때 ChatGPT 계정에서 지원되지 않는 모델이라는 오류가 발생하던 문제를 수정합니다. Codex adapter만 비어 있지 않은 `openai-codex/` prefix를 제거하며, bare Codex model ID와 Claude·Agy·Pi의 model 값은 그대로 유지합니다. Windows transport 구현과 runtime 검증 상태는 0.0.1에서 변경하지 않습니다. 자세한 추적 기록은 [Issue #20](https://github.com/jy1655/agent-bridge/issues/20)에 있습니다.

## 0.0.1 마이그레이션

0.0.1은 예전 embedded multi-PTY TUI와 그 전용 표면을 제거한 뒤 처음 고정한 설치 가능 릴리스입니다.

- 인자 없는 `agent-bridge`, workspace 직접 인자, `--restore`, 전역 `--yolo`는 더 이상 TUI를 시작하지 않습니다.
- 예전 `open`, `prompt`, `status`, `read`, `wait`, `list`, `close`, `hook` 명령은 제거되었습니다. 새 명령은 `ask`, `tell`, `sessions`, `prune-sessions`, `close-session`입니다.
- `~/.agent-bridge/agents.json`, 이전 layout, spool 등 레거시 파일은 읽지 않으며 자동 삭제하지도 않습니다.
- 실행 중인 구버전 TUI가 있다면 종료한 뒤 바이너리를 교체하세요.

과거 설계와 벤치마크 기록은 Git 이력과 프로젝트 Wiki에 보존하고, 이 저장소 문서는 현재 지원 계약만 설명합니다.

## 검증

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

`tests/native_live.rs`의 ignored 테스트는 감지되거나 `--terminal`로 지정한 실제 session surface와 로그인된 provider를 사용합니다. macOS에서는 지원 앱을, Windows에서는 `windows-console`을 지정합니다. 각 테스트는 `ask → result → tell → result → close-session --explicit → closed 상태 조회`를 한 번에 검증하고 정상 경로에서 생성한 surface를 닫습니다. 종료 단계 자체가 실패하면 진단을 위해 surface가 남을 수 있으므로 `sessions`로 확인합니다. Pi 0.84.1 이상은 Node.js 22.19.0 이상이 필요합니다.

```sh
AGENT_BRIDGE_LIVE_TERMINAL=terminal \
AGENT_BRIDGE_LIVE_CLAUDE_MODEL=Fable5 \
AGENT_BRIDGE_LIVE_CLAUDE_EFFORT=max \
cargo test --test native_live \
  live_native_claude_returns_result_and_closes_session \
  -- --ignored --exact --nocapture
```

## License

[MIT](LICENSE)
