# Terminal surfaces

Choose the terminal where Bridge opens your sessions, and check what it will close when you finish.
On macOS, Bridge detects the invoking terminal or uses Terminal.app; on native Windows, it tries a
Windows Terminal tab, then a separate console window. Use `--terminal` to override the selection.

Bridge creates a new surface for each session: a tab, window, pane, or console. It does not adopt
one of your existing tabs or attach to an independently running provider CLI. Linux is not supported
(#6), including a Linux Bridge process inside WSL. A provider's own WSL support does not provide
Bridge's missing surface implementation. See [CLI commands](cli.md) for a worked session.

## Selection and settings

`ask` and `self-test` accept `--terminal`; `reopen` currently supports native Windows Claude only.
The documented terminal names are `iterm2`, `terminal`, `ghostty`, `wezterm`, `warp`, and
`windows-console`. Parsing is case-insensitive. Accepted aliases are `iterm`, `iterm.app`,
`apple-terminal`, `apple_terminal`, `default`, `terminal.app`, `warpterminal`, `windows`, and
`console`. Structured output names Terminal.app `apple-terminal`.

On macOS an explicit option wins. Otherwise Bridge first examines `TERM_PROGRAM`, recognizing those
terminal families. An unrecognized value falls back to Terminal.app. Only when `TERM_PROGRAM` is
absent does it check `TERM=xterm-ghostty`, then the presence of `ITERM_SESSION_ID`, then
`TERM_SESSION_ID`; no match also falls back to Terminal.app. Detection selects an application, not
ownership of the invoking surface.

```sh
tabcli settings macos-open-mode new-window
tabcli ask codex --terminal wezterm --prompt "Explain this project."
```

Run `tabcli settings macos-open-mode tab-first` to restore the default.

The macOS default is `tab-first`: request a new tab in a safely usable existing local window, with a
new window when a safe target or creation API is unavailable. `new-window` always requests a new
window. Terminal.app always uses a new window because its native scripting interface has no new-tab
creation command. Once a creation has an uncertain outcome, Bridge does not create another surface.
Inspect the session and look for its surface before starting a replacement.

WezTerm discovers and verifies local GUI sockets, process identity, and a unique target window.
Inherited WezTerm pane/socket variables do not select the target. Its new-window route starts a
private GUI. Closing uses the saved creation scope, including whether Bridge owns that GUI; changing
the setting later cannot make Bridge close more of that GUI.

On native Windows, `--terminal` accepts only the `windows-console` adapter (including its `windows`
and `console` aliases). Bridge chooses its tab or console-window surface, whether you invoke it from
PowerShell or cmd. Bridge tries a Windows Terminal tab first, then a separate console window. It
uses a separate window named `agent-bridge` by default:

```powershell
tabcli settings windows-tab-window current
tabcli ask claude --workspace C:\path\to\project --prompt "Explain this project."
```

Run `tabcli settings windows-tab-window dedicated` to restore the default.

`windows-tab-window current` chooses the most recently used Windows Terminal window. The new tab
becomes selected inside that window; Windows Terminal has no supported command to select the earlier
tab again. `dedicated` is the default and is also used if the setting cannot be read during tab
selection. Both settings are stored under the selected Bridge state root. Neither affects the close
scope of existing sessions.

A Windows tab is unavailable when the tab executable is not found on an absolute `PATH` entry, there
is no desktop shell window, the command paths would be altered by Windows Terminal, or no host
offers a root within the bounded wait. Bridge withdraws that tab request before creating a console
window. A late host can leave an empty tab briefly, but only one surface starts the session. If the
tab host has already decided to start its root, Bridge waits for that launch; delayed confirmation
does not create a second surface.

Bridge uses its terminal adapter to create the surface, check ownership, deliver terminal input
where needed, and close it. The Windows rows below describe native Windows; the five application
adapters above them are macOS-only.

| Platform | Terminal | Creation | Fallback input | Close |
| --- | --- | --- | --- | --- |
| macOS | iTerm2 | Tab or window | Session scripting | Session |
| macOS | Terminal.app | Window | Window/TTY scripting | Window |
| macOS | Ghostty | Tab or window | Terminal scripting | Owned surface; proven failed-launch handle retained |
| macOS | WezTerm | Tab/private GUI | Exact-pane CLI | Pane; scoped GUI |
| macOS | Warp | Tab/window URI | Unsupported | Bound tab/window |
| Windows | Terminal tab | Tab | Console input | Console and tab |
| Windows | Console window | Window fallback | Console input | Console |

Codex follow-ups use its thread-addressed queue; Claude uses official cross-session messaging.
Neither falls back to unaddressed terminal input. Agy and Pi need the terminal fallback, which rules
out Warp for their follow-ups.

For iTerm2, scripting writes the prompt to the exact session. Terminal.app targets its recorded
window and TTY. Ghostty uses `input text` and submit; WezTerm's official CLI addresses the pane.
Windows emits console input records. Close preserves unrelated sessions; a private WezTerm GUI can
end only when no sibling panes remain. Warp requires exact proof within its bound instance.

## Requirements and first-run prompts

Provider installation, authentication, and workspace trust are separate from terminal support. Run
`tabcli doctor --provider PROVIDER --probe` to check local prerequisites. Run `tabcli
self-test PROVIDER` when you want to open a surface and make real model calls. See
[providers](providers.md) and
[CLI reference](cli.md); a passing executable/version check is not a delivery guarantee.

On macOS, install the selected application. The iTerm2, Terminal.app, and Ghostty scripting routes
can require macOS Automation consent. If the first launch times out in Automation, check for a
visible system prompt and follow [macOS permissions](macos-permissions.md).
Review and answer prompts yourself; Bridge does not grant system access. After handling the
permission prompt, inspect the failed session and close any surface it created before starting
another. WezTerm uses its official CLI and a verified local GUI connection.

Bridge expects Ghostty at `/Applications/Ghostty.app` with the required native scripting commands.
If you see `Ghostty native scripting dictionary is unavailable` or `Ghostty native scripting lacks
…`, install a compatible application there or select another terminal. Bridge reads
`/Applications/Ghostty.app/Contents/Resources/Ghostty.sdef` and requires `new surface
configuration`, `new tab`, `new window`, `select tab`, `input text`, and `close tab`.

Warp requires its Scripting opt-in and a reachable, authorized official Warp Control endpoint. The
tab route also requires enabled TabConfigs; new-window mode uses the separate Launch Configuration
URI. Installed application/version checks alone are insufficient: required control actions and the
bound instance must be available. Bridge uses ownership titles to prove the created surface. Warp's
Control API cannot submit terminal input, so Agy/Pi follow-ups are unsupported there. Codex/Claude
native input paths do not remove the terminal's creation and ownership checks. If the Control
endpoint is unavailable, enable Scripting in Warp and check its endpoint, or select another
terminal. Warp remains limited (#63); authenticated operation is not verified in [M11]/[M12].

On native Windows, install PowerShell 7 with `pwsh.exe` discoverable on an absolute `PATH` entry.
Windows Terminal's `wt.exe` must likewise be discoverable for tabs; it is optional for the console
window fallback. If `pwsh.exe` is not found, correct its `PATH` entry before launching again. Both
launch forms run the same PowerShell root and provider configuration. If initial input waits on
workspace trust, look at the managed surface and decide whether to trust that directory. Bridge does
not treat an empty screen as proof of trust, blindly press Enter, or resend input whose receipt is
uncertain.

Agy's terminal fallback needs provider-owned readiness and a receipt (#43, #48). The native Windows
initial paste and macOS follow-ups require both exact workspace trust in Agy's store and this
session's log evidence of workspace customization loading. Readiness requires startup and a quiet
period, plus redraw on Windows; there is no additional reload window. If input is withheld because
Bridge cannot verify this session’s trust, check the surface. If it shows the composer instead of a
trust dialog, or input remains withheld after approval, close the session and start a new one. Do
not resend after uncertain delivery. A missing receipt leaves delivery uncertain. Inspect the
request and wait for its result or close the session; do not paste again. Codex's addressed queue
avoids relying on the currently displayed agent thread, but manual agent-picker changes and
additional TUI input have separate verification limits (#57, #65). Consult
[provider documentation](providers.md) before mixing manual input with Bridge requests.

## Ownership and close

`close-session` targets the surface Bridge created for that session, not whichever tab you have
selected. It preserves your other tabs. If Bridge cannot prove that it still controls the recorded
surface, it refuses the close instead of choosing another one.

The owner is the Bridge process that launched the session. Before signalling it, Bridge checks that
the recorded owner is still the same process. On macOS it compares the PID, process birth,
controlling TTY, and process groups; Windows uses process identity. This check is called owner
attestation. The surface handle also names its Bridge session. Reusing a PID or TTY does not
transfer ownership to a new process.

Terminal.app needs one more check: Bridge records which run of the application created the window,
using the application's PID and birth. That run is the app incarnation. A window id and TTY are
meaningful only within it, so a restarted or second Terminal process cannot receive a close meant
for the earlier one. Bridge checks the recorded surface and attested process groups before stopping
them.

Those checks determine whether Bridge can act with the live owner, act on the recorded surface
alone, or leave it alone because it is proven absent. That decision is close authority. A newly
selected tab or a window that appeared during launch provides no such authority. If none of the
three outcomes is proven, Bridge refuses and keeps the handle. The
[architecture reference](architecture.md) explains the records behind this decision.

If Ghostty initialization fails after Bridge proves the created terminal, tab, and window ids,
Bridge attempts the existing scoped cleanup once. If cleanup cannot confirm closure, the adapter
returns those ids as data. When the launch still owns its claim and is still `launching`, the
launcher saves them in `terminal.json` and records `failed`; explicit close can close that exact
surface later. A handle-less close during pending creation is refused until the launch deadline.
If close completes after that deadline but before the failed-launch handoff, no handle is added
to the closed session. Its error and launch log name the residual surface, which may remain and
is not closed by Bridge. Repeating close preserves the warning and does not close that surface.
Unproven creation evidence grants no close authority. This preserves failure evidence after an
automation timeout; it does not establish what caused the timeout.

If Warp proves the created tab but cannot save its control binding, it publishes Abort and
attempts exact cleanup once. Confirmed cleanup leaves no handle. If cleanup cannot confirm
absence, the launcher retains the proven instance, tab, and window ids in `terminal.json` and
records `failed`, with the surface and residual warning in `status.error`. The same pending-close
refusal and late-handoff rules apply. This handle does not restore the missing control binding:
Warp requires its recorded app incarnation and control endpoint, and ownerless startup close
still proves absence only. Explicit close cannot recover this tab; check the named surface in
Warp. Persisting replacement control evidence and granting recovery close authority remain
unimplemented (#87). Warp is not live-verified (#63), and this record-write failure has not
been observed live.

When a close fails, read the error and run `tabcli doctor SESSION`. Check the managed surface
before retrying `tabcli close-session SESSION --explicit`. Bridge restores the handle after a
terminal close error so the retry addresses the same surface. If close was interrupted, the saved
closing handle, close intent where applicable, and tombstone show what remains to do. Repeating a
completed close does not close anything else. Recorded results remain available until you explicitly
prune them.

A dead owner does not prove that its surface closed (#69). Inspect the session and use `tabcli
close-session SESSION --explicit` to close a retained surface. Repair first checks whether a
completed result needs publication. On macOS, Terminal.app, Ghostty, WezTerm, and Warp keep handles
for surfaces that can outlive their owner; they also keep a pending close intent. Those records do
not grant permission to signal another process. Windows repair can close the managed console whose
owner exited, using the same close checks. Missing or unreadable evidence can stop repair. Run
`sessions` to attempt repair. Use `inspect`, timeline, `result`, `search`, or `doctor` when you want
to read the records without changing them.

Terminal.app sometimes lists a window after closing it: the object has no tabs and is not visible
(#64). Do not infer a running session from that list alone. Immediately after a successful close,
Bridge accepts that hidden, empty object as closed. A later query or retry cannot use the same
observation as proof on its own. When restoring focus, Bridge selects only a previously visible
window, so it does not bring the closed empty object back. See the
[cleanup evidence](verification/2026-10-02-terminal-cleanup.md) and
[follow-up verification](verification/2026-10-03-v0.1.0-issues.md) (Korean).

On Windows, close ends the processes it finds in the managed console. It ends a remaining attested
tab host last, with a successful exit, so Windows Terminal does not retain a failed tab. A process
started during close can escape that enumeration; this limit applies to both surface forms. Bridge
ends an unattached root directly only when its records show that the wrapper never ran and its
process identity still matches. The records must have no owner and no recorded spawn attempt;
unreadable launch evidence does not qualify. Failure to attach alone is insufficient.

## Focus and keyboard behaviour

A new surface can briefly select a tab or bring an application forward (#58). Bridge returns the
keyboard to where you were working only while it can distinguish its own selection from yours. If
you select something else during launch, it leaves your selection alone under the checks below.

- iTerm2: after a new tab, restore the previously selected tab only while the created session
  remains selected. After a new window, wait for iTerm2's own activation observation, then restore
  the earlier window and application while the expected selection remains. A user's intervening
  selection is preserved.
- Terminal.app restores the earlier visible window only while the launch’s expected selection
  still holds. It does not select a hidden, closed window object.
- Ghostty: wait for the new surface to be ready, then conditionally restore the earlier tab and
  return the foreground to the recorded application. It checks the expected selection and
  application identity; missing evidence prevents restoration. A cold start has no earlier tab.
- WezTerm: conditionally reactivate the earlier pane only when the GUI reports the newly created
  pane as focused. A background GUI's stale focus record is not sufficient. Failure to restore
  selection does not fail the launch.
- Warp: the implemented Control/URI route does not establish a general foreground-restoration
  guarantee. Keyboard preservation for an authenticated round trip is not verified here.

The [macOS focus record](verification/2026-10-03-v0.1.0-focus.md) (Korean) includes actual typing
checks and earlier failures, followed by corrections and final checks. A model result alone does
not verify keyboard preservation. The later [M12] round trips do not repeat every physical-input
test.

Foreground restoration covers surface startup and a further 600 ms settling period. It is best
effort; failure to identify or restore the host window does not fail the launch. It does not
continue throughout the provider’s startup or model turn. During that period, Windows tracks the
foreground independently while launch waits. It obtains the surface's host window from inside its
console and gives the foreground back only from that identified window. A different window selected
during launch becomes the restoration target. If the surface comes back more than 200 ms after
restoration, Bridge treats it as a user selection and leaves it selected; earlier activation is
treated as launch-related. This cannot perfectly distinguish very fast user selection from
application activation. The tab host discards input accumulated before it starts the root, so an
early Enter cannot answer the provider's first dialog. A separate console window has no such host
input buffer protection. See [W01] for sampled timings and limits. `windows-tab-window current`
still leaves the new tab selected inside its window.

## Verification evidence

The tables distinguish implemented routes, automated tests, and authenticated live runs. Fixture and
script tests do not make model calls or check physical keyboard behaviour. Follow the linked record
for the tested binary, CLI versions, settings, and date.

| Platform | Terminal | Code | Automated tests | Live evidence |
| --- | --- | --- | --- | --- |
| macOS | iTerm2 | Implemented | Creation/input/ownership/close | Codex [M12] |
| macOS | Terminal.app | Implemented | App identity/retry/hidden window | Codex [M12], [M11] |
| macOS | Ghostty | Implemented | Readiness/input/close/focus | Codex [M12], [M11] |
| macOS | WezTerm | Implemented | GUI identity/input/close | Codex/Claude [M12]; Agy [M11] |
| macOS | Warp | Limited | Control/URI/partial launch/close | Not verified |
| Windows | Terminal tab | Implemented | Host/console/identity/input/focus | Four providers [W02] |
| Windows | Console window | Fallback | Launch/identity/input/close/focus | Initial/close [W01] |

Codex and Claude also pass on WezTerm in [M11]. Other provider/terminal combinations are not
verified by these records. [M12] uses explicit bypass options; [M11] uses no model, effort, or
bypass override. Pi on WezTerm reached an authentication failure in [M11]; cleanup passed, but that
was not a successful round trip. Warp had no reachable endpoint in [M11] and was not run in [M12].

The four providers in [W02] are Codex, Claude, Agy, and Pi. [W01] also records tab surface/focus
checks. It does not supply a four-provider console-window follow-up matrix. [M12] did not run native
Windows; its cross-target checks are compile/lint evidence, not a new runtime pass.

[W02]'s daemon-absent Codex refusal describes the tested build, not the current prerequisite.
Codex now selects the queue backend itself. The
[queue record](verification/2026-10-04-codex-queue.md) distinguishes mock-model protocol probes
from authenticated model runs.

[M12]: verification/2026-10-06-macos-0.1.2.md
[M11]: verification/2026-10-04-macos-0.1.1.md
[W02]: verification/2026-10-02-windows.md
[W01]: verification/2026-10-01-windows.md
