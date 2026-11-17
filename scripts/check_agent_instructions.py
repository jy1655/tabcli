#!/usr/bin/env python3
"""Check the project instruction budget and explicit boundary-file routing."""

import argparse
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
MAX_BYTES = 32_768
BOUNDARY_LINK = re.compile(r"\[[^\]\n]+\]\((docs/agents/[^)\s]+)\)")


def check(root):
    raw = (root / "AGENTS.md").read_bytes()
    text = raw.decode("utf-8")
    errors = []
    if len(raw) > MAX_BYTES:
        errors.append(f"AGENTS.md: {len(raw)} bytes exceeds {MAX_BYTES}-byte budget")
    if (root / "CLAUDE.md").read_text(encoding="utf-8").strip() != "@AGENTS.md":
        errors.append("CLAUDE.md: must import the same entry with @AGENTS.md")
    links = BOUNDARY_LINK.findall(text)
    if not links:
        errors.append("AGENTS.md: no explicit boundary links")
    for name in links:
        path = root / name
        if (path.parent != root / "docs/agents" or path.suffix != ".md"
                or path.is_symlink() or not path.is_file()):
            errors.append(f"AGENTS.md: invalid boundary file: {name}")
        elif not path.read_text(encoding="utf-8").strip():
            errors.append(f"AGENTS.md: empty boundary file: {name}")
    for path in sorted((root / "docs/agents").glob("*.md")):
        name = path.relative_to(root).as_posix()
        if name not in links:
            errors.append(f"{name}: missing read condition/link in AGENTS.md")
    return len(raw), errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        size, errors = check(args.root.resolve())
    except (OSError, UnicodeError) as error:
        print(f"Agent instructions: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"Agent instructions: OK ({size}/{MAX_BYTES} bytes; boundary links and Claude entry)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
