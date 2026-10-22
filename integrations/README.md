# Maintaining host integrations

See [installation and updates](../docs/integrations.md) for user setup. These packages are
instructions around the existing CLI. They do not add provider transports, hooks, an MCP
server, credentials, an engine installer, or a background updater.

Edit `shared/skills/agent-bridge/` for the workflow and `manifests/` for host metadata.
`scripts/integrations.py` reads the release version from `Cargo.toml`, copies the shared
resources and license into each host package, and writes the repository marketplace catalogs.
The generated `claude/`, `codex/`, `agy/`, and `pi/` directories are checked in so a repository
installation does not need Python or a build step. Do not edit generated copies.

From the checkout root, using Python 3.11 or newer:

```sh
python scripts/integrations.py generate
python scripts/integrations.py check
python -m unittest discover -s scripts -p 'test_*.py' -v
python scripts/integrations.py build --output /new/empty-parent/integration-dist
python scripts/integrations.py verify --output /new/empty-parent/integration-dist
```

`build` requires a destination that does not exist. It produces four platform-independent
ZIP files and their SHA-256 files. Claude and Codex ZIPs contain a marketplace root and a
`plugin/` child; Agy and Pi ZIPs contain the native package directly. All archive entries are
regular files under one versioned root, including the license and skill references. The
verifier compares the complete member list, sizes, file types, bytes, and checksum sidecars
against the shared source; it never extracts unverified archives. Text is normalized to LF
so a Windows checkout produces the same artifacts as a Unix checkout.

`integration.json` records the host, release version, supported platforms, and minimum CLI
version. Agy's documented strict manifest schema has no version property, so its version
lives in this separate record and the artifact name. The other three native manifests also
carry the release version. Codex uses the supported `.codex-plugin/plugin.json` compatibility
layout, verified with Codex 0.163.0; changing to the portable root manifest requires another
host loading check, not just schema validation.

For a CLI release bump, regenerate before committing. The minimum compatible engine version
is a separate decision (`MIN_TABCLI` in the generator and the shared skill); keep it at the
oldest verified version that supports the workflow, not automatically the package version.
CI checks all generated content, manifests, resource paths, and version agreement and exercises
the packager on Linux, macOS, and Windows. This does not add Linux engine support.

The release workflow builds the integrations from the same validated commit as the CLI,
verifies the downloaded bytes before publication, and includes all eight integration assets
in the immutable release. Rehearsing an older tag preserves that tag's original four-asset
release format; a separate rehearsal-only job builds/tests the candidate integration source
without adding those candidate files to the old tag's release assets. See
[release procedure](../docs/releasing.md).

Installation and authenticated host/provider round trips remain manual checks. Record the
host, target, platform, versions, source/package identity, invocation, permission/trust settings,
request addresses and confirmed cleanup in `docs/verification/`. Parser checks or direct
`tabcli self-test` runs alone do not prove a host discovered and used its installed skill.
