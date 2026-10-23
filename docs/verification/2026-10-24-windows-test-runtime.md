# Windows serial test runtime, 2026-10-24

Issue #305 concerns the native Windows test runner, not provider delivery. The baseline
is PR #304 at `b417126bf5bf0d7c8375f6e08be55d6f43a384bb`, version 0.4.25.
The change replaces a `whoami.exe` subprocess for every private-permission operation
with `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY)` and `TokenUser`.
It introduces no cache. DACL application, lock permissions, file and directory flushes,
test fixtures, serial execution and test coverage remain in place.

## Baseline and attribution

The original [Windows CI job](https://github.com/jy1655/tabcli/actions/runs/39972324974/job/120477940989)
took 24m36s, including a 21m57s test step and a 1,209.89s main unit-test executable.
The [preceding CI run](https://github.com/jy1655/tabcli/actions/runs/39969434644)
had a 17m59s Windows test step. Those hosted runs establish substantial variability.

Local measurements used native x86-64 Windows, Rust 1.97.1, the same checkout and serial
test harness. A temporary `cfg(test)` module accumulated count and `Instant` elapsed
nanoseconds by named test thread, then printed one JSON summary on thread exit. Spans
surrounded the complete SID query, DACL application, `sync_file` and Windows
`sync_directory_entries`. Additional spans covered the Codex fixture constructor and the
whole byte-split and synthetic-origin tests, including temporary-directory destruction.
Those fixture/whole-test spans overlap the primitive spans and must not be added to them.
The instrumentation was removed before the final checks; no profiler or bypass ships.

The baseline command was `cargo test --all-targets --all-features -- --test-threads=1
--nocapture`; the after-change profile used the same main unit-test target via
`cargo test --bin tabcli --all-features -- --test-threads=1 --nocapture`. A Python
`subprocess.Popen` reader timestamped each output line with `time.perf_counter`, preserving
the per-test output and summaries. Intervals between completed test lines include harness
overhead; the explicitly instrumented whole-test spans below are direct measurements.
Subprocess fixtures' printed primitive counters are included in the aggregate.

| Main unit executable | Baseline | In-process SID |
| --- | ---: | ---: |
| libtest elapsed | 406.75 s | 136.60 s |
| Passed / ignored / failed | 1,117 / 1 / 0 | 1,120 / 1 / 0 |
| SID query calls | 33,909 | 33,915 |
| SID query elapsed, summed | 233.152 s | 0.158 s |
| DACL applications / elapsed | 33,909 / 3.864 s | 33,918 / 2.079 s |
| File syncs / elapsed | 34,888 / 47.400 s | 34,888 / 18.921 s |
| Directory syncs / elapsed | 22,126 / 4.451 s | 22,126 / 2.516 s |

The three additional tests verify the new identity lookup and actual ACL behavior; their
oracle still runs `whoami` once per identity test. Timed SID calls in the after profile
measure the new API, not these oracle subprocesses. The DACL tests also deliberately apply
broad permissions before tightening them again.

The main unit execution fell 66.4%. The directly measured SID-query saving was 232.995s;
other timings also varied, so the complete wall-time reduction is not attributed solely
to process creation. No antivirus attribution is made.

| Representative test | Baseline | In-process SID | Attribution |
| --- | ---: | ---: | --- |
| Codex terminal split at every byte | 47.973 s | 10.490 s | 292 unchanged fixtures; 4,672 SID calls, 32.263 s to 0.022 s |
| Close fault before every mutation | SID 10.506 s | SID 0.007 s | 1,619 unchanged SID calls; 1,702 file syncs and 1,315 directory syncs |
| Scan thousands of sessions | 8.766 s | 8.402 s | 3,276 unchanged session fixtures; zero SID calls |

The synthetic scan itself took 0.500s in the baseline; its fixture creation and cleanup
accounted for most of that test's time. This independent filesystem cost remains. No
fixture reduction was needed for the demonstrated improvement.

## Identity and security checks

The old child inherited the process primary token. Microsoft documents that
[`CreateProcessW`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw)
does not inherit an impersonating thread's token. Reading the current process's `TokenUser`
therefore preserves the selected identity; `TokenOwner`, which can be Administrators on an
elevated runner, is not used. The aligned token buffer outlives SID conversion, the token
handle is owned, and the allocated SID text is freed even on a conversion error.

Native Windows tests compare the SID with the former absolute-System32 `whoami` oracle,
read back file and directory DACLs, preserve the owner, widen then reapply permissions
twice, and check that an OS application failure is reported. The expected ACL is protected,
has one full-access ACE for the process user, and has inheritance flags only for directories.
Windows may retain its auto-inherited descriptor bit; identity aliases are normalized by
Windows's SDDL conversion. Neither changes the required ACE or protected flag.

Independent Claude review found no production identity, resource or DACL defect. Its test
review caught code-page decoding and built-in SID-alias portability issues, both fixed.
The first native readback test also exposed the retained auto-inherited bit and was corrected
to check the actual ACL contract rather than an incidental serialization difference.

## Final verification

After removing every profiling edit, the full serial all-target/all-feature suite passed:
1,304 tests passed, 5 ignored and none failed. The main unit executable took 136.14s; the
entire command, including 7.38s incremental compilation, took 177.99s. The baseline full
command took 455.59s, including 10.42s incremental compilation. Host Clippy, all-feature
Linux and Windows cross-target Clippy, rustfmt, diff checks and language lint passed.

Both post-change hosted runs passed all seven CI jobs, including native Windows, macOS,
Linux, MSRV, both release builds and integration packaging. They use the unchanged Windows
workflow and serial command; the PR and squash-merged main commits have identical source trees.

| Hosted Windows run | Source | Test step including compilation | Main unit executable |
| --- | --- | ---: | ---: |
| [Original #304 baseline](https://github.com/jy1655/tabcli/actions/runs/39972324974/job/120477940989) | `b417126` | 21m57s | 1,209.89 s |
| [Same-day main baseline](https://github.com/jy1655/tabcli/actions/runs/39980836467/job/120507378007) | `df6afc3` | 15m11s | 809.68 s |
| [#306 PR](https://github.com/jy1655/tabcli/actions/runs/39980993825/job/120507906489) | `176898f` | 12m11s | 631.57 s |
| [#306 main push](https://github.com/jy1655/tabcli/actions/runs/39983292196/job/120515871420) | `8aae856` | 10m38s | 541.41 s |

Test compilation in the same-day baseline and two after runs was 54.76s, 54.55s and 53.31s,
respectively. The after steps are 20% and 30% shorter than the same-day baseline, and 44%
and 52% shorter than the original reported baseline. The baseline variation is material;
these are hosted observations, not a guarantee of a fixed percentage. All 1,117 pre-existing
main unit tests remain, with three new native security tests. No test was disabled or
parallelized. The synthetic-origin fixture and its remaining filesystem cost are unchanged.

Authenticated provider behavior is outside this performance change; earlier PR #304 runtime
evidence continues to identify only its recorded candidate and conditions.
