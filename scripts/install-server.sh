#!/bin/sh
set -eu
exec python3 - "$@" <<'PY'
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

OFFICIAL_REPO = "momota1029/fuzukue"
NUMBER = r"(?:0|[1-9][0-9]*)"
IDENTIFIER = rf"(?:{NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
TAG = re.compile(rf"v({NUMBER})\.({NUMBER})\.({NUMBER})(?:-({IDENTIFIER}(?:\.{IDENTIFIER})*))?")
HASH = re.compile(r"[a-f0-9]{64}")
COMMIT = re.compile(r"[a-f0-9]{40}|[a-f0-9]{64}")
LOCAL_VERSION = re.compile(rf"local-(?:{COMMIT.pattern}|original)-{HASH.pattern}")
MARKER = ".fuzukue-package"
MANIFEST = "release-manifest.json"
PLATFORM = "linux-x86_64"
UNIT = "fuzukue-release.service"
UNIT_HEADER = "# Managed by fuzukue release installer\n"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def semver(tag):
    major, minor, patch, prerelease = TAG.fullmatch(tag).groups()
    identifiers = tuple(
        (0, int(part)) if part.isdigit() else (1, part)
        for part in (prerelease or "").split(".") if part
    )
    return (int(major), int(minor), int(patch), prerelease is None, identifiers)


def xdg(name, default):
    path = Path(os.environ.get(name, ""))
    return path if path.is_absolute() else Path.home() / default


def check_file(path, header):
    if os.path.lexists(path):
        require(not path.is_symlink() and path.is_file(), f"Unexpected file type: {path}")
        require(path.read_text(encoding="utf-8").startswith(header), f"Unmanaged file: {path}")


def write_file(path, text, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=path.parent, prefix=".fuzukue-") as directory:
        temp = Path(directory) / "file"
        temp.write_text(text, encoding="utf-8")
        temp.chmod(mode)
        temp.replace(path)


def fetch(url, output):
    require(url.startswith("https://"), f"HTTPS is required: {url}")
    subprocess.run([
        "curl", "--disable", "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "10",
        "--connect-timeout", "15", "--max-time", "600", "--output", str(output), url,
    ], check=True)


def read_json(path):
    return json.loads(path.read_bytes())


def choose_release(api, temp, channel, pin, current, names):
    page_file = temp / "api.json"

    def get(path):
        fetch(f"{api}/{path}", page_file)
        return read_json(page_file)

    def eligible(row):
        tag = row["tag_name"]
        if row["draft"] or not TAG.fullmatch(tag):
            return False
        prerelease = not semver(tag)[3]
        if prerelease != row["prerelease"] or (prerelease and channel == "stable"):
            return False
        assets = {asset["name"] for asset in row["assets"]}
        return set(names(tag)) <= assets

    if pin:
        row = get(f"releases/tags/{pin}")
        require(row["tag_name"] == pin and eligible(row), f"No eligible release for {pin}")
        tag = pin
    else:
        candidates = []
        for page in range(1, 1001):
            rows = get(f"releases?per_page=100&page={page}")
            candidates.extend(row["tag_name"] for row in rows if eligible(row))
            if len(rows) < 100:
                break
        else:
            raise ValueError("Too many release pages; set FUZUKUE_VERSION.")
        if current:
            candidates = [tag for tag in candidates if semver(tag) >= semver(current)]
        elif channel == "beta":
            candidates = [tag for tag in candidates if not semver(tag)[3]] or candidates
        require(candidates, "No eligible release. Use FUZUKUE_VERSION to select an older tag.")
        tag = max(candidates, key=semver)

    ref = get(f"git/ref/tags/{tag}")
    require(ref["ref"] == f"refs/tags/{tag}", "GitHub returned a different tag reference.")
    obj = ref["object"]
    for _ in range(10):
        require(COMMIT.fullmatch(obj["sha"]), "Invalid Git object SHA.")
        if obj["type"] == "commit":
            return tag, obj["sha"]
        require(obj["type"] == "tag", "Release tag must resolve to a commit.")
        obj = get(f'git/tags/{obj["sha"]}')["object"]
    raise ValueError("Too many annotated tag references.")


