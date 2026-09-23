# Agent Bridge

로컬에 설치되고 로그인된 `codex`, `claude`, `agy`, `pi` CLI를 사용자가 볼 수 있는 실제 터미널 세션에서 연결하는 브리지입니다. API 키나 로그인 토큰을 대신 소유하지 않고 각 CLI의 기존 인증·설정·대화형 UI를 그대로 사용합니다.

첫 설치 가능 릴리스는 **v0.0.1**이며 최신 설치 가이드는 **v0.0.5** tag를 기준으로 합니다. macOS에서는 iTerm2와 내장 Terminal.app을 지원합니다. Agent Bridge는 자신을 실행한 터미널을 감지해 같은 앱의 새 surface에서 세션을 시작하고, 자신이 만든 surface만 제어합니다. Terminal.app에서는 기존 tab/window를 사용하지 않고 항상 전용 새 window를 엽니다. 감지할 수 없는 호스트에서는 Terminal.app으로 안전하게 fallback합니다. 이미 독립적으로 실행 중인 임의의 CLI에는 사후 attach하지 않습니다.

## 지원 범위

| 환경 | 상태 | transport |
| --- | --- | --- |
| macOS + Ghostty | v0.0.5 미지원 | 현재 설치 가능한 1.3.1은 AppleScript surface 회귀가 있고 1.3.0은 이번 후보의 양성 runtime 근거가 없어 모든 버전을 surface 생성 전에 fail-closed |
| macOS + iTerm2 | 지원 | iTerm2 AppleScript 직접 제어 |
| macOS + Terminal.app | 지원 | Terminal AppleScript 직접 제어 |
| macOS의 다른 터미널 | fallback | 별도 adapter가 없으면 Terminal.app에서 시작 |
| Windows PowerShell / cmd | 지원 | PowerShell 7(`pwsh.exe`) 기반 전용 visible console; `ask`/`tell`/`sessions`/explicit prune·close |
| Linux 터미널 | 미지원 | [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)에서 별도 구현 |
| VS Code 통합 터미널 | 현재 비범위 | 전용 adapter가 필요하면 별도로 판단 |

macOS에서는 `TERM_PROGRAM`, `TERM`, `ITERM_SESSION_ID`, `TERM_SESSION_ID` 순으로 현재 호스트를 식별합니다. `--terminal ghostty|iterm2|terminal`로 명시 선택할 수 있고, 선택을 생략한 상태에서 호스트를 식별하지 못하면 내장 Terminal.app을 엽니다. Terminal.app은 복원되거나 기존에 열린 surface를 채택하지 않고 항상 전용 새 window를 만듭니다. 명시 선택한 adapter가 실패하면 다른 앱으로 조용히 우회하지 않고 오류를 반환합니다.

