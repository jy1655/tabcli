# Contributing

Send bug reports, fixes, or documentation corrections in English or Korean. Bridge's job is to
launch, observe, continue, and close first-party coding-agent CLI sessions in visible terminal
surfaces. Changes should help those operations without taking over what the provider CLI does.
Review and support are best effort, with no promised response time.

## Changes we accept

Keep provider fixes in the affected adapter and add a test that reproduces the bug. For terminal
integrations and documentation corrections, show the missing or incorrect behavior.
Keep unrelated formatting, dependency updates, and provider changes out of the patch.

We will decline a common paste helper that makes all four providers share one input path.
Their official input mechanisms differ, and we need to remove a fallback for one provider
without disturbing the others. Use the provider's official mechanism when it is available;
check the installed version, platform, backend, and configuration before selecting it.

If an official capability is missing, keep the fallback inside that provider's adapter and
preserve the behavior the official capability would provide. Explain which upstream change
will let us delete it. Keep provider-specific launch options, prompt delivery, and result
matching there too; shared code should track sessions rather than choose provider behavior.

Proposals to replace provider-owned capabilities or attach to arbitrary existing CLI sessions
are outside the current scope: Bridge controls sessions it launches. Read [AGENTS.md](AGENTS.md)
for the full rules and use the names in [CONTEXT.md](CONTEXT.md) when describing a change.

## Development setup

You need Rust 1.97.1 or newer, Cargo, Clippy, and rustfmt. [Cargo.toml](Cargo.toml) declares
1.97.1 as the minimum supported Rust version; CI also checks stable Rust.

Use macOS or native Windows to develop and exercise managed sessions. Linux can run the
platform-independent checks; Linux managed sessions are not supported.
For live work, install and authenticate the provider CLI and install the terminal application
being tested. On macOS, install Ghostty and iTerm2 for the AppleScript compile test; see
[Testing](docs/testing.md). Native Windows sessions use PowerShell 7.
Use the checkout command in [Testing](docs/testing.md#manual-live-verification) to test your
changes rather than the installed executable on PATH.

## Required checks

Run these checks from the repository root before you open a pull request:

```sh
cargo test --all-targets --all-features -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
git diff --check
```

Run the test harness with one test thread, as required by AGENTS.md. This policy was adopted
after deadline-sensitive fake-Codex tests failed under the parallel harness in
[issue #46](https://github.com/jy1655/agent-bridge/issues/46); concurrency inside individual
tests remains enabled.

Also check platform-gated code for Linux and Windows:

```sh
cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings
cargo clippy --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
```

Install the Rust standard library component (`rust-std`) for each target first. These Clippy
checks need that component, not a target linker or a runnable target environment. They check
code; they do not execute Windows or Linux sessions.

Tests requiring authenticated CLIs and visible terminal surfaces are manual live tests.
An ignored live test is never evidence that a runtime path passed. Follow
[Testing](docs/testing.md) and report the combinations actually exercised.

## Pull requests

Describe the problem, the resulting behavior, and the scope of the change. Link the issue when
there is one. Include a concrete before/after example when it helps explain the fix.

- Add or update tests with behavior changes.
- For lifecycle or concurrency changes, reproduce the race or partial transition in a
  deterministic test before patching it.
- For provider changes, add or update tests in that provider's adapter and explain why unrelated
  provider adapters need no modification.
- State each check that ran, its result, and any check not run with the reason.
- Report live verification separately, with provider, terminal, platform, versions, and a link
  to the run record. Mark combinations you did not exercise "not verified".

For release workflow or packaging changes, follow [Releasing](docs/releasing.md) (Korean).
The release rehearsal must finish and its result must be read before relying on a tag push;
see the exact requirements in [AGENTS.md](AGENTS.md).

## Reporting safely

Use the issue forms for ordinary bugs and feature requests. Remove prompts from commands and
redact paths and identifiers from diagnostic excerpts. Do not upload session directories,
transcripts, tokens, or complete diagnostic reports to a public issue.

For vulnerabilities, follow [SECURITY.md](SECURITY.md) and use private reporting.
See [Security and data](docs/security-and-data.md) for the boundaries and local records.

By contributing, you agree that your contributions are provided under the repository's
[MIT license](LICENSE).
