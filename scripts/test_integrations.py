"""Behavioral checks for generated packages and hostile/stale release artifacts."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

import integrations as subject


class IntegrationsTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name) / "repo"
        self.root.mkdir()
        for name in ("Cargo.toml", "LICENSE"):
            shutil.copyfile(subject.ROOT / name, self.root / name)
        for name in ("shared", "manifests"):
            shutil.copytree(subject.ROOT / "integrations" / name, self.root / "integrations" / name)
        subject.generate(self.root)
        self.output = Path(self.scratch.name) / "dist"

    def test_checked_in_packages_are_current(self):
        subject.check(subject.ROOT)

    def test_shared_workflow_changes_reach_every_host_and_detect_stale_copy(self):
        source = self.root / "integrations/shared/skills/agent-bridge/SKILL.md"
        source.write_text(source.read_text(encoding="utf-8") + "\nAdditional task guidance.\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "generated content differs"):
            subject.check(self.root)
        subject.generate(self.root)
        for host in subject.HOSTS:
            target = self.root / f"integrations/{host}/skills/agent-bridge/SKILL.md"
            self.assertEqual(target.read_text(encoding="utf-8"), source.read_text(encoding="utf-8"))

    def test_cargo_version_drives_all_packages_and_archives(self):
        cargo = self.root / "Cargo.toml"
        old = subject.version(self.root)
        cargo.write_text(cargo.read_text(encoding="utf-8").replace(f'version = "{old}"', 'version = "9.8.7"', 1), encoding="utf-8")
        with self.assertRaises(ValueError):
            subject.check(self.root)
        subject.generate(self.root)
        subject.build(self.root, self.output)
        self.assertEqual(len(list(self.output.iterdir())), 8)
        for host in subject.HOSTS:
            meta = json.loads((self.root / f"integrations/{host}/integration.json").read_text())
            self.assertEqual(meta["version"], "9.8.7")
            self.assertTrue((self.output / f"agent-bridge-{host}-9.8.7.zip").is_file())

    def test_archives_are_reproducible_and_relocatable(self):
        subject.build(self.root, self.output)
        other = Path(self.scratch.name) / "other"
        subject.build(self.root, other)
        for path in self.output.iterdir():
            self.assertEqual(path.read_bytes(), (other / path.name).read_bytes())
        for host in subject.HOSTS:
            with zipfile.ZipFile(self.output / subject.archive_name(self.root, host)) as bundle:
                extracted = Path(self.scratch.name) / f"extract-{host}"
                # verify() has checked names, types and bytes before this extraction.
                bundle.extractall(extracted)
            base = extracted / f"agent-bridge-{host}-{subject.version(self.root)}"
            package = base / "plugin" if host in ("claude", "codex") else base
            self.assertTrue((package / subject.HOSTS[host]).is_file())
            self.assertEqual((package / "skills/agent-bridge/SKILL.md").read_bytes(),
                             (self.root / "integrations/shared/skills/agent-bridge/SKILL.md").read_bytes())
            if host in ("claude", "codex"):
                name, _ = subject.catalog(host, "./plugin")
                entry = json.loads((base / name).read_text(encoding="utf-8"))["plugins"][0]
                path = entry["source"] if host == "claude" else entry["source"]["path"]
                self.assertEqual((base / path).resolve(), package.resolve())

    def test_extra_file_and_unbundled_reference_fail(self):
        extra = self.root / "integrations/pi/secret.txt"
        extra.write_text("not a resource")
        with self.assertRaisesRegex(ValueError, "generated files"):
            subject.check(self.root)
        extra.unlink()
        source = self.root / "integrations/shared/skills/agent-bridge/SKILL.md"
        with source.open("a", encoding="utf-8") as writer:
            writer.write("\n[missing](references/absent.md)\n")
        with self.assertRaisesRegex(ValueError, "unbundled resource"):
            subject.generate(self.root)

    def test_manifest_escape_and_unsupported_components_fail(self):
        path = self.root / "integrations/manifests/codex.json"
        manifest = json.loads(path.read_text(encoding="utf-8"))
        for unsafe in ("../skills", "./../skills", "C:/skills", "./C:/skills", "./..\\skills"):
            with self.subTest(unsafe=unsafe):
                manifest["skills"] = unsafe
                path.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaises(ValueError):
                    subject.generate(self.root)
        manifest["skills"] = "./skills/"
        manifest["hooks"] = "./hooks.json"
        path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "unexpected codex manifest"):
            subject.generate(self.root)

    def test_existing_artifacts_are_not_overwritten(self):
        subject.build(self.root, self.output)
        with self.assertRaises(FileExistsError):
            subject.build(self.root, self.output)

    def test_checksums_have_portable_lf_and_reject_crlf(self):
        subject.build(self.root, self.output)
        checksum = next(self.output.glob("*.sha256"))
        content = checksum.read_bytes()
        self.assertTrue(content.endswith(b"\n"))
        self.assertNotIn(b"\r", content)
        checksum.write_bytes(content.replace(b"\n", b"\r\n"))
        with self.assertRaisesRegex(ValueError, "checksum"):
            subject.verify(self.root, self.output)

    def test_setup_cannot_drift_from_minimum_cli_version(self):
        setup = self.root / "integrations/shared/skills/agent-bridge/references/setup.md"
        setup.write_text(setup.read_text(encoding="utf-8").replace(subject.MIN_TABCLI, "0.0.1"), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "setup minimum"):
            subject.generate(self.root)

    def test_corrupt_checksum_wrong_content_duplicate_and_traversal_are_rejected(self):
        subject.build(self.root, self.output)
        archive = self.output / subject.archive_name(self.root, "codex")
        original = archive.read_bytes()
        checksum = self.output / f"{archive.name}.sha256"
        checksum.write_text("bad checksum")
        with self.assertRaisesRegex(ValueError, "checksum"):
            subject.verify(self.root, self.output)
        for extra in ("../../outside.txt", "unlisted.txt", "replace-content", "duplicate", "link"):
            archive.write_bytes(original)
            with zipfile.ZipFile(archive) as bundle:
                entries = [(info, bundle.read(info)) for info in bundle.infolist()]
            with zipfile.ZipFile(archive, "w") as bundle:
                for index, (info, content) in enumerate(entries):
                    if index == 0 and extra == "replace-content":
                        content = b"x" * len(content)
                    if index == 0 and extra == "link":
                        info.external_attr = 0o120777 << 16
                    bundle.writestr(info, content)
                if extra == "duplicate":
                    with self.assertWarns(UserWarning):
                        bundle.writestr(*entries[0])
                elif extra not in ("replace-content", "link"):
                    bundle.writestr(extra, b"untrusted")
            checksum.write_bytes(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n".encode("ascii"))
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                subject.verify(self.root, self.output)

    def test_release_verification_requires_exact_cli_and_package_asset_set(self):
        subject.build(self.root, self.output)
        with self.assertRaises(ValueError):
            subject.verify(self.root, self.output, allow_cli_assets=True)
        for platform in ("aarch64-apple-darwin.tar.gz", "x86_64-pc-windows-msvc.zip"):
            for suffix in ("", ".sha256"):
                (self.output / f"tabcli-{subject.version(self.root)}-{platform}{suffix}").write_bytes(b"checked by CLI verifier")
        subject.verify(self.root, self.output, allow_cli_assets=True)
        (self.output / "unrelated.zip").write_bytes(b"unexpected")
        with self.assertRaises(ValueError):
            subject.verify(self.root, self.output, allow_cli_assets=True)

    @unittest.skipUnless(sys.platform.startswith("linux"), "executes the Ubuntu release job's GNU Bash verifier")
    def test_actual_release_verifier_accepts_twelve_assets_and_rejects_changed_package(self):
        subject.build(self.root, self.root / "dist")
        scripts = self.root / "scripts"
        scripts.mkdir()
        shutil.copyfile(subject.ROOT / "scripts/integrations.py", scripts / "integrations.py")
        release = subject.version(self.root)
        dist = self.root / "dist"
        stage = self.root / "stage"
        stage.mkdir()
        for filename in ("tabcli", "tabcli.exe", "LICENSE", "THIRD_PARTY_NOTICES.md"):
            (stage / filename).write_text("fixture")
        macos = dist / f"tabcli-{release}-aarch64-apple-darwin.tar.gz"
        with tarfile.open(macos, "w:gz") as archive:
            for filename in ("tabcli", "LICENSE", "THIRD_PARTY_NOTICES.md"):
                archive.add(stage / filename, arcname=filename)
        windows = dist / f"tabcli-{release}-x86_64-pc-windows-msvc.zip"
        with zipfile.ZipFile(windows, "w") as archive:
            for filename in ("tabcli.exe", "LICENSE", "THIRD_PARTY_NOTICES.md"):
                archive.write(stage / filename, filename)
        for archive in (macos, windows):
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (dist / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")
        workflow = (subject.ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        step = workflow.split("      - name: Verify downloaded release assets\n", 1)[1].split("      - name:", 1)[0]
        script = "\n".join(line[10:] for line in step.split("        run: |\n", 1)[1].splitlines() if line.startswith("          "))
        environment = {**os.environ, "PACKAGE": "tabcli", "RELEASE_VERSION": release}
        completed = subprocess.run(["bash", "-c", script], cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        changed = dist / subject.archive_name(self.root, "agy")
        changed.write_bytes(b"bad archive")
        completed = subprocess.run(["bash", "-c", script], cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertNotEqual(completed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
