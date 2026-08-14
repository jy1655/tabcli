# Agent Bridge

로컬에 설치되고 로그인된 `codex`, `claude`, `agy`, `pi` CLI를 사용자가 볼 수 있는 실제 터미널 세션에서 연결하는 브리지입니다. API 키나 로그인 토큰을 대신 소유하지 않고 각 CLI의 기존 인증·설정·대화형 UI를 그대로 사용합니다.

현재 릴리스는 macOS에서 **Ghostty 1.3 이상, iTerm2, 내장 Terminal.app**을 지원합니다. Agent Bridge는 자신을 실행한 터미널을 감지해 같은 앱의 새 surface에서 세션을 시작하고, 자신이 만든 surface만 제어합니다. Terminal.app에서는 기존 tab/window를 사용하지 않고 항상 전용 새 window를 엽니다. 감지할 수 없는 호스트에서는 Terminal.app으로 안전하게 fallback합니다. 이미 독립적으로 실행 중인 임의의 CLI에는 사후 attach하지 않습니다.

## 지원 범위

| 환경 | 상태 | transport |
| --- | --- | --- |
| macOS + Ghostty 1.3+ | 지원 | Ghostty AppleScript 직접 제어 |
| macOS + iTerm2 | 지원 | iTerm2 AppleScript 직접 제어 |
| macOS + Terminal.app | 지원 | Terminal AppleScript 직접 제어 |
| macOS의 다른 터미널 | fallback | 별도 adapter가 없으면 Terminal.app에서 시작 |
| Windows PowerShell / cmd | 지원 | PowerShell 7(`pwsh.exe`) 기반 전용 visible console; `ask`/`tell`/`sessions`/explicit close |
| Linux 터미널 | 미지원 | [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)에서 별도 구현 |
| VS Code 통합 터미널 | 현재 비범위 | 전용 adapter가 필요하면 별도로 판단 |

macOS에서는 `TERM_PROGRAM`, `TERM`, `ITERM_SESSION_ID`, `TERM_SESSION_ID` 순으로 현재 호스트를 식별합니다. `--terminal ghostty|iterm2|terminal`로 명시 선택할 수 있고, 선택을 생략한 상태에서 호스트를 식별하지 못하면 내장 Terminal.app을 엽니다. Terminal.app은 복원되거나 기존에 열린 surface를 채택하지 않고 항상 전용 새 window를 만듭니다. 명시 선택한 adapter가 실패하면 다른 앱으로 조용히 우회하지 않고 오류를 반환합니다.

Ghostty의 AppleScript는 1.3에서 추가된 preview API이며 macOS Automation 권한이 필요합니다. Ghostty 설정에서 `macos-applescript = false`이면 adapter를 사용할 수 없습니다. Terminal.app은 기존 tab이나 UI scripting을 사용하지 않고 native AppleScript로 항상 전용 새 window를 만듭니다.

Windows는 PowerShell 또는 cmd에서 호출할 수 있으며 PowerShell 7(`pwsh.exe`)이 설치되어 있어야 합니다. bridge는 `CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP`로 전용 visible console을 만들고, 기록한 process-group identity에만 후속 입력과 explicit close를 전달합니다. Linux는 아직 미지원입니다. provider/session 계약은 공유하되 OS와 terminal transport는 각각 독립 모듈로 유지합니다.

의미를 이해하고 완료 결과를 회수하는 provider는 다음 네 가지입니다.

- Codex: 세션별 `notify`
- Claude Code: 세션별 `Stop` hook
- Agy: 세션 로그와 완료 transcript
- Pi: 세션 전용 lifecycle 확장

## 설치

Rust 1.88 이상, 지원하는 macOS 터미널 하나, 그리고 사용할 provider CLI가 필요합니다. Ghostty를 사용하면 1.3 이상이어야 합니다. 각 CLI는 먼저 직접 실행해 로그인과 초기 설정을 완료해야 합니다.

```sh
git clone https://github.com/jy1655/agent-bridge.git
cd agent-bridge
cargo install --path . --locked
```

