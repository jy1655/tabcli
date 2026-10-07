# Terminal Agent Bridge (TAB)

[English](README.md) · 한국어 README는 영어 README를 따르며, 내용이 다르면 영어 README를 기준으로 합니다.

`tabcli`로 이미 설치하고 로그인한 Codex, Claude Code, Agy, Pi를 눈에 보이는 터미널 탭이나 창에서 실행합니다. 결과를 읽고 같은 대화에 후속 요청을 보낸 뒤, 작업을 마치면 세션을 명시적으로 닫습니다. TAB은 macOS와 Windows에서 직접 실행하는 환경을 지원합니다. WSL 안의 Linux 프로세스를 포함해 Linux는 지원하지 않으며 Warp 지원은 제한적입니다.

## 세션 실행해 보기

[`tabcli`를 설치](#설치)하고 Codex 0.149.0 이상에 로그인한 뒤 프로젝트 디렉터리에서 실행합니다. 스크립트나 다른 에이전트가 `tabcli`를 사용하는 동안 사용자는 CLI의 대화와 도구 실행을 지켜봅니다. 각 CLI의 UI와 설정은 그대로 사용합니다. 아래 예제는 CLI 기본값을 사용하며, ID와 결과는 예시이므로 이후 명령에는 실제 출력된 ID를 넣습니다.

```sh
tabcli ask codex --prompt "Where does this program start?"
```

```text
session: session-K7m2Qx
request: request-1791285000000000000-4217-0

The program starts in src/main.rs.
```

`ask`는 결과가 나올 때까지 기다립니다. 세션은 계속 열려 있습니다. 후속 요청을 보내면 같은 대화가 이어집니다.

```sh
tabcli tell session-K7m2Qx --prompt "Which function parses its arguments?"
```

```text
session: session-K7m2Qx
request: request-1791285060000000000-4281-0

parse_args_from parses the command-line arguments.
```

작업을 마치면 세션을 닫습니다. 기록된 결과는 닫은 뒤에도 남습니다.

```sh
tabcli close-session session-K7m2Qx --explicit
```

```text
closed session-K7m2Qx
```

종료에 실패하면 표시된 세션을 조회하고 `tabcli doctor SESSION`을 실행한 뒤 재시도합니다. [종료 오류 복구 절차](docs/cli.md#close-session)를 따릅니다.

결과를 기다리다 시간이 초과되어도 이미 전달된 작업은 취소되지 않습니다. 시작 단계의 시간 초과는 실행을 막기도 합니다. 다시 시도하기 전에 원래 요청을 조회하고 [요청 복구 절차](docs/cli.md#shared-prompt-and-output-options)를 따릅니다.

실행에 실패하면 `tabcli doctor --provider codex --probe`를 실행합니다. 이미 세션이 있다면 `SESSION`을 해당 ID로 바꿔 `tabcli doctor SESSION`과 `tabcli inspect SESSION --timeline`으로 확인하며, 자세한 내용은 [진단 절차](docs/cli.md#doctor)를 따릅니다.

## 지원 범위

구현 여부와 로그인한 CLI로 실제 실행해 확인한 범위를 구분합니다. 아래 표는 과거 실행 기록 중 일부를 요약합니다. 연결된 기록에 당시 버전, 설정과 한계를 남겼으며, 0.2.5의 모든 조합을 검증했다는 뜻은 아닙니다.

| 플랫폼 / 터미널 | 상태 | 로그인 후 실행 검증 |
| --- | --- | --- |
| macOS / iTerm2 | 구현됨 | Codex, 2026-10-07 |
| macOS / Terminal.app | 구현됨 | Codex, 2026-10-07 |
| macOS / Ghostty | 구현됨 | Codex, 2026-10-07 |
| macOS / WezTerm | 구현됨 | Codex/Claude/Agy, 2026-10-07 |
| macOS / Warp | 제한적 | 미검증 |
| Windows에서 직접 실행 | 구현됨 | 네 CLI 모두, 2026-10-02 |
| Windows Terminal 탭 | 우선 사용 | 탭 생성/후속 요청/종료, 2026-10-01 |
| Windows 콘솔 창 | 대체 경로 | 초기 요청/종료, 2026-10-01 |
| Linux 및 WSL 안의 Linux 프로세스 | 미지원 | 세션 전송 경로 없음 |

macOS에서는 호출한 터미널을 감지하며, 호스트를 식별하지 못하면 Terminal.app을 사용합니다. Terminal.app은 항상 새 창을 엽니다. Windows에서는 전용 Windows Terminal 창의 탭을 우선 사용하고, 탭을 만들지 못하면 별도 콘솔 창을 엽니다. `settings macos-open-mode`와 `settings windows-tab-window`를 포함한 [터미널 선택과 설정](docs/terminals.md#selection-and-settings)을 참고합니다.

Warp는 접근이 허용된 공식 Control 연결과 탭·창 생성 기능이 있어야 사용할 수 있습니다. 터미널 입력을 제출하지 못하므로 Agy와 Pi의 후속 요청은 지원하지 않습니다. 로그인한 CLI로 요청·결과·후속 요청·종료를 거치는 경로도 미검증입니다. 이 두 CLI에는 다른 터미널을 사용합니다.

| 에이전트 CLI | 최소 실행 버전 |
| --- | --- |
| Codex (`codex`) | 0.147.0 |
| Claude Code (`claude`) | 2.1.234 |
| Agy (`agy`) | 1.1.12 |
| Pi (`pi`) | 0.84.1 |

Codex 후속 요청에는 0.149.0 이상이 필요하며 공유 데몬은 필요하지 않습니다. Claude는 최소 버전을 충족하고, 사용하는 백엔드와 설정에서 세션 간 메시지 전달도 지원해야 합니다. 전달 경로와 오류 처리는 [에이전트 CLI 요구 사항](docs/providers.md)을 따릅니다.

- [2026-10-07 macOS, 0.2.5](docs/verification/2026-10-07-macos-0.2.5.md): iTerm2에서 Codex·Claude·Agy·Pi의 초기 요청·후속 요청·종료를 모두 확인했습니다.
- [2026-10-07 macOS, 0.2.4](docs/verification/2026-10-07-macos-0.2.4.md): 네 터미널의 Codex와 WezTerm의 Claude·Agy·Pi 왕복, Pi 자격 증명 누락 타임아웃 진단을 확인했습니다.
- [2026-10-07 macOS, 0.2.3](docs/verification/2026-10-07-macos-0.2.3.md): iTerm2의 Codex, WezTerm의 Pi(로그인된 provider와 자격 증명이 없는 provider, #83의 사유 확인), WezTerm의 Claude와 Agy를 검증했습니다. Warp는 실행하지 않았습니다.
- [2026-10-07 macOS, 0.2.2](docs/verification/2026-10-07-macos-0.2.2.md): 화면이 잠기지 않은 상태에서 위의 네 터미널의 Codex, WezTerm의 Claude, WezTerm의 Agy(기본 권한 모드와 `--yolo`), 로그인된 provider로 WezTerm의 Pi를 검증했습니다.
- [2026-10-07 macOS, 0.2.1](docs/verification/2026-10-07-macos-0.2.1.md): 위의 네 터미널에서 Codex를, WezTerm에서 Claude를 CLI 기본값으로 검증했습니다. Agy는 WezTerm에서 `--yolo`로 검증했습니다. 화면이 잠긴 상태에서는 Terminal.app과 Ghostty가 요청부터 종료까지의 과정을 마치지 못했습니다. 기본 권한 모드의 Agy와 Pi는 첫 결과를 기다리다 시간이 초과되어, 성공한 검증에 포함하지 않습니다.
- [2026-10-06 macOS](docs/verification/2026-10-06-macos-0.1.2.md): 위의 네 터미널에서 Codex를, WezTerm에서 Claude를 검증했습니다. 기본 권한 모드 대신 `--yolo`를 사용했습니다.
- [2026-10-04 macOS](docs/verification/2026-10-04-macos-0.1.1.md): Codex·Claude·Agy를 CLI 기본값으로 검증했습니다. 최근 WezTerm에서 실행한 Pi 검사는 인증에 실패했습니다. 요청부터 후속 요청과 종료까지 성공한 검증에는 포함하지 않습니다.
- [2026-10-02 Windows](docs/verification/2026-10-02-windows.md): 네 CLI를 검증했습니다. 각 세션이 탭인지 별도 콘솔 창인지는 기록에서 구분하지 않습니다.
- [2026-10-01 Windows](docs/verification/2026-10-01-windows.md): 탭과 콘솔 창의 생성, 포커스와 종료를 확인했습니다.
- [2026-09-29 macOS](docs/verification/0.0.9.md): iTerm2에서 Pi의 초기 요청, 후속 요청과 종료를 확인했습니다. 테스트용 동의 기록을 사용했고 모델을 명시했습니다.

## 설치

[GitHub Releases](https://github.com/jy1655/tabcli/releases)에서 운영체제에 맞는 압축 파일과 같은 이름의 `.sha256` 파일을 받습니다. 미리 빌드된 파일을 설치할 때는 Rust가 필요하지 않습니다. Apple Silicon macOS는 `tabcli-<version>-aarch64-apple-darwin.tar.gz`, x64 Windows는 `tabcli-<version>-x86_64-pc-windows-msvc.zip`을 사용합니다.

0.2.5를 설치한다면 다운로드한 디렉터리에서 압축을 풀기 전에 체크섬을 확인합니다. macOS에서는 체크섬 명령이 `OK`를 출력한 경우에만 다음 명령으로 진행합니다.

```sh
shasum -a 256 -c tabcli-0.2.5-aarch64-apple-darwin.tar.gz.sha256
tar -xzf tabcli-0.2.5-aarch64-apple-darwin.tar.gz
```

Windows에서는 PowerShell에서 실행합니다.

```powershell
$archive = "tabcli-0.2.5-x86_64-pc-windows-msvc.zip"
$expected = (Get-Content "$archive.sha256").Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
Expand-Archive $archive -DestinationPath .\tabcli
```

체크섬이 다르면 설치를 멈추고 같은 릴리스에서 압축 파일과 체크섬을 다시 받습니다. 압축을 푼 `tabcli`(macOS) 또는 `tabcli.exe`(Windows)를 `PATH`에 있는 디렉터리에 둡니다.

소스에서 설치하려면 Rust 1.97.1 이상과 Cargo가 필요합니다.

```sh
cargo install --git https://github.com/jy1655/tabcli --tag v0.2.5 --locked
```

이 패키지는 crates.io에 등록되어 있지 않습니다. 그곳의 `agent-bridge`와 `tab-cli`는 이 프로젝트와 무관합니다. 실행 파일과 Cargo 패키지 이름은 모두 `tabcli`입니다.

첫 실행 전에 사용할 에이전트 CLI를 설치하고 로그인한 뒤, 해당 프로젝트의 작업 디렉터리 신뢰 질문을 처리합니다. iTerm2, Terminal.app, Ghostty 연동은 macOS Automation을 사용합니다. 실행 중 Apple Events 권한 오류가 나면 [macOS 승인 확인 절차](docs/macos-permissions.md)를 따릅니다. Windows에서 직접 실행할 때는 cmd에서 호출하더라도 `PATH`에 PowerShell 7(`pwsh.exe`)이 있어야 합니다.

## 일상적인 사용

결과를 기다리지 않고 돌아오려면 `tabcli ask codex --prompt "Review this project." --detach`를 실행합니다. 이후 출력된 세션 ID와 요청 ID로 결과를 기다립니다.

```sh
tabcli result session-K7m2Qx --request request-1791285000000000000-4217-0 --wait
```

`tabcli sessions --sort updated`로 기록된 세션을 찾습니다. 이 명령은 도중에 중단된 상태 전이를 마무리하고, 소유 프로세스가 종료된 세션의 상태도 갱신하므로 읽기 전용이 아닙니다.

시간 초과나 전달 오류 뒤에 기록만 읽으려면 `tabcli inspect session-K7m2Qx --timeline`을 실행합니다. 이 조회는 기록을 바꾸지 않습니다.

`tabcli self-test codex --workspace .`는 설치된 환경에서 실제 모델을 호출하고 터미널을 열어 초기 결과와 후속 요청을 확인한 뒤, 자신이 만든 세션만 닫습니다. 별도 옵션을 지정하지 않으면 평소 쓰는 기록 저장 위치에서 검사합니다. 실패하면 재실행 전에 실패 단계와 종료 결과를 읽습니다. 새 작업 디렉터리 신뢰는 자동으로 승인하지 않습니다.

옵션, 출력 필드와 복구 절차는 [CLI 명령 문서](docs/cli.md)에 있습니다.

## 사용 전에 알아둘 경계

Bridge는 모델 클라이언트나 샌드박스가 아닙니다. 직접 실행하지 않은 세션에는 연결하지 못합니다. Bridge가 운영하는 서버는 없습니다. 각 CLI가 설정된 서비스에 기존 계정으로 모델 요청을 보냅니다.

**Bridge가 실행한 Claude 세션은 같은 계정의 다른 Claude 세션에서 오는 세션 간 메시지를 받습니다. `--yolo`로 실행했다면 다른 세션에서 받은 작업도 Claude의 권한 검사 없이 실행됩니다.** 같은 계정의 다른 Claude 세션을 통제하지 못한다면 이 조합을 쓰지 않습니다. [Claude 외부 메시지 수신](docs/security-and-data.md#claude-inbound-messages)을 확인합니다.

`--yolo`는 CLI 자체의 우회 옵션을 전달하며 Bridge는 별도 샌드박스를 추가하지 않습니다. Pi의 `--approve`는 프로젝트 승인만 처리하고 기본 도구 정책은 유지합니다. 기존 CLI 설정도 그대로 적용됩니다. [권한 모드](docs/security-and-data.md#permission-modes)를 참고합니다.

작업 디렉터리 신뢰는 에이전트 CLI가 관리합니다. Bridge는 정확히 같은 디렉터리에 대해 검증된 기존 동의를 재사용하며, 새 신뢰를 부여하지 않습니다. 신뢰 대화상자 때문에 진행이 멈추면 관리 중인 터미널 화면에서 내용을 확인합니다. [작업 디렉터리 신뢰와 동의](docs/security-and-data.md#workspace-trust-and-consent)에 조건을 정리했습니다.

전달 결과가 불확실한 요청은 재전송하지 않습니다. 원래 요청을 조회하고 기다리거나 세션을 명시적으로 닫습니다. 같은 프롬프트를 다시 보내면 작업이 중복될 위험이 있습니다. [전달과 신뢰의 경계](docs/security-and-data.md#workspace-trust-and-consent)를 따릅니다.

세션 기록에는 결과와 보관된 프롬프트 본문이 들어 있으며, `~/.agent-bridge/native-sessions` 아래에 암호화하지 않은 상태로 저장합니다. 세션을 닫아도 기록은 남지만 일부 임시 전달 파일은 그 전에 지웁니다. Windows의 홈 디렉터리는 `HOME`, `USERPROFILE` 순으로 정하며, `AGENT_BRIDGE_NATIVE_STATE_DIR`로 저장 위치를 바꿉니다. 기록을 공유하기 전에 내용을 검토합니다. [로컬 기록](docs/security-and-data.md#local-records)을 참고합니다.

설치를 제거하려면 세션을 닫고 [세션 종료와 Bridge 데이터 삭제](docs/security-and-data.md#list-close-and-remove-sessions)에 따라 기록을 지운 뒤, 다운로드한 실행 파일을 삭제하거나 Cargo로 설치했다면 `cargo uninstall tabcli`를 실행합니다. 에이전트 CLI의 대화 기록과 자격증명은 별도로 관리됩니다.

## `agent-bridge` 0.1.x에서 업그레이드

실행 파일 이름이 `tabcli`로 바뀌었습니다. 기존 세션, 설정과 동의 기록은 그대로 읽으며, 기록 디렉터리와 `AGENT_BRIDGE_*` 환경 변수 이름도 유지합니다. `agent-bridge` 별칭은 설치하지 않으므로 스크립트의 명령을 바꿉니다.

기존 실행 파일이 만든 세션을 모두 닫을 때까지 그 파일을 원래 위치에 둡니다. 해당 세션의 훅이 그 경로를 호출하기 때문입니다. `tabcli` 설치는 실행 중인 세션의 훅을 바꾸지 않습니다.

## 문서

- [CLI 명령](docs/cli.md): 명령, 출력과 오류 복구 절차입니다.
- [터미널](docs/terminals.md): 화면 생성 위치, 설정, 소유권과 검증 범위입니다.
- [에이전트 CLI](docs/providers.md): CLI 요구 사항, 전달 경로와 결과 처리 방식입니다.
- [보안과 데이터](docs/security-and-data.md): 권한, 작업 디렉터리 신뢰와 보관되는 기록입니다.
- [macOS 승인](docs/macos-permissions.md): Automation과 CLI 승인을 구분하는 영어 문서입니다.
- [구조](docs/architecture.md): 모듈, 세션 기록과 세션 상태 변경 규칙입니다.
- [검증](docs/testing.md): 자동 검사와 로그인한 CLI의 수동 실행 검사 절차입니다.
- [릴리스 노트](docs/releases/README.md): 버전별 변경과 검증 기록입니다.

## 기여와 보안 제보

버그 제보나 변경 제안은 [기여 안내](CONTRIBUTING.md)를 읽고 영어 또는 한국어로 작성합니다.
취약점은 공개 이슈 대신 [보안 정책](SECURITY.md)에 따라 비공개로 제보합니다.

## 라이선스

TAB에는 [MIT 라이선스](LICENSE)가 적용됩니다. 의존성의 라이선스는 [제3자 고지](THIRD_PARTY_NOTICES.md)에서 확인합니다.
TAB은 독립 프로젝트이며 OpenAI, Anthropic, Google 및 연동하는 터미널·CLI 제작사와 제휴하거나 이들의 보증을 받은 프로젝트가 아닙니다.
