# Agent Bridge

로컬에 설치되고 로그인된 퍼스트파티 `codex`, `claude`, `agy` CLI를 하나의 터미널에서 다루는 Rust TUI입니다. API 키나 중간 SaaS 없이 각 CLI를 실제 PTY에서 그대로 실행합니다.

좌측 세션 레일에서 활성 CLI를 전환할 수 있고, 같은 CLI를 여러 번 열거나 서로 다른 CLI를 원하는 비율로 조합할 수 있습니다. 예를 들어 `Codex 1`, `Codex 2`, `Claude 1`을 동시에 실행할 수 있습니다.

## 실행

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

```json
{
  "agents": {
    "claude": { "command": "/opt/claude/claude" },
    "agy": { "args": ["--effort", "high"] }
  }
}
```

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

- 인증과 세션은 각 퍼스트파티 CLI가 소유합니다.
- Agent Bridge는 토큰을 읽거나 저장하지 않습니다.
- 기본 모드에서의 자동 승인, IDE, 웹 UI, 외부 오케스트레이션 서비스는 포함하지 않습니다. `--yolo` 모드는 Agent Bridge가 자체 승인 로직을 구현하지 않고 각 CLI의 공식 위험 플래그만 전달합니다.
