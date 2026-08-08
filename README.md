# Agent Bridge

로컬에 설치되고 로그인된 퍼스트파티 `codex`, `claude`, `agy` CLI를 하나의 터미널에서 다루는 Rust TUI입니다. API 키나 중간 SaaS 없이 각 CLI를 실제 PTY에서 그대로 실행합니다.

좌측 세션 레일에서 활성 CLI를 전환할 수 있고, 같은 CLI를 여러 번 열거나 서로 다른 CLI를 원하는 비율로 조합할 수 있습니다. 예를 들어 `Codex 1`, `Codex 2`, `Claude 1`을 동시에 실행할 수 있습니다.

## 실행

Rust 및 Visual C++ Build Tools가 설치된 Developer PowerShell에서:

Windows에서 최신 네이티브 Claude Code를 임베드하려면 bundled ConPTY transport를 제공하는 `node-pty`가 필요합니다. Claude CLI 자체는 공식 네이티브 설치본을 그대로 사용합니다.

```powershell
npm install -g node-pty
```

```powershell
cargo run -- D:\Dev
```

승인 확인과 sandbox 보호를 우회해야 하는 명시적인 작업에서는 `--yolo`를 workspace 앞이나 뒤에 지정할 수 있습니다.

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

## 키

- `F12`: 앱 안에서 전체 단축키 도움말 열기
- `Ctrl+F11` (`F11`도 가능): 다음 키 하나를 Agent Bridge 단축키 처리 없이 활성 CLI로 전달. Windows Terminal이 plain `F11`을 전체화면 전환으로 소비하므로 `Ctrl+F11`을 권장
- `F3`: 새 탭 열기 (`←`/`→`로 CLI 선택, `Enter`로 생성)
- `F1`: 현재 workspace의 tracked `git diff HEAD` 읽기 전용 보기
- `F4`: 활성 탭 종료
- `F5` / `F6`: 이전/다음 탭으로 이동
- `F7`: 탭 제목과 보관된 터미널 출력 검색
- `F2`: 현재 탭 이름으로 다른 탭에 프롬프트 전달 (탭 2개 이상 필요)
- `F8`: 스크롤백 모드 (`↑`/`↓`, `PageUp`/`PageDown`, `Home`/`End`, `Esc`로 복귀)
- 마우스 휠: CLI가 마우스 리포팅을 요청하지 않으면 터미널 스크롤백 이동. 마우스 캡처 중 Windows Terminal의 텍스트 선택·복사는 `Shift`를 누른 채 드래그
- `F9`: 종료된 탭을 같은 CLI·이름으로 새 세션 재시작
- `F10`: Agent Bridge 종료
- Agent Bridge가 예약하지 않은 키: 활성 에이전트 PTY에 그대로 전달 (`Ctrl+F11`로 예약 키도 전달 가능)
- 릴레이 입력 중 `←` / `→`: 대상 변경, `Enter` 두 번: 검토 후 전송, `Esc`: 취소

앱은 처음에 `Codex 1`만 엽니다. `F3`을 반복해서 원하는 구성을 만드세요. 마지막 탭을 닫아도 앱은 유지되며 다시 `F3`으로 세션을 만들 수 있습니다.

## 상태와 알림

- Claude 세션은 세션별 임시 `--settings` hook을 사용해 `working`, `waiting`, `idle`, `finished`를 표시합니다. `waiting`은 Claude의 Notification hook 전체에 반응합니다. 전역 Claude 설정은 수정하지 않으며 임시 파일은 탭 종료 시 삭제합니다.
- Codex와 Agy는 신뢰할 수 있는 사용자 대기 이벤트 계약이 확인되지 않아 PTY에서 관측한 `active`, `quiet`, `exited`, `unknown`만 표시합니다. `quiet`은 의미상 완료나 대기를 뜻하지 않습니다.
- `AGENT_BRIDGE_NOTIFICATIONS=1`을 설정하면 Claude가 `waiting` 또는 `finished`로 전이할 때 터미널 벨과 앱 내 알림을 냅니다. 알림은 승인 동작을 수행하지 않습니다.

## 설계 근거

세션 UX는 오픈소스 경쟁 프로젝트를 비교해 결정했습니다. 자세한 비교와 이번 버전에 반영한 범위는 [docs/benchmark.md](docs/benchmark.md)에 있습니다.

## 원칙

- 인증과 세션은 각 퍼스트파티 CLI가 소유합니다.
- Agent Bridge는 토큰을 읽거나 저장하지 않습니다.
- 기본 모드에서의 자동 승인, IDE, 웹 UI, 외부 오케스트레이션 서비스는 포함하지 않습니다. `--yolo` 모드는 Agent Bridge가 자체 승인 로직을 구현하지 않고 각 CLI의 공식 위험 플래그만 전달합니다.
