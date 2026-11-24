#!/usr/bin/env python3
"""Assemble schema 1 from the three archives using secretaire's release CLI."""

import argparse
import hashlib
import json
import re
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("repository", help="GitHub OWNER/REPO")
    parser.add_argument("assets", type=Path)
    parser.add_argument("secretaire", type=Path, help="packaged Linux server binary")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", args.tag):
        parser.error("expected a v-prefixed SemVer tag")
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+", args.repository):
        parser.error("expected a GitHub OWNER/REPO")

    source = (root / "crates/fuzukue-proto/src/lib.rs").read_text(encoding="utf-8")
    constants = dict(re.findall(r"^pub const (\w+): u32 = (\w+);$", source, re.M))

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
    for kind, binary, platform, extension in (
        ("server", "secretaire", "linux-x86_64", "tar.gz"),
        ("linux-client", "fuzukue", "linux-x86_64", "tar.gz"),
        ("windows-client", "fuzukue", "windows-x86_64", "zip"),
    ):
        name = f"{binary}-{args.tag}-{platform}.{extension}"
        spec["artifacts"].append({
            "kind": kind, "platform": platform, "path": name, "url": f"{base}/{name}",
        })

    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=assets) as spec_file:
        json.dump(spec, spec_file)
        spec_file.flush()
        manifest = subprocess.check_output(
            [str(args.secretaire.resolve()), "release", "manifest-from-build", spec_file.name,
             "--repo", str(root)],
        )
    rows = [(item["sha256"], item["name"]) for item in json.loads(manifest)["artifacts"]]
    rows.append((hashlib.sha256(manifest).hexdigest(), "release-manifest.json"))
    (assets / "release-manifest.json").write_bytes(manifest)
    (assets / "SHA256SUMS").write_text(
        "".join(f"{sha}  {name}\n" for sha, name in sorted(rows, key=lambda row: row[1])),
        encoding="ascii",
    )


if __name__ == "__main__":
    main()
