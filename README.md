# Agent Bridge

로컬에 설치되고 로그인된 `codex`, `claude`, `agy`, `pi` CLI를 **실제 iTerm 탭**에서 연결하는 로컬 브리지입니다. API 키나 세션 토큰을 대신 소유하지 않고, 각 CLI의 기존 로그인·설정·대화형 UI를 그대로 사용합니다.

실행 중인 CLI가 새 iTerm 탭을 열어 다른 지원 CLI에 작업을 맡기고 결과를 돌려받을 수 있습니다. 그 탭은 첫 응답 뒤에도 닫히지 않으므로 브리지가 후속 프롬프트를 보낼 수도 있고, 사용자가 직접 탭을 선택해 그대로 이어서 작업할 수도 있습니다.

기존의 다중 PTY TUI는 호환 경로로 남아 있지만, 새 기본 사용 흐름은 `ask` / `tell` / `sessions` / `close-session`입니다.

## 네이티브 iTerm 브리지

현재 네이티브 브리지는 macOS + iTerm2를 대상으로 합니다. 먼저 바이너리를 빌드하고 PATH에 두거나 절대 경로로 실행합니다.

```sh
cargo build --release
./target/release/agent-bridge ask claude \
  --workspace ~/Dev/project \
  --title "Claude reviewer" \
  --prompt "이 변경을 검토하고 결과만 요약해줘"
```

`--workspace`를 생략하면 호출한 현재 디렉터리를 사용하고, `--title`을 생략하면 CLI 이름과 workspace 이름으로 탭 제목을 만듭니다.
Codex, Claude, Agy, Pi 네 provider 모두 `--model <MODEL>`로 해당 요청에만 모델을 지정할 수 있습니다. 네 provider 모두 `--effort <EFFORT>`도 지원하며 Codex의 `model_reasoning_effort`, Claude/Agy의 `--effort`, Pi의 `--thinking`으로 전달합니다. 지정하지 않으면 각 CLI의 기존 세션 기본값을 유지하고 어떤 CLI의 전역 설정도 바꾸지 않습니다. model과 effort 값은 선택한 CLI·모델이 최종 검증하므로 새 버전에서 추가된 값을 브리지가 임의로 차단하지 않습니다.

`ask`는 다음 순서로 동작합니다.

1. 절대 PATH 항목에서 요청한 `codex`, `claude`, `agy`, `pi` 실행 파일을 찾아 canonical 절대 경로로 고정하고 최소 지원 버전을 확인합니다. PATH 자체는 사용자가 신뢰한 실행 환경으로 간주합니다.
2. iTerm에 실제 탭을 만들고 지정한 workspace에서 해당 대화형 CLI를 실행합니다.
3. Codex `notify`, Claude `Stop` hook, Agy의 세션 transcript, Pi의 세션 전용 lifecycle 확장 중 해당 provider의 계약으로 결과를 받아 호출자에게 반환합니다.
4. CLI와 iTerm 탭은 그대로 유지합니다.

기계 판독이 필요하면 `--json`을 사용합니다. 반환된 `session` id로 같은 탭에 다음 프롬프트를 실제 키 입력처럼 전달할 수 있습니다.

```sh
agent-bridge ask codex --workspace ~/Dev/project --model gpt-daybreak-blue-latest --effort xhigh --prompt "원인을 진단해줘" --json
agent-bridge tell session-XXXXXXXX --prompt "그중 2번만 수정해줘" --json
agent-bridge sessions --json
```

호출자가 기다리지 않고 탭만 열려면 `--detach`를 붙입니다. 기본 결과 대기 시간은 900초이며 `--timeout-secs`로 조정합니다. 플랫폼의 monotonic `Instant`가 표현할 수 없는 timeout은 탭이나 요청을 만들기 전에 거부합니다. 대기가 실패하거나 시간 초과되어도 이미 열린 탭은 닫지 않습니다.

