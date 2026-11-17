"""Guard the actual checkout in CI and exercise instruction lint failure cases."""

from pathlib import Path
import tempfile
import unittest

import check_agent_instructions as subject


class AgentInstructionsTest(unittest.TestCase):
    def test_repository_instructions(self):
        _, errors = subject.check(subject.ROOT)
        self.assertEqual(errors, [])

    def test_budget_counts_utf8_bytes_and_accepts_exact_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            path = root / "AGENTS.md"
            routing = path.read_text(encoding="utf-8")
            prefix = routing + "\u2192"
            content = prefix + "x" * (subject.MAX_BYTES - len(prefix.encode("utf-8")))
            path.write_bytes(content.encode("utf-8"))
            self.assertEqual(subject.check(root), (subject.MAX_BYTES, []))
            path.write_bytes((content + "x").encode("utf-8"))
            self.assertIn("32769 bytes exceeds", "\n".join(subject.check(root)[1]))

    def test_missing_and_unrouted_boundary_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            (root / "docs/agents/example.md").rename(root / "docs/agents/renamed.md")
            errors = "\n".join(subject.check(root)[1])
            self.assertIn("invalid boundary file: docs/agents/example.md", errors)
            self.assertIn("docs/agents/renamed.md: missing read condition/link", errors)

    def test_divergent_claude_entry_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            (root / "CLAUDE.md").write_text("Different rules\n", encoding="utf-8")
            self.assertIn("CLAUDE.md: must import", "\n".join(subject.check(root)[1]))

    @staticmethod
    def fixture(directory):
        root = Path(directory)
        (root / "docs/agents").mkdir(parents=True)
        (root / "docs/agents/example.md").write_text("# Rules\n", encoding="utf-8")
        (root / "AGENTS.md").write_text(
            "Read [Example](docs/agents/example.md) for example changes.\n", encoding="utf-8")
        (root / "CLAUDE.md").write_text("@AGENTS.md\n", encoding="utf-8")
        return root