기존 설치를 교체할 때는 `cargo install --path . --locked --force`를 사용합니다. 기본 설치 위치인 `~/.cargo/bin`이 `PATH`에 없다면 추가하거나 빌드한 바이너리를 절대 경로로 실행합니다.

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

Claude provider에서 정확한 모델 입력값 `Fable5`는 Claude Code가 요구하는 `Fable`로 변환됩니다. Pi provider에서는 정확한 입력값 `Fable`만 `anthropic/claude-fable-5`로 변환합니다. 각 provider의 다른 모델 값은 그대로 전달합니다.

`--workspace`를 생략하면 현재 디렉터리를 사용합니다. `--title`은 세션 metadata이며 지원하는 provider의 native session name으로 전달되지만 terminal surface의 제목을 설정하거나 고정하지 않습니다. 생략하면 provider와 workspace 이름으로 metadata를 만듭니다. `--model`과 `--effort`를 생략하면 각 CLI의 기존 기본값을 유지합니다. `--terminal`을 생략하면 호출 환경을 자동 감지하고, 감지 불가 시 Terminal.app을 사용합니다.

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
agent-bridge close-session session-XXXXXXXX --explicit
```

`ask`와 `tell`은 기본적으로 다음 provider 결과를 최대 900초 기다립니다. `--timeout-secs`로 제한을 바꾸거나, 결과를 기다리지 않고 탭만 열려면 `--detach`를 사용합니다. 대기 실패나 timeout은 이미 열린 탭을 자동으로 닫지 않습니다.

전체 명령은 다음과 같습니다.

```text
agent-bridge ask <codex|claude|agy|pi> [--workspace PATH] --prompt TEXT [--title NAME]
    [--model MODEL] [--effort EFFORT] [--terminal <ghostty|iterm2|terminal|windows-console>]
    [--yolo] [--timeout-secs N] [--detach] [--json]