### 권한과 버전 정책

- 새 자식 세션의 `--yolo`는 **해당 `ask` 요청에 명시된 경우에만** 적용합니다. 부모 CLI가 yolo로 실행 중이어도 자동 상속하지 않습니다.
- `--model`과 `--effort`도 부모 CLI에서 추측하거나 상속하지 않습니다. 해당 `ask`에 명시한 값만 새 세션 시작 인자로 전달하며, 실행 중인 세션의 effort를 `tell`로 바꾸지는 않습니다.
- `ask`와 `tell`로 주입하는 모든 프롬프트에는 호출한 브리지 세션의 provenance를 강제로 붙입니다. `tell`은 세션별 한 턴만 허용하고 bracketed paste로 전송하며, Enter·ESC 등 별도 터미널 동작을 만들 수 있는 제어문자는 거부합니다.
- 사용자는 결과가 반환된 뒤 열린 탭에서 그대로 작업을 이어갈 수 있습니다. 다만 provider가 수동 입력과 bridge 입력을 권위 있게 대응시키는 공통 신호를 제공하지 않으므로, 진행 중인 `ask`/`tell`과 같은 탭의 수동 입력을 겹치지 않아야 합니다. 겹치면 먼저 끝난 수동 턴이 대기 중인 bridge 결과로 인식될 수 있습니다.
- 명시적 `--yolo`는 Codex의 `--dangerously-bypass-approvals-and-sandbox`, Claude와 Agy의 `--dangerously-skip-permissions`를 전달합니다.
- Pi는 원래 내장 승인 팝업이나 sandbox가 없으므로 브리지가 별도 권한 계층을 만들지 않습니다. Pi에서 `--yolo`는 허용되지만 추가 인자를 전달하지 않는 no-op이며, project trust를 대신 승인하는 `--approve`도 합성하지 않습니다.
- 최소 지원 버전은 Codex `>= 0.147.0`, Claude `>= 2.1.229`, Agy `>= 1.1.12`, Pi `>= 0.84.1`입니다. 정확 버전 고정이 아니므로 더 새로운 버전도 허용합니다.
- 브리지가 연 탭을 닫는 명령은 `agent-bridge close-session <session> --explicit`처럼 명시적 확인 플래그가 있어야 실행됩니다. 기록된 iTerm session id와 정확히 일치하는 세션만 대상으로 하며, 탭이 이미 사라졌거나 launch 실패로 `terminal.json`이 없는 경우에도 명시적 close는 idempotent하게 기록을 `closed`로 만들고 turn claim을 해제합니다.
- 네이티브 세션 프로세스의 PID를 별도로 기록합니다. 탭 수동 종료나 SIGHUP 뒤 그 프로세스가 죽은 것이 확인되면 `tell`, `sessions`, 결과 대기가 stale `running`/`working` 상태와 claim을 복구합니다. `turn.claim`을 만든 짧게 사는 `tell` 호출자의 PID만으로 stale 여부를 판단하지 않습니다.
- 세션 메타데이터와 결과는 `~/.agent-bridge/native-sessions` 아래의 세션별 비공개 디렉터리에 저장하며, 종료 후에도 과거 디렉터리와 결과를 보존합니다. 전역 CLI 설정이나 workspace hook 파일은 수정하지 않습니다. Agy adapter는 세션별 로그에서 매번 가장 최신의 `Created conversation` id를 얻어(따라서 `/clear` 뒤 새 대화로 전환) Agy가 생성한 `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl`을 읽기만 하며, Pi adapter는 세션 비공개 디렉터리의 결과 회수 확장만 `--extension`으로 로드합니다.
- 최초 `ask` 프롬프트는 각 CLI의 대화형 시작 인자로 전달되므로 실행 중 같은 머신의 프로세스 인자 검사에서 보일 수 있습니다. iTerm에 입력하는 launch command의 workspace·state root·실행 파일 등 동적 구성요소는 terminal control 문자를 거부하고, 일반 shell metacharacter는 한 인자로 quote합니다. `tell` 프롬프트는 권한 `0600` 임시 파일을 iTerm 입력으로 전달하고 즉시 폐기하며, 사람이 읽는 결과 출력에서는 터미널 제어문자를 가시적인 문자열로 이스케이프합니다.

