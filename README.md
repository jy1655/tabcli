# Agent Bridge

로컬에 설치되고 로그인된 `codex`, `claude`, `agy`, `pi` CLI를 사용자가 볼 수 있는 실제 터미널 세션에서 연결하는 브리지입니다. API 키나 로그인 토큰을 대신 소유하지 않고 각 CLI의 기존 인증·설정·대화형 UI를 그대로 사용합니다.

현재 릴리스는 **macOS + iTerm2** 전용입니다. Agent Bridge가 새 iTerm2 탭에서 시작한 세션만 제어하며, 이미 독립적으로 실행 중인 임의의 CLI에 사후 attach하지 않습니다.

## 지원 범위

| 환경 | 상태 | transport |
| --- | --- | --- |
| macOS + iTerm2 | 지원 | iTerm2 AppleScript 직접 제어 |
| macOS의 다른 터미널 | 미지원 | 별도 terminal adapter 필요 |
| Windows PowerShell / cmd | 미지원 | [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)에서 별도 구현 |
| Linux 터미널 | 미지원 | [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)에서 별도 구현 |
| VS Code 통합 터미널 | 현재 비범위 | 전용 adapter가 필요하면 별도로 판단 |

Windows와 Linux도 여러 내부 터미널을 그리는 TUI를 다시 만드는 방향이 아니라, 사용자가 보는 OS 터미널 창·탭에서 CLI를 시작하고 그 세션을 관리하는 방향으로 확장합니다. provider/session 계약은 공유하되 terminal transport는 OS별 구현을 허용합니다.

의미를 이해하고 완료 결과를 회수하는 provider는 다음 네 가지입니다.

- Codex: 세션별 `notify`
- Claude Code: 세션별 `Stop` hook
- Agy: 세션 로그와 완료 transcript
- Pi: 세션 전용 lifecycle 확장

## 설치

Rust 1.88 이상, iTerm2, 그리고 사용할 provider CLI가 필요합니다. 각 CLI는 먼저 직접 실행해 로그인과 초기 설정을 완료해야 합니다.

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

Claude provider에서 정확한 모델 입력값 `Fable5`는 Claude Code가 요구하는 `Fable`로 변환됩니다. `Fable`, `fable5`를 포함한 다른 값과 다른 provider의 모델 값은 그대로 전달합니다.

`--workspace`를 생략하면 현재 디렉터리를 사용하고, `--title`을 생략하면 provider와 workspace 이름으로 탭 제목을 만듭니다. `--model`과 `--effort`를 생략하면 각 CLI의 기존 기본값을 유지합니다.

기계 판독이 필요하면 `--json`을 사용합니다. 반환된 `session` id로 같은 탭에 후속 프롬프트를 전달할 수 있습니다.

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
    [--model MODEL] [--effort EFFORT] [--yolo] [--timeout-secs N] [--detach] [--json]
agent-bridge tell <session> --prompt TEXT [--timeout-secs N] [--detach] [--json]
agent-bridge sessions [--json]
agent-bridge close-session <session> --explicit [--json]
agent-bridge --help | --version
```

## 권한과 세션 경계

- Agent Bridge는 자신이 `ask`로 시작하고 소유 정보를 기록한 iTerm2 세션만 입력하거나 닫습니다.
- 새 세션의 `--model`, `--effort`, `--yolo`는 부모 CLI에서 추측하거나 상속하지 않습니다. 해당 `ask` 요청에 명시된 값만 사용합니다.
- `--yolo`는 Codex의 `--dangerously-bypass-approvals-and-sandbox`, Claude와 Agy의 `--dangerously-skip-permissions`를 전달합니다. Pi에서는 추가 인자를 만들지 않는 no-op이며 Pi의 native 권한 정책을 유지합니다.
- `tell`은 세션별 한 턴만 허용합니다. 프롬프트를 하나의 bracketed paste로 전송하고 Enter·ESC 같은 별도 터미널 동작을 만들 수 있는 제어문자를 거부합니다.
- 모든 bridge 프롬프트에는 source provenance가 붙습니다. 사람이 읽는 결과의 터미널 제어문자는 가시적인 문자열로 이스케이프합니다.
- 결과가 돌아온 뒤 탭은 열린 채 유지되어 사용자가 직접 이어서 작업할 수 있습니다. 진행 중인 bridge 요청과 같은 탭의 수동 입력을 겹치면 수동 턴 결과가 bridge 요청의 결과로 먼저 인식될 수 있으므로 동시에 입력하지 않아야 합니다.
- `close-session`은 `--explicit`이 있어야 하며 기록된 iTerm2 session id만 대상으로 합니다. 이미 사라진 탭이나 launch에 실패한 세션도 idempotent하게 종료 상태로 정리합니다.
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
src/native/terminal/          OS·터미널별 visible session transport
src/native.rs                 세션 상태, lifecycle, 명령 및 현재 결과 monitor orchestration
```

새 CLI를 추가할 때는 provider registry와 두 provider adapter를 추가하고, model/effort/권한 및 실제 결과 회수 계약을 각각 테스트합니다. 새 OS나 터미널은 provider adapter를 재사용하면서 `src/native/terminal/`에 transport를 추가합니다. 다만 현재 iTerm2 terminal record와 POSIX launch command 조립 일부는 `src/native.rs`에 남아 있으므로, Windows/Linux 구현에서는 [Issue #6](https://github.com/jy1655/agent-bridge/issues/6)의 transport-neutral session 계약으로 함께 분리해야 합니다. 공통화가 플랫폼의 native 동작을 약화한다면 플랫폼별 구현을 우선합니다.

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

`tests/native_live.rs`의 ignored 테스트는 실제 iTerm2 탭과 로그인된 provider를 사용합니다. 한 번에 한 provider만 실행하고, 테스트가 남긴 탭은 확인 후 `close-session --explicit`로 닫습니다.

```sh
AGENT_BRIDGE_LIVE_CLAUDE_MODEL=Fable5 \
AGENT_BRIDGE_LIVE_CLAUDE_EFFORT=max \
cargo test --test native_live \
  live_native_claude_forwards_flags_and_returns_result \
  -- --ignored --exact --nocapture
```

## License

[MIT](LICENSE)
