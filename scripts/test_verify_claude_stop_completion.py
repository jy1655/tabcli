import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
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

    @unittest.skipIf(os.name == "nt", "The macOS probe uses POSIX SIGINT semantics")
    def test_real_interrupt_preserves_flushed_output(self):
        harness = '''import importlib.util, json, os, pathlib, signal, sys, threading, time
source, output = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("probe", source)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
ready = output / "ready"
code = "import pathlib,sys,time;print('partial launch',flush=True);print('partial diagnostic',file=sys.stderr,flush=True);pathlib.Path(sys.argv[1]).touch();time.sleep(10)"
def interrupt():
    deadline = time.monotonic() + 3
    while not ready.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    if ready.exists():
        os.kill(os.getpid(), signal.SIGINT)
thread = threading.Thread(target=interrupt, daemon=True)
thread.start()
interrupted = False
try:
    probe.run_json([sys.executable, "-c", code, str(ready)], output, "launch", timeout=5)
except KeyboardInterrupt:
    interrupted = True
thread.join(timeout=1)
sys.exit(0 if interrupted and ready.exists() else 1)
'''
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            result = subprocess.run(
                [sys.executable, "-c", harness, probe.__file__, str(output)],
                capture_output=True, text=True, timeout=12,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((output / "launch.json").exists())
            self.assertTrue((output / "launch.stderr").exists())
            self.assertEqual((output / "launch.json").read_text(), "partial launch\n")
            self.assertEqual((output / "launch.stderr").read_text(), "partial diagnostic\n")

    def test_timeout_preserves_partial_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            code = "import sys,time;print('partial launch',flush=True);print('deadline detail',file=sys.stderr,flush=True);time.sleep(10)"
            with self.assertRaises(subprocess.TimeoutExpired):
                probe.run_json([sys.executable, "-c", code], output, "launch", timeout=1)
            self.assertEqual((output / "launch.json").read_text(), "partial launch\n")
            self.assertEqual((output / "launch.stderr").read_text(), "deadline detail\n")


if __name__ == "__main__":
    unittest.main()
