#!/usr/bin/env python3
"""Generate and verify self-contained host packages. Requires Python 3.11+, no dependencies."""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
HOSTS = {
    "claude": ".claude-plugin/plugin.json",
    "codex": ".codex-plugin/plugin.json",
    "agy": "plugin.json",
    "pi": "package.json",
}
MIN_TABCLI = "0.4.22"


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def version(root):
    with (root / "Cargo.toml").open("rb") as source:
        value = tomllib.load(source)["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", value):
        raise ValueError(f"unsupported release version: {value!r}")
    return value


def regular_files(directory):
    """Do not follow resource links into another package or the developer's machine."""
    if directory.is_symlink() or getattr(directory, "is_junction", lambda: False)():
        raise ValueError(f"linked directory is not a package resource: {directory}")
    if not directory.is_dir():
        raise ValueError(f"missing directory: {directory}")
    files = {}
    for path in sorted(directory.iterdir()):
        if (path.is_symlink() or getattr(path, "is_junction", lambda: False)()
                or not path.resolve().is_relative_to(directory.resolve())):
            raise ValueError(f"linked resource is not allowed: {path}")
        if path.is_dir():
            files.update({f"{path.name}/{key}": value for key, value in regular_files(path).items()})
        elif path.is_file():
            files[path.name] = path
        else:
            raise ValueError(f"non-file package resource: {path}")
    return files


