#!/usr/bin/env python3
"""Assemble release metadata and checksums for the desktop or macOS assets."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import re
import subprocess
import tempfile
from pathlib import Path


def file_hash(path):
    sha = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            sha.update(block)
    return sha.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("repository", help="GitHub OWNER/REPO")
    parser.add_argument("assets", type=Path)
    parser.add_argument("secretaire", type=Path, nargs="?", help="packaged Linux server binary")
    parser.add_argument("--macos", action="store_true", help="generate supplemental macOS metadata")
    parser.add_argument("--include-intel", action="store_true", help="require macOS Intel packages as well")
    parser.add_argument("--with-app", action="store_true", help="require macOS app bundle packages")
    args = parser.parse_args()
    if args.macos == (args.secretaire is not None) or ((args.include_intel or args.with_app) and not args.macos):
        parser.error("provide either the Linux server binary or --macos")
    if args.macos and not args.with_app and "-" not in args.tag:
        parser.error("legacy macOS metadata requires a prerelease tag")
    root = Path(__file__).resolve().parents[2]
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", args.tag):
        parser.error("expected a v-prefixed SemVer tag")
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+", args.repository):
        parser.error("expected a GitHub OWNER/REPO")

    constants = {}
    for source_path in sorted((root / "crates/fuzukue-proto/src").glob("*.rs")):
        source = source_path.read_text(encoding="utf-8")
        for name, value in re.findall(r"^pub const (\w+): u32 = (\w+);$", source, re.M):
            if name in constants and constants[name] != value:
                parser.error(f"conflicting protocol constant {name} in {source_path}")
            constants[name] = value

    def protocol(name):
        seen = set()
        while not name.isdecimal():
            if name in seen or name not in constants:
                parser.error(f"could not resolve protocol constant {name}")
            seen.add(name)
            name = constants[name]
        return int(name)

    assets = args.assets.resolve()
    base = f"https://github.com/{args.repository}/releases/download/{args.tag}"
    spec = {
        "channel": "beta" if "-" in args.tag else "stable",
        "version": args.tag[1:],
        "protocol": protocol("PROTOCOL_VERSION"),
        # The floors describe the existing desktop distribution capability.
        "min_client_protocol": protocol("DISTRIBUTION_PROTOCOL"),
        "min_server_protocol": protocol("DISTRIBUTION_PROTOCOL"),
        "artifacts": [],
    }
    packages = [
        ("server", "secretaire", "linux-x86_64", "tar.gz"),
        ("linux-client", "fuzukue", "linux-x86_64", "tar.gz"),
        ("windows-client", "fuzukue", "windows-x86_64", "zip"),
    ]
    if args.macos:
        platforms = ["macos-aarch64"]
        if args.include_intel:
            platforms.append("macos-x86_64")
        packages = [
            (kind, binary, platform, "tar.gz")
            for platform in platforms
            for kind, binary in (("server", "secretaire"), ("macos-client", "fuzukue"))
        ]
        if args.with_app:
            packages.extend(("macos-app", "fuzukue-app", platform, "tar.gz") for platform in platforms)
    for kind, binary, platform, extension in packages:
        name = f"{binary}-{args.tag}-{platform}.{extension}"
        spec["artifacts"].append({
            "kind": kind, "platform": platform, "path": name, "url": f"{base}/{name}",
        })

    if args.macos:
        # The Rust release CLI has no macos-client kind. Keep this manifest separate
        # so that existing desktop releases remain readable by installed servers.
        commit, timestamp = subprocess.check_output(
            ["git", "-C", str(root), "show", "-s", "--format=%H%n%cI", "HEAD"],
            text=True,
        ).splitlines()
        spec.update(schema_version=1, commit=commit, commit_date=timestamp,
                    created_at=datetime.now(timezone.utc).isoformat(), next_public_keys=[])
        for item in spec["artifacts"]:
            path = assets / item.pop("path")
            if not path.is_file() or path.stat().st_size == 0:
                parser.error(f"missing or empty package: {path}")
            item.update(name=path.name, size=path.stat().st_size, sha256=file_hash(path),
                        changed_since_previous=True)
        manifest = (json.dumps(spec, indent=2) + "\n").encode("utf-8")
        names = ("macos-release-manifest.json", "SHA256SUMS-macos")
    else:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=assets) as spec_file:
            json.dump(spec, spec_file)
            spec_file.flush()
            manifest = subprocess.check_output(
                [str(args.secretaire.resolve()), "release", "manifest-from-build", spec_file.name,
                 "--repo", str(root)],
            )
        names = ("release-manifest.json", "SHA256SUMS")
    manifest_name, sums_name = names
    rows = [(item["sha256"], item["name"]) for item in json.loads(manifest)["artifacts"]]
    rows.append((hashlib.sha256(manifest).hexdigest(), manifest_name))
    (assets / manifest_name).write_bytes(manifest)
    (assets / sums_name).write_text(
        "".join(f"{sha}  {name}\n" for sha, name in sorted(rows, key=lambda row: row[1])),
        encoding="ascii",
    )


if __name__ == "__main__":
    main()