Ghostty의 AppleScript는 1.3에서 추가된 preview API이며 macOS Automation 권한이 필요합니다. [Ghostty 1.3.1에는 AppleScript로 만든 tab의 terminal surface가 초기화되지 않는 회귀](https://github.com/ghostty-org/ghostty/issues/12730)가 있고, 1.3.0은 이번 릴리스 후보에서 다시 확인한 양성 runtime 근거가 없습니다. 따라서 Agent Bridge v0.0.5는 명시적인 `--terminal ghostty`를 AppleScript 실행 전에 거부하고, Ghostty 환경 자동 감지는 지원되는 Terminal.app으로 대체합니다. `--terminal iterm2` 또는 `--terminal terminal`을 사용하세요. Terminal.app은 기존 tab이나 UI scripting을 사용하지 않고 native AppleScript로 항상 전용 새 window를 만듭니다.

Windows는 PowerShell 또는 cmd에서 호출할 수 있으며 PowerShell 7(`pwsh.exe`)이 설치되어 있어야 합니다. bridge는 absolute PATH entry에서 찾은 `pwsh.exe`의 절대 경로를 `CreateProcessW`에 전달하고, `CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED`로 전용 visible console을 만든 뒤 identity-bound handle을 기록한 후에만 실행을 재개합니다. 후속 입력과 explicit close는 managed session ID, PID 생성 시각, 실행 파일 identity가 모두 일치할 때만 전달합니다. npm provider shim은 `.exe`, `.ps1`, `.cmd`, `.bat` 순으로 찾고 PowerShell shim을 우선해 `%NAME%`의 `cmd.exe` 확장을 피합니다. Linux는 아직 미지원입니다. provider/session 계약은 공유하되 OS와 terminal transport는 각각 독립 모듈로 유지합니다. provider 간에도 transport 구현을 억지로 공통화하지 않습니다. 각 provider adapter가 공식 session messaging·follow-up·result identity를 우선 사용하고, upstream에서 제공하지 않는 플랫폼·버전에만 같은 의미론의 fallback을 소유합니다. upstream 지원이 추가되면 공통층을 늘리는 대신 해당 fallback을 삭제·교체합니다.

의미를 이해하고 완료 결과를 회수하는 provider는 다음 네 가지입니다.

- Codex: 세션별 `notify`
- Claude Code: 세션별 `Stop` hook
- Agy: 세션 로그와 완료 transcript
- Pi: 세션 전용 lifecycle 확장

Windows와 macOS는 같은 세션 계약을 구현하지만 provider transport와 v0.0.5의 authenticated live 검증 범위는 다릅니다. 아래의 `구현·CI 검증`은 정적·단위·CI 근거를 뜻하며, 별도 표기 없는 Windows provider를 authenticated runtime 검증 완료로 해석하면 안 됩니다.

| Provider | macOS transport | native Windows transport | authenticated Windows live 근거 |
| --- | --- | --- | --- |
| Codex | 0.149+ native queue availability gate + provider session notify; 명확한 사전 부재만 terminal follow-up | 동일한 queue/notify adapter + 필요할 때만 Windows console follow-up | v0.0.3 terminal-follow-up 경로는 exact MSVC artifact로 검증; native queue 경로는 미검증 |
| Claude Code | 지원 버전·backend·설정 gate를 모두 통과할 때 공식 cross-session `ListAgents`/`SendMessage` + `Stop` hook | Claude Code 2.1.234+의 공식 named-pipe cross-session `ListAgents`/`SendMessage` + `Stop` hook; 초기 prompt도 argv가 아닌 공식 메시지로 전달 | 구현·CI 검증, 이 후보의 authenticated Windows runtime은 미검증 |
| Agy | transcript/result monitor + provider-owned terminal follow-up | transcript/result monitor + Windows console follow-up; 다중행 prompt는 한 줄 JSON 문자열로 framing | 구현·CI 검증, 이 후보의 authenticated Windows runtime은 미검증 |
| Pi | session lifecycle extension + provider-owned terminal follow-up | lifecycle extension + Windows console follow-up; 다중행 prompt는 한 줄 JSON 문자열로 framing | 구현·CI 검증, 이 후보의 authenticated Windows runtime은 미검증 |

provider별 console follow-up은 각 adapter 내부에 격리되어 있으며, bridge 공통층이 provider payload나 결과 identity를 추측하지 않습니다. Claude의 공식 cross-session 기능을 runtime gate 때문에 사용할 수 없으면 terminal injection으로 우회하지 않고 실패합니다. Codex만 구버전 또는 native queue가 메시지를 받지 않았다고 명확히 확인된 경우에 한해 같은 claim을 terminal follow-up으로 전달합니다.

## 설치

소스에서 설치할 때는 Rust 1.97.1 이상이 필요합니다. macOS에서는 iTerm2 또는 Terminal.app이 필요하며 v0.0.5의 Ghostty adapter는 fail-closed입니다. Windows에서는 PowerShell 7이 필요합니다. 두 OS 모두 사용할 provider CLI를 먼저 직접 실행해 로그인과 초기 설정을 완료해야 합니다.

```sh
git clone --branch v0.0.5 --depth 1 https://github.com/jy1655/agent-bridge.git
cd agent-bridge
cargo install --path . --locked
agent-bridge --version
```

마지막 명령은 `agent-bridge 0.0.5`를 출력해야 합니다. 개발 중인 `main`이 아니라 릴리스 tag에서 설치해야 설치본과 소스의 경계가 명확합니다.

Windows 명령줄 한도를 넘는 요청은 `--prompt-file`로 전달합니다. 파일은 UTF-8 텍스트로 읽고 CRLF는 LF로 정규화하며, Agent Bridge가 원본을 삭제하거나 수정하지 않습니다. 단독 CR과 그 밖의 제출·escape 제어문자는 거부합니다.

기존 설치를 교체할 때는 `cargo install --path . --locked --force`를 사용합니다. 기본 설치 위치인 `~/.cargo/bin`이 `PATH`에 없다면 추가하거나 빌드한 바이너리를 절대 경로로 실행합니다.

GitHub Release에는 Apple Silicon macOS용 `agent-bridge-<version>-aarch64-apple-darwin.tar.gz`와 64비트 Windows용 `agent-bridge-<version>-x86_64-pc-windows-msvc.zip`을 게시하며, 각 archive와 같은 이름의 `.sha256` 파일을 함께 제공합니다. prebuilt archive 설치에는 Rust가 필요하지 않습니다. archive를 푼 뒤 macOS에서는 `agent-bridge`, Windows에서는 `agent-bridge.exe`를 `PATH`에 있는 디렉터리로 옮깁니다. 다운로드한 파일은 실행 전에 체크섬을 검증하세요.

```sh
shasum -a 256 -c agent-bridge-0.0.5-aarch64-apple-darwin.tar.gz.sha256
tar -xzf agent-bridge-0.0.5-aarch64-apple-darwin.tar.gz
./agent-bridge --version
```

```powershell
$archive = "agent-bridge-0.0.5-x86_64-pc-windows-msvc.zip"
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
agent-bridge sessions [--workspace PATH] [--provider <codex|claude|agy|pi>] [--state STATE]
    [--sort <id|updated>] [--json]
agent-bridge inspect <session> [--json]
agent-bridge result <session> [--latest | --list | --event EVENT | --request REQUEST] [--json]
    [--wait --timeout-secs N]
agent-bridge doctor <session> [--probe] [--json]
agent-bridge doctor --provider <codex|claude|agy|pi> [--probe] [--json]
agent-bridge prune-sessions --closed-before-days N --explicit [--json]
agent-bridge close-session <session> --explicit [--json]
agent-bridge --help | --version
```

### 상태 확인과 요청별 결과 회수

`inspect`는 저장된 상태·마지막 오류·생성/갱신 시각, 시작할 때 지정한 model/effort/yolo,
owner 프로세스 생존 여부와 결과·요청 주소를 보여줍니다. `configured`는 저장된 시작 설정이며
현재 provider 설정을 다시 읽은 값이 아닙니다. `owner_process_alive`도 PID 생존 관측일 뿐
provider 응답 가능 여부나 terminal 소유권 검증을 뜻하지 않습니다.

```sh
agent-bridge sessions --workspace . --provider claude --sort updated --json
agent-bridge inspect session-XXXXXXXX --json
agent-bridge result session-XXXXXXXX --list --json
agent-bridge result session-XXXXXXXX --latest --json
agent-bridge result session-XXXXXXXX --event event-123-456.json --json
```

`--sort updated`는 최근 갱신 순, 기본 정렬은 session ID 순입니다. `sessions --json`은 기존 배열
형태를 유지합니다. 새 조회 명령의 JSON 객체와 `ask/tell` 응답에는 `schema_version: 1`이 있습니다.
`result --list --json`의 `events`와 `requests`는 본문을 제외한 참조 목록입니다. 본문은 정확한
`--event` 또는 `--request`로 읽습니다. event ID는 `.json`을 포함한 보존 파일명입니다.

새 `ask/tell`은 provider에 보내기 전에 공개 `request_id`를 저장합니다. 이 ID는 provider의
session/turn ID 또는 내부 claim token을 대체하지 않습니다. `--detach`는 접수 뒤 반환하며
`request_state: accepted`, `result: null`입니다. 이미 완료됐을 수도 있으므로 실제 결과는
요청 ID로 조회합니다. timeout이나 전송 오류가 발생해도 접수 기록이 생성됐다면 stdout JSON에
session과 request ID를 반환하고 exit code는 0이 아닙니다. 접수 전 거부에는 요청 ID가 없습니다.

```sh
agent-bridge ask codex --workspace . --prompt "변경을 검토해줘" --detach --json
# 위 응답의 session과 request_id를 사용합니다.
agent-bridge result session-XXXXXXXX --request request-XXXXXXXX \
  --wait --timeout-secs 300 --json
```

Codex가 메인이든 Claude가 메인이든 같은 흐름을 사용합니다. 부모 도구의 제한 시간보다 짧은
`--timeout-secs`를 지정하거나 `--detach` 후 결과를 기다리면, 부모 도구가 종료되면서 영수증을
받지 못하는 일을 줄일 수 있습니다. 호출자가 사라져도 `inspect`와 `result --list --json`에서
저장된 요청 주소를 찾을 수 있습니다. 다음 요청이 먼저 끝나거나 응답 본문이 같아도 과거의
요청 ID는 그 요청에 연결된 결과만 반환합니다. 기존 기록에는 `request_id: null`을 표시하며
연결을 추측하지 않습니다. 닫힌 세션의 보존 결과도 조회할 수 있습니다.

| `request_state` | 의미 |
| --- | --- |
| `completed` / `failed` | provider가 검증한 완료 결과 또는 실패 결과가 게시됨 |
| `pending` | 해당 요청이 현재 claim을 보유하고 완료 결과를 기다리는 중 |
| `recovery_required` | 완료 journal이 남아 있어 게시 완료를 확정하지 않음 |
| `unresolved` | 게시된 결과 없이 claim이 해제됐거나 owner가 종료됨. 재전송이 안전하다는 뜻이 아님 |
| `unavailable` | 조회할 최근 결과가 없음 |
| `busy` / `unknown` | 일관된 snapshot을 아직 얻지 못했거나 오류 응답 시 상태 확인 실패 |

`--wait`는 정확한 `--request`에만 사용할 수 있습니다. 성공 결과를 회수하면 exit 0,
실패 결과·미해결 요청·복구 필요·timeout이면 JSON을 출력하고 nonzero로 끝납니다.
대기 없이 조회하면 exit 0은 조회 성공을 뜻하므로 결과의 `request_state`도 확인해야 합니다.
`--latest` 또는 `--event`에서 읽을 게시 결과가 없어도 nonzero로 끝납니다. 손상된 영수증은
`unreadable_requests`, 읽을 수 없는 요청 디렉터리는 `request_index_error`로 알리고 개별 event
조회는 유지합니다. `inspect`의 요청 목록에는 호출 출처 `source`도 남습니다.
JSON은 원문 채널이며 사람이 읽는 출력의 terminal 제어문자는 기존 정책대로 이스케이프합니다.

`inspect`와 `result`는 `--wait`에서도 파일·상태를 변경하거나 요청을 전송·재전송하지 않습니다.
완료 journal의 복구가 필요하면 기존 `sessions --workspace PATH`로 해당 workspace의 복구를
수행한 뒤 다시 조회합니다. `sessions`의 기존 복구 동작은 유지되므로 읽기 전용 명령은 아닙니다.
조회 timeout은 요청을 취소하거나 세션을 닫지 않습니다. 결과를 다음 작업의 참고 자료로 보낼 때는
정확한 결과를 조회해 출처와 함께 `--prompt-file`에 넣고 기존 `ask/tell`을 사용합니다.

### 기능 가용성과 다음 조치 진단

`doctor`는 특정 세션 또는 provider의 상태를 설명합니다. 기본 실행은 저장된 설정·상태·요청,
owner process의 생존·identity, terminal 기록, 설치 경로를 읽습니다. 세션을 지정하지 않으면
세션 저장소를 탐색하지 않습니다.

```bash
agent-bridge doctor session-XXXXXXXX --json
agent-bridge doctor session-XXXXXXXX --probe --json
agent-bridge doctor --provider codex --probe --json
```

현재 설치된 CLI 버전은 `--probe`가 있을 때만 `<provider> --version`으로 확인합니다.
Codex는 추가로 `codex app-server daemon version`을 실행합니다. 두 조회는 총 5초 예산을 공유하고
출력은 각 조회별 stdout·stderr 합산 64 KiB로 제한합니다. probe helper와 그 자식 process는 종료 시 회수하며 Windows batch shim의
임시 파일은 세션 디렉터리 밖에 둡니다. 모델 호출, 메시지 전송, daemon 시작, terminal 앱 제어,
설정 변경은 하지 않습니다. 외부 CLI를 실행하는 probe와 기본 로컬 관측은 출력의 `probe`로 구분합니다.

JSON의 `ok: true`와 exit 0은 진단 보고서를 만들었다는 뜻입니다. 각 `checks` 항목의
`availability`(`available` / `unavailable` / `unknown`), `reason_code`, `observed_unix_ms`,
`evidence`, `next_action`을 함께 읽어야 합니다. 각 항목은 이름에 명시된 조건만 설명하며,
모든 관측이 성공해도 메시지 전달·모델 실행·완료까지 보증하지 않습니다. 잘못된 인자는 nonzero입니다.
손상·경합 중인 저장 기록은 가능한 나머지 진단과 함께 미확인으로 보고하며 자동 복구하지 않습니다.

- Codex는 실제 sender와 같은 launch-version·thread identity·daemon 판정을 재사용합니다.
  시작 때 기록한 버전과 probe로 읽은 현재 버전은 별도로 표시하며, daemon 통과가 TUI의 thread
  표시나 queue 소비를 증명하지는 않습니다. 읽기 실패와 daemon 부재를 같은 사실로 취급하지 않습니다.
  probe 실행·해석 실패는 `unknown`, CLI가 nonzero로 응답해 native queue gate를 통과하지 못한
  경우는 `unavailable`입니다. 이 응답만으로 daemon process 자체의 부재를 단정하지 않습니다.
- Claude의 `crossSessionInbound: accept`는 저장된 설정입니다. 실제 backend·feature flag·정책·
  `ListAgents` discovery를 확인하지 않았으므로 공식 메시징의 현재 가용성은 `unknown`입니다.
  진단을 위해 messenger를 호출하거나 terminal 입력으로 우회하지 않습니다.
- Agy와 Pi는 각 adapter가 소유한 terminal fallback과 결과 확인 방식을 설명합니다.
  진단만으로 실제 입력 가능 여부를 확정하지 않습니다.

진행 중이거나 전송 여부가 불확실한 요청에는 같은 `request_id`의 `result` 조회를 안내합니다.
잠금을 오래 유지하지 않으며, 완료 journal 복구·재전송·close·trust/permission 변경은 수행하지 않습니다.
안내된 `next_action`도 자동 실행하지 않습니다. 저장된 model/effort/yolo는 `configured`,
상태와 정확한 active request 참조는 `observations`에서 확인할 수 있습니다.
복구 안내의 `next_command`는 실제 workspace를 담은 argv 배열이며, `sessions`는 journal 복구뿐
아니라 해당 workspace의 dead-owner 세션 정리도 수행합니다. 닫는 중인 handle과 소진된 tombstone은
단순 누락과 구별합니다. 삭제된 workspace는 별도 진단하며 버전 조회는 임시 디렉터리에서 계속할 수
있지만, Codex daemon 조회는 원래 workspace를 사용할 수 있을 때만 실행합니다.

## 권한과 세션 경계

- Agent Bridge는 같은 `ask` 작업에서 새로 만든 surface만 기록합니다. 새 handle은 managed session ID와 host가 제공하는 stable ID를 결합하며 `tell`과 `close-session` 직전에 다시 검증합니다. 지원되는 macOS iTerm2·Terminal.app 경로는 target `native-session` owner의 managed session ID·PID·controlling TTY device·process start fingerprint·foreground process group과 전용 login shell identity를 검증하고, surface가 보고하는 TTY도 owner와 일치해야 합니다. 시작 명령을 보내기 전에 handle을 내구성 있게 기록하고, 시작 실패 시 전체 timeout 안에 예약한 정리 구간에서 정확한 surface를 닫은 뒤 handle을 제거합니다. 정리가 실패한 경우에만 `launching`/`failed` 상태의 bound handle을 남겨 명시적 `close-session --explicit`이 stable ID로 회수할 수 있습니다. 비활성화된 Ghostty adapter 코드는 terminal·tab·window ID 복합체와 live owner 검증 경계를 유지하지만 v0.0.5에서는 선택될 수 없습니다. Windows 입력과 close는 console root와 `native-session` owner 각각의 PID 생성 시각·실행 파일 identity를 검증하고, console root의 검증된 process handle을 `AttachConsole`과 control이 끝날 때까지 유지해 PID 재사용을 fail-closed합니다. suspended console의 identity-bound handle은 private state에 내구성 있게 기록한 뒤에만 실행을 재개합니다. Windows Codex adapter는 정확한 canonical workspace를 provider process의 inherited current directory로 유지하고, verbatim 경로를 거부하는 Codex에는 의미가 달라질 수 있는 정규화 경로를 `-C`로 다시 전달하지 않습니다. 호출 당시 터미널을 재감지하거나 복원된 front/current/selected surface를 채택하지 않습니다.
- 새 세션의 `--model`, `--effort`, `--yolo`는 부모 CLI에서 추측하거나 상속하지 않습니다. 해당 `ask` 요청에 명시된 값만 사용합니다.
- `--yolo`는 Codex의 `--dangerously-bypass-approvals-and-sandbox`, Claude와 Agy의 `--dangerously-skip-permissions`를 전달합니다. Pi에서는 해당 실행의 project-local files를 신뢰하는 `--approve`를 전달하며 Pi 자체 tool 정책은 유지합니다.
- `tell`은 세션별 한 턴만 허용합니다. Claude Code 2.1.234 이상은 macOS와 native Windows 모두 별도의 비영속·격리 설정 print-mode Claude 프로세스에서 공식 `ListAgents`로 고유 managed session name을 찾고 `SendMessage`로 전달합니다. Claude의 공식 cross-session 기능은 macOS·Linux에서 2.1.224부터, native Windows에서 per-session named pipe를 사용하는 2.1.234부터 제공됩니다. messenger의 stdin JSON에는 본문 대신 요청별 짧은 전달 참조 `agent-bridge-payload:<Claude turn ID>`만 들어가며, 본문은 messenger의 argv에도 stdin에도 들어가지 않고 messenger model을 통과하지 않습니다. messenger model이 본문을 `SendMessage` 입력으로 옮겨 적는 방식에서는 provider가 그 응답을 도중에 중단하거나 입력을 거부하면 본문이 잘리거나 `SendMessage`가 실행되지 않기 때문입니다. 임시 `PreToolUse` hook(guard)은 정확한 local session name·요약·전달 참조를 실행 전에 검증하여 일치하면 공식 hook 출력 `updatedInput`으로 정확한 본문을 실제 실행 입력으로 넣고 일치하지 않으면 호출을 거부합니다. guard는 요청 하나에 `SendMessage` 호출을 한 번만 승인하며, 승인 기록을 먼저 남긴 뒤에만 승인합니다. 임시 `PostToolUse` hook은 Claude가 보고한 실제 실행 입력이 본문과 일치하는지를 실행 영수증으로 남깁니다. 같은 임시 설정의 `isolatePeerMachines`로 cross-machine 전송도 막습니다. 출력 stream에서 확인한 discovery와 단일 `SendMessage` 성공 결과, guard의 승인 기록, 실행 영수증이 모두 일치해야 전송 성공으로 인정합니다. messenger는 Unix process group 안에 격리하며, Windows에서는 정지 상태로 생성해 Job Object에 먼저 할당한 뒤 재개합니다. timeout 때 messenger의 하위 프로세스까지 종료하며, 재시도는 `SendMessage`가 실행되지 않았다고 증명된 시도에만 합니다. 증명 조건은 승인 기록과 실행 영수증이 모두 없고, 출력의 모든 `SendMessage` 호출이 각각 한 번만 나타나며 Claude의 차단 보고로만 끝났거나, `SendMessage` 호출이 전혀 없었던(discovery miss) 경우입니다. 차단 보고는 guard의 거부 사유가 그대로 담긴 오류 결과, 또는 provider가 응답을 중단해 그 호출이 실행되지 않았다고 Claude가 알리는 오류 결과의 두 가지이며, 그 밖의 오류 결과는 호출이 실행된 뒤에 나온 것일 수 있으므로 미전송의 증거로 보지 않습니다. Claude는 hook이 timeout을 넘기면 그 출력을 버리고 호출을 guard 없이 실행하며 `SendMessage`는 권한 승인 대상이 아니어서 그렇게 실행될 수 있으므로, guard 자신의 거부 결정도 증거로 쓰지 않습니다. 재시도는 첫 실패 시점부터 시작하는 5초 창과 전체 timeout 안에서만 하며, 끝내 전송하지 못한 `tell`은 turn claim을 해제하고 세션을 `ready`로 되돌리므로 같은 세션에 다시 `tell`할 수 있습니다. 대상 세션의 `Stop` hook은 요청별 고유 Claude turn ID와 정확한 최종 마커가 일치한 응답만 결과로 기록하고 반환값에서는 마커를 제거합니다. 마커가 다른 수동·비상관 턴에는 개입하지 않고 pending claim을 유지합니다. 이 provider 전용 상관관계 프로토콜은 Claude가 대상 turn identity를 공식 결과로 제공하면 교체할 경계입니다. 이 provider-native 전송에는 `tell`마다 별도의 Claude transport turn이 한 번 필요합니다. provider·feature-flag·정책 설정 때문에 공식 기능을 사용할 수 없으면 terminal injection으로 자동 전환하지 않고 전송 전에 실패합니다. 전송을 시도한 뒤 성공 여부를 확인할 수 없으면 중복 재전송을 막기 위해 turn claim을 유지하며, 정상적으로 진행 중인 `working` 세션과 구분할 수 있도록 그 이유를 세션 `status.json`의 `error`에 기록합니다. turn claim은 대상 결과가 도착하거나 `close-session --explicit`으로 닫을 때 해제됩니다. Codex 0.149 이상은 저장된 provider 버전, 기존 완료 event의 authoritative thread UUID, 실행 중인 0.149+ local app-server daemon을 확인한 뒤 `codex queue --thread <UUID> --message <TEXT>`를 사용합니다. queue 성공 출력은 메시지 수락만 증명하며 turn 완료로 간주하지 않습니다. 최종 완료는 기존 Codex notify가 같은 thread UUID와 현재 claim marker를 함께 증명할 때만 기록됩니다. 구버전, 확립되지 않은 thread, 사용할 수 없거나 호환되지 않는 local daemon, 또는 server가 미수락을 명확히 보고한 경우에만 terminal fallback으로 전환합니다. queue 실행 뒤 결과가 불명확하면 terminal로 재전송하지 않고 claim을 유지하며, queue/probe helper는 Unix process group 또는 Windows Job Object 안에서 실행해 timeout 시 하위 프로세스까지 종료합니다. Agent Bridge는 global daemon을 시작·종료하거나 remote target을 선택하지 않습니다. Pi·Agy는 각 provider adapter가 소유한 terminal paste fallback을 계속 사용합니다. Codex는 공식 notify의 `input-messages`에 든 claim marker로 turn을 확인해 exact-output 응답 본문을 바꾸지 않습니다. Pi의 세션 확장은 `before_agent_start`의 실제 입력에 현재 claim marker가 포함됐는지 확인하고 그 claim token과 함께 settled result를 제출하므로 exact-output 본문을 바꾸지 않으며, Agy는 claim token과 정확한 final marker가 일치한 결과만 수락합니다. Enter·ESC 같은 별도 터미널 동작을 만들 수 있는 제어문자는 거부합니다.
- Codex daemon probe는 native transport를 보수적으로 선택하는 gate이며 visible TUI가 그 thread를 현재 표시한다는 liveness 증거는 아닙니다. `run_tell`은 먼저 관리 중인 terminal owner가 살아 있음을 검증하지만, 수락된 항목은 Codex 자체 queue store에 내구성 있게 남고 현재 upstream 구현에서는 외부 항목 감지가 약 10초 poll 주기를 가질 수 있습니다. 완료 notify 전에 TUI를 닫으면 `close-session --explicit`은 upstream queue 항목을 취소하지 못하므로, 같은 thread UUID를 나중에 resume할 때 그 입력이 실행될 수 있습니다. 결과가 불확실한 turn은 중복 `tell`을 보내지 말고 기존 session을 확인해야 합니다.
- 모든 bridge 프롬프트에는 source provenance가 붙습니다. 사람이 읽는 결과의 터미널 제어문자는 가시적인 문자열로 이스케이프합니다.
- 결과가 돌아온 뒤 탭은 열린 채 유지되어 사용자가 직접 이어서 작업할 수 있습니다. terminal paste fallback을 쓰는 provider에서는 진행 중인 bridge 요청과 같은 탭의 수동 입력을 겹치면 수동 턴 결과가 bridge 요청의 결과로 먼저 인식될 수 있으므로 동시에 입력하지 않아야 합니다.
- `close-session`은 `--explicit`이 있어야 합니다. Terminal.app은 live `native-session` owner attestation과 전용 window ID·TTY가 모두 일치하고 owner가 현재 terminal foreground process group의 leader임을 확인합니다. 이어 같은 TTY의 실제 parent login shell이 별도 process-group leader이고 owner를 foreground group으로 보고하는지도 검증한 뒤, managed group에는 `SIGTERM`, 전용 shell group에는 `SIGKILL`을 보내 Terminal.app이 idle 전이를 관찰한 경우에만 전용 window를 닫습니다. 과거 owner record에 process-group·shell 필드가 없어도 PID·시작시각·parent 관계·TTY가 일치하는 live identity에서 같은 관계를 모두 증명해야 하며, terminal control character나 UI scripting은 사용하지 않습니다. close finality에서는 `terminal.json` handle을 `terminal.closed.json` tombstone으로 소진하며, 이미 `closed`인 세션의 반복 close는 terminal adapter를 호출하지 않습니다.
- `prune-sessions`도 `--explicit`이 있어야 합니다. `closed.json` 시각이 보존 기간보다 오래됐고 현재 status도 `closed`이며 terminal handle, pending resume, turn claim, live owner가 없는 관리 디렉터리만 삭제합니다. 열린 세션이나 판별할 수 없는 owner는 유지하며 자동 보존 기간이나 암묵적 삭제는 없습니다.
- 세션별 상태와 결과는 권한을 제한한 `~/.agent-bridge/native-sessions` 아래에 저장합니다. 상태·event·turn claim은 파일과 상위 디렉터리까지 동기화하고, 중간 완료 journal을 복구한 뒤 event·terminal 상태·claim 해제를 한 lifecycle lock 아래에서 수렴시킵니다. Windows는 사용자 지정 state root에서도 ACL 상속을 제거하고 현재 사용자 전용 ACL을 적용합니다. provider의 전역 설정이나 workspace hook 파일은 수정하지 않습니다. Agent Bridge가 만든 Claude 세션의 private `--settings` 파일에는 모든 지원 OS에서 `crossSessionInbound: "accept"`와 `Stop`·`StopFailure` hook을 기록합니다. Messenger의 임시 hook 설정(`PreToolUse` guard와 `PostToolUse` hook), 기대 본문 파일, 승인 기록, 실행 영수증은 요청마다 별도 파일로 만들며, 해당 `tell` 동안만 같은 private 세션 디렉터리에 존재하고 종료 시 제거합니다. 요청별로 나누는 이유는, 대상 세션이 전달된 턴을 먼저 끝내면 이전 `tell`이 정리를 마치기 전에 다음 `tell`이 시작될 수 있어 이전 `tell`의 정리가 다음 요청의 파일을 건드리지 않게 하기 위해서입니다. 같은 요청의 중단된 이전 시도가 남긴 승인 기록과 실행 영수증은 새 시도를 시작할 때 먼저 지웁니다. 요청별 pending turn 레코드는 claim token에 묶이며 다음 turn 준비 시 원자적으로 교체되므로, 완료 직후 정리와 다음 claim 설치가 경합하지 않습니다.

최소 지원 버전은 다음과 같습니다. 더 새로운 버전은 허용합니다.

| Provider | 최소 버전 |
| --- | --- |
| Codex | 0.147.0 |
| Claude Code | 2.1.234 |
| Agy | 1.1.12 |
| Pi | 0.84.1 |

Codex 0.147.x와 0.148.x는 계속 지원하지만 `tell`에는 provider-owned terminal fallback을 사용합니다. Native `codex queue` 선택 기준은 Codex CLI와 실행 중인 local app-server가 모두 0.149.0 이상인 경우입니다. Codex 0.153.2까지의 native Windows 배포에는 local daemon lifecycle이 없으므로 해당 버전에서는 이 gate가 닫히고 Windows console follow-up을 사용합니다. 이후 Windows daemon 지원 버전도 authenticated LIVE 확인 전에는 검증된 것으로 간주하지 않습니다.

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

## 0.0.5 업데이트

0.0.5는 Claude 세션에 보내는 후속 `tell`이 `could not confirm delivery` 오류를 내고, 대상에는 아무것도 전달되지 않았는데 세션이 `working`에 남던 결함을 수정합니다. 0.0.4의 messenger는 본문 전체를 `SendMessage` 입력으로 옮겨 적어야 했습니다. Claude Code 2.1.276부터 2.1.278까지에서 provider가 그 응답을 도중에 중단하거나 messenger 입력을 거부하면 호출이 실행되지 않았습니다. Agent Bridge는 messenger 출력에 `SendMessage` 호출이 보인다는 이유로 그 시도를 전달 불확실로 분류해 turn claim을 유지했습니다.

이제 본문은 messenger model을 통과하지 않으며, messenger는 요청별 전달 참조만 제시합니다. guard가 수신자·요약·전달 참조를 검증한 뒤 공식 hook 출력 `updatedInput`으로 정확한 본문을 실행 입력에 넣고, 요청 하나에 호출을 한 번만 승인합니다. 임시 `PostToolUse` hook의 실행 영수증으로 Claude가 실제로 실행한 입력이 본문과 같은지 확인합니다. 재시도는 Claude가 출력의 모든 `SendMessage` 호출을 차단 보고로 끝냈거나 호출이 전혀 없었던 시도에만 수행하며, 끝내 전송하지 못하면 turn claim을 해제하고 세션을 `ready`로 되돌립니다. 그 밖의 미확정 시도는 중복 전달을 막기 위해 turn claim을 유지하고 그 이유를 세션 `status.json`의 `error`에 기록합니다. messenger의 임시 파일은 요청마다 따로 만듭니다.

macOS에서 Claude Code 2.1.278과 iTerm2로 수행한 authenticated LIVE에서, 설치된 0.0.4는 같은 결함을 재현했습니다(`tell` 1회 중 1회 실패, `working` 고착). 수정 빌드는 `tell` 20회를 모두 전달했으며, 짧은 프롬프트, 5.5 KB 산문, 적대적 본문, `--detach` 호출을 포함해 대상 transcript에서 본문이 바이트 단위로 일치했습니다. native Windows runtime은 `NOT-VERIFIED`이며, Windows의 초기 prompt도 같은 messenger를 사용합니다. Codex·Agy·Pi adapter는 변경하지 않았습니다. 자세한 내용과 검증 경계는 [0.0.5 릴리스 노트](docs/releases/0.0.5.md)에 있습니다.

## 0.0.4 업데이트

0.0.4는 [Issue #30](https://github.com/jy1655/agent-bridge/issues/30)의 Codex 0.149+ native queue follow-up을 추가합니다. 저장된 provider 버전과 authoritative thread UUID를 확인하고 호환되는 local app-server daemon이 실행 중일 때 `codex queue`로 후속 메시지를 전달합니다. 명확한 pre-enqueue 거부만 기존 terminal follow-up으로 전환하며, queue 실행 뒤 수락 여부가 불명확하면 중복 전송을 막기 위해 claim을 유지합니다. 완료는 같은 thread UUID와 현재 claim marker를 포함한 공식 Codex notify로만 확정합니다.

queue probe와 제출 helper는 전체 `tell` deadline 안에서 실행되고, Unix process group 또는 Windows Job Object로 자식 process tree까지 회수하며 stdout·stderr 합산 1 MiB 상한을 둡니다. Agent Bridge는 global Codex daemon을 시작·종료하지 않습니다. 수락된 upstream queue 항목은 bridge session을 닫아도 취소되지 않으며, 같은 Codex thread를 나중에 resume할 때 실행될 수 있습니다.

macOS Codex 0.153.2+iTerm2에서는 실제 daemon queue가 대상 thread에서 `0 → 1 → 0`으로 수락·소비되고, 동일 thread의 서로 다른 turn 두 개가 정확한 결과로 상관된 뒤 `ready`로 복귀하는 것을 authenticated LIVE로 확인했습니다. Codex 0.153.2 native Windows에는 local daemon lifecycle이 없어 terminal fallback을 유지하며, Windows native queue와 Job Object runtime은 `NOT-VERIFIED`입니다.

## 0.0.3 업데이트

0.0.3은 native Windows를 macOS와 같은 관리형 세션 계약으로 배포하는 첫 릴리스입니다. `cmd.exe`, Windows PowerShell 5.1, PowerShell 7에서 호출 shell과 분리된 PowerShell 7 visible console을 사용합니다. Codex의 전체 흐름은 exact MSVC artifact로 live 검증했고, Claude·Agy·Pi의 Windows adapter는 구현·CI 검증 범위이며 authenticated Windows runtime 검증은 아직 남아 있습니다. Claude Code 2.1.234+의 공식 native-Windows cross-session messaging으로 구형 resume supervisor를 대체했고, 모든 provider의 turn correlation, crash-recovery journal, 단조 상태 전이, 전체 timeout 예산, PID/TTY 소유권 검증을 보강했습니다.

GitHub Release는 Apple Silicon macOS와 64비트 Windows archive 및 SHA-256 checksum을 함께 게시합니다. Linux transport, 독립 실행 중인 CLI에 대한 사후 attach, provider 간 workspace trust 공유는 이번 릴리스 범위에 포함되지 않습니다.

Release workflow의 immutable-release preflight는 Actions secret `IMMUTABLE_RELEASES_READ_TOKEN`에 대상 저장소의 fine-grained `Administration: read` 권한이 있어야 합니다. 기본 `GITHUB_TOKEN`은 이 설정 조회 권한을 보장하지 않으며, secret 부재·권한 부족·설정 비활성화는 모두 publication 전에 실패합니다.

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

### 릴리스 리허설

Release workflow는 `v*` tag push로 실행되며 그 경로만 릴리스를 만들고 게시합니다. 같은 workflow를 `workflow_dispatch`로 실행하면 이미 있는 tag에 대한 리허설이 됩니다. 리허설은 tag 검증, macOS·Windows test, 두 release archive의 build, checksum과 archive 구성 검증까지 모두 실행하지만 릴리스를 만들거나 게시하지 않습니다.

tag 검증은 tag가 원격에서 annotated tag인지, tag가 가리키는 commit이 checkout된 commit과 같고 `main`에 포함되는지, tag·Cargo version·릴리스 노트 파일이 일치하는지를 확인합니다. annotated tag 여부와 tag의 commit은 로컬 ref가 아니라 GitHub API로 확인합니다. 그 이유는 두 가지로, tag push에서 기본 `actions/checkout`은 로컬 tag ref를 commit으로 바꿔 놓고, credential을 남기지 않는 checkout 뒤에는 private 저장소에서 다시 fetch할 수 없기 때문입니다. tag 이름은 검증 job만 해석하며, test·build·게시 job은 tag 이름이 아니라 검증된 commit을 checkout합니다. 실행 도중 tag가 옮겨질 수 있으므로, 게시 job은 draft를 만들기 직전과 게시 직전에 원격 tag가 여전히 검증된 tag object인지 다시 확인하고 달라졌으면 그 작업(draft 생성 또는 게시)을 하지 않고 실패합니다. 게시 직후에도 같은 확인을 하며, 이때 달라졌으면 실행은 실패로 끝나지만 이미 이루어진 게시는 되돌릴 수 없습니다. 이 확인들은 권한이 있는 다른 사용자가 같은 순간에 tag를 옮기는 것을 막는 원자적 잠금이 아닙니다.

```sh
gh workflow run release.yml -f tag=v0.0.5
gh workflow run release.yml --ref <branch> -f tag=v0.0.5
```

첫 명령은 `main`의 workflow로 리허설하며, 둘째 명령은 workflow를 고친 branch의 workflow로 리허설하여 merge 전에 변경을 검증할 때 씁니다. Release workflow나 release packaging을 바꿨을 때는 새 tag를 push하기 전에 리허설을 먼저 실행합니다. 리허설에서는 `IMMUTABLE_RELEASES_READ_TOKEN` secret이 없어도 경고만 남기고 계속하지만, 실제 tag push에서는 같은 secret이 없거나 저장소의 immutable releases 설정을 확인할 수 없으면 test와 build를 시작하기 전에 실패하고 게시 직전에 같은 확인을 다시 합니다. 게시 단계는 draft에 올라간 asset의 이름과 digest를 build 결과와 대조한 뒤 게시하고, 게시된 릴리스가 immutable인지 확인합니다.

## License

[MIT](LICENSE)