def local_path(value):
    if not isinstance(value, str) or not value.startswith("./") or "\\" in value:
        raise ValueError(f"package paths must start with './': {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or ":" in value:
        raise ValueError(f"path escapes package: {value!r}")
    return path.as_posix()


def catalog(host, source):
    local_path(source)
    if host == "claude":
        return ".claude-plugin/marketplace.json", json_bytes({
            "name": "agent-bridge", "owner": {"name": "jy1655"},
            "description": "코딩 에이전트 위임과 작업용 세션 관리를 위한 에이전트 브리지",
            "plugins": [{"name": "agent-bridge", "source": source}],
        })
    if host == "codex":
        return ".agents/plugins/marketplace.json", json_bytes({
            "name": "agent-bridge", "interface": {"displayName": "에이전트 브리지"},
            "plugins": [{
                "name": "agent-bridge", "source": {"source": "local", "path": source},
                "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
                "category": "Developer Tools",
            }],
        })
    raise ValueError(f"no marketplace for {host}")


def validate_manifest(host, manifest, release):
    expected_name = "tabcli-agent-bridge" if host == "pi" else "agent-bridge"
    if manifest.get("name") != expected_name or not manifest.get("description"):
        raise ValueError(f"invalid {host} identity")
    if host != "agy" and manifest.get("version") != release:
        raise ValueError(f"{host} version does not match Cargo.toml")
    if host == "agy":
        # Agy's documented strict schema has no version or component path fields.
        if set(manifest) != {"name", "description"}:
            raise ValueError("Agy manifest supports only name and description")
    elif host == "pi":
        if manifest.get("pi") != {"skills": ["./skills"]}:
            raise ValueError("Pi package must expose only its bundled skills")
        if manifest.get("files") != ["skills", "integration.json", "LICENSE"]:
            raise ValueError("Pi package files must stay self-contained")
        if "scripts" in manifest or "dependencies" in manifest:
            raise ValueError("this instruction-only Pi package has no scripts or dependencies")
    else:
        if local_path(manifest.get("skills")) != "skills":
            raise ValueError(f"{host} skills must resolve inside the package")
        if set(manifest) - {"name", "version", "description", "author", "homepage",
                            "repository", "license", "skills", "interface"}:
            raise ValueError(f"unexpected {host} manifest component")


def packages(root):
    release = version(root)
    if (root / "LICENSE").is_symlink():
        raise ValueError("license must be a regular file inside the source root")
    source = root / "integrations/shared/skills"
    shared = {f"skills/{name}": path.read_text(encoding="utf-8").encode("utf-8")
              for name, path in regular_files(source).items()}
    skill = shared["skills/agent-bridge/SKILL.md"].decode("utf-8")
    if not skill.startswith("---\nname: agent-bridge\ndescription: ") or f"**{MIN_TABCLI} or newer**" not in skill:
        raise ValueError("shared skill identity or minimum CLI requirement differs")
    if "@VERSION@" in skill:
        raise ValueError("unexpanded skill version placeholder")
    # Relative Markdown links must resolve to a bundled file, not the source checkout.
    for name, content in shared.items():
        if not name.endswith(".md"):
            continue
        for link in re.findall(r"\]\(([^)]+)\)", content.decode("utf-8")):
            if link.startswith(("https://", "#")):
                continue
            target = PurePosixPath(name).parent / local_path("./" + link)
            if target.as_posix() not in shared:
                raise ValueError(f"unbundled resource in {name}: {link}")
    result = {}
    for host, manifest_path in HOSTS.items():
        template = root / f"integrations/manifests/{host}.json"
        if template.is_symlink():
            raise ValueError(f"linked manifest: {template}")
        manifest = json.loads(template.read_text(encoding="utf-8").replace("@VERSION@", release))
        validate_manifest(host, manifest, release)
        result[host] = {
            **shared,
            manifest_path: json_bytes(manifest),
            "LICENSE": (root / "LICENSE").read_text(encoding="utf-8").encode("utf-8"),
            "integration.json": json_bytes({
                "schema": 1, "host": host, "version": release,
                "tabcli_min_version": MIN_TABCLI,
                "platforms": ["macos", "windows"],
            }),
        }
    return result


def generated_files(root):
    files = {}
    for host, contents in packages(root).items():
        files.update({f"integrations/{host}/{name}": value for name, value in contents.items()})
    for host in ("claude", "codex"):
        name, content = catalog(host, f"./integrations/{host}")
        files[name] = content
    return files


def generate(root):
    for name, content in generated_files(root).items():
        destination = root / name
        for parent in (destination, *destination.parents):
            if parent == root:
                break
            if parent.is_symlink() or getattr(parent, "is_junction", lambda: False)():
                raise ValueError(f"refusing to write through linked path: {parent}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(content)
    check(root)


def check(root):
    expected = generated_files(root)
    actual_names = set()
    for host in HOSTS:
        actual_names.update(f"integrations/{host}/{name}"
                            for name in regular_files(root / f"integrations/{host}"))
    for host in ("claude", "codex"):
        name, _ = catalog(host, f"./integrations/{host}")
        actual_names.add(name)
    if actual_names != set(expected):
        raise ValueError(f"unexpected/missing generated files: {sorted(actual_names ^ set(expected))}")
    for name, content in expected.items():
        path = root / name
        if path.is_symlink() or not path.is_file() or path.read_text(encoding="utf-8").encode("utf-8") != content:
            raise ValueError(f"generated content differs: {name}; run python scripts/integrations.py generate")


def archive_contents(root, host, contents):
    prefix = f"agent-bridge-{host}-{version(root)}"
    if host in ("claude", "codex"):
        name, content = catalog(host, "./plugin")
        return {f"{prefix}/{name}": content,
                **{f"{prefix}/plugin/{name}": data for name, data in contents.items()}}
    return {f"{prefix}/{name}": data for name, data in contents.items()}


def archive_name(root, host):
    return f"agent-bridge-{host}-{version(root)}.zip"


def build(root, output):
    check(root)
    # A fresh destination avoids overwriting a downloaded or published artifact.
    output.mkdir(parents=True, exist_ok=False)
    for host, contents in packages(root).items():
        archive = output / archive_name(root, host)
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
            for name, content in sorted(archive_contents(root, host, contents).items()):
                info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                bundle.writestr(info, content)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (output / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    verify(root, output)


def verify(root, output, allow_cli_assets=False):
    expected_names = {archive_name(root, host) + suffix for host in HOSTS for suffix in ("", ".sha256")}
    actual_names = set(regular_files(output))
    if allow_cli_assets:
        # The release's existing verifier owns the two CLI archives and checksums.
        cli = {f"tabcli-{version(root)}-{platform}{suffix}"
               for platform in ("aarch64-apple-darwin.tar.gz", "x86_64-pc-windows-msvc.zip")
               for suffix in ("", ".sha256")}
        expected_names |= cli
    if actual_names != expected_names:
        raise ValueError(f"unexpected/missing release assets: {sorted(actual_names ^ expected_names)}")
    for host, contents in packages(root).items():
        archive = output / archive_name(root, host)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        if (output / f"{archive.name}.sha256").read_text(encoding="ascii") != f"{digest}  {archive.name}\n":
            raise ValueError(f"checksum differs: {archive.name}")
        expected = archive_contents(root, host, contents)
        with zipfile.ZipFile(archive) as bundle:
            names = bundle.namelist()
            if len(names) != len(set(names)) or set(names) != set(expected):
                raise ValueError(f"archive members differ: {archive.name}")
            for info in bundle.infolist():
                if info.file_size != len(expected[info.filename]) or info.external_attr >> 16 != 0o100644:
                    raise ValueError(f"invalid archive member: {info.filename}")
                if bundle.read(info) != expected[info.filename]:
                    raise ValueError(f"archive content differs: {info.filename}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("generate", "check", "build", "verify"))
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--allow-cli-assets", action="store_true")
    args = parser.parse_args()
    try:
        root = args.root.resolve()
        if args.action in ("build", "verify") and args.output is None:
            parser.error("build/verify require --output")
        if args.action == "generate":
            generate(root)
        elif args.action == "check":
            check(root)
        elif args.action == "build":
            build(root, args.output.resolve())
        else:
            verify(root, args.output.resolve(), args.allow_cli_assets)
        print(f"Integrations {args.action}: OK ({version(root)}; {', '.join(HOSTS)})")
    except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f"Integrations {args.action}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
