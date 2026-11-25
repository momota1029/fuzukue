#!/bin/sh
set -eu
exec python3 - "$0" "$@" <<'PY'
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
import plistlib
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
import urllib.error
import urllib.request

INSTALLER = Path(sys.argv.pop(1)).resolve()
OFFICIAL_REPO = "momota1029/fuzukue"
NUMBER = r"(?:0|[1-9][0-9]*)"
IDENTIFIER = rf"(?:{NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
TAG = re.compile(rf"v({NUMBER})\.({NUMBER})\.({NUMBER})(?:-({IDENTIFIER}(?:\.{IDENTIFIER})*))?")
HASH = re.compile(r"[a-f0-9]{64}")
COMMIT = re.compile(r"[a-f0-9]{40}|[a-f0-9]{64}")
LOCAL_VERSION = re.compile(rf"local-(?:{COMMIT.pattern}|original)-{HASH.pattern}")
MARKER = ".fuzukue-package"
MANIFEST = "release-manifest.json"
MAC_MANIFEST = "macos-release-manifest.json"
MAC_SUMS = "SHA256SUMS-macos"
PLATFORMS = {
    ("Linux", "x86_64"): "linux-x86_64",
    ("Darwin", "arm64"): "macos-aarch64",
    ("Darwin", "x86_64"): "macos-x86_64",
}
UNIT = "fuzukue-release.service"
UNIT_HEADER = "# Managed by fuzukue release installer\n"
AGENT = "com.fuzukue.secretaire"
UPDATE_AGENT = "com.fuzukue.release-update"
CHECK_INTERVAL = 3600
STARTUP_CHECK_FLOOR = 900
AGENT_HEADER = '<?xml version="1.0" encoding="UTF-8"?>\n<!-- Managed by fuzukue release installer -->\n'
AGENT_CLI = (
    ("Claude Code", "claude", ("auth", "status"), "claude auth login", "https://claude.ai/install.sh", "/bin/bash"),
    ("Codex CLI", "codex", ("login", "status"), "codex login", "https://chatgpt.com/codex/install.sh", "/bin/sh"),
)
CLI_VERSION = re.compile(r"\b[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?\b")


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


def fetch_command(url, output):
    require(url.startswith("https://"), f"HTTPS is required: {url}")
    return [
        "curl", "--disable", "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "10",
        "--connect-timeout", "15", "--max-time", "600", "--output", str(output), url,
    ]


def fetch(url, output):
    subprocess.run(fetch_command(url, output), check=True)


def agent_search_path():
    home = Path.home()
    known = [
        home / ".local/bin", home / "bin", home / ".npm-global/bin", home / ".volta/bin",
        Path("/opt/homebrew/bin"), Path("/usr/local/bin"), Path("/usr/bin"), Path("/bin"),
    ]
    # Keep the terminal's order and version manager paths. Exclude relative
    # entries because the service executes in the workspace.
    entries = [entry for entry in os.get_exec_path() if Path(entry).is_absolute()]
    return os.pathsep.join(dict.fromkeys(entries + [str(directory) for directory in known]))


def confirmation():
    # stdin contains the installer program. Ask only via an interactive terminal.
    if not sys.stderr.isatty():
        return False
    try:
        with open("/dev/tty", "r", encoding="utf-8") as terminal:
            print("Run these commands? [y/N] ", end="", file=sys.stderr, flush=True)
            return terminal.readline().strip().lower() in ("y", "yes")
    except OSError:
        return False


def offer_agent_install(url, shell, explicit):
    with tempfile.TemporaryDirectory(prefix="fuzukue-agent-installer-") as tmp:
        dest = Path(tmp) / "install.sh"
        download = fetch_command(url, dest)
        execute = [shell, str(dest)]
        print(f"  {shlex.join(download)} && {shlex.join(execute)}", flush=True)
        if not explicit and not confirmation():
            return
        try:
            subprocess.run(download, check=True, stdin=subprocess.DEVNULL)
            subprocess.run(execute, check=True, stdin=subprocess.DEVNULL)
        except (OSError, subprocess.CalledProcessError):
            print("  The agent installer failed. Continuing with the server.", file=sys.stderr)


def agent_status_command(executable, command, capture=False):
    try:
        return subprocess.run(
            [executable, *command], timeout=8, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, encoding="utf-8", errors="replace",
            env={**os.environ, "PATH": agent_search_path()},
        )
    except (OSError, subprocess.TimeoutExpired):
        return None