```sh
agent-bridge ask claude --workspace ~/Dev/project --model MODEL --effort high --prompt "테스트까지 실행해줘" --yolo
agent-bridge ask agy --workspace ~/Dev/project --model MODEL --effort high --prompt "취약점을 검토해줘"
agent-bridge ask pi --workspace ~/Dev/project --model MODEL --effort high --prompt "이 변경을 검토해줘"
agent-bridge close-session session-XXXXXXXX --explicit
```

> [!WARNING]
> Codex, Claude, Agy에서 `--yolo`는 해당 CLI의 승인·sandbox 우회 플래그입니다. 브리지는 요청 단위의 명시 여부와 탭/경로 경계만 통제하며, yolo 세션 내부의 명령 실행을 다시 sandbox하지 않습니다. Pi는 `--yolo` 여부와 관계없이 Pi 자체의 권한 모델을 그대로 사용합니다.

### 수동 네이티브 release smoke

provider별 ignored smoke는 실제 iTerm 탭을 열고 first-party 로그인 세션을 사용하며, 지정한 model/effort flag와 결과 회수 계약을 함께 확인합니다. 일반 `cargo test`에서는 실행되지 않습니다. 실행할 provider가 지원하는 값을 환경변수에 넣고 **한 테스트만** 수동 실행하세요. 테스트는 탭을 자동으로 닫지 않으므로 확인 후 `close-session --explicit`을 사용합니다.

```sh
AGENT_BRIDGE_LIVE_CODEX_MODEL=MODEL AGENT_BRIDGE_LIVE_CODEX_EFFORT=medium \
  cargo test --test native_live live_native_codex_forwards_flags_and_returns_result -- --ignored --exact --nocapture
AGENT_BRIDGE_LIVE_CLAUDE_MODEL=MODEL AGENT_BRIDGE_LIVE_CLAUDE_EFFORT=high \
  cargo test --test native_live live_native_claude_forwards_flags_and_returns_result -- --ignored --exact --nocapture
AGENT_BRIDGE_LIVE_AGY_MODEL=MODEL AGENT_BRIDGE_LIVE_AGY_EFFORT=high \
  cargo test --test native_live live_native_agy_forwards_flags_and_returns_result -- --ignored --exact --nocapture
AGENT_BRIDGE_LIVE_PI_MODEL=MODEL AGENT_BRIDGE_LIVE_PI_EFFORT=medium \
  cargo test --test native_live live_native_pi_forwards_flags_and_returns_result -- --ignored --exact --nocapture
```

## 레거시 PTY TUI 실행

### Windows

Rust 및 Visual C++ Build Tools가 설치된 Developer PowerShell에서:

Windows에서 최신 네이티브 Claude Code를 임베드하려면 bundled ConPTY transport를 제공하는 `node-pty`가 필요합니다. Claude CLI 자체는 공식 네이티브 설치본을 그대로 사용합니다.

```powershell
npm install -g node-pty
```

```powershell
cargo run -- D:\Dev
```

workspace를 생략하면 `agent-bridge`를 실행한 현재 디렉터리를 사용합니다. `F3` 생성 화면에서는 기본 경로를 `Ctrl+U`로 지운 뒤 다른 디렉터리를 입력해 탭마다 별도 workspace를 선택할 수 있습니다. 탭을 재시작해도 선택한 workspace가 유지됩니다.

승인 확인과 sandbox 보호를 우회해야 하는 명시적인 작업에서는 `--yolo` 또는 `-yolo`를 workspace 앞이나 뒤에 지정할 수 있습니다. 이 mode는 실행 중 만드는 모든 새 탭과 restart 세션에도 유지됩니다.

