# Current Codex Boundary

- The Codex adapter decides surface eligibility, the Herdr version floor, `--no-daemon`,
  and the initial-prompt route in one place (`surface_route`), shared by `surface_support`
  and `prepare_launch`. `surface_support` returns the selected initial `Transport`, which
  `session_start` carries directly into `turn_run::run_initial` so initial delivery never
  re-selects a transport independently.
- Off Windows, Herdr launches require stable Codex 0.169.0 or newer and explicitly pass
  `--no-daemon`, so tool processes retain the hc terminal identity instead of running
  in the shared app-server. Pre-creation admission (`provider::admit_surface`) refuses
  Codex below stable 0.169.0 on Herdr right after the CLI version probe, before any
  session or terminal exists: an explicit `--terminal herdr` fails with that refusal, and an
  automatic Herdr choice inside hc takes the OS's own terminal instead
  (`session_start::admit_choice`). Its caller check has already run by then, once, before the
  probe, as in 0.4.49; the fallback ignores its outcome. `surface_version_requirement` declares that this
  choice depends on the version (stable 0.169.0 or newer), so `doctor` decides it with the
  launch's own probe (`provider_installation::read_installed_version`). `prepare_launch`
  applies the same decision from the recorded
  surface (never an inherited `HC_TERMINAL_ID`) and the recorded version. That gate reads
  the version text by one grammar (`herdr_codex_version`). The label is the word
  `codex-cli`, the start of every Codex `--version` output this repository records
  (`OpenAI Codex (vX.Y.Z)` is only the TUI header in screen captures); the field is the next
  word on the label's line, read whole as an optional `v` and SemVer with no punctuation
  removed. With exactly one label, the field's result is the answer, error included: words
  before the label (a warning, a timestamp, another program's version) and after the field
  are never read as the version and are only scanned for a second label, so `0.170.0--` is a
  prerelease and `0.170.0+`, `0.170.0.` and `codex-cli unknown (runtime 22.0.0)` are
  refused. Without a label (`unlabelled_codex_version`, the one place of that rule), the text
  is read only when exactly one version-like word is present (owner's decisions 4B and B2,
  2026-11-26). The text is split on whitespace, and a word counts when it contains three
  dot-separated numbers anywhere in it; no word is exempt. With exactly one such word, its
  enclosing punctuation is removed (opening brackets and quotes before it; closing brackets,
  quotes and `.`, `,`, `;`, `:`, `!`, `?` after it), and the rest must read whole as the
  labelled field does: one optional `v`, then SemVer. `OpenAI Codex (v0.170.0)`, `(0.170.0)`
  and `Codex 0.170.0.` are 0.170.0, `0.170.0--` and `0.168.0-rc-.` are prereleases, and
  `0.170.0+`, `0.170.0-`, `<0.169.0`, `codex/0.168.0`, `v0.168.0.1-rc.1` and `0.168.0.1--` do
  not read. With no such word, with two or more (the same version printed twice included), or
  with one that does not read, the text is not read for Herdr. Where the general provider
  probe has accepted a supported version from the same text, an automatic choice then takes
  its fallback and an explicit `--terminal herdr` is refused; text the general probe cannot
  parse (`unknown`, `codex/0.168.0` alone) or refuses (`0.146.0+`, below TAB's 0.147.0
  minimum) is refused there, before any surface is chosen. The rule
  refuses whenever more than one word could be the version, since a misread version could let
  a Codex that lacks `--no-daemon` start on Herdr, where a detached `ask` can already have
  reported the request accepted when Codex refuses the option. Its accepted cost: a banner that
  also shows an address, a URL with a numeric host, a four-part build number or another
  program's version is not read, including banners 0.4.48 admitted such as
  `OpenAI Codex (v0.170.0) (proxy=127.0.0.1:8080)`. What it establishes is limited to the one
  word it reads: it does not read the words around that word, so a sole version-like word is
  read whatever program it names (`unknown (runtime 22.0.0)` is 22.0.0), and a comparison
  written apart from its version is not seen (`Codex < 0.169.0` is 0.169.0). A second label
  and a label without a field on its line are refused.
  The wrapper reads the recorded version by the same rule, so a pending launch that another
  release's binary starts (the executable at its path replaced in between) can be refused
  there, after its Session and terminal exist. Do not replace the binary while a Herdr launch
  is pending. What each release's wrapper does with a recorded text is this table, which
  `pending_herdr_launches_across_the_wrapper_handoff` reads and checks against the frozen
  0.4.48 and 0.4.49 readers and this build's (`\n` is a line break):

  <!-- herdr-version-handoff -->
  | `--version` text | Codex meets the floor | 0.4.48 | 0.4.49 | this build |
  | --- | --- | --- | --- | --- |
  | `codex-cli 0.170.0` | yes | launches | launches | launches |
  | `codex-cli 0.168.0` | no | refuses | refuses | refuses |
  | `codex-cli 0.170.0-alpha.1` | no | refuses | refuses | refuses |
  | `0.170.0` | yes | launches | launches | launches |
  | `OpenAI Codex (v0.170.0)` | yes | launches | refuses | launches |
  | `(0.170.0)` | yes | launches | refuses | launches |
  | `0.170.0+` | yes | launches | refuses | refuses |
  | `codex 0.170.0` | yes | launches | refuses | launches |
  | `unknown (runtime 22.0.0)` | unknown | launches | refuses | launches |
  | `plugin 0.160.0\ncodex-cli 0.170.0` | yes | refuses | launches | launches |
  | `codex-cli 0.170.0+` | yes | launches | refuses | refuses |
  | `node v22.0.0\ncodex-cli 0.168.0` | no | launches | refuses | refuses |
  | `node v22.0.0\nOpenAI Codex (v0.170.0)` | yes | launches | refuses | refuses |
  | `codex 0.170.0 (v0.170.0)` | yes | launches | refuses | refuses |
  | `0.170.0-01 (runtime 22.0.0)` | no | launches | refuses | refuses |
  | `0.170.0--` | no | launches | refuses | refuses |
  | `(0.170.0--)` | no | launches | refuses | refuses |
  | `0.170.0-` | unknown | launches | refuses | refuses |
  | `0.168.0- runtime 22.0.0` | no | refuses | refuses | refuses |
  | `OpenAI Codex 0.168.0-rc-. (runtime 22.0.0)` | no | refuses | refuses | refuses |
  | `OpenAI Codex (v0.168.0)` | no | refuses | refuses | refuses |
  | `unknown` | unknown | refuses | refuses | refuses |
  | `OpenAI Codex 0.170.0 (proxy 127.0.0.1:8080)` | yes | launches | refuses | refuses |
  | `OpenAI Codex (v0.170.0) (proxy=127.0.0.1:8080)` | yes | launches | refuses | refuses |
  | `OpenAI Codex (v0.170.0) (proxy http://127.0.0.1:8080/)` | yes | launches | refuses | refuses |
  | `codex 0.170.0 build 1.2.3.4` | yes | launches | refuses | refuses |
  | `Codex <0.169.0` | no | launches | refuses | refuses |
  | `Codex < 0.169.0` | no | launches | refuses | launches |
  | `OpenAI Codex (v0.168.0.1-rc.1) (runtime 22.0.0)` | no | launches | refuses | refuses |
  | `OpenAI Codex (0.168.0.1--) (runtime 22.0.0)` | no | launches | refuses | refuses |
  | `codex/0.168.0 (node 22.0.0)` | no | launches | refuses | refuses |
  <!-- /herdr-version-handoff -->

  Notes on the table. The Codex column says whether the Codex the text describes meets the
  floor (stable 0.169.0 or newer): `yes`, `no`, or `unknown` where the text does not show
  Codex's own version. This build launches a labelled row only when its `codex-cli` field is a
  stable version at or above the floor, whatever other versions the text holds
  (`plugin 0.160.0\ncodex-cli 0.170.0` launches from its field), and an unlabelled row only
  when its one version-like word, read alone, is. The test checks that the rows it launches
  whose Codex is not shown to meet the floor are exactly `unknown (runtime 22.0.0)` and
  `Codex < 0.169.0`, the limits stated above. A `<` written onto the version is part of the
  word and no enclosing punctuation, so `Codex <0.169.0`, which 0.4.48 read as 0.169.0 and
  launched, does not read here.

  The queue floor, hook eligibility and doctor keep the general probe's reading (the first
  token that parses once surrounding punctuation is removed), under which earlier releases
  wrote and admitted launch records.
  Other surfaces retain their launch arguments and require no 0.169.0 floor.
  The version floor is eligibility; ask, queue follow-up and completion need separate
  live verification. No Codex configuration file or daemon is changed.