agent-bridge tell <session> --prompt TEXT [--timeout-secs N] [--detach] [--json]
agent-bridge sessions [--json]
agent-bridge close-session <session> --explicit [--json]
agent-bridge --help | --version
```

## 권한과 세션 경계

- Agent Bridge는 같은 `ask` 작업에서 새로 만든 surface만 기록합니다. 새 handle은 managed session ID와 host가 제공하는 stable ID를 결합하며 `tell`과 `close-session` 직전에 live wrapper와 함께 다시 검증합니다. Terminal.app은 전용 window ID·TTY에 target `native-session` owner의 managed session ID·PID·controlling TTY device·process start fingerprint를 결합해 TTY나 PID 재사용을 fail-closed합니다. 호출 당시 터미널을 재감지하거나 복원된 front/current/selected surface를 채택하지 않습니다.
- 새 세션의 `--model`, `--effort`, `--yolo`는 부모 CLI에서 추측하거나 상속하지 않습니다. 해당 `ask` 요청에 명시된 값만 사용합니다.
- `--yolo`는 Codex의 `--dangerously-bypass-approvals-and-sandbox`, Claude와 Agy의 `--dangerously-skip-permissions`를 전달합니다. Pi에서는 추가 인자를 만들지 않는 no-op이며 Pi의 native 권한 정책을 유지합니다.
- `tell`은 세션별 한 턴만 허용합니다. 프롬프트를 하나의 bracketed paste로 전송하고 Enter·ESC 같은 별도 터미널 동작을 만들 수 있는 제어문자를 거부합니다.
- 모든 bridge 프롬프트에는 source provenance가 붙습니다. 사람이 읽는 결과의 터미널 제어문자는 가시적인 문자열로 이스케이프합니다.
- 결과가 돌아온 뒤 탭은 열린 채 유지되어 사용자가 직접 이어서 작업할 수 있습니다. 진행 중인 bridge 요청과 같은 탭의 수동 입력을 겹치면 수동 턴 결과가 bridge 요청의 결과로 먼저 인식될 수 있으므로 동시에 입력하지 않아야 합니다.
- `close-session`은 `--explicit`이 있어야 합니다. Terminal.app은 live `native-session` owner attestation과 전용 window ID·TTY가 모두 일치할 때만 busy surface에 interrupt를 보내고 닫습니다. close finality에서는 `terminal.json` handle을 `terminal.closed.json` tombstone으로 소진하며, 이미 `closed`인 세션의 반복 close는 terminal adapter를 호출하지 않습니다.
- 세션별 상태와 결과는 권한을 제한한 `~/.agent-bridge/native-sessions` 아래에 저장합니다. provider의 전역 설정이나 workspace hook 파일은 수정하지 않습니다.

최소 지원 버전은 다음과 같습니다. 더 새로운 버전은 허용합니다.

| Provider | 최소 버전 |
| --- | --- |
| Codex | 0.147.0 |
| Claude Code | 2.1.229 |
| Agy | 1.1.12 |
| Pi | 0.84.1 |

> [!WARNING]
> Codex, Claude, Agy에서 `--yolo`는 해당 CLI의 승인·sandbox 보호를 우회합니다. 신뢰하는 코드와 workspace에서만 명시적으로 사용하세요. Agent Bridge는 그 세션 내부의 명령을 다시 sandbox하지 않습니다.

## 구조와 확장

코드는 provider 의미와 terminal transport를 분리합니다.

```text
src/providers/                 공통 provider 정책: 명령, 버전, model/effort/yolo 인자
src/native/provider/          provider별 실행 인자와 완료 monitor 선택
src/native/terminal/mod.rs     공통 terminal kind, session record, OS dispatch
src/native/terminal/macos/     iTerm2, Terminal.app, Ghostty adapter
src/native/terminal/linux/     Linux transport 경계(현재 미지원)
src/native/terminal/windows/   Windows managed visible-console transport
src/native.rs                 세션 상태, lifecycle, 명령 및 현재 결과 monitor orchestration
```

새 CLI를 추가할 때는 provider registry와 두 provider adapter를 추가하고, model/effort/권한 및 실제 결과 회수 계약을 각각 테스트합니다. 새 터미널은 해당 OS 디렉터리에 adapter를 추가하고 OS dispatcher에 등록합니다. 새 OS는 독립 디렉터리에서 같은 `detect/open_tab/send_file/close_session` 계약을 구현합니다. launch command quoting은 POSIX shell과 Windows PowerShell을 분리해 유지합니다. 공통화가 플랫폼의 native 동작을 약화한다면 플랫폼별 구현을 우선합니다.

기존 `{"iterm_session_id":"..."}` 형식의 `terminal.json`은 iTerm2 세션으로 계속 읽습니다. 새 세션은 terminal-neutral한 `terminal`, `session_id`, 선택적 `tab_id`·`window_id`와 내부 `managed_session_id` binding을 기록합니다. 가시적인 terminal title은 설정하거나 ownership record에 저장하지 않습니다. Terminal.app의 추가 owner attestation은 target `native-session`이 별도 private record에 기록합니다.

## 0.2.0 마이그레이션

0.2.0은 예전 embedded multi-PTY TUI와 그 전용 표면을 제거했습니다.

- 인자 없는 `agent-bridge`, workspace 직접 인자, `--restore`, 전역 `--yolo`는 더 이상 TUI를 시작하지 않습니다.
- 예전 `open`, `prompt`, `status`, `read`, `wait`, `list`, `close`, `hook` 명령은 제거되었습니다. 새 명령은 `ask`, `tell`, `sessions`, `close-session`입니다.
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

`tests/native_live.rs`의 ignored 테스트는 감지되거나 `--terminal`로 지정한 실제 session surface와 로그인된 provider를 사용합니다. macOS에서는 지원 앱을, Windows에서는 `windows-console`을 지정합니다. 각 테스트는 `ask → result → close-session --explicit → closed 상태 조회`를 한 번에 검증하고 정상 경로에서 생성한 surface를 닫습니다. 종료 단계 자체가 실패하면 진단을 위해 surface가 남을 수 있으므로 `sessions`로 확인합니다.

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
