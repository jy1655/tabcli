# macOS initial approvals and live application verification

Claude's first-run workspace trust and tool permissions, macOS permissions, and Codex/ChatGPT tool access policies are separate approvals. Granting one does not grant the others. In particular, `Computer Use is not allowed to use the app '…' for safety reasons.` does not mean that Claude login or workspace trust failed.

## Check the layer where the error occurs

| Observation | Meaning | Next action |
| --- | --- | --- |
| Computer Use refuses access to a terminal app with the message above | Computer Use's app safety policy | You cannot operate the terminal GUI through Computer Use. In a separately authorized shell environment, you can use the CLI diagnostics and `self-test` below. |
| An actual Codex Auto-review `Denied` result and its reason | Approval review of a specific tool call | Check the exact action, target, and reason. In a supported Codex TUI, `/approve` requests one reconsideration of that denied action; it does not lift Computer Use's terminal app restriction. |
| macOS asks whether to allow control of another app, or an Apple Events permission error occurs | Automation permission from the calling app to the target app | The user must check the actual calling app and target app pair in System Settings → Privacy & Security → Automation. |
| Claude shows a workspace trust or tool execution approval screen | Claude's workspace trust and tool policy | Run Claude directly in the workspace you will pass to Bridge, review the prompt, and approve it. Check the rules with `/permissions`. Do not record this as permission to control the entire terminal app. |
| A keychain access dialog, `CSSMERR_CSP_OPERATION_AUTH_DENIED`, or a login failure message | A clue to a credential access or authentication problem | The user must check the requesting app and keychain item. Do not conclude from authentication failure alone that there is a keychain problem or that a token was revoked. |
| Warp Control access fails or creation features are unavailable | Availability of Warp's endpoint, Scripting, and TabConfigs | Check the conditions in [requirements and first-run prompts](terminals.md#requirements-and-first-run-prompts) and the actual error. Approving Claude does not enable Warp features. |

