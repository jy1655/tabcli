# Current macOS Shell Startup Boundary

- iTerm2 and WezTerm run Bridge's host as the surface program (#376). The host runs the user's
  zsh login and interactive startup files once without a terminal (`/bin/zsh -l -i`, `setsid`,
  stdin `/dev/null`, environment on fd 3, output discarded), bounded by ten seconds and the
  launch receipt, then `execve`s `/bin/zsh -f -i -c <launch command>` with that environment.
  Before #376 the surface ran `/bin/zsh -l -i -c` under its TTY, and a startup file waiting for
  terminal input (Oh My Zsh's update prompt) held every launch until the 30-second deadline,
  with nothing recorded (four times on one Mac, 2026-10-06 to 2026-11-16).
- A capture that needs a terminal is not supported: exports made only under `[[ -t 0 ]]` or
  after `tty` are not carried. Do not give the capture a terminal or a controlling TTY to
  restore them: zsh's `read -k` opens `/dev/tty` even with stdin redirected.
- Processes left in the capture's process group after a successful capture are stopped,
  counted and recorded; the launch continues (owner's decision, 2026-11-16). Never store, log
  or show startup-file output or environment values, including in doctor.
- The launch-owned `launch-host.json` admits one host per launch receipt under the lifecycle
  lock and only moves forward. Launch wait and repair settle a recorded host failure through
  the same locked decision; a confirmed spawn or ended status wins. Missing records are not
  evidence that a host never started. Sessions without the record keep the exact legacy
  deadline text.
- Terminal.app and Warp type the launch into the terminal's own shell, and Ghostty starts a
  clean shell with the launcher's `PATH`; this boundary does not change them.

