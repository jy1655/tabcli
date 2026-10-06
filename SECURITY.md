# Security policy

Report a Bridge vulnerability through
[private vulnerability reporting](https://github.com/jy1655/tabcli/security/advisories/new).
**Do not open a public issue for a vulnerability.** Security fixes target the latest release
only. Reports and fixes are best effort, with no promised response time.

## What to send

Include the Bridge version, operating system, provider CLI and terminal versions, and the
steps that reproduce the problem. Explain the impact, such as exposed session records, input
delivered to the wrong session, or an action taken without consent.
Include the relevant permission options and workspace-trust choices so we can reproduce the
same conditions.

Do not post tokens, credentials, session directories, transcripts, complete `doctor --json`
reports, or provider configuration publicly. These contain paths, identifiers, prompts, or
other private content. Send a minimal reproduction instead, with paths and identifiers
redacted. Review attachments even when submitting them privately.

## What belongs in a Bridge security report

- Bridge delivers a prompt to, or closes, a terminal surface it did not create.
- Bridge delivers input to an unanswered workspace-trust dialog and accepts it for you.
- Bridge creates session records that another account can read or change without authorization.
- Bridge applies `--yolo` or overrides your model choice without a request to do so.

Report Claude follow-ups sent to the wrong session or reported as delivered without verification
of the executed input. See [Security and data](docs/security-and-data.md) for the delivery checks.

The documented behavior of `--yolo` is not a vulnerability: when you explicitly request it,
Codex, Claude, and Agy receive their native bypass flags. Pi receives project-trust approval
while its native tool policy remains in effect. Report flaws in provider CLIs or terminal
applications themselves to their maintainers.

Session records are local plain text, not encrypted storage. Bridge runs no hosted service
of its own, but provider CLIs send model requests to their providers. Keep session records
private even when Bridge has closed the session.