The [official Computer Use documentation](https://learn.chatgpt.com/docs/computer-use), checked on 2026-10-03, restricts bypassing security policies by operating terminal apps. The error above was confirmed again that day when accessing Warp (`dev.warp.Warp-Stable`) and Terminal.app (`com.apple.Terminal`). Do not report this error as merely an “automatic approval review denial.” Do not tell users that Always allow for ordinary apps, Full Access, or Bridge's `--yolo` lifts this restriction. Do not retry the denied app operation through another UI technology.

The same official documentation distinguishes file edits and shell commands as subject to separate approval and sandbox policies. A Computer Use denial therefore does not require all CLI integration checks to be run manually by the user. An authorized shell tool can check the product's existing CLI/API. If the actual shell call is separately denied, follow that denial's reason; this explanation does not authorize bypassing it.

[Auto-review](https://learn.chatgpt.com/docs/sandboxing/auto-review) changes who reviews approval requests. It is separate from Computer Use app approval; automatic review neither allows every action nor grants OS permissions. Likewise, `--yolo` only forwards [each provider's permission options](security-and-data.md#permission-modes).

## First use of Claude

1. Open the terminal you intend to use and change to the directory you will specify in Bridge's `--workspace`. Check the actual executable and version with `command -v claude` and `claude --version`, then run `claude`.
2. Review each login, workspace trust, and tool permission prompt shown. If you are already logged in and trust is valid, you do not need to approve it again. The approval scope follows Claude's settings and workspace. It does not mean “approving each terminal once permanently allows every project.” Distinguish one-time tool permission from saved rules. [Claude permissions documentation](https://code.claude.com/docs/en/permissions)
3. The user handles a keychain dialog only if one actually appears. Apple's Allow Once permits this access only; Always Allow permits that access in the future as well. Do not record a workspace trust dialog as keychain approval. [Apple explanation](https://support.apple.com/en-euro/guide/keychain-access/kyca1243/mac)
4. Request a short marker response that needs no tool execution to check that the CLI itself works. This success does not prove Bridge's follow-up delivery, tab creation, or cleanup.

Different shell environments in different terminals can select different Claude executables or configuration directories. Different `CLAUDE_CONFIG_DIR` values can also mean different authentication stores, so compare only the paths when needed. You do not need to print or copy authentication files or keychain secrets. [Claude authentication documentation](https://code.claude.com/docs/en/authentication)

Automation permission differs from tool approval inside Claude. The user must check the app that actually requests control and its target. [Apple Automation guide](https://support.apple.com/en-hk/guide/mac-help/mchl108e1718/mac)

## CLI diagnostics and live application checks without Computer Use

`doctor --provider claude --probe --json` checks availability, including the CLI path, version, and workspace. Claude's `--probe` does not call a model or messenger, so it does not prove successful delivery. Read existing requests with `inspect <session> --timeline --json` and `result <session> --request <request-id> --json`. Do not resend the same request while it is pending or delivery is uncertain.

The `self-test` below checks Bridge's actual `ask`, `result`, `tell`, and `close-session` paths without calling Computer Use. Agents can also run it in an authorized shell environment. It makes real model calls and creates and closes a new terminal surface; it is different from a provider check without a visible surface. If an approval dialog appears, the user must review and handle it.

First, record the exact source commit and the SHA-256 of the binary under test. The early candidate `3648cc65abf1d2a76f28ba6e17d78fbabafe25ff` included Warp, WezTerm, and `macos-open-mode` but still printed `0.0.10`. This is a historical candidate; do not infer identical source from the version string alone. For 0.1.0 checks, use the per-candidate hashes in the [focus and startup input verification record](verification/2026-10-03-v0.1.0-focus.md) (Korean). Use the absolute path to the binary under verification without replacing the installed copy.

```sh
git rev-parse HEAD
git status --short
cargo build --locked
shasum -a 256 target/debug/tabcli

ab_binary="$PWD/target/debug/tabcli"
ab_workspace="$PWD"
ab_report_dir="$(mktemp -d "${TMPDIR:-/tmp}/agent-bridge-check.XXXXXX")"
```

Preserve the diff too if the source has changed. The examples below use the provider's default permission mode. If you use `--yolo` under a separate, already authorized execution policy, record that fact and do not report it as verification of the normal approval mode. The user must handle keychain, workspace trust, and Automation dialogs.

```sh
"$ab_binary" self-test claude \
  --workspace "$ab_workspace" --terminal terminal \
  --timeout-secs 120 --isolated --json \
  > "$ab_report_dir/terminal-claude.json" \
  2> "$ab_report_dir/terminal-claude.stderr"
ab_terminal_exit=$?
printf 'Terminal exit=%s; report=%s\n' "$ab_terminal_exit" "$ab_report_dir/terminal-claude.json"
```

After the Terminal check and cleanup finish, check Warp separately if its prerequisites are met. The following command uses Warp Stable's official Control CLI only to list instances. This path is the installed Stable app's control mode; do not infer a path for another channel such as Preview from it.

```sh
/Applications/Warp.app/Contents/MacOS/stable --warpctrl --output-format json instance list
```

`instances: []` means no controllable endpoint was found. Check whether Warp is running, whether the build supports it, and Settings → Scripting; if enabling it is necessary, the user must make that choice. Do not conclude from an empty list alone that Scripting is off. On 2026-10-03, Stable `0.2026.09.30.08.29.01` returned exit 0 and an empty list for the command above. This result indicates neither a Claude workspace trust problem nor successful Warp tab creation. Even with an endpoint, the required actions and TabConfigs/Launch Configuration must be available separately.

```sh
"$ab_binary" self-test claude \
  --workspace "$ab_workspace" --terminal warp \
  --timeout-secs 120 --isolated --json \
  > "$ab_report_dir/warp-claude.json" \
  2> "$ab_report_dir/warp-claude.stderr"
ab_warp_exit=$?
printf 'Warp exit=%s; report=%s\n' "$ab_warp_exit" "$ab_report_dir/warp-claude.json"
```

`--isolated` separates Bridge state, not provider logins or settings. Settings and consent from the ordinary Bridge state root do not apply, so the checks above use the default `tab-first` policy. Do not report them as verification of a separate `new-window` setting. The timeout is a per-command budget, not a limit on the whole run. On failure, read the JSON, stderr, and actual screen before repeating the same command.

Read the isolated run's records using `state_root` and `session` from the report.

```sh
AGENT_BRIDGE_NATIVE_STATE_DIR="<report.state_root>" "$ab_binary" inspect "<report.session>" --timeline --json
AGENT_BRIDGE_NATIVE_STATE_DIR="<report.state_root>" "$ab_binary" doctor "<report.session>" --probe --json
```

Record the following separately.

- CLI exit code 0 and `outcome: "passed"` in JSON; `ask`, `initial_result`, `tell`, `follow_up_result`, and `cleanup` all `passed`.
- The tested commit, binary SHA-256, provider version, terminal, workspace, session, request/event, and state root.
- The new tab/window appearing and disappearing, preservation of existing tabs/windows, and effects on focus and keyboard input that the user actually observed. Do not claim to have confirmed that the surface disappeared based only on `session_state: "closed"` and `cleanup: passed` in JSON.
- If an approval screen appeared, its exact type, requesting app and workspace, and the scope the user selected. Do not record passwords or tokens.

This candidate does not support terminal-based follow-up submission for Agy or Pi in Warp. First-run approval does not resolve that failure; do not extend a successful Claude round trip to all four providers.

## Reading Terminal.app close results

The `3648cc65` CLI self-test on 2026-10-03 passed Claude's initial response and official follow-up delivery, but cleanup failed with `window no longer holds its tab`. The window remained in the Terminal API's list with `tabs=0` and `visible=false`, then was observed with `visible=true`; the user confirmed that an empty window remained. **The intermediate fix that treated an empty tab list and invisibility alone as proof of closure, and its self-test PASS, were retracted.**

A second candidate that accepted that state only after an actual `close` was also not adopted as a product change at the time. After confirming once that the original window had disappeared, the user reported **the reappearance of the same window ID and title**. Responses from the official `close` and `close saving no` commands and `visible=false` alone could not prove permanent cleanup. The product's close and absence checks were restored to their previous strict behavior, with regression cases that do not turn hidden or visible empty windows into successful closes.

The cause of the reappearance was later reproduced in a 0.1.0 preparation candidate. The next launch's focus restoration code set a closed window still in the list to `frontmost`, displaying the empty window again. The new candidate restores focus only to visible windows. It also accepts invisible/zero-tab as `closed` only immediately after confirming an actual close in the same transaction, avoiding an unnecessary second close. Ordinary queries or later cleanup do not classify that state as `missing`. Keep the retraction of the earlier experiment's PASS separate from verification of the new candidate; follow the [0.1.0 verification record](verification/2026-10-03-v0.1.0-issues.md) (Korean) for the detailed source, native API, and provider runtime scope.

For an experimental session whose handle has already been consumed, `close-session` can return success without controlling that window again. Do not run another check; preserve the exact window ID and original request records. For a remaining window whose cleanup the native API cannot prove, the user must use that window's close button to confirm closure. Do not close other user sessions by quitting the entire app or terminating a guessed TTY.

If you need to preserve the exact window and tab IDs of a failed run, run each command from `ask --detach --json` through `result`, `tell`, and `close-session` instead of `self-test`, and preserve the `ask` response. The current self-test report has no surface ID, and closing a session consumes its terminal handle; do not rely on reconstructing it afterward. If screen observations differ from JSON, retract PASS and record what the person saw first.

## Checking keyboard input and focus during startup

A successful provider round trip and cleanup in `self-test` do not replace physical keyboard verification. With the screen unlocked, check separately with another app in the foreground and while typing in the same terminal's input area. The startup gate discards keys received while the new surface is briefly selected; it does not resend them to the original input area. Do not force restoration if the user selects another window, tab, or app. Record each terminal's behavior, the original failures, and results after the fixes separately in the [0.1.0 focus verification record](verification/2026-10-03-v0.1.0-focus.md) (Korean).