def check_agents(offer=False, explicit=False):
    signed_in = 0
    for label, name, probe, login, url, shell in AGENT_CLI:
        executable = shutil.which(name, path=agent_search_path())
        if not executable and offer:
            print(f"{label} is missing. Official installer:", flush=True)
            offer_agent_install(url, shell, explicit)
            executable = shutil.which(name, path=agent_search_path())
        if not executable:
            print(f"{label}: missing. Install with: curl -fsSL {url} | {Path(shell).name}")
            print(f"  Sign in on this machine with: {login}")
            continue
        version = agent_status_command(executable, ("--version",), capture=True)
        match = CLI_VERSION.search(version.stdout) if version and version.returncode == 0 else None
        status = agent_status_command(executable, probe)
        result = status.returncode if status else None
        auth = {0: "signed in", 1: "not signed in"}.get(result, "sign-in status unavailable")
        version_text = match[0] if match else "version unavailable"
        print(f"{label}: {version_text}, {auth}")
        if result == 0:
            signed_in += 1
        else:
            print(f"  Sign in on this machine with: {login}")
    if signed_in == 0:
        print("The server can start. Sign in to either CLI to use agent conversations.")
    print("Recheck from the installer directory with: sh scripts/install-server.sh --check-agents")


def read_json(path):
    return json.loads(path.read_bytes())


def eligible_release(row, channel, names, prerelease_only):
    tag = row["tag_name"]
    if row["draft"] or not TAG.fullmatch(tag):
        return False
    prerelease = not semver(tag)[3]
    if prerelease != row["prerelease"]:
        return False
    if (prerelease and channel == "stable") or (prerelease_only and not prerelease):
        return False
    assets = {asset["name"] for asset in row["assets"]}
    return set(names(tag)) <= assets


def mac_names(tag, platform):
    return (MAC_MANIFEST, MAC_SUMS, f"secretaire-{tag}-{platform}.tar.gz",
            f"fuzukue-{tag}-{platform}.tar.gz")


def choose_release(api, temp, channel, pin, current, names, prerelease_only):
    page_file = temp / "api.json"

    def get(path):
        fetch(f"{api}/{path}", page_file)
        return read_json(page_file)

    def eligible(row):
        return eligible_release(row, channel, names, prerelease_only)

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


def download(args, binary, platform, current, temp):
    macos = platform.startswith("macos-")
    kind = ("macos-client" if macos else "linux-client") if binary == "fuzukue" else "server"
    repo = args.repository
    fixture = args.fixture

    def archive(tag):
        return f"{binary}-{tag}-{platform}.tar.gz"

    manifest_path = temp / (MAC_MANIFEST if macos else MANIFEST)
    sums_path = temp / (MAC_SUMS if macos else "SHA256SUMS")
    if fixture:
        # Explicit offline testing mode. Production downloads always use HTTPS.
        def get_asset(name):
            source = fixture / name
            require(source.is_file() and not source.is_symlink(), f"Missing fixture file: {source}")
            shutil.copyfile(source, temp / name)

        tag = commit = None
    else:
        def names(tag):
            required = (manifest_path.name, sums_path.name)
            return mac_names(tag, platform) if macos else required + (archive(tag),)

        tag, commit = choose_release(
            f"https://api.github.com/repos/{repo}", temp, args.channel, args.version,
            current, names, macos,
        )

        def get_asset(name):
            fetch(f"https://github.com/{repo}/releases/download/{tag}/{name}", temp / name)

    get_asset(sums_path.name)
    get_asset(manifest_path.name)
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
    require(manifest["channel"] == channel and (args.channel == "beta" or channel == "stable")
            and (not macos or channel == "beta"), "Release manifest channel does not match.")
    protocol = manifest["protocol"]
    limits = [manifest[key] for key in ("min_client_protocol", "min_server_protocol")]
    require(type(protocol) is int and all(type(n) is int and 0 < n <= protocol for n in limits),
            "Invalid release protocol compatibility bounds.")

    name = archive(tag)
    artifacts = [item for item in manifest["artifacts"]
                 if item["kind"] == kind and item["platform"] == platform]
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


def install_version(root, version, binary, checksum, populate, verify_existing=False):
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
            # Packaged desktops may have been updated in place. A source
            # install always uses the binary from the current build.
            if not verify_existing or digest(exe) == checksum:
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