def check_sum(sums, path):
    entries = []
    for line in sums.splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line)
        if match and match[2] == path.name:
            entries.append(match[1].lower())
    require(len(entries) == 1, f"Expected one SHA256SUMS entry for {path.name}")
    require(digest(path) == entries[0], f"SHA-256 mismatch for {path.name}")
    return entries[0]


def download(args, client, current, temp):
    binary = "fuzukue" if client else "secretaire"
    kind = "linux-client" if client else "server"
    repo = args.repository
    fixture = args.fixture

    def archive(tag):
        return f"{binary}-{tag}-{PLATFORM}.tar.gz"

    manifest_path = temp / MANIFEST
    sums_path = temp / "SHA256SUMS"
    if fixture:
        # Explicit offline testing mode. Production downloads always use HTTPS.
        def get_asset(name):
            source = fixture / name
            require(source.is_file() and not source.is_symlink(), f"Missing fixture file: {source}")
            shutil.copyfile(source, temp / name)

        tag = commit = None
    else:
        tag, commit = choose_release(
            f"https://api.github.com/repos/{repo}", temp, args.channel, args.version,
            current, lambda tag: (MANIFEST, "SHA256SUMS", archive(tag)),
        )

        def get_asset(name):
            fetch(f"https://github.com/{repo}/releases/download/{tag}/{name}", temp / name)

    get_asset("SHA256SUMS")
    get_asset(MANIFEST)
    sums = sums_path.read_text(encoding="ascii")
    check_sum(sums, manifest_path)
    manifest = read_json(manifest_path)
    if fixture:
        tag = "v" + manifest["version"]
        require(TAG.fullmatch(tag), "Invalid fixture version.")
        require(not args.version or tag == args.version, "Fixture version differs from pin.")
    require(manifest["schema_version"] == 1 and manifest["version"] == tag[1:],
            "Release manifest version or schema does not match.")
    require(COMMIT.fullmatch(manifest["commit"]) and (not commit or manifest["commit"] == commit),
            "Release manifest commit does not match the resolved tag.")
    channel = "stable" if semver(tag)[3] else "beta"
    require(manifest["channel"] == channel and (args.channel == "beta" or channel == "stable"),
            "Release manifest channel does not match.")
    protocol = manifest["protocol"]
    limits = [manifest[key] for key in ("min_client_protocol", "min_server_protocol")]
    require(type(protocol) is int and all(type(n) is int and 0 < n <= protocol for n in limits),
            "Invalid release protocol compatibility bounds.")

    name = archive(tag)
    artifacts = [item for item in manifest["artifacts"]
                 if item["kind"] == kind and item["platform"] == PLATFORM]
    require(len(artifacts) == 1 and artifacts[0]["name"] == name,
            f"Expected one manifest entry for {name}")
    item = artifacts[0]
    get_asset(name)
    path = temp / name
    checksum = check_sum(sums, path)
    require(checksum == item["sha256"] and path.stat().st_size == item["size"] > 0,
            f"Release manifest size or SHA-256 mismatch for {name}")
    return tag, manifest["commit"], path, checksum


def extract(archive, binary, dest):
    # Copy one regular member to a path chosen by the installer. Archive paths
    # and file modes never control where or how the installer writes.
    with tarfile.open(archive, mode="r:gz") as tar:
        members = [member for member in tar if member.name == binary]
        require(len(members) == 1 and members[0].isreg() and members[0].size > 0,
                f"Expected one regular {binary} in {archive.name}")
        with tar.extractfile(members[0]) as source, dest.open("xb") as output:
            shutil.copyfileobj(source, output)
    dest.chmod(0o755)


