#!/usr/bin/env python3
"""Reject non-ASCII letters outside the repository's explicit language exceptions."""

import argparse
import fnmatch
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import unicodedata

ROOT = Path(__file__).resolve().parents[1]
POLICY = ".language-lint.json"
JSON_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')


def non_english_letters(text):
    # Punctuation, arrows and emoji do not imply a language. This is a character
    # check, not a natural-language classifier for words using only ASCII letters.
    return sorted({c for c in text if not c.isascii()
                   and (unicodedata.category(c).startswith("L")
                        or unicodedata.category(c) in {"Mn", "Mc"})
                   and not (0xFE00 <= ord(c) <= 0xFE0F or 0xE0100 <= ord(c) <= 0xE01EF)})


def relative_path(value):
    if (not isinstance(value, str) or not value or "\\" in value or ":" in value
            or PurePosixPath(value).is_absolute()
            or any(part in ("", ".", "..") for part in value.split("/"))):
        raise ValueError(f"invalid repository-relative path: {value!r}")
    return value


def read_policy(root):
    policy = json.loads((root / POLICY).read_text(encoding="utf-8"))
    if (not isinstance(policy, dict)
            or set(policy) != {"schema", "allowed_paths", "allowed_lines"}
            or policy["schema"] != 1):
        raise ValueError("unsupported language policy schema")
    for key, fields in (("allowed_paths", {"path", "reason"}),
                        ("allowed_lines", {"path", "text", "reason"})):
        if not isinstance(policy[key], list):
            raise ValueError(f"{key} must be an array")
        seen = set()
        for entry in policy[key]:
            if not isinstance(entry, dict) or set(entry) != fields:
                raise ValueError(f"invalid {key} entry")
            relative_path(entry["path"])
            if not isinstance(entry["reason"], str) or not entry["reason"].strip():
                raise ValueError("every exception needs a reason")
            if key == "allowed_lines":
                if (not isinstance(entry["text"], str) or "\n" in entry["text"]
                        or "\r" in entry["text"] or not non_english_letters(entry["text"])):
                    raise ValueError("line exceptions must name one line with non-ASCII letters")
                if any(c in entry["path"] for c in "*?["):
                    raise ValueError("line exceptions require an exact file path")
            identity = (entry["path"], entry.get("text"))
            if identity in seen:
                raise ValueError(f"duplicate exception: {entry['path']}")
            seen.add(identity)
    return policy


def repository_files(root):
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    return sorted({relative_path(name) for name in result.stdout.decode("utf-8").split("\0") if name})


def check(root):
    policy = read_policy(root)
    used_paths = set()
    used_lines = set()
    errors = []
    checked = 0
    for name in repository_files(root):
        path = root / name
        if path.is_symlink():
            errors.append(f"{name}: symlinks are not inspected; use a regular repository file")
            continue
        if not path.exists():  # A tracked deletion in the worktree.
            continue
        matched = [i for i, entry in enumerate(policy["allowed_paths"])
                   if fnmatch.fnmatchcase(name, entry["path"])]
        if matched:
            used_paths.update(matched)
            continue
        raw = path.read_bytes()
        if b"\0" in raw:
            # Known text formats must not bypass inspection through NUL/UTF-16.
            if path.suffix.lower() in {".md", ".rs", ".py", ".json", ".jsonl", ".yaml",
                                       ".yml", ".toml", ".txt", ".sh", ".ps1", ".js", ".ts"}:
                errors.append(f"{name}: text files must be UTF-8 without NUL bytes")
            continue
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            errors.append(f"{name}: cannot inspect non-UTF-8 content")
            continue
        checked += 1
        lines = text.splitlines()
        findings = {number: set(non_english_letters(line))
                    for number, line in enumerate(lines, 1)}
        if path.suffix.lower() in {".json", ".jsonl", ".yaml", ".yml"}:
            # JSON and JSON-compatible YAML quoted strings can spell visible text
            # with Unicode escapes. Inspect the decoded text as well as the source.
            for token in JSON_STRING.finditer(text):
                try:
                    decoded = json.loads(token.group())
                except ValueError:
                    continue
                number = text.count("\n", 0, token.start()) + 1
                findings.setdefault(number, set()).update(non_english_letters(decoded))
        for number, letters in findings.items():
            if not letters:
                continue
            line = lines[number - 1]
            allowed = [i for i, entry in enumerate(policy["allowed_lines"])
                       if name == entry["path"] and line.strip() == entry["text"]]
            if allowed:
                used_lines.update(allowed)
                continue
            chars = ", ".join(f"{c!r} (U+{ord(c):04X})" for c in sorted(letters)[:8])
            errors.append(f"{name}:{number}: non-English letters: {chars}")
    for key, used in (("allowed_paths", used_paths), ("allowed_lines", used_lines)):
        for i, entry in enumerate(policy[key]):
            if i not in used:
                errors.append(f"{POLICY}: unused {key} exception for {entry['path']}")
    return checked, errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root (default: this checkout)")
    args = parser.parse_args()
    try:
        checked, errors = check(args.root.resolve())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Language lint: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        print(f"Language lint: FAIL ({len(errors)} findings)", file=sys.stderr)
        return 1
    print(f"Language lint: OK ({checked} text files checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