def launcher_text(header, root, binary, macos, legacy=False):
    if macos:
        source_case = "" if legacy else f"    local-*/{binary}) ;;\n"
        release_target = "" if legacy else '[ "${target#v}" != "$target" ] && '
        return f"""#!/bin/sh
{header}root={shlex.quote(str(root))}
target=$(readlink "$root/current") || exit 1
case "$target" in
    */*/*) exit 1 ;;
    v[0-9]*/{binary}) ;;
{source_case}    *) echo 'Invalid fuzukue release pointer.' >&2; exit 1 ;;
esac
if [ "$#" -eq 1 ] && [ "$1" = --version ]; then
    printf '%s %s\\n' '{binary}' "${{target%/*}}"
    exit 0
fi
if {release_target}[ -x /bin/launchctl ] && [ ! -e "$root/../auto-update-off" ]; then
    /bin/launchctl kickstart "gui/$(/usr/bin/id -u)/{UPDATE_AGENT}" >/dev/null 2>&1 || :
fi
exec "$root/$target" "$@"
"""
    return f"""#!/bin/sh
{header}root={shlex.quote(str(root))}
target=$(realpath -e -- "$root/current") || exit 1
case "$target" in
    "$root"/*/{binary}) exec "$target" "$@" ;;
    *) echo 'Invalid fuzukue release pointer.' >&2; exit 1 ;;
esac
"""


def managed_launcher(root, binary, macos):
    launcher = Path.home() / ".local/bin" / binary
    header = f"# Managed by fuzukue release installer ({binary})\n"
    text = launcher_text(header, root, binary, macos)
    check_file(launcher, "#!/bin/sh\n" + header)
    require(not launcher.exists() or launcher.read_text(encoding="utf-8") in
            (text, launcher_text(header, root, binary, macos, legacy=True)),
            f"Launcher points to a different install root: {launcher}")
    return launcher, text


