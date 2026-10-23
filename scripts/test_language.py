"""Exercise language lint through real Git file discovery and its command exit status."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import check_language as subject


class LanguageTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
        self.policy = {"schema": 1, "allowed_paths": [], "allowed_lines": []}
        self.save_policy()

    def save_policy(self):
        self.write(subject.POLICY, json.dumps(self.policy, ensure_ascii=True))

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def run_lint(self):
        return subprocess.run([sys.executable, str(Path(subject.__file__).resolve()),
                               "--root", str(self.root)], capture_output=True, text=True)

    def allow_path(self, name):
        self.policy["allowed_paths"].append({"path": name, "reason": "Test translation."})
        self.save_policy()

    def test_new_untracked_plugin_metadata_fails_with_a_location(self):
        for text in ("\ud55c\uae00", "\u4e2d\u6587", "\u041f\u0440\u0438\u0432\u0435\u0442"):
            with self.subTest(text=text):
                self.write("integrations/plugin.json", '{\n  "description": "' + text + '"\n}\n')
                result = self.run_lint()
                self.assertEqual(result.returncode, 1)
                self.assertIn("integrations/plugin.json:2:", result.stderr)

    def test_escaped_json_and_yaml_display_text_is_inspected(self):
        for name in ("plugin.json", "agents/openai.yaml"):
            with self.subTest(name=name):
                path = self.write(name, '"displayName": "\\ud55c\\uae00"\n')
                self.assertIn(name + ":1:", self.run_lint().stderr)
                path.unlink()

    def test_ascii_prose_punctuation_and_emoji_pass(self):
        self.write("README.md", "English text \u2014 next \u2192 done \u2713 \U0001f642\ufe0f\n")
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_accented_letters_and_decomposed_accents_fail(self):
        for text in ("r\u00e9sum\u00e9", "re\u0301sume\u0301"):
            with self.subTest(text=text):
                self.write("README.md", text)
                self.assertEqual(self.run_lint().returncode, 1)

    def test_translation_exception_does_not_allow_other_files(self):
        self.allow_path("README.ko.md")
        self.write("README.ko.md", "\ud55c\uae00")
        self.assertEqual(self.run_lint().returncode, 0)
        self.write("README.md", "\ud55c\uae00")
        self.assertIn("README.md:1:", self.run_lint().stderr)

    def test_exact_line_exception_does_not_allow_a_changed_line(self):
        self.allow_path(subject.POLICY)
        self.policy["allowed_lines"].append({"path": "source.rs", "text": 'let text = "\ud55c";',
                                              "reason": "Unicode input fixture."})
        self.save_policy()
        self.write("source.rs", '    let text = "\ud55c";\n')
        self.assertEqual(self.run_lint().returncode, 0)
        self.write("source.rs", 'let text = "\ud55c\uae00";\n')
        result = self.run_lint()
        self.assertEqual(result.returncode, 1)
        self.assertIn("source.rs:1:", result.stderr)
        self.assertIn("unused allowed_lines", result.stderr)

    def test_unused_path_exception_fails(self):
        self.allow_path("missing.md")
        self.assertIn("unused allowed_paths", self.run_lint().stderr)

    def test_git_ignored_files_are_not_inspected(self):
        self.write(".gitignore", "local/\n")
        self.write("local/note.md", "\ud55c\uae00")
        self.assertEqual(self.run_lint().returncode, 0)

    def test_modified_tracked_files_and_deleted_files(self):
        path = self.write("README.md", "English")
        subprocess.run(["git", "add", "README.md"], cwd=self.root, check=True, capture_output=True)
        path.write_text("\ud55c\uae00", encoding="utf-8")
        self.assertEqual(self.run_lint().returncode, 1)
        path.unlink()
        self.assertEqual(self.run_lint().returncode, 0)

    def test_non_utf8_or_nul_cannot_hide_text_metadata(self):
        for content in (b'"name": "\xff"', '"name": "\ud55c"'.encode("utf-16"), b"English\0"):
            with self.subTest(content=content):
                self.write("plugin.json", "").write_bytes(content)
                self.assertEqual(self.run_lint().returncode, 1)

    def test_binary_file_is_not_treated_as_prose(self):
        self.write("image.png", "").write_bytes(b"\x89PNG\r\n\x1a\n\0\xff")
        self.assertEqual(self.run_lint().returncode, 0)

    def test_invalid_policy_fails_instead_of_disabling_checks(self):
        for policy in ({}, {"schema": 2, "allowed_paths": [], "allowed_lines": []},
                       {"schema": 1, "allowed_paths": [{"path": "../outside", "reason": "x"}],
                        "allowed_lines": []},
                       {"schema": 1, "allowed_paths": [{"path": "README.md", "reason": ""}],
                        "allowed_lines": []}):
            with self.subTest(policy=policy):
                self.policy = policy
                self.save_policy()
                self.assertEqual(self.run_lint().returncode, 1)

    def test_repository_policy_passes(self):
        _, errors = subject.check(subject.ROOT)
        self.assertEqual(errors, [])

    def test_real_policy_rejects_localized_public_surfaces(self):
        self.policy = subject.read_policy(subject.ROOT)
        self.save_policy()
        names = ["README.md", ".agents/plugins/marketplace.json", ".claude-plugin/marketplace.json",
                 "integrations/manifests/codex.json", "integrations/agy/plugin.json",
                 "integrations/pi/package.json", "integrations/claude/.claude-plugin/plugin.json",
                 "integrations/codex/.codex-plugin/plugin.json",
                 "integrations/shared/skills/agent-bridge/agents/openai.yaml"]
        for name in names:
            self.write(name, '"description": "\ud55c\uae00"\n')
        _, errors = subject.check(self.root)
        for name in names:
            self.assertTrue(any(error.startswith(name + ":1:") for error in errors), name)


if __name__ == "__main__":
    unittest.main()