```powershell
cargo run -- --yolo D:\Dev
agent-bridge D:\Dev --yolo
```

> [!WARNING]
> `--yolo`는 새로 만드는 모든 Codex, Claude, Agy 세션에 각 CLI의 공식 위험 플래그를 전달합니다. 해당 세션은 명령 실행 승인을 묻지 않고 sandbox 제한도 우회할 수 있으므로, 신뢰하는 코드와 workspace에서만 사용하세요. 플래그를 생략한 기본 모드의 승인 흐름은 바뀌지 않습니다.

릴리스 바이너리:

```powershell
cargo build --release
.\target\release\agent-bridge.exe D:\Dev
```

### macOS / Linux

Rust 툴체인만 있으면 됩니다. `node-pty`는 Windows에서 Claude를 임베드할 때만 필요하며 macOS/Linux에서는 설치하지 않습니다. 각 CLI(`codex`, `claude`, `agy`)는 PATH에서 찾습니다.

```sh
cargo run -- ~/Dev
```

릴리스 바이너리:

```sh
cargo build --release
./target/release/agent-bridge ~/Dev
```

workspace 선택과 `--yolo` 동작은 위 Windows 설명과 동일합니다.

## 설정 (선택)

`~/.agent-bridge/agents.json`으로 내장 3개 CLI의 실행 명령·역할 표시·기본 추가 인자를 재정의할 수 있습니다. 파일이 없으면 기본값으로 동작하고, 파싱 실패나 알 수 없는 에이전트 이름이 있으면 파일 전체를 무시하고 기본값으로 기동하며 헤더에 사유를 표시합니다. 신규 에이전트 종류 추가는 아직 지원하지 않습니다.

이 파일은 같은 OS owner가 관리하는 **신뢰된 설정**입니다. non-yolo 모드에서는 알려진 직접 danger flag뿐 아니라 우회 mode를 선택할 수 있는 Codex `--profile`/`-p`와 Claude `--settings` indirection도 `args`에서 차단합니다(Agy에 존재하지 않는 `--settings` 계약은 가정하지 않습니다). 그러나 `command`에 지정한 임의 wrapper가 내부에서 어떤 인자를 추가하는지는 브리지가 강제할 수 없습니다. 신뢰하지 않는 wrapper를 설정하지 말고, 강제 sandbox 경계가 필요하다고 가정하지 마세요.

```json
{
  "agents": {
    "claude": { "command": "/opt/claude/claude" },
    "agy": { "args": ["--effort", "high"] }
  }
}
```

## 레거시 TUI Visible delegation

탭 안에서 도는 에이전트는 자신이 Agent Bridge 안에 있음을 env(`AGENT_BRIDGE_REQUESTS`, `AGENT_BRIDGE_TAB`)로 알 수 있고, 같은 바이너리의 서브커맨드로 **보이는 탭**에 다른 CLI를 열어 위임할 수 있습니다. 백그라운드 자식 프로세스 대신 사용자가 전 과정을 화면에서 관전합니다:

```sh
agent-bridge open codex --workspace ~/Dev/x --title Reviewer --prompt "이 diff 리뷰해줘"
agent-bridge wait Reviewer --until finished --timeout-secs 900
agent-bridge read Reviewer --lines 200        # 스크롤백 포함 최근 200줄
agent-bridge prompt Reviewer --wait "테스트도 돌려줘"   # 제출+완료 대기 원자 결합
agent-bridge list                             # 전체 탭: 제목·상태·에이전트·workspace
agent-bridge close Reviewer --explicit
```

모든 서브커맨드는 `--json`으로 기계 판독 출력을 지원합니다. TUI를 `agent-bridge --restore .`로 띄우면 저장된 레이아웃을 기동 즉시 복원합니다.