@contextmanager
def install_lock(root):
    root.mkdir(parents=True, exist_ok=True)
    with os.fdopen(os.open(root / ".install.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600), "r+") as lock:
        require(stat.S_ISREG(os.fstat(lock.fileno()).st_mode), "Invalid install lock file.")
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def systemctl(*args, check=True, capture=False):
    return subprocess.run(["systemctl", "--user", *args], check=check, text=True,
                          stdout=subprocess.PIPE if capture else None)


def unit_text(project, launcher, port, loopback=False):
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
    paths.append(quote(f"PATH={agent_search_path()}"))
    if loopback:
        paths.append(quote(f"FUZUKUE_ADDR=127.0.0.1:{port}"))
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


def switch(root, launcher, text, unit, new_unit, current, previous, candidate, backup, finish=None):
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
        if finish:
            finish()
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


def launchctl(*args, query=False):
    return subprocess.run(
        ["/bin/launchctl", *args], check=not query, text=True,
        stdout=subprocess.PIPE if query else None,
        stderr=subprocess.DEVNULL if query else None,
    )


def launch_domain():
    domain = f"gui/{os.getuid()}"
    require(launchctl("print", domain, query=True).returncode == 0,
            "launchd needs a logged-in macOS desktop session. Run from Terminal or launch manually.")
    return domain


def agent_text(project, launcher, port, packaged=True):
    home = Path.home()
    logs = home / "Library/Logs/fuzukue"
    env = {
        "HOME": str(home),
        "PATH": agent_search_path(),
        "FUZUKUE_ADDR": f"127.0.0.1:{port}",
        "XDG_STATE_HOME": str(xdg("XDG_STATE_HOME", ".local/state")),
    }
    if packaged:
        env["FUZUKUE_MACOS_PACKAGE"] = "1"
    payload = plistlib.dumps({
        "Label": AGENT,
        "ProgramArguments": [str(launcher), str(project)],
        "WorkingDirectory": str(project),
        "EnvironmentVariables": env,
        "RunAtLoad": True,
        "KeepAlive": {"SuccessfulExit": False},
        "Umask": 0o077,
        "ExitTimeOut": 75,
        "StandardOutPath": str(logs / "secretaire.log"),
        "StandardErrorPath": str(logs / "secretaire.err.log"),
    }).decode("utf-8")
    return AGENT_HEADER + payload.split("\n", 1)[1]


def agent_status(unit, domain, label=AGENT):
    result = launchctl("print", f"{domain}/{label}", query=True)
    if result.returncode != 0:
        return None
    path = re.search(r"(?m)^\s*path = (.+)$", result.stdout)
    require(path and Path(path[1]).resolve() == unit.resolve(),
            f"A different launch agent is registered as {label}.")
    return result.stdout


def stop_agent(unit, domain, label=AGENT):
    launchctl("bootout", f"{domain}/{label}")
    deadline = time.monotonic() + 85
    while agent_status(unit, domain, label) is not None:
        require(time.monotonic() < deadline, f"{label} did not unload.")
        time.sleep(0.25)


def check_agent_health(unit, domain, port):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        status = agent_status(unit, domain) or ""
        pid = re.search(r"(?m)^\s*pid = ([1-9][0-9]*)$", status)
        if pid:
            try:
                with opener.open(f"http://127.0.0.1:{port}/self-update/health", timeout=1) as response:
                    if response.read(64).strip() == pid[1].encode("ascii"):
                        return
            except OSError:
                pass
        time.sleep(0.5)
    raise ValueError(f"{AGENT} failed its startup health check on port {port}. "
                     "See ~/Library/Logs/fuzukue/secretaire.err.log.")


def mac_base():
    return Path.home() / "Library/Application Support/fuzukue/install"


def agent_path(label):
    return Path.home() / "Library/LaunchAgents" / f"{label}.plist"


def update_schedule(base, remove=False):
    unit = agent_path(UPDATE_AGENT)
    script = base / "release-update.sh"
    check_file(unit, AGENT_HEADER)
    if remove and not unit.is_file():
        return
    domain = launch_domain()
    active = agent_status(unit, domain, UPDATE_AGENT) is not None
    if active:
        require(unit.is_file(), f"Expected a managed plist for {UPDATE_AGENT}.")
    if remove:
        if active:
            stop_agent(unit, domain, UPDATE_AGENT)
        unit.unlink()
        script.unlink(missing_ok=True)
        return

    logs = Path.home() / "Library/Logs/fuzukue"
    payload = plistlib.dumps({
        "Label": UPDATE_AGENT,
        "ProgramArguments": ["/bin/sh", str(script), "--scheduled-update"],
        "EnvironmentVariables": {"HOME": str(Path.home()), "PATH": os.environ.get("PATH", os.defpath)},
        "RunAtLoad": True,
        "StartInterval": CHECK_INTERVAL,
        "ProcessType": "Background",
        "Umask": 0o077,
        "StandardOutPath": str(logs / "release-update.log"),
        "StandardErrorPath": str(logs / "release-update.err.log"),
    }).decode("utf-8")
    text = AGENT_HEADER + payload.split("\n", 1)[1]
    write_file(script, INSTALLER.read_text(encoding="utf-8"), 0o700)
    if unit.is_file() and unit.read_text(encoding="utf-8") == text and active:
        return
    if active:
        stop_agent(unit, domain, UPDATE_AGENT)
    write_file(unit, text, 0o644)
    launchctl("bootstrap", domain, str(unit))


def retry_delay(headers):
    delay = headers.get("Retry-After", "")
    reset = headers.get("X-RateLimit-Reset", "")
    if delay.isdecimal():
        return int(delay)
    if headers.get("X-RateLimit-Remaining") == "0" and reset.isdecimal():
        return max(0, int(reset) - int(time.time()))
    return 0


def backoff_seconds(failures):
    return min(STARTUP_CHECK_FLOOR * 2 ** min(failures - 1, 5), 6 * CHECK_INTERVAL)


def cached_releases(base):
    cache = base / "release-catalog.json"
    state = read_json(cache) if cache.is_file() else {}
    now = time.time()
    if now < state.get("retry_at", 0):
        return []
    if now < state.get("checked_at", 0) + STARTUP_CHECK_FLOOR:
        return state["releases"]
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "fuzukue-release-installer"}
    if state.get("etag"):
        headers["If-None-Match"] = state["etag"]
    url = f"https://api.github.com/repos/{OFFICIAL_REPO}/releases?per_page=100"
    error = None
    delay = 0
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=15) as response:
            rows = json.load(response)
            # Only the fields needed for selection go in the user's catalog cache.
            state["releases"] = [{
                "tag_name": row["tag_name"], "draft": row["draft"], "prerelease": row["prerelease"],
                "assets": [{"name": item["name"]} for item in row["assets"]],
            } for row in rows]
            state["etag"] = response.headers.get("ETag")
    except urllib.error.HTTPError as exc:
        if exc.code != 304 or "releases" not in state:
            delay = retry_delay(exc.headers)
            error = exc
    except (OSError, ValueError, KeyError, TypeError) as exc:
        error = exc
    if error is None:
        state.update(checked_at=now, retry_at=0, failures=0)
    else:
        failures = state.get("failures", 0) + 1
        state.update(retry_at=now + max(backoff_seconds(failures), delay), failures=failures)
        print(f"Release check deferred: {error}", file=sys.stderr)
    write_file(cache, json.dumps(state), 0o600)
    return state["releases"] if error is None else []


