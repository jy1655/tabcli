## Summary

Describe what failed or was missing and what Bridge does after this change, in English or Korean.

Link the issue, or say there is none:

## Verification

For each item, state what ran and the result, or why it was not run. See
[Contributing](https://github.com/jy1655/agent-bridge/blob/main/CONTRIBUTING.md) and
[Testing](https://github.com/jy1655/agent-bridge/blob/main/docs/testing.md).

- [ ] `cargo test --all-targets --all-features -- --test-threads=1`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt -- --check`
- [ ] `git diff --check`
- [ ] Linux cross-target Clippy below
- [ ] Windows cross-target Clippy below

```sh
cargo clippy --all-targets --all-features --target x86_64-unknown-linux-gnu -- -D warnings
cargo clippy --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
```

Name the behavior tests you added or updated, or explain why none are needed:

Complete the applicable items below; mark the others "not applicable".

- For a lifecycle or concurrency fix, name the deterministic test that reproduced it before
  the fix.
- For a provider change, name its adapter tests and explain why other adapters need no changes.
- For a release workflow or packaging change, link the rehearsal and give its result.

Link the live-verification record and name the tested provider, terminal, platform and versions.
State which relevant combinations and failure paths remain unverified, or write "not run".