- 주입되는 모든 프롬프트에는 `[Agent Bridge delegation · from <탭>]` provenance가 강제로 붙습니다.
- 전송은 프로세스마다 무작위로 독점 생성한 권한 `0700` 파일 스풀(요청/응답 JSON)로 이루어지며 1초 주기로 수거됩니다. 응답은 무작위 임시 일반 파일에서 원자적으로 교체하므로 고정 `.part` 심볼릭 링크를 따라가지 않습니다.
- 레거시 `close`도 `--explicit`이 없으면 거부합니다. 지연 전송은 탭 제목뿐 아니라 세션 세대에도 묶여, 같은 제목으로 다시 연 탭에 이전 프롬프트가 전달되지 않습니다.
- `open --prompt`는 CLI 기동을 고정 지연(약 2.5초)으로 기다린 뒤 주입합니다 — 단문 프롬프트를 권장하며, 정교한 제어는 `open` → `wait --until idle` → `prompt` 순서를 쓰세요.
- 별도 스킬·지침 문서 없이도 `agent-bridge --help`가 서브커맨드·env 컨텍스트·상태 의미·왕복 예제를 담은 에이전트용 레퍼런스입니다 (서브커맨드 뒤 `--help`도 동일 출력). 에이전트 지침(CLAUDE.md/AGENTS.md)에는 "위임은 `agent-bridge open` 사용 — 자세한 건 `agent-bridge --help`" 한 줄이면 충분합니다.
- **Agent Bridge 밖에서도 호출 가능**: TUI가 실행 중이면 `~/.agent-bridge/instance.json` 포인터를 통해 일반 터미널의 Claude/Codex도 같은 서브커맨드로 그 TUI 창에 탭을 만들 수 있습니다. 포인터가 가리키는 private spool이 남아 있어도 기록된 TUI PID가 죽었으면 즉시 stale 오류를 반환합니다. TUI가 없으면 명확한 오류("no running Agent Bridge found")가 나며, TUI를 대신 띄워주지는 않습니다(보이는 탭 원칙). 포인터는 마지막에 뜬 인스턴스를 가리키고 정상 종료 시 정리됩니다.

## 키

- `F12`: 앱 안에서 전체 단축키 도움말 열기
- `Ctrl+F11` (`F11`도 가능): 다음 키 하나를 Agent Bridge 단축키 처리 없이 활성 CLI로 전달. Windows Terminal이 plain `F11`을 전체화면 전환으로 소비하므로 `Ctrl+F11`을 권장
- `F3`: 새 탭 열기 (`←`/`→`로 CLI 선택, workspace 경로 입력, `Ctrl+U`로 기본 경로 지우기, `Enter`로 생성). 직전 실행의 탭 구성이 `~/.agent-bridge/last-layout.json`에 저장되어 있으면 `Ctrl+L`로 한 번에 복원 (없어진 디렉터리는 건너뜀, `--yolo` 여부는 현재 실행 모드를 따름)
- `F1`: 현재 workspace의 tracked `git diff HEAD` 읽기 전용 보기
- `F4`: 활성 탭 종료
- `F5` / `F6`: 이전/다음 탭으로 이동
- `F7`: 탭 제목과 보관된 터미널 출력 검색. 스크롤백 매치는 해당 위치로 점프해 스크롤백 모드로 표시 (`Esc`로 라이브 복귀)
- `F2`: 현재 탭의 최근 visible terminal context(최대 6,000자), tab/workspace provenance, 사용자가 입력한 요청을 다른 탭에 handoff (탭 2개 이상 필요, 첫 `Enter`로 context를 캡처하고 두 번째 `Enter`로 전송)
- `F8`: 스크롤백 모드 (`↑`/`↓`, `PageUp`/`PageDown`, `Home`/`End`, `Esc`로 복귀). 보관 줄 수는 기본 2,000이며 `AGENT_BRIDGE_SCROLLBACK`(1~100,000)으로 조정
- 마우스: 좌측 레일에서 세션 클릭으로 전환(F2 handoff 중에는 대상 선택), 레일 위 휠로 이전/다음 세션 이동, F3 화면의 CLI 칩 클릭 선택, F1 diff 뷰는 휠로 스크롤. CLI가 마우스 리포팅을 요청하면 터미널 영역 안의 이벤트는 그대로 CLI에 전달되고, 요청하지 않으면 터미널 위 휠은 스크롤백 이동. 마우스 캡처 중 Windows Terminal의 텍스트 선택·복사는 `Shift`를 누른 채 드래그
- `F9`: 종료된 탭을 같은 CLI·이름으로 새 세션 재시작
- `F10`: Agent Bridge 종료
- Agent Bridge가 예약하지 않은 키: 활성 에이전트 PTY에 그대로 전달 (`Ctrl+F11`로 예약 키도 전달 가능)
- 릴레이 입력 중 `←` / `→`: 대상 변경, `Enter` 두 번: 검토 후 전송, `Esc`: 취소