def scheduled_update():
    uname = os.uname()
    platform = PLATFORMS.get((uname.sysname, uname.machine), "")
    require(platform.startswith("macos-"), "Scheduled updates require macOS.")
    base = mac_base()
    if (base / "auto-update-off").exists():
        return
    server = base / "server"
    client = base / "client"
    old_server = read_link(server, "current", "secretaire")
    old_client = read_link(client, "current", "fuzukue")
    if not old_server or not TAG.fullmatch(old_server.split("/")[0]):
        return
    rows = cached_releases(base)
    tags = [row["tag_name"] for row in rows
            if eligible_release(row, "beta", lambda tag: mac_names(tag, platform), True)]
    if not tags:
        return
    tag = max(tags, key=semver)
    retry_path = base / "release-apply-retry.json"
    retry = read_json(retry_path) if retry_path.is_file() else {}
    if retry.get("tag") != tag:
        retry = {}
    if time.time() < retry.get("retry_at", 0):
        return

    command = ["/bin/sh", str(INSTALLER), "--automatic", "--channel", "beta", "--version", tag]
    server_tag = old_server.split("/")[0]
    try:
        if semver(tag) > semver(server_tag):
            subprocess.run(command, check=True)
            installed = read_link(server, "current", "secretaire")
            server_tag = installed.split("/")[0] if installed else None
        # A running desktop keeps its old image. Launch the new client only after
        # the corresponding server has passed its health check.
        client_tag = old_client.split("/")[0] if old_client else None
        if client_tag and server_tag == tag and (not TAG.fullmatch(client_tag) or semver(tag) > semver(client_tag)):
            subprocess.run(command + ["--client"], check=True)
    except subprocess.CalledProcessError:
        failures = retry.get("failures", 0) + 1
        state = {"tag": tag, "failures": failures, "retry_at": time.time() + backoff_seconds(failures)}
        write_file(retry_path, json.dumps(state), 0o600)
        raise
    else:
        retry_path.unlink(missing_ok=True)


def agent_idle(unit):
    if unit is None or not unit.is_file():
        return False
    status = agent_status(unit, launch_domain()) or ""
    pid = re.search(r"(?m)^\s*pid = ([1-9][0-9]*)$", status)
    if not pid:
        return False
    config = plistlib.loads(unit.read_bytes())
    port = int(config["EnvironmentVariables"]["FUZUKUE_ADDR"].split(":", 1)[1])
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(f"http://127.0.0.1:{port}/self-update/wait?package_idle=true", timeout=2) as response:
            body = json.load(response)
            return body.get("idle") is True and body.get("pid") == int(pid[1])
    except (OSError, ValueError):
        return False


def switch_launchd(root, launcher, text, unit, new_unit, current, previous, candidate, backup, finish=None):
    old_unit = unit.read_text(encoding="utf-8") if unit and unit.is_file() else None
    old_launcher = launcher.read_text(encoding="utf-8") if launcher.is_file() else None
    domain = launch_domain() if old_unit is not None or new_unit is not None else None
    active = domain is not None and agent_status(unit, domain) is not None
    if active:
        require(old_unit is not None, f"Expected a managed plist for the loaded {AGENT}.")
    must_start = (active and current != candidate) or new_unit is not None
    if must_start:
        config = plistlib.loads((new_unit or old_unit).encode("utf-8"))
        port = int(config["EnvironmentVariables"]["FUZUKUE_ADDR"].split(":", 1)[1])
    if active and must_start:
        stop_agent(unit, domain)
    try:
        if old_launcher != text:
            write_file(launcher, text, 0o755)
        set_link(root, "previous", backup)
        set_link(root, "current", candidate)
        if new_unit is not None:
            (Path.home() / "Library/Logs/fuzukue").mkdir(mode=0o700, parents=True, exist_ok=True)
            write_file(unit, new_unit, 0o644)
        if must_start:
            launchctl("bootstrap", domain, str(unit))
            check_agent_health(unit, domain, port)
        if finish:
            finish()
    except (ValueError, OSError, subprocess.CalledProcessError):
        if must_start and agent_status(unit, domain) is not None:
            stop_agent(unit, domain)
        set_link(root, "current", current)
        set_link(root, "previous", previous)
        if old_launcher is None:
            launcher.unlink(missing_ok=True)
        elif old_launcher != text:
            write_file(launcher, old_launcher, 0o755)
        if new_unit is not None:
            if old_unit is None:
                unit.unlink(missing_ok=True)
            else:
                write_file(unit, old_unit, 0o644)
        if active and must_start:
            launchctl("bootstrap", domain, str(unit))
        raise


