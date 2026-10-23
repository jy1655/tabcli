import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "probe", Path(__file__).with_name("verify_claude_stop_completion.py")
)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeCleanupTests(unittest.TestCase):
    def check_launch_failure(self, failure):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary = root / "binary"
            binary.touch()
            workspace = root / "workspace"
            workspace.mkdir()
            unrelated = root / "unrelated" / "session-foreign"
            unrelated.mkdir(parents=True)
            calls = []

            def prepare(output, _claude):
                state = output / "state"
                state.mkdir()
                return {"AGENT_BRIDGE_NATIVE_STATE_DIR": str(state)}

            def run(command, output, name, environment=None, timeout=110):
                calls.append(command)
                self.assertIsNotNone(environment)
                state = Path(environment["AGENT_BRIDGE_NATIVE_STATE_DIR"])
                if command[1] == "ask":
                    owned = state / "session-owned"
                    owned.mkdir()
                    if os.name != "nt":
                        (state / "session-symlink").symlink_to(unrelated, target_is_directory=True)
                    probe.write_json(owned / "terminal.json", {
                        "terminal": "iterm2", "session_id": "owned-surface",
                    })
                    raise failure
                self.assertEqual(command[2], "session-owned")
                if command[1] == "close-session":
                    return {"ok": True, "closed": True}, 0
                return {"ok": True}, 0

            argv = ["probe", "--bridge", str(binary), "--workspace", str(workspace),
                    "--output", str(root / "probe")]
            with (patch.object(probe.sys, "argv", argv),
                  patch.object(probe.sys, "platform", "darwin"),
                  patch.object(probe.shutil, "which", return_value=str(binary)),
                  patch.object(probe, "prepare_probe", side_effect=prepare),
                  patch.object(probe, "run_json", side_effect=run),
                  patch.object(probe, "capture_surface") as capture,
                  contextlib.redirect_stderr(io.StringIO())):
                self.assertEqual(probe.main(), 1)
            self.assertEqual([call[1] for call in calls], ["ask", "inspect", "close-session"])
            capture.assert_called_once_with(root / "probe", "owned-surface")
            self.assertTrue(unrelated.is_dir())

    def test_launch_timeout_recovers_only_owned_session(self):
        self.check_launch_failure(subprocess.TimeoutExpired("ask", 1))

    def test_invalid_launch_json_recovers_only_owned_session(self):
        self.check_launch_failure(json.JSONDecodeError("invalid output", "", 0))

    def test_interrupt_recovers_only_owned_session(self):
        self.check_launch_failure(KeyboardInterrupt())

    def test_timeout_preserves_partial_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            failure = subprocess.TimeoutExpired(
                "ask", 1, output=b"partial launch", stderr=b"deadline detail",
            )
            with patch.object(probe.subprocess, "run", side_effect=failure):
                with self.assertRaises(subprocess.TimeoutExpired):
                    probe.run_json(["bridge", "ask"], output, "launch")
            self.assertEqual((output / "launch.json").read_text(), "partial launch")
            self.assertEqual((output / "launch.stderr").read_text(), "deadline detail")


if __name__ == "__main__":
    unittest.main()