def install_version(root, version, binary, checksum, populate):
    dest = root / version
    exe = dest / binary
    existing = os.path.lexists(dest)
    if existing:
        require(not dest.is_symlink() and dest.is_dir(), f"Unmanaged version: {dest}")
        marker = dest / MARKER
        require(marker.is_file() and not marker.is_symlink()
                and marker.read_text(encoding="ascii").strip() == checksum,
                f"Existing version differs: {dest}")
        if os.path.lexists(exe):
            require(exe.is_file() and not exe.is_symlink() and os.access(exe, os.X_OK),
                    f"Existing executable is unusable: {exe}")
            # The desktop updater may have replaced this executable in place.
            return f"{version}/{binary}"

    with tempfile.TemporaryDirectory(dir=root, prefix=".stage-") as temp:
        stage = Path(temp) / "payload"
        stage.mkdir(mode=0o755)
        populate(stage / binary)
        if existing:
            (stage / binary).rename(exe)
        else:
            (stage / MARKER).write_text(checksum + "\n", encoding="ascii")
            stage.rename(dest)
    return f"{version}/{binary}"


def migrate(root, binary):
    legacy = xdg("XDG_STATE_HOME", ".local/state") / "fuzukue/server-builds"
    links = {}
    for name in ("current", "previous"):
        link = legacy / name
        if not os.path.lexists(link):
            continue
        require(link.is_symlink(), f"Expected a legacy server symlink: {link}")
        target = link.resolve(strict=True)
        try:
            commit, build, executable = target.relative_to(legacy.resolve()).parts
        except ValueError:
            raise ValueError(f"Unexpected legacy server build path: {target}") from None
        require((COMMIT.fullmatch(commit) or commit == "original") and build.startswith("build-")
                and executable == binary and target.is_file() and os.access(target, os.X_OK),
                f"Unexpected legacy server build: {target}")
        checksum = digest(target)

        def copy_binary(dest):
            shutil.copy2(target, dest)
            require(digest(dest) == checksum, f"Legacy server changed during copy: {target}")

        links[name] = install_version(root, f"local-{commit}-{checksum}", binary, checksum, copy_binary)
    # Keep both copies. The old current is the first rollback target.
    return links.get("current") or links.get("previous")


def read_link(root, name, binary):
    path = root / name
    if not os.path.lexists(path):
        return None
    require(path.is_symlink(), f"Unmanaged pointer: {path}")
    target = os.readlink(path)
    parts = target.split("/")
    require(len(parts) == 2 and (TAG.fullmatch(parts[0]) or LOCAL_VERSION.fullmatch(parts[0]))
            and parts[1] == binary, f"Invalid pointer: {path}")
    version = root / parts[0]
    exe = root / target
    marker = version / MARKER
    require(not version.is_symlink() and not exe.is_symlink() and exe.is_file()
            and marker.is_file() and not marker.is_symlink(), f"Missing managed version: {path}")
    return target


def set_link(root, name, target):
    path = root / name
    if target is None:
        path.unlink(missing_ok=True)
        return
    with tempfile.TemporaryDirectory(dir=root, prefix=".link-") as temp:
        link = Path(temp) / "link"
        link.symlink_to(target)
        link.replace(path)


def launcher_text(header, root, binary):
    return f"""#!/bin/sh
{header}root={shlex.quote(str(root))}
target=$(realpath -e -- "$root/current") || exit 1
case "$target" in
    "$root"/*/{binary}) exec "$target" "$@" ;;
    *) echo 'Invalid fuzukue release pointer.' >&2; exit 1 ;;
esac
"""


def systemctl(*args, check=True, capture=False):
    return subprocess.run(["systemctl", "--user", *args], check=check, text=True,
                          stdout=subprocess.PIPE if capture else None)


def unit_text(project, launcher, port):
    def quote(value):
        value = str(value)
        require(not any(ord(char) < 32 or ord(char) == 127 for char in value),
                "The service requires paths without control characters.")
        return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'

    paths = [
        quote(f"{key}={xdg(key, default)}") for key, default in (
            ("XDG_CONFIG_HOME", ".config"), ("XDG_STATE_HOME", ".local/state"),
            ("XDG_DATA_HOME", ".local/share"), ("XDG_CACHE_HOME", ".cache"),
        )
    ]
    return f"""{UNIT_HEADER}# Health port: {port}
[Unit]
Description=fuzukue packaged server

[Service]
Type=exec
ExecStart=:/bin/sh -c 'exec "$@"' sh {quote(launcher)} {quote(project)}
WorkingDirectory={quote(project)}
Environment={" ".join(paths)}
Restart=on-failure
RestartSec=3
KillMode=mixed
TimeoutStopSec=75

[Install]
WantedBy=default.target
"""