def uninstall(root, launcher, unit, binary, macos):
    if unit and unit.is_file():
        if macos:
            domain = launch_domain()
            if agent_status(unit, domain) is not None:
                stop_agent(unit, domain)
        else:
            systemctl("disable", "--now", UNIT)
        unit.unlink()
        if not macos:
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


def setup_built(argv):
    parser = argparse.ArgumentParser(prog="setup-unix.sh")
    parser.add_argument("build", type=Path)
    parser.add_argument("commit")
    parser.add_argument("project", type=Path)
    parser.add_argument("--skip-agents", action="store_true")
    args = parser.parse_args(argv)
    require(COMMIT.fullmatch(args.commit), "Expected a checkout commit.")
    home = Path.home().resolve()
    project = args.project.resolve(strict=True)
    require(project.is_dir() and home in project.parents, "Use a project folder inside your home.")

    macos = os.uname().sysname == "Darwin"
    base = mac_base() if macos else xdg("XDG_DATA_HOME", ".local/share") / "fuzukue"
    server = base / "server" if macos else base
    client = base / "client"
    unit = agent_path(AGENT) if macos else xdg("XDG_CONFIG_HOME", ".config") / "systemd/user" / UNIT
    header = AGENT_HEADER if macos else UNIT_HEADER
    for root in (base, server, client):
        require(not root.is_symlink(), f"Unexpected install root: {root}")
    server, client = server.resolve(), client.resolve()
    check_file(unit, header)
    for root, binary in ((server, "secretaire"), (client, "fuzukue")):
        managed_launcher(root, binary, macos)
        built = args.build / binary
        require(built.is_file() and os.access(built, os.X_OK), f"Missing build output: {built}")
    if not args.skip_agents:
        check_agents(offer=True)

    def stage(root, binary):
        built = args.build / binary
        checksum = digest(built)
        return install_version(root, f"local-{args.commit}-{checksum}", binary, checksum,
                               lambda dest: shutil.copy2(built, dest), verify_existing=True)

    # Lock in a consistent order. Stage both versions before stopping a server.
    with install_lock(server), install_lock(client):
        check_file(unit, header)
        server_launcher, server_text = managed_launcher(server, "secretaire", macos)
        client_launcher, client_text = managed_launcher(client, "fuzukue", macos)
        old_server = read_link(server, "current", "secretaire")
        old_client = read_link(client, "current", "fuzukue")
        previous_server = read_link(server, "previous", "secretaire")
        previous_client = read_link(client, "previous", "fuzukue")
        next_server = stage(server, "secretaire")
        next_client = stage(client, "fuzukue")
        port = 7878
        activate = switch_launchd if macos else switch
        service_text = (agent_text(project, server_launcher, port, packaged=False) if macos
                        else unit_text(project, server_launcher, port, loopback=True))

        def configure_client():
            # Use the client's settings writer to retain the rest of settings.toml.
            def save_settings():
                subprocess.run([str(client / next_client), "--configure-external"], check=True)

            activate(client, client_launcher, client_text, None, None,
                     old_client, previous_client, next_client,
                     old_client if old_client and old_client != next_client else previous_client,
                     finish=save_settings)

        activate(server, server_launcher, server_text, unit, service_text,
                 old_server, previous_server, next_server,
                 old_server if old_server and old_server != next_server else previous_server,
                 finish=configure_client)
        if macos:
            update_schedule(base, remove=True)
    print(f"Server is listening on 127.0.0.1:{port}. Open the desktop with:")
    print("  " + shlex.quote(str(client_launcher)))