F2 handoff는 source CLI의 현재 화면에 보이는 최근 응답과 대화 문맥만 캡처합니다. 전체 transcript나 숨겨진 세션 상태를 읽지 않으며, target 변경이나 요청 편집 시 캡처를 폐기하고 다시 검토합니다. 민감한 terminal 출력이 보이는 경우 전송 전에 source 화면과 캡처 문자 수를 확인하고 취소하세요.

앱은 처음에 `Codex 1`만 엽니다. `F3`을 반복해서 원하는 구성을 만드세요. 마지막 탭을 닫아도 앱은 유지되며 다시 `F3`으로 세션을 만들 수 있습니다.

## 상태와 알림

- Claude 세션은 세션별 임시 `--settings` hook을 사용해 `working`, `waiting`, `idle`, `finished`를 표시합니다. `waiting`은 Claude의 Notification hook 전체에 반응합니다. 전역 Claude 설정은 수정하지 않으며 임시 파일은 탭 종료 시 삭제합니다.
- Codex는 세션별 `-c notify` 주입으로 공식 `agent-turn-complete` 이벤트를 받아 턴 종료 시에만 `finished`를 표시합니다. 대기(waiting) 신호는 Codex notify hook에 공식 계약이 없어 계속 표시하지 않으며, `finished` 표시는 해당 탭에 Enter로 새 입력을 보내면 해제됩니다.
- Agy는 신뢰할 수 있는 이벤트 계약이 확인되지 않아 PTY에서 관측한 `active`, `quiet`, `exited`, `unknown`만 표시합니다. `quiet`은 의미상 완료나 대기를 뜻하지 않습니다.
- `AGENT_BRIDGE_NOTIFICATIONS=1`을 설정하면 Claude가 `waiting` 또는 `finished`로 전이할 때 터미널 벨과 앱 내 알림을 냅니다. 알림은 승인 동작을 수행하지 않습니다.

## 설계 근거

세션 UX는 오픈소스 경쟁 프로젝트를 비교해 결정했습니다. 자세한 비교와 이번 버전에 반영한 범위는 [docs/benchmark.md](docs/benchmark.md)에 있습니다.

## 원칙

- 인증과 provider 세션은 각 CLI가 소유합니다.
- Agent Bridge는 토큰을 읽거나 저장하지 않습니다.
- 네이티브 브리지는 Codex, Claude, Agy, Pi를 허용하며, 글로벌 설정을 수정하지 않고 세션별 hook·notify·log·extension 인자만 사용합니다.
- 기본 모드에서의 자동 승인, IDE, 웹 UI, 외부 오케스트레이션 서비스는 포함하지 않습니다. `--yolo`는 provider에 원래 대응 기능이 있을 때만 그 위험 플래그를 전달하며, Pi에 없는 권한 기능을 브리지가 새로 만들지 않습니다.
