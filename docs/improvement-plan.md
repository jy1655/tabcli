# 개선 계획 (2026-08-10)

2026-08-10 전체 코드 리뷰 + herdr 비교 분석의 실행 계획이다. 검증 근거: macOS 로컬에서 `cargo clippy --all-targets --all-features` 경고 0, `cargo test --all-targets` 53 pass / 1 ignore(live). 경쟁 비교의 채택·제외 원칙은 [benchmark.md](benchmark.md)가 SSOT이며 이 문서는 그 원칙을 유지한다.

## herdr 코드 참조 라이선스 게이트

herdr(https://github.com/ogulcancelik/herdr)는 재라이선스 이력이 있다 (2026-08-10 GitHub API + LICENSE 커밋 이력 확인):

- 2026-03-27 initial release: **AGPL-3.0-or-later** — crates.io `herdr` 0.1.0이 이 시점
- 2026-07-22 **Apache-2.0 재라이선스** — 현재 master의 LICENSE

**참조 규칙 (본 프로젝트는 MIT):**

1. 참조는 현재 master 체크아웃만. 체크아웃 후 LICENSE 파일이 Apache-2.0인지 직접 재확인.
2. 2026-07-22 이전 태그·crates.io 0.1.0·구버전 포크 코드는 AGPL — 어떤 형태로도 유입 금지.
3. 코드 복사 시 Apache-2.0 attribution(라이선스 사본·NOTICE·해당 파일 헤더) 유지. 기본은 개념 차용(클린룸 재구현) 우선.

## P0 — 즉시 (저위험·고효익)

| # | 항목 | 대상 | 수용 기준 |
|---|---|---|---|
| 1 | CI에 `macos-latest` 추가 | `.github/workflows/ci.yml:11` | 3-OS 매트릭스 green. 2026-08-10 macOS 로컬 전체 통과로 리스크 없음 확인됨 |
| 2 | README에 macOS/Linux 실행 문단 | `README.md` | 두 플랫폼에서 README만 보고 빌드·실행 가능. `node-pty`가 Windows Claude 전용 요구임을 명시 |

> 적용 현황: **P0-1·P0-2 — 2026-08-10 로컬 적용 완료 (미커밋)**. push 후 3-OS 매트릭스 green 확인 필요.

## P1 — 단기 (버그성·비효율 수정)

| # | 항목 | 대상 | 수용 기준 |
|---|---|---|---|
| 3 | semantic state 파일 읽기를 1초 heartbeat로 이동, 렌더는 캐시 사용 | `src/main.rs:771-774`(read), `src/main.rs:1990`(렌더 호출), 기존 `observed_states` 활용 | 렌더 경로에 fs 호출 0회. 상태 표시 지연 ≤ 1s 유지 |
| 4 | `submit_relay`의 UI 스레드 sleep 제거 (50+250ms) — 전송을 워커 스레드/지연 큐로 | `src/main.rs:683-697` | handoff 전송 중 키 입력·렌더 지연 없음. 기존 live relay 테스트 통과 유지 |
| 5 | Unix에도 `~/.local/bin/claude` 폴백 (Windows `resolve_windows_agent_command`와 대칭) | `src/main.rs:392` 부근 spawn 경로 | PATH에 claude가 없어도 공식 네이티브 설치 위치에서 기동 |
| 6 | 탭별 git context — launch cwd 전용 해제 | `src/main.rs:1775-1787`(표시 조건), `spawn_git_context_reader` | 활성 탭의 workspace 기준 branch/dirty 표시 |

> 적용 현황: **P1 전체(3·4·5·6) — 2026-08-10 로컬 적용 완료 (미커밋)**. P1-4는 시한부 쓰기 큐(`pending_writes` + select 데드라인)로 구현 — UI 스레드 sleep 0. P1-6은 활성 workspace 공유(`Arc<Mutex<PathBuf>>`) + workspace 키 캐시 맵으로 구현 — 헤더 git 라벨이 활성 탭 기준, 미방문 workspace는 최대 5초 내 표시.

## P2 — 구조 개선 (herdr 참조 지점, 위 라이선스 게이트 적용)

| # | 항목 | 내용 | 수용 기준 |
|---|---|---|---|
| 7 | 에이전트 레지스트리 설정 파일화 | `src/lib.rs:27-45` 하드코딩을 선택적 TOML로. 기본값은 현행 3종 zero-config 유지 | 설정 파일 없이 현행과 동일 동작 + 설정으로 CLI 추가/인자 커스텀 가능 |

> 적용 현황: **P2-7 — 2026-08-10 로컬 부분 적용 (미커밋)**. `~/.agent-bridge/agents.json`(TOML 대신 JSON — 기존 serde_json 의존성 재사용, 신규 의존성 0)으로 내장 3종의 command·role·추가 인자 재정의. 시작 시 1회 로드, 파싱 실패·미지 에이전트는 파일 전체 무시 + 기본값 폴백 + 헤더 통지. `Box::leak` 1회로 기존 `&'static str` 계약 유지. 레이아웃 manifest는 command 대신 정규 키(codex/claude/agy) 저장으로 전환(기본값과 동일 문자열 — 기존 파일 호환). **신규 에이전트 종류 추가는 미지원** — AgentId·hook 정책 전면 동적화가 필요해 후속으로 분리(수용 기준 "CLI 추가"는 부분 충족).
| 8 | 세션 레이아웃 영속화 | 탭 구성(CLI 종류·workspace·yolo)을 manifest로 저장, 재시작 시 복원 제안. **프로세스 영속화(데몬)는 계속 비범위** — benchmark.md 원칙 유지 | 비정상 종료 후 재시작 시 이전 탭 구도를 1키로 복원 |

> 적용 현황: **P2-8 — 2026-08-10 로컬 적용 완료 (미커밋)**. `~/.agent-bridge/last-layout.json`에 탭 추가/종료 시마다 저장(serde_json, 신규 의존성 없음). 시작 시 이전 manifest를 메모리로 먼저 적재한 뒤 파일을 덮어쓰므로 복원 기회가 유실되지 않음. 복원은 F3 화면의 `Ctrl+L` 1키(예약 F키 미증설), 없어진 workspace는 건너뛰고 yolo는 manifest 기록과 무관하게 **현재 실행 모드**를 따름(권한 상향 방지). 다중 인스턴스는 last-writer-wins.
| 9 | handoff 대상 상태 게이팅 | 대상 Claude가 `working`이면 경고 또는 `waiting`/`idle` 전이까지 대기. herdr wait-until-blocked 시맨틱의 개념 차용 — 단 우리는 화면 추측이 아닌 hook 상태로 판정 | working 대상에 무경고 주입 불가. Codex/Agy(semantic 없음)는 현행 유지 + 경고 문구 |

> 적용 현황: **P2-9 — 2026-08-10 로컬 적용 완료 (미커밋)**. 전송 확정 시점에 hook 상태 캐시를 검사, `working`이면 차단 + "Enter 한 번 더 = 의도적 인터럽트" 명시 확인(`override_busy`). 편집·타겟 변경·paste 시 override 리셋. 자동 대기(전이까지 보류)는 자동 루프 금지 원칙과 복잡도 때문에 채택하지 않음. Codex/Agy는 semantic 부재로 게이트 미적용(현행 유지).

| 14 | Codex `finished` 부분 승격 (확인점 a 판정으로 2026-08-10 신설) | Codex spawn에 status-file env 상속 + `-c notify=["<agent-bridge 경로>","hook","finished"]` 세션별 주입. 선행: `hook` 서브커맨드가 Codex가 덧붙이는 JSON payload 인자를 허용하도록 완화 | Codex 탭이 턴 종료 시 `finished` 표시. waiting은 계속 activity만 표시(상태 정직성 유지) |

> 적용 현황: **P2-14 — 2026-08-10 로컬 적용 완료 (미커밋)**. `hook <state> [payload]` 완화, Codex spawn에 빈 상태 파일 + `-c notify=['<exe>','hook','finished']`(TOML 리터럴 문자열 — Windows 백슬래시 안전, 경로에 `'` 포함 시 주입 생략). 정직성 장치: Codex 상태 파일은 빈 값으로 시작해 첫 관측 전 semantic 미표시, `finished`는 Enter 재입력(터미널·pass-through·relay flush 경로) 시 해제, heartbeat가 semantic 소멸 시 캐시 항목도 제거. **잔여 live 검증**: 실제 Codex 세션에서 notify 주입 동작·`finished` 표시 확인 필요 (`live_codex_prompt_survives_mouse_wheel_scrolling` 계열 스모크에 추가 후보).

## P3 — 호환성·배포

| # | 항목 | 내용 |
|---|---|---|
| 10 | 터미널 쿼리 응답 확대 | 현행 DA1/DA2뿐(`src/main.rs:618-626`). DSR `ESC[6n`, OSC 10/11 등 — **선행 조건: 확인점 b의 실측** |
| 11 | 릴리스 자동화 | cargo-dist 등으로 3-OS 바이너리 릴리스. 현재는 소스 빌드가 유일한 설치 경로 |
| 12 | 스크롤백 개선 | 길이 설정화(현 2,000 고정, `src/main.rs:333`), F7 검색의 매치 위치 점프 |

> 적용 현황: **P3-12 — 2026-08-10 로컬 적용 완료 (미커밋)**. `AGENT_BRIDGE_SCROLLBACK`(1~100,000 클램프, 기본 2,000)으로 보관 줄 수 조정. F7은 가장 최근 매치의 스크롤백 오프셋으로 점프해 Scrollback 모드로 표시(라이브 화면 매치·타이틀 매치는 기존처럼 전환만). 검색 스캔 방향을 라이브→과거로 뒤집어 최근 매치 우선.

Housekeeping: `src/lib.rs:47-49` `relay_text` 구 API 정리 (앱 미사용, 테스트 전용). — **완료 2026-08-10**: `relay_text`/`relay_text_from` 제거, 빈 입력 검증 커버리지는 `handoff_rejects_an_empty_request`(tests/core.rs)로 이관.

### P5 — Visible delegation (2026-08-10 착수 — herdr agent-automation 벤치마킹, 전체 표면)

herdr에서는 pane 안의 에이전트가 `herdr` CLI/소켓(JSON-RPC, env `HERDR_PANE_ID`로 자기 위치 인지)으로 `pane split → agent start codex → agent prompt --wait --until done → agent read`를 호출해, 위임받은 에이전트가 **보이는 pane에서** 돌아간다. 초기에는 read-back을 제외한 축소안을 설계했으나, **2026-08-10 owner 결정으로 자동 판독·전달 루프 금지가 해제**되어(benchmark.md 개정 참조 — 상용 검증된 기능의 도입) 전체 표면을 채택한다:

| # | 항목 | 내용 | 수용 기준 |
|---|---|---|---|
| 18 | 파일 스풀 delegation 채널 | 소켓 대신 인스턴스별 요청 디렉터리(요청/응답 JSON, tmp+rename 원자성) + 세션 env(`AGENT_BRIDGE_REQUESTS`, `AGENT_BRIDGE_TAB`) 주입, 1s heartbeat가 수거. 응답 경로는 스풀 디렉터리 내부로 강제(임의 파일 쓰기 차단). 기존 hook 파일 패턴 재사용, 신규 의존성 0, 크로스플랫폼 | 탭 안의 에이전트가 `agent-bridge open <cli> [--workspace] [--prompt]`로 새 보이는 탭을 만들고 탭 이름을 돌려받는다 |
| 19 | prompt/status/read/wait 서브커맨드 | `prompt <탭> <텍스트>`(provenance 배너 부착 주입), `status <탭>`(semantic/activity 라벨), `read <탭>`(현재 보이는 화면 텍스트), `wait <탭> --until <상태>`(클라이언트 측 status 폴링 루프) | 호출 에이전트가 위임 대상의 완료를 기다렸다가 출력을 회수하는 전 과정이 가능하고, 그 전 과정이 사용자 화면에 보인다 |
| 20 | 불변 유지 장치 | 모든 주입에 `[Agent Bridge delegation · from <탭>]` provenance 강제(생략 불가), 위임 탭은 레일에 즉시 표시, 자동 승인·권한 우회는 계속 `--yolo`만 | provenance 없는 주입 경로가 코드에 존재하지 않음 |

전제·한계(v1): 에이전트가 이 채널을 쓰도록 CLAUDE.md/AGENTS.md 한 줄 지시 필요(herdr도 skill로 동일하게 opt-in), Claude 내부 Task 서브에이전트는 가로챌 수 없음(herdr도 동일), 나란히 보기는 pane 분할 부재로 레일 전환 관전(P4-17 split 후보와 연결), `open --prompt`는 CLI 기동 대기를 고정 지연(약 2.5s)으로 처리하며 단문 프롬프트 권장(정교한 준비 감지·bracketed 주입은 후속).

| # | 항목 | 내용 | 수용 기준 |
|---|---|---|---|
| 22 | self-documenting `--help` (2026-08-10 신설·**같은 날 로컬 적용 완료, 미커밋**) | 스킬·지침 문서 없이 `--help` 자체가 에이전트용 delegation 레퍼런스(서브커맨드·env 컨텍스트·상태 의미·왕복 예제). 서브커맨드 뒤 `--help`/`-h`도 인식 | 낯선 에이전트가 `--help` 한 번으로 위임 왕복을 수행할 수 있는 정보량 |
| 23 | 플래그 확장 패키지 (2026-08-10 평가·**같은 날 ①~⑦ 전부 로컬 적용 완료, 미커밋**) | 우선순위순: ① `read --lines N`(스크롤백 포함 최근 N줄 — "보이는 화면만"의 문서화된 한계 해소) ② `list`(전체 탭·상태·workspace 열람 — 밖에서 호출 시 핸들 발견) ③ 전 서브커맨드 `--json`(기계 판독 출력) ④ `prompt --wait [--until S]`(herdr처럼 제출+대기 원자 결합 — 별도 호출 간 상태 전이 경쟁 제거) ⑤ `open --title T`(의미 있는 탭 핸들, 중복 검사) ⑥ `close <tab>`(위임 탭 정리) ⑦ TUI `--restore`(기동 시 레이아웃 즉시 복원). **의도적 비채택**: `open --yolo`(위임 에이전트발 권한 상향 경로 — yolo는 launch 전역 전용 유지), `prompt --raw`(provenance 불변 위반) | 각 항목 채택 시 개별 수용 기준 정의 |
| 21 | 외부 호출 디스커버리 (2026-08-10 신설·**같은 날 로컬 적용 완료, 미커밋**) | 현재 delegation 서브커맨드는 세션 env(`AGENT_BRIDGE_REQUESTS`)가 필요해 **agent-bridge 밖의** Claude/Codex는 호출 불가. TUI 기동 시 `~/.agent-bridge/instance.json`(pid·스풀 경로)을 기록하고 종료 시 정리, 클라이언트는 env 부재 시 이 포인터를 읽어 stale-pid 검증 후 실행 중인 인스턴스의 스풀로 요청 — herdr의 고정 소켓 경로와 같은 역할을 데몬 없이 수행 | 밖의 터미널에서 `agent-bridge open codex --prompt "..."`가 이미 떠 있는 TUI 창에 보이는 탭을 만든다. TUI 미실행 시 명확한 오류("실행 중인 Agent Bridge가 없음"). 완전 headless(TUI 없는 spawn)는 비범위 — 보이는 탭 원칙과 충돌, 상시 가용이 필요해지면 daemon 전환을 별도 결정 |

### P4 — 마우스 친화 TUI (2026-08-10 사용자 요청 신설, herdr 벤치마킹)

| # | 항목 | 내용 | 수용 기준 |
|---|---|---|---|
| 16 | 마우스 친화 1차 | 레일 클릭 세션 전환, relay 중 레일 클릭으로 대상 선택, 레일 휠 세션 순환, F3 CLI 칩 클릭 선택, F1 diff 휠 스크롤. 자식 CLI mouse reporting은 터미널 영역 내 최우선 유지 | 키보드 없이 세션 전환·relay 대상 지정·CLI 선택 가능. 기존 마우스 전달·스크롤백 회귀 없음 |
| 17 | 마우스 친화 2차 (후속) | 푸터 힌트 클릭으로 F키 동작 실행, 레일 항목의 닫기(×) 클릭, 레일 스크롤(세션 다수), scrollbar 표시 | 미착수 |

> 적용 현황: **P4-16 — 2026-08-10 로컬 적용 완료 (미커밋)**. 우선순위: 레일 영역(클릭·휠, 모든 모드에서 소비) → diff 휠 → Add 칩 → 기존 자식 전달/스크롤백. hit-test는 순수 함수(`rail_row_to_session_index`, `agent_chip_at`)로 분리해 유닛 테스트. 레일 클릭은 Terminal/Scrollback/PassThrough에서 전환, Relay에서는 대상 선택(확인·캡처 리셋), 모달(Add/Search/Diff/Help)에서는 무시.

## 확인점 (구현 착수 전 검증 필요)

- a. **Codex notify 계약 재조사** — benchmark.md의 "신뢰할 수 있는 대기 이벤트 계약 미확인"은 2026-08-08 기준. 당시 판정 근거(Wiki 2026-08-08 기록): Codex `PermissionRequest` hook은 자동 review 중에도 발생해 사용자 대기 신호로 부적합. 따라서 재조사 대상은 PermissionRequest가 아니라 `notify`(turn-complete 계열) 경로이며, 세션별 주입 가능한지 최신 문서·실측으로 재확인. 성립 시 "Codex semantic state 승격"을 P2에 추가.
  - **판정 (2026-08-10 웹 재조사, 3+소스 교차)**: `notify` hook은 `agent-turn-complete` 단일 이벤트만 지원하고 `-c notify=[...]` 런타임 플래그로 세션별 주입 가능 → **Codex `finished` 부분 승격 성립** (P2-14 신설). `approval-requested`는 notify hook 미지원(openai/codex#11808 open) — waiting급 신호는 계속 부재. 구현 함정 2가지: ① notify payload는 stdin이 아니라 **argv 마지막 인자**로 전달되므로 `hook` 서브커맨드의 "정확히 1개 인자" 검증을 완화해야 함, ② auto-review 환경의 approval 계열 false-positive 보고(#8387 등) — waiting 부적합 판정 재확인. `[tui].notifications`의 OSC 9 방출은 PTY 스트림에서 관찰 가능한 잠재 waiting 신호이나 focus 의존 동작이 불명 — 확인점 b 실측에 통합.
- b. **임베드 CLI의 실제 터미널 쿼리 실측** — PTY 출력 로깅으로 각 CLI가 보내는 쿼리 목록 수집. P3-10의 범위를 추측이 아닌 관측으로 결정.
- c. **CJK 와이드 문자 live 테스트** — 렌더러는 wide continuation 처리(`src/main.rs:2041`)하나 한국어 입력/에코 실측 없음.
- d. **node-pty 전역 설치 의존** — nvm/volta 환경의 `npm root -g` 경로 차이, 버전 고정. 장기: conpty 직접 호스팅으로 사이드카 제거 가능성.

## 의도적 비범위 (benchmark.md 유지)

자동 worktree 생성 · tmux 의존 · 외부 웹 대시보드 · 자동 승인/권한 우회 · 에이전트 응답 자동 판독 무제한 루프 · **외부 데몬** (herdr식 프로세스 영속성은 P2-8 레이아웃 복원으로만 절충).