def main():
    if sys.argv[1:2] == ["--setup-built"]:
        setup_built(sys.argv[2:])
        return
    if sys.argv[1:] == ["--scheduled-update"]:
        scheduled_update()
        return
    parser = argparse.ArgumentParser(prog=f"install-{'client' if sys.argv[1:2] == ['--client'] else 'server'}.sh", description="Install a verified fuzukue release on Linux, WSL2 or macOS.")
    parser.add_argument("--client", action="store_true", help=argparse.SUPPRESS)
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--uninstall", action="store_true", help="remove the managed installation")
    operation.add_argument("--rollback", action="store_true", help="swap current and previous")
    operation.add_argument("--dry-run", action="store_true", help="download and verify without installing")
    operation.add_argument("--auto-updates", choices=("on", "off"),
                           help="enable or disable Mac beta auto-updates for both packages")
    operation.add_argument("--automatic", action="store_true", help=argparse.SUPPRESS)
    operation.add_argument("--check-agents", action="store_true", help="report agent CLI versions and sign-in status")
    agents = parser.add_mutually_exclusive_group()
    agents.add_argument("--install-agents", "--yes", dest="install_agents", action="store_true",
                        help="approve the official installers for missing agent CLIs without prompting")
    agents.add_argument("--skip-agents", action="store_true", help="skip agent CLI checks and installation offers")
    service = parser.add_mutually_exclusive_group()
    service.add_argument("--systemd", metavar="PROJECT", type=Path, help="enable and start a Linux server user unit")
    service.add_argument("--launchd", metavar="PROJECT", type=Path, help="load and start a macOS server launch agent")
    parser.add_argument("--health-port", type=int, default=7878, help="loopback health port (also sets the launchd listen port)")
    parser.add_argument("--fixture", type=Path, help="offline test only: read artifacts from a local directory")
    parser.add_argument("--fixture-platform", choices=PLATFORMS.values(), help=argparse.SUPPRESS)
    parser.add_argument("--repository", default=OFFICIAL_REPO, metavar="OWNER/REPO")
    parser.add_argument("--channel", default=os.environ.get("FUZUKUE_CHANNEL", "beta"), choices=("beta", "stable"))
    parser.add_argument("--version", default=os.environ.get("FUZUKUE_VERSION", ""), metavar="TAG")
    args = parser.parse_args()
    require(not args.fixture_platform or (args.fixture and args.dry_run),
            "--fixture-platform requires --fixture and --dry-run.")
    uname = os.uname()
    platform = args.fixture_platform or PLATFORMS.get((uname.sysname, uname.machine))
    require(platform, "Linux x86_64 or macOS arm64 / x86_64 is required.")
    macos = platform.startswith("macos-")
    require(args.channel in ("beta", "stable"), "FUZUKUE_CHANNEL must be beta or stable.")
    require(not args.version or TAG.fullmatch(args.version), "FUZUKUE_VERSION must be a v-prefixed SemVer tag.")
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+", args.repository)
            and args.repository.split("/")[1] not in (".", ".."), "Expected a GitHub OWNER/REPO.")
    require(not args.systemd or not (macos or args.client or args.uninstall or args.rollback
                                    or args.dry_run or args.check_agents or args.auto_updates),
            "--systemd requires a Linux server installation.")
    require(not args.launchd or (macos and not (args.client or args.uninstall or args.rollback
                                                or args.auto_updates or args.automatic or args.check_agents)),
            "--launchd requires a macOS server installation.")
    require(not args.auto_updates or (macos and not args.client),
            "--auto-updates is a shared macOS setting. Use install-server.sh.")
    require(not args.automatic or (macos and args.version and not args.fixture
                                   and args.repository == OFFICIAL_REPO),
            "Automatic updates require a macOS release from the official repository.")
    if args.check_agents:
        require(not (args.client or args.install_agents or args.skip_agents),
                "--check-agents reports server prerequisites only.")
        check_agents()
        return
    require(not args.install_agents or not (args.client or args.uninstall or args.rollback
                                            or args.dry_run or args.automatic or args.auto_updates),
            "--install-agents requires a manual server installation.")
    if macos and not (args.uninstall or args.rollback or args.auto_updates):
        require(args.channel == "beta" and (not args.version or not semver(args.version)[3]),
                "macOS dogfood packages require the beta channel and a prerelease tag.")
    require(1 <= args.health_port <= 65535, "Invalid service health port.")

    binary = "fuzukue" if args.client else "secretaire"
    if macos:
        base = mac_base()
        root = base / ("client" if args.client else "server")
    else:
        base = xdg("XDG_DATA_HOME", ".local/share") / "fuzukue"
        root = base / "client" if args.client else base
    require(not base.is_symlink() and not root.is_symlink(), f"Unexpected install root: {root}")
    off = base / "auto-update-off"
    if args.auto_updates:
        if args.auto_updates == "off":
            write_file(off, "off\n", 0o600)
        else:
            off.unlink(missing_ok=True)
        print(f"Automatic macOS beta updates: {args.auto_updates}.")
        return
    root = root.resolve()
    rejected = root / ".rejected-auto-release"
    launcher, text = managed_launcher(root, binary, macos)
    unit = None
    unit_header = AGENT_HEADER if macos else UNIT_HEADER
    if not args.client:
        if macos:
            unit = agent_path(AGENT)
        else:
            unit = xdg("XDG_CONFIG_HOME", ".config") / "systemd/user" / UNIT
        check_file(unit, unit_header)
    new_unit = None
    if args.systemd:
        project = args.systemd.resolve(strict=True)
        require(project.is_dir(), "The systemd project must be a directory.")
        new_unit = unit_text(project, launcher, args.health_port)
    if args.launchd:
        project = args.launchd.resolve(strict=True)
        require(project.is_dir() and Path.home().resolve() in project.parents,
                "The launchd project must be a subdirectory of your home.")
        new_unit = agent_text(project, launcher, args.health_port)
    activate = switch_launchd if macos else switch

    def run():
        current = read_link(root, "current", binary)
        previous = read_link(root, "previous", binary)
        if args.uninstall:
            if macos and not args.client:
                update_schedule(base, remove=True)
            uninstall(root, launcher, unit, binary, macos)
            if macos:
                rejected.unlink(missing_ok=True)
            return
        if args.rollback:
            require(previous and current and previous != current, "No previous release to restore.")
            activate(root, launcher, text, unit, None, current, previous, previous, current)
            if macos:
                write_file(rejected, current.split("/")[0] + "\n", 0o600)
            print(f"Restored {previous}.")
            return
        active_tag = current.split("/")[0] if current else None
        if args.automatic:
            if (off.exists() or not active_tag
                    or (TAG.fullmatch(active_tag) and semver(args.version) <= semver(active_tag))
                    or (rejected.is_file() and rejected.read_text(encoding="utf-8").strip() == args.version)):
                return
            if not args.client and not agent_idle(unit):
                return
        with tempfile.TemporaryDirectory(prefix="fuzukue-download-") as directory:
            tag, commit, archive, checksum = download(
                args, binary, platform, active_tag if active_tag and TAG.fullmatch(active_tag) else None,
                Path(directory),
            )
            if args.dry_run:
                extract(archive, binary, Path(directory) / binary)
                print(f"Verified {archive.name} ({commit}).")
                if macos:
                    print(f"Would install to {root / tag} and launch with {launcher}.")
                    if args.launchd:
                        print(f"Would load {unit} for {project} on 127.0.0.1:{args.health_port}.")
                return
            candidate = install_version(root, tag, binary, checksum, lambda dest: extract(archive, binary, dest))
        if not (args.client or args.automatic or args.skip_agents):
            check_agents(offer=True, explicit=args.install_agents)
        if args.automatic and (off.exists() or (not args.client and not agent_idle(unit))):
            return
        if args.automatic and args.client:
            if read_link(base / "server", "current", "secretaire") != f"{tag}/secretaire":
                return
        backup = previous
        if current and candidate != current:
            backup = current
        elif not current and not args.client and not macos:
            backup = migrate(root, binary)
        try:
            activate(root, launcher, text, unit, new_unit, current, previous, candidate, backup)
        except (ValueError, OSError, subprocess.CalledProcessError):
            if args.automatic:
                write_file(rejected, tag + "\n", 0o600)
            raise
        if args.launchd:
            update_schedule(base)
        if macos:
            rejected.unlink(missing_ok=True)
        print(f"Installed {tag}. Launch with {launcher}")
        if macos:
            print('For command names, add $HOME/.local/bin to PATH.')
            if args.launchd:
                print(f"Started {AGENT} on 127.0.0.1:{args.health_port}.")

    if args.dry_run:
        run()
        return
    if args.uninstall and not root.exists() and not launcher.exists() and not (unit and unit.exists()):
        return
    with install_lock(root):
        # Recheck ownership after waiting for another installer or uninstaller.
        managed_launcher(root, binary, macos)
        if unit:
            check_file(unit, unit_header)
        run()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit(f"fuzukue installer: {error}")
PY
