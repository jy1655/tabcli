# Release notes

Choose your installed version below to check its changes and recorded test coverage. Dates are
from the local annotated tags. Curated notes start at 0.0.3; earlier tags have no notes here.
Older notes retain their own requirements and limitations; use the
[providers](../providers.md) and [terminals](../terminals.md) pages for current behavior.
Each note separates live checks from automated tests.

- [0.3.0](0.3.0.md) (2026-10-11): Adds `status`, `wait`, and `hold`; renders `inspect` and doctor
  from one session observation; the doctor report states its scope.
- [0.2.6](0.2.6.md) (2026-10-07): Uses the provider Report path for completion tests, separates
  Request observation from JSON, and concentrates Reopen and Agy Result evidence rules.
- [0.2.5](0.2.5.md) (2026-10-07): Repairs dead owners despite completion recovery damage;
  removes unused delivery fallback and consolidates status writes and helper containment.
- [0.2.4](0.2.4.md) (2026-10-07): Consolidates launch waits and delivery settlement, preserves
  delivery failure reasons and residual-surface evidence, and localizes timeout diagnostics.
- [0.2.3](0.2.3.md) (2026-10-07): Keeps the record of a Warp launch that fails after its tab
  exists; reports in `doctor` and in a timed-out self-test whether Pi has credentials for a
  session's provider; explains the Codex line about the shared background server.
- [0.2.2](0.2.2.md) (2026-10-07): Keeps the surface of a Ghostty launch that fails after its tab
  exists so that a later close can close it; names a pending Agy tool approval in a timed-out
  self-test; says that a launch may need the screen unlocked.
- [0.2.1](0.2.1.md) (2026-10-07): Fixes a rare failure when closing a Warp session; the macOS
  permissions page and the release procedure are now in English.
- [0.2.0](0.2.0.md) (2026-10-06): Renames the command and package to `tabcli`; your sessions and
  settings are read as they are. Update scripts that call `agent-bridge`.
- [0.1.2](0.1.2.md) (2026-10-06): Reorganizes session records and terminal ownership;
  no migration or behavior change.
- [0.1.1](0.1.1.md) (2026-10-04): Codex follow-ups work without a shared daemon.
- [0.1.0](0.1.0.md) (2026-10-03): Adds macOS terminal selection and opening modes; restores Ghostty.
- [0.0.10](0.0.10.md) (2026-10-02): Fixes Codex startup and result correlation; adds request
  timelines, observed elapsed time, and self-test.
- [0.0.9](0.0.9.md) (2026-10-02): Shares workspace consent and opens Windows Terminal tabs.
- [0.0.8](0.0.8.md) (2026-09-28): Adds launch diagnostics and a Pi startup gate; native Windows
  launches are broken. Upgrade to 0.0.9 or later.
- [0.0.7](0.0.7.md) (2026-09-24): Adds result search, prompt context, and Windows Claude reopen.
- [0.0.6](0.0.6.md) (2026-09-23): Adds session inspection, results by request, and diagnostics.
- [0.0.5](0.0.5.md) (2026-09-21): Keeps Claude follow-up text out of the messenger model.
- [0.0.4](0.0.4.md) (2026-09-04): Addresses Codex follow-ups through its native queue.
- [0.0.3](0.0.3.md) (2026-08-28): Ships a native Windows archive with a SHA-256 checksum.

Published releases are immutable. Follow [the release procedure](../releasing.md) to publish a
release or verify its archive checksums and attestations.