def check_health(port, expected_exe):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        pid = systemctl("show", "-p", "MainPID", "--value", UNIT, capture=True).stdout.strip()
        if pid.isdecimal() and pid != "0":
            try:
                with opener.open(f"http://127.0.0.1:{port}/self-update/health", timeout=1) as response:
                    if (response.read(64).strip() == pid.encode("ascii")
                            and Path(f"/proc/{pid}/exe").resolve(strict=True) == expected_exe):
                        return
            except OSError:
                pass
        time.sleep(0.5)
    raise ValueError(f"{UNIT} failed its startup health check on port {port}.")


def switch(root, launcher, text, unit, new_unit, current, previous, candidate, backup):
    old_unit = unit.read_text(encoding="utf-8") if unit and unit.is_file() else None
    had_launcher = launcher.is_file()
    active = enabled = False
    if old_unit or new_unit:
        systemctl("show-environment", capture=True)
        active = systemctl("is-active", "--quiet", UNIT, check=False).returncode == 0
        enabled = systemctl("is-enabled", "--quiet", UNIT, check=False).returncode == 0
    must_start = active or new_unit is not None
    if active and (current != candidate or old_unit != (new_unit or old_unit)):
        systemctl("stop", UNIT)
    try:
        if not had_launcher:
            write_file(launcher, text, 0o755)
        set_link(root, "previous", backup)
        set_link(root, "current", candidate)
        if new_unit is not None:
            write_file(unit, new_unit, 0o644)
            systemctl("daemon-reload")
            systemctl("enable", UNIT)
        if must_start and (not active or current != candidate or new_unit is not None):
            systemctl("start", UNIT)
            port = int((new_unit or old_unit).splitlines()[1].split()[-1])
            check_health(port, root / candidate)
    except (ValueError, OSError, subprocess.CalledProcessError):
        if must_start:
            systemctl("stop", UNIT, check=False)
        set_link(root, "current", current)
        set_link(root, "previous", previous)
        if not had_launcher:
            launcher.unlink(missing_ok=True)
        if new_unit is not None:
            if not enabled:
                systemctl("disable", UNIT, check=False)
            if old_unit is None:
                unit.unlink(missing_ok=True)
            else:
                write_file(unit, old_unit, 0o644)
            systemctl("daemon-reload")
        if active:
            systemctl("start", UNIT)
        raise


def uninstall(root, launcher, unit, binary):
    if unit and unit.is_file():
        systemctl("disable", "--now", UNIT)
        unit.unlink()
        systemctl("daemon-reload")
    launcher.unlink(missing_ok=True)
    set_link(root, "current", None)
    set_link(root, "previous", None)
    for version in root.iterdir():
        if version.is_symlink() or not version.is_dir():
            continue
        if not (TAG.fullmatch(version.name) or LOCAL_VERSION.fullmatch(version.name)):
            continue
        marker = version / MARKER
        if marker.is_symlink() or not marker.is_file() or not HASH.fullmatch(marker.read_text().strip()):
            continue
        (version / binary).unlink(missing_ok=True)
        if list(version.iterdir()) == [marker]:
            marker.unlink()
            version.rmdir()
    print(f"Uninstalled {binary}. Settings and data were kept.")


