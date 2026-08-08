# Session UX benchmark

Agent Bridge의 목표와 가장 가까운 오픈소스 프로젝트에서 세션 탐색과 CLI 할당 방식을 비교했다. 브라우저 UI나 외부 SaaS 대신 로컬 TUI와 퍼스트파티 CLI를 유지하는 기능만 채택했다.

| 프로젝트 | 확인한 방식 | Agent Bridge에 반영한 결정 |
| --- | --- | --- |
| [Agent Deck](https://github.com/asheshgoplani/agent-deck) | 세션 목록, 새 세션의 도구 선택, 같은 도구의 여러 세션, 키보드 탐색, 앱 내 전체 키 표 | CLI 종류와 세션 인스턴스를 분리하고 동일 CLI의 중복 탭을 허용하며, `F12`로 전체 단축키를 확인하게 한다. |
| [ccmux](https://github.com/epilande/ccmux) | 좌측 사이드바, working/waiting/idle 상태, pane·transcript 검색, 종료 세션 재시작, Git/PR·diff 맥락 | 좌측 레일과 활성 터미널을 유지하면서 검증 가능한 상태, 검색, 재시작, 읽기 전용 Git 맥락을 반영한다. |
| [Claude Squad](https://github.com/smtg-ai/claude-squad) | 세션 생성 시 프로그램 프로필 선택, 여러 로컬 에이전트 병렬 관리 | `F3` 생성 화면에서 Codex, Claude, Agy를 매번 자유롭게 선택한다. |
| [Squad](https://github.com/mco-org/squad) | 로컬 에이전트 사이 메시지 전달과 SQLite 기반 협업 | 외부 서비스 없이 `F2` 릴레이를 유지하되 중복 세션을 구분하는 탭 이름을 출처로 넣는다. |

## Workspace 벤치마킹 (2026-08-09)

- ccmux는 cwd를 세션 identity로 표시하고 새 세션의 디렉터리를 선택한 세션/그룹에서 파생한다. restart도 해당 세션 문맥을 유지한다.
- Agent Deck은 `add .`로 현재 디렉터리를 세션에 결합하고, 병렬 격리가 필요하면 세션별 Git worktree를 선택적으로 만든다.
- Agent Bridge는 자동 worktree 수명주기를 도입하지 않는다. 대신 F3에서 탭별 workspace를 명시적으로 입력하고, rail/header/restart/relay에 같은 경로를 보존·표시한다. 이는 서로 다른 저장소와 같은 저장소의 모듈 디렉터리를 모두 다루면서 기존 PTY·퍼스트파티 CLI 원칙을 유지한다.

## Relay 재검증 (2026-08-09)

- ccmux의 `send`는 text를 대상 pane에 보내는 낮은 수준의 prompt dispatch이며, 별도의 screen/transcript 검색과 diff review handback이 문맥 전달을 보완한다.
- Agent Deck의 `session handoff`는 session working context를 읽기 전용으로 요약하는 handoff prompt builder다.
- Squad는 SQLite inbox/history와 task ack/complete 상태를 제공해 비동기 협업을 내구성 있게 추적한다.
- Agent Bridge F2는 source의 최근 visible terminal context를 최대 6,000자로 제한해 tab/workspace provenance와 사용자 요청에 결합하는 최소 handoff다. 전체 transcript·자동 요약·task queue는 포함하지 않는다. 실제 Codex가 만든 marker를 context로 캡처해 Claude가 새 ACK 값을 생성하는 live test로 source 응답 전달까지 검증했다. target 변경이나 요청 편집은 기존 capture를 폐기하며 두 번째 Enter 전까지 전송하지 않는다.

2026-08-08 재조사에서는 경쟁 제품의 공식 GitHub 문서만 근거로 사용했다. 자동 승인·자동 worktree 생성·외부 데몬 도입은 현재 제품 원칙과 맞지 않아 백로그에 넣지 않았다.

후속 구현으로 앱 내 `F12` 전체 키 도움말, Windows Terminal에서도 도달하는 `Ctrl+F11` 다음 키 직접 전달, 스크롤백 검색, fresh restart, branch/dirty 표시와 읽기 전용 diff 뷰를 반영했다. 세션 상태는 PTY 관측만으로 의미를 추측하지 않도록 activity(`active/quiet/exited/unknown`)와 authoritative hook 상태를 분리했다. Claude는 공식 `UserPromptSubmit`, 전체 `Notification`, `Stop`, `SessionEnd` 이벤트를 세션별 임시 설정으로 연결하며, opt-in waiting/finished 벨 알림도 이 상태 전이에만 반응한다. Codex와 Agy는 신뢰할 수 있는 사용자 대기 이벤트 계약이 없는 동안 activity만 유지한다.

## 이번 버전의 세션 모델

- 시작 시 `Codex 1` 한 개만 생성한다.
- 각 `F3` 생성은 독립 PTY와 증가하는 탭 이름을 만든다.
- CLI 종류별 수량 제한은 없다. `Codex 1`, `Codex 2`, `Claude 1` 같은 구성이 가능하다.
- 좌측 레일은 모든 세션을 표시하고, 우측에는 활성 세션만 크게 렌더링한다.
- `F4`는 활성 세션만 종료하며 이웃 탭으로 초점을 이동한다.
- `F2`는 탭 이름을 포함한 메시지를 대상 PTY에 입력하고 대상으로 초점을 이동한다.

## 의도적으로 제외한 범위

- git worktree 자동 생성
- tmux 의존성
- 외부 웹 대시보드
- 자동 승인 및 권한 우회
- 에이전트 응답을 자동 판독해 다른 에이전트에 무제한 전달하는 루프

이 항목들은 현재 요청의 탭/할당/직접 상호작용 범위를 넘어가며, 퍼스트파티 CLI의 인증과 승인 흐름을 보존하기 위해 포함하지 않았다.
