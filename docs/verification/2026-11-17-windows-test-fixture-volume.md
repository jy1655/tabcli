# Windows test fixture volume, 2026-11-17

The `rust (windows-latest)` CI job took 11–17 minutes while macOS took 6–8 and Linux 3.
Almost all of it was the serial test step (693 s and 948 s in runs 44037020612 and
44034716584, same source tree), not compilation (51–73 s). Per test, Windows took a median
4.5 times the macOS interval and 30 times the Linux interval, across hundreds of short
filesystem-heavy tests rather than a few outliers. The runner image disables Defender
real-time protection and excludes `C:\` and `D:\`; the measurement below confirmed both at
runtime (`RealTimeProtectionEnabled: False`).

## Measurement

A temporary branch (`measure/windows-test-io`, run
[44047423177](https://github.com/jy1655/tabcli/actions/runs/44047423177), never merged)
counted and timed file flushes, directory opens, directory flushes and DACL applications
in the unit-test executable, attributed by libtest thread. Each of two `windows-latest`
jobs built the tests once and ran the unchanged serial suite four times, in opposite orders:

- `baseline`: default `TEMP` (`C:\Users\RUNNER~1\AppData\Local\Temp`, the OS disk);
- `temp-workspace-drive`: `TEMP` and `TMP` under `RUNNER_TEMP` (`D:\a\_temp`, the runner's
  temporary-storage volume);
- `skip-flush`: default `TEMP`, real flushes replaced by success in the test build only;
- both changes together.

Both volumes were NTFS. Job 1 ran on an AMD EPYC 9V45 with an NVMe virtual disk, job 2 on
an Intel Xeon Platinum 8573C with SAS virtual disks.

| Arm | Job 1 main unit / all suites (s) | Job 2 main unit / all suites (s) | File flush time, 47,306 calls (s) |
| --- | ---: | ---: | ---: |
| baseline | 501.8 / 541.1 | 391.7 / 436.6 | 330.5 / 202.7 |
| temp-workspace-drive | 92.0 / 126.3 | 125.6 / 167.0 | 6.5 / 13.8 |
| skip-flush | 148.7 / — | 212.8 / 257.6 | 0 / 0 |
| both | 83.6 / 118.1 | 114.5 / 155.7 | 0 / 0 |

A file flush cost 4.3–7.0 ms on the OS disk and 0.14–0.29 ms on `D:`. Directory flushes
(29,504 calls) took 8–11 s on the OS disk and under 1.5 s on `D:`; DACL applications
(46,116 calls) took 2–6 s. Skipping flushes on the OS disk still left the suite slower
than real flushes on `D:`: other writes to the OS disk are slow too. On `D:`, skipping
flushes saved only another 8–11 s, so the change keeps every flush.

Every arm had two failures from the measurement itself: its import in
`terminal/windows/security.rs` broke `tests/windows_security_policy.rs`, which guards that
module's dependencies. Job 1's `skip-flush` arm also failed
`the_self_test_runner_classifies_its_own_deadlines_from_typed_evidence` once (a 50 ms
spawn deadline expired before the child started), and Cargo stopped before the
integration binaries, so that arm has no all-suites total. Counts were identical in all
other arms (1,661 passed, 5 ignored). Two jobs are a small sample; hosted load still varies.

## Change

`scripts/ci_test_timing.py` points the tests' `TEMP` and `TMP` at `CI_TEST_FIXTURE_TEMP`
when that directory's volume reports NTFS, records the decision in `timings.json` and the
summary, and does not pass the variable to the tests. CI and Release set it to
`RUNNER_TEMP/test-fixtures`. Tests, permissions, flushes, retries and the serial harness are
unchanged. GitHub formats `D:` as ReFS from 2026-11-30
([actions/runner-images#15315](https://github.com/actions/runner-images/issues/15315)); the
recorder then keeps the default `TEMP` and writes a warning, so Windows tests stay on NTFS
until ReFS is evaluated. Earlier tags' recorders ignore the variable.