- On native Windows the adapter refuses a Herdr terminal in `surface_support` before any
  resolution, probe, or session exists, and `prepare_launch` applies the same decision to
  a recorded handle as a backstop: the initial prompt there is the injected console paste
  below (text without Return, then one Return against the exact accepted screen), which
  `terminal.send` cannot do, and the launch-argument route above would put the multi-line
  framed prompt on a Windows command line (32,767 characters; 8,191 and no line break
  through an npm `codex.cmd`).
  Inside an hc terminal the automatic choice for Codex is therefore the managed console, the
  fallback every OS uses for a provider Herdr does not admit (on macOS and Linux, a Codex below
  stable 0.169.0 gets Ghostty or tmux the same way; an explicit `--terminal herdr` is refused).
  Replace the refusal with a Windows Herdr route that keeps argument semantics and size, or a
  first-party addressed input.

- The injected Windows initial paste is a bounded best-effort console fallback (#124),
  pinned to Codex 0.163.0. It assumes no concurrent input; screen snapshots are not atomic
  with TUI transitions and placeholder counts are not content receipts. Non-injected or
  missing launch records retain the legacy console route, including its consent-only limit.
- Codex groups plain input at 8 ms intervals, flushes on Windows after 60 ms, and collapses
  individual pastes over 1000 Unicode scalars. Active-burst Enter and Shift+Enter insert LF.
  Applying a complete paste clears the 120 ms suppression window: no 150 ms proof exists.
  A visible prefix with input still pending can absorb Return into a later burst.
- Before writing, require the positive empty composer and a 20 s reserve. Write/drain body
  text only and verify its scalar count, then wait 150 ms before writing/draining LF plus the claim
  marker and verify that count, with no screen read between writes. The final exact inline
  marker is completion evidence under ordered console/reader/composer processing; its
  applied paste clears burst suppression. Wrapped/scrolled body rows need no positional
  matching. If the last segment is a placeholder, require the Codex-normalized framed scalar total
  from placeholders plus inline text (physical rows joined with LF): cardinality only,
  not a content receipt. Repeated labels use ` #2` outside the bracket. A dialog/picker
  veto precedes success; an unobserved/clipped marker or ambiguous fallback never submits.
  The request has one absolute deadline, and Sent still requires the claim binding.
- The 150 ms split gap only encourages the 60 ms idle flush; it proves nothing.
  The exact inset-stripped marker row ends composer content; subsequent rows still undergo
  the UI veto. Numbered pinned menu labels and confirmation hints anywhere, and any
  column-zero `› ` row outside the span, remain unresolved UI; the UI recognizer is pinned
  to Codex 0.163.0 forms at this PC's 120-column managed Windows Terminal tab and console
  window widths, the upstream 40-column snapshot and the 43–47-column sandbox wrap.
  Otherwise use the first recognized footer (workspace-qualified model line, warning,
  shortcut/queue/interrupt/edit/quit hint or context/token count), trimming separator blanks
  but preserving internal blanks. No marker and no recognized footer remains ambiguous.
  Count normalization is CRLF/CR to LF and control/CSI sanitization, not blank-line collapse
  (Codex rust-v0.163.0 paste_input.rs:145-176; history_cell/messages.rs:25-85).
  TAB trims source prompts before framing: the live 25596/1094 estimates become
  25595/1093 there. Cursor-dependent blockquote fallback stays ambiguous; exact inline
  marker evidence remains usable. Unknown custom status lines require that marker.
- Recheck the current claim, session and deadline after blocking work and at input dispatch.
  The injected Windows helpers hold the lifecycle lock across the final claim/session/expiry
  check and each write; screen reads and drain waits remain outside that lock.
  Submit one guarded Return against the exact accepted full screen; a changed screen gets
  none. After body dispatch all unconfirmed failures are uncertain, except a helper's
  proven pre-write refusal. Never retry text or Return, including after late binding.
- Sent requires this claim's `UserPromptSubmit` binding, which proves inspection; acceptance
  and completion remain the transcript observer's job. Missing binding is uncertain with
  a final composer classification. Recovery is conditional: inspect result and surface;
  consider Enter once only with the complete draft, no dialog and no running turn, otherwise
  close or diagnose. Never claim unreadable content was observed.
- Replace this protocol when Codex exposes a verified Windows console paste event or
  newline-preserving input API. The legacy delayed-Return machinery remains until its
  supported non-injected configurations have a replacement.

- Codex 0.163.0 runs Claude-compatible lifecycle hooks (feature `hooks`, stable and on by
  default): a command hook receives one JSON object on stdin, runs in the turn's cwd with
  Codex's environment, and must never exit with code 2 (that blocks the turn). The legacy
  `notify` is `hooks/src/legacy_notify.rs`: the whole payload as one argv argument,
  spawned and not awaited, marked for removal upstream; on native Windows a payload past
  32,767 UTF-16 units never spawns (#127). TAB therefore injects a `UserPromptSubmit`
  and a `Stop` command hook as one `-c hooks=` session-flag value and keeps `-c notify`
  as the fallback. The hook path is the official one; the notify path stays until every
  supported configuration has a verified replacement route (settings off, hook feature
  off, a launcher the shell cannot run, a transcript TAB cannot observe, and Codex
  below 0.163.0 all still complete through notify), and removing notify needs that
  capability matrix, not an upstream deletion.
- Injection is decided at launch and recorded in `codex-launch-mode.json`: the recorded
  preflight version must be 0.163.0 or newer and not a prerelease, the state root's
  `codex-lifecycle-hooks` setting on, and the executable path one that cmd.exe and
  PowerShell run unquoted (the npm `codex.ps1` shim strips double quotes inside a `-c`
  argument, observed 2026-10-18, so the value uses TOML literal strings and no `"`).
  Every later decision reads that record, never current settings or a probe. The record
  says what was configured; only the session's own records say what ran.
- A non-managed hook runs only when `hooks.state.<key>.trusted_hash` equals the hash
  Codex computes from the normalized handler, and the state is read from the user and
  session-flag layers, so TAB passes the two hashes it computes itself: SHA-256 over
  the compact, key-sorted JSON of the TOML identity `{event_name, hooks:[{type, command,
  timeout, async}]}` (`discovery.rs:777-793`, `fingerprint.rs:54-65`), pinned to the four
  values Codex wrote on Windows and macOS (R0, 2026-10-18; keys
  `C:\<session-flags>\config.toml:<label>:0:0` and `/<session-flags>/config.toml:...`).
  This replication is a fallback with a drift risk: a hash Codex no longer accepts makes
  the TUI open its hook-review picker before the composer on both platforms, and the
  picker holds the initial prompt (macOS awaits the review before `App::run`). TAB
  never answers the picker. On Windows the Codex adapter requires the empty composer row
  before every initial paste of an injected launch, regardless of stored workspace trust
  or an applied trust override; a deadline names the picker as diagnosis. The doctor's
  `hooks/list` probe (`codex app-server`, JSON-RPC) reports the installed Codex's own
  trust status and hash without writing anything. `--dangerously-bypass-hook-trust` is
  never passed: it would run every user and project hook unreviewed.
- `UserPromptSubmit` gives the turn's identity before the answer exists: `session_id`,
  `turn_id` and `transcript_path`. It proves inspection, not acceptance; another input
  hook can still reject TAB's input. TAB binds the current claim to that identity
  (`codex-turn-<claim>.json`) only for a prompt that begins with the delegation header
  (directly or after the exact IDE wrapper) and ends with the pending marker, under the
  lifecycle lock against the live claim; an identical rebinding is idempotent, a
  different identity is a diagnostic. The marker alone never identifies a turn (#61), and
  a message typed into the TUI during the turn is steered into the same turn without the
  header (#65). The title turn is a temporary thread with hooks disabled, so it sends no
  hook payload, but its legacy notify can still arrive first; keep the #61 fixtures.
- The delegation header `[Agent Bridge native delegation]` is the Codex frame's own first
  line (#396): the shared layer adds no framing and the request receipt keeps the Request
  source, so the frame has no Source line. Keep the header byte-identical: the transcript
  watcher of a running owner that an earlier binary started checks it, and a prompt that
  binary framed (with its Source line) still begins with it. Shorten it only together with
  a reader that also accepts the old header in every running owner.
- `Stop` is not the end of the turn: every Stop handler runs while Codex decides whether
  to stop, and another handler's block continues the same turn (`session/turn.rs`).
  TAB records the last Stop as a candidate (`codex-candidate-<claim>.json`) and never
  publishes from it. Legacy notify runs after that decision but before the turn function
  returns, so a post-turn compaction interrupted by Esc can follow a notify success with
  `turn_aborted`. For a bound claim notify is therefore evidence only
  (`codex-notify-<claim>.json`) and does not release the claim; for an unbound claim
  (no hook injected or no binding) notify settles exactly as before.
- A bound claim settles from Codex's own terminal record in the rollout transcript,
  observed by the adapter's session watcher (`LaunchPlan.watcher`): `task_complete` (or
  its `turn_complete` alias) and `turn_aborted` for the bound turn id, emitted after the
  hooks and notify of the turn (`tasks/mod.rs:829-866`) and persisted with a flush per
  append but no latency guarantee. The watcher reads the bound file only, from byte zero,
  newline-terminated records with strict UTF-8 and JSON, verifying the file's identity and
  its largest observed length around every read; replacement, shrink, a read failure, a
  malformed or oversized record or a malformed attribution field suspend that claim with a
  diagnostic (`codex-observer-<claim>.json`) and are never evidence about the prompt.
  A record past 8 MiB is reduced in place, not retained (#269: tool outputs carrying
  several base64 images, observed up to 50 MB): every string over 4 KiB becomes `""`
  after its bytes pass serde_json's `String` checks, the whole line passes strict UTF-8,
  and the reduced record must fit 8 MiB and parse as JSON. Only a non-message
  `response_item` after the verified `session_meta` is skipped, and only with a non-empty `payload.type` and no repeated
  top-level `type` or `payload` or `payload.type` (`Value` keeps the last duplicate). Any
  other record past 8 MiB, a user message with pasted images included, stays `oversized`:
  cutting it could hide the framed prompt. A fault in cut text suspends with its own reason
  as soon as it is read. Each running poll reads at most 256 KiB; the reduction at 8 MiB
  and the final parse are each up to 8 MiB of work within one poll. Acceptance is
  proved by one `user.text` message of the bound turn
  (`payload.internal_chat_message_metadata_passthrough.turn_id`) that carries the framed
  prompt; its absence is proved only by a complete, validated prefix, and then the request
  fails as not accepted. An abort fails with Codex's reason, a terminal error with its
  text, a null or blank answer as such; otherwise the answer is published after the
  marker is stripped, with `provenance` naming the signal. The transcript is a
  provider-owned record, not a documented contract: replace the watcher when Codex
  provides a terminal lifecycle event that carries the answer, delivers notify through
  stdin, or lets a TUI launched with overrides attach to the daemon (the embedded app
  server is in-process, so no external client can observe `turn/completed`).
- The same Cursor observes question emissions only from the bound turn's completed
  `AgentMessage` with `delivery: async` and validated structured questions, after accepted
  framed input. Only a request bound through injected Codex hooks can have this evidence;
  an unbound, notify-settled Claim cannot. Raw calls, tool output and prose supply no emission
  evidence. Optional turn attribution that is absent, malformed or different is ignored
  during byte-zero replay; an attributable item's malformed optional content marks incompleteness without
  suspending completion. One private `codex-questions-<claim>.json` holds a prefix that
  replay never rewrites. The 64 KiB limit covers the whole pretty-encoded record, Request
  identity included. Replay preserves prior items and monotone incompleteness. Publication
  checks the same locked Claim/status/pending predicate as binding plus exact Binding equality; failure
  never changes completion, and there is no backfill after Claim release, so a write that
  still fails at the terminal poll loses that evidence. The terminal flush reuses the
  successful terminal integrity checkpoint; nonterminal publication verifies separately.
  Queries use the selected historical Receipt under their existing lock. Ordinary single
  `result` shows content; status focuses its active Receipt or existing latest Result's Receipt;
  ordinary inspect shows per-request presence. Successful waited `ask`/`tell`/`reopen`,
  and an ended Request's `result --wait` and default wait add only the retained count;
  after a failed `ask`/`tell`/`reopen` wait, query the Request's result. A pending poll without
  `--questions` reads no record. List, timeline, attachments and completion records
  retain their contracts. Opt-in `wait --questions` returns unseen retained
  emissions without settling the Request; its caller-held cursor is observation only.
  Emitted never means pending or answered, absence is
  unknown, and a later `tell` is another Request, not resolution. Tool availability depends
  on model/configuration and retained history. Replace this provider-owned transcript
  fallback when Codex provides a verified, addressed question event or lifecycle for the
  same injected TUI launch; never switch on a version check. Question component verification
  is recorded in `docs/verification/2026-10-19-codex-question-emissions.md`.
  Measured on macOS with codex-cli 0.166.0, once per case
  (`docs/verification/2026-11-05-macos-342-codex-question-answer.md`): the question tool
  returns at once and the turn goes on; a `codex queue` message sent during that turn never
  joined it and started the next turn (`turn_trigger: queue`); a child with nothing else to do
  kept its turn open waiting, so its request stayed pending and no `tell` could reach it.
- The question notice is a fallback for that wait (#342): no person is at a managed session's
  terminal, and Codex 0.166.0 offers no addressed way to answer a question in the running turn.
  The state root's `codex-question-notice` setting is read once at launch. It defaults to off
  (owner's decision, 2026-11-05, after 0.4.36 shipped it on): the hook only adds advice that a
  caller may not need, so the user opts in. Do not turn the default on from a version check or
  because Claude's deflection is on.
  `codex-question-notice.json` records whether the hook was injected and, if not, why (setting
  off, lifecycle hooks not injected, launcher unavailable, Codex below 0.166.0); every later
  read uses that record, and a session without it has no notice hook. When it is injected, the
  one `-c hooks=` value also carries a `PreToolUse` group with the exact matcher
  `request_user_input_async` (Codex compares a pattern of letters, digits and `_` with the tool
  name and its aliases, and this tool has none) and its `trusted_hash` under the session-flags
  `pre_tool_use` key. The matcher is part of Codex's trust identity: the `hooks/list` probe of
  codex-cli 0.166.0 reported the pinned value `trusted` and the matcher-less hash on the same
  group `modified` (2026-11-05). This replication shares the drift risk of the two lifecycle
  hooks: a hash Codex no longer accepts holds the hook-review picker before the composer and
  the initial prompt, so the doctor's probe proposes and checks this hook whenever a new launch
  would inject it. For that tool the hook prints one `hookSpecificOutput` with
  `additionalContext` only: no permission decision, reason or input rewrite. It is a notice, not
  a refusal: the tool is asynchronous by design, the child may keep working while its question
  is open, and a refusal would cut that work short. The tool still runs, the Cursor records the
  emission as above, and the notice tells the child that no reply can arrive during the turn,
  to finish what does not depend on the answer and to end the turn with the line
  `QUESTION FOR CALLER: <question> Options: <option labels>` (Claude's deflection format). The
  hook reads only that launch record: it takes no lock or Claim, writes nothing and sets no
  `deflected` flag. An unreadable record exits 1, which Codex reports as a failed hook and then
  runs the tool without the notice; it never exits 2. `inspect` reports
  `capabilities.questions.response` as `answer_after_turn` (`hook_configured_unverified`,
  `answer_by: tell`): configuration, not evidence that the child read or followed the notice.
  The notice is advice to the model: a child that ignores it can still wait in its turn, and
  then the Request stays pending, `tell` is refused and a queued message does not join the
  turn, exactly as before. The 0.166.0 floor is eligibility, not runtime proof. Codex runs every
  matched `PreToolUse` handler; TAB's group neither removes nor reorders a user's own, and a
  user's hook that denies the tool blocks it, leaving no emission. Native Windows at runtime,
  other Codex versions, more than one question in a turn, a user's own `PreToolUse` hook beside
  this one and subagent questions are unverified. Replace this fallback when Codex offers a
  verified addressed way to answer a question in the running turn of the injected TUI launch.
  The macOS owner check with codex-cli 0.166.0 (two requests with the notice on, their
  follow-up answers, and one request with it off; run while the default was on) is in
  `docs/verification/2026-11-05-macos-342-codex-question-notice.md`.
- Lifecycle completion live verification on native Windows, 2026-10-18, codex-cli 0.163.0
  (`docs/verification/2026-10-18-windows-127-codex-lifecycle-hooks.md`): bound claims settled
  from `task_complete` with and without a notify record (a 25,463-character prompt with a
  10,092-character answer left none), a steered TUI message joined the bound turn without
  rebinding, a user Stop hook's block continued the turn and only the final answer was
  published, a model error failed the request from `task_complete{error}` with no Stop and no
  notify at all, `settings_off` and `features.hooks=false` settled through notify, and an
  untrusted user hook held the picker, which the Windows gate named at the deadline. Not
  verified: Esc during post-turn compaction, a TAB hash-drift build, and macOS. The
  native-queue `tell` passes the framed prompt as one `--message` argument, so on native
  Windows a follow-up above the command-line limit fails to start `codex queue` before anything
  is sent (os error 206 at 35,520 characters; #251): a send-side limit that the hook route does
  not change.
- When a correlation question comes up, record the raw payloads with a temporary build
  and read them: hook stdin, notify argv and the transcript lines. Do not reason from the
  result alone. The two payloads of #61 looked alike in everything but the thread, the
  input and the answer.
- Codex's background memory consolidation delegated through `tabcli ask` from its memory
  workspace and left the sessions open (issue #216: three on 2026-10-15 with tabcli 0.4.10 and
  codex-cli 0.162.1, three on 2026-10-08 with 0.2.6), and a `LOCAL.md` instruction did not
  stop it.
  Launch admission therefore refuses a workspace that is `memories` beside Codex's
  `config.toml` or lies inside it, for every target provider, unless the state root's
  `codex-memory-workspace` setting is `allow`. This is TAB's admission policy for an
  observed path, not a fallback for a Codex transport; the Codex adapter only declares the
  location. Re-examine the default when Codex's behavior is verified to stop through an
  official control or an upstream fix; do not disable it from a version check alone.
- A `tabcli` started from a Codex tool shell inherits Codex's caller context. On native
  Windows, 2026-10-29 (#111, names only), the Terminal, tab host, PowerShell root and managed
  Claude all carried `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_TOOL_CALL_ID`,
  `CODEX_DAEMON_SHUTDOWN_FILE` and `CODEX_DAEMON_SHUTDOWN_SOCKET`. In Codex 0.166.0 the first
  three name the caller's session, thread and tool call; a managed Codex rewrites them for its
  own tool shells, but its command hooks and legacy notify replay the inherited process
  environment. On Windows the presence of the socket marker lets a control-socket app-server
  accept the PID-checked shutdown route, and the file marker is one updater's shutdown
  request path. The Codex adapter owns exactly these five names as caller markers and every
  adapter selects them for its probes, launches, messenger and the Codex queue; non-provider
  helpers drop them through the union. Never strip by `CODEX_*` prefix: `CODEX_HOME`, CA,
  authentication, install hints, `CODEX_CI`, `CODEX_VERSION`, the permission profile, patch
  compatibility and sandbox or proxy context stay. A hook that relied on an outer Codex id no
  longer receives it. Source evidence and the unverified runtime matrix are in
  `docs/verification/2026-10-30-codex-caller-environment-source-notes.md`. Revisit an entry
  when Codex stops exporting it or isolates it across every child path, hooks, notify and
  nested launches included; rewriting tool-shell ids alone is not that condition.