def main():
    parser = argparse.ArgumentParser(prog=f"install-{'client' if sys.argv[1:2] == ['--client'] else 'server'}.sh", description="Install a verified fuzukue release on Linux or WSL2.")
    parser.add_argument("--client", action="store_true", help=argparse.SUPPRESS)
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--uninstall", action="store_true", help="remove the managed installation")
    operation.add_argument("--rollback", action="store_true", help="swap current and previous")
    operation.add_argument("--dry-run", action="store_true", help="download and verify without installing")
    parser.add_argument("--systemd", metavar="PROJECT", type=Path, help="enable and start a server user unit")
    parser.add_argument("--health-port", type=int, default=7878, help="localhost port for the user unit health check")
    parser.add_argument("--fixture", type=Path, help="offline test only: read artifacts from a local directory")
    parser.add_argument("--repository", default=OFFICIAL_REPO, metavar="OWNER/REPO")
    parser.add_argument("--channel", default=os.environ.get("FUZUKUE_CHANNEL", "beta"), choices=("beta", "stable"))
    parser.add_argument("--version", default=os.environ.get("FUZUKUE_VERSION", ""), metavar="TAG")
    args = parser.parse_args()
    require(os.uname().sysname == "Linux" and os.uname().machine == "x86_64", "Linux x86_64 is required.")
    require(args.channel in ("beta", "stable"), "FUZUKUE_CHANNEL must be beta or stable.")
    require(not args.version or TAG.fullmatch(args.version), "FUZUKUE_VERSION must be a v-prefixed SemVer tag.")
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+", args.repository)
            and args.repository.split("/")[1] not in (".", ".."), "Expected a GitHub OWNER/REPO.")
    require(not args.systemd or not (args.client or args.uninstall or args.rollback or args.dry_run),
            "--systemd requires a server installation.")
    require(1 <= args.health_port <= 65535, "Invalid service health port.")

    binary = "fuzukue" if args.client else "secretaire"
    base = xdg("XDG_DATA_HOME", ".local/share") / "fuzukue"
    root = base / "client" if args.client else base
    require(not base.is_symlink() and not root.is_symlink(), f"Unexpected install root: {root}")
    root = root.resolve()
    launcher = Path.home() / ".local/bin" / binary
    header = f"# Managed by fuzukue release installer ({binary})\n"
    text = launcher_text(header, root, binary)

    def check_launcher():
        check_file(launcher, "#!/bin/sh\n" + header)
        require(not launcher.exists() or launcher.read_text(encoding="utf-8") == text,
                f"Launcher points to a different install root: {launcher}")

    check_launcher()
    unit = None if args.client else xdg("XDG_CONFIG_HOME", ".config") / "systemd/user" / UNIT
    if unit:
        check_file(unit, UNIT_HEADER)
    new_unit = None
    if args.systemd:
        project = args.systemd.resolve(strict=True)
        require(project.is_dir(), "The systemd project must be a directory.")
        new_unit = unit_text(project, launcher, args.health_port)

    def run():
        current = read_link(root, "current", binary)
        previous = read_link(root, "previous", binary)
        if args.uninstall:
            uninstall(root, launcher, unit, binary)
            return
        if args.rollback:
            require(previous and current and previous != current, "No previous release to restore.")
            switch(root, launcher, text, unit, None, current, previous, previous, current)
            print(f"Restored {previous}.")
            return
        active_tag = current.split("/")[0] if current else None
        with tempfile.TemporaryDirectory(prefix="fuzukue-download-") as directory:
            tag, commit, archive, checksum = download(
                args, args.client, active_tag if active_tag and TAG.fullmatch(active_tag) else None,
                Path(directory),
            )
            if args.dry_run:
                extract(archive, binary, Path(directory) / binary)
                print(f"Verified {archive.name} ({commit}).")
                return
            candidate = install_version(root, tag, binary, checksum, lambda dest: extract(archive, binary, dest))
        backup = previous
        if current and candidate != current:
            backup = current
        elif not current and not args.client:
            backup = migrate(root, binary)
        switch(root, launcher, text, unit, new_unit, current, previous, candidate, backup)
        print(f"Installed {tag}. Launch with {launcher}")

    if args.dry_run:
        run()
        return
    if args.uninstall and not root.exists() and not launcher.exists() and not (unit and unit.exists()):
        return
    root.mkdir(parents=True, exist_ok=True)
    with os.fdopen(os.open(root / ".install.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600), "r+") as lock:
        require(stat.S_ISREG(os.fstat(lock.fileno()).st_mode), "Invalid install lock file.")
        fcntl.flock(lock, fcntl.LOCK_EX)
        # Recheck ownership after waiting for another installer or uninstaller.
        check_launcher()
        if unit:
            check_file(unit, UNIT_HEADER)
        run()


try:
    main()
except (ValueError, OSError, KeyError, TypeError, tarfile.TarError, subprocess.CalledProcessError) as error:
    sys.exit(f"fuzukue installer: {error}")
PY
