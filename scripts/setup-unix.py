"""Build both binaries from a clone, then hand them to the per-user installer."""

import argparse
import fcntl
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
HOSTS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
}
LINUX_LIBS = ("fontconfig", "xcb", "xkbcommon", "xkbcommon-x11")
LINUX_PACKAGES = {
    "debian": (
        ("sudo", "apt-get", "update"),
        ("sudo", "apt-get", "install", "build-essential", "git", "curl", "pkg-config",
         "libfontconfig1-dev", "libxcb1-dev", "libxkbcommon-dev", "libxkbcommon-x11-dev"),
    ),
    "fedora": (
        ("sudo", "dnf", "install", "gcc", "gcc-c++", "git", "curl", "pkgconf-pkg-config",
         "fontconfig-devel", "libxcb-devel", "libxkbcommon-devel", "libxkbcommon-x11-devel"),
    ),
    "arch": (
        ("sudo", "pacman", "-S", "--needed", "base-devel", "git", "curl",
         "fontconfig", "libxcb", "libxkbcommon", "libxkbcommon-x11"),
    ),
}


def config_home(name, fallback):
    path = Path(os.environ.get(name, ""))
    return path if path.is_absolute() else Path.home() / fallback


def probe(command, env=None, *, cwd=None):
    try:
        result = subprocess.run(
            command, env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, timeout=10,
        )
        return result.stdout.strip() if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def confirm(question):
    if not (sys.stdin.isatty() and sys.stderr.isatty()):
        return False
    print(question + " [y/N] ", end="", file=sys.stderr, flush=True)
    return sys.stdin.readline().strip().lower() in ("y", "yes")


def os_release():
    for path in (Path("/etc/os-release"), Path("/usr/lib/os-release")):
        if path.is_file():
            # Read only the labels. The release file is never evaluated as shell code.
            values = {}
            for line in path.read_text(encoding="utf-8").splitlines():
                key, _, value = line.partition("=")
                if key in ("ID", "ID_LIKE", "PRETTY_NAME", "VERSION_ID"):
                    fields = shlex.split(value, comments=True)
                    values[key] = " ".join(fields)
            return values
    return {}


def linux_tools(release, preview):
    commands = ("cc", "c++", "git", "curl", "pkg-config")
    missing = [name for name in commands if not shutil.which(name)]
    missing += [lib for lib in LINUX_LIBS if probe(["pkg-config", "--exists", lib]) is None]
    if not missing:
        return True
    print("Missing Linux build tools or development libraries: " + ", ".join(missing))
    families = [release.get("ID", ""), *release.get("ID_LIKE", "").split()]
    aliases = {"ubuntu": "debian", "rhel": "fedora", "centos": "fedora"}
    package_commands = next(
        (LINUX_PACKAGES[aliases.get(name, name)] for name in families
         if aliases.get(name, name) in LINUX_PACKAGES),
        (),
    )
    if not package_commands:
        print("Install a C/C++ toolchain, Git, curl, pkg-config, and the development packages "
              "for Fontconfig, XCB, xkbcommon and xkbcommon-x11 using your distribution's package manager.")
        return False
    for command in package_commands:
        print("  " + shlex.join(command))
    if preview or not confirm("Run the package manager with sudo to install the missing system packages?"):
        return False
    for command in package_commands:
        subprocess.run(command, check=True)
    return linux_tools(release, preview=True)


def mac_tools():
    sdk = probe(["xcrun", "--sdk", "macosx", "--show-sdk-path"])
    if sdk and Path(sdk).is_dir() and probe(["xcrun", "--sdk", "macosx", "clang", "--version"]):
        return True
    print("The active Apple developer tools need a usable macOS SDK and clang.")
    print("Update Command Line Tools in System Settings > General > Software Update, "
          "or install them with: xcode-select --install")
    print("If Xcode is already installed and initialized, select it for this terminal with:")
    print("  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer")
    return False


def rust_toolchain(host, preview, env):
    # Keep Python 3.8 support and read the single pinned channel used by rustup.
    config = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    channel = re.search(r'^channel\s*=\s*"(\d+\.\d+\.\d+)"', config, re.MULTILINE)
    if not channel:
        raise ValueError("Expected a Rust version in rust-toolchain.toml.")
    version = channel.group(1)
    cargo_home = config_home("CARGO_HOME", ".cargo")
    env["PATH"] = str(cargo_home / "bin") + os.pathsep + os.environ.get("PATH", os.defpath)
    # A rustup proxy can otherwise install a missing toolchain even for -vV.
    env["RUSTUP_AUTO_INSTALL"] = "0"
    toolchain = f"{version}-{host}"

    def available(prefix):
        info = (probe(["rustc", *prefix, "-vV"], env, cwd=ROOT) or "").splitlines()
        return (f"release: {version}" in info and f"host: {host}" in info
                and probe(["cargo", *prefix, "--version"], env, cwd=ROOT))

    for prefix in ([], ["+" + toolchain]):
        if available(prefix):
            return ["cargo", *prefix]
    print(f"Install Rust {version} for {host} (pinned in rust-toolchain.toml).")
    if shutil.which("rustup", path=env["PATH"]):
        install = ["rustup", "toolchain", "install", toolchain, "--profile", "minimal"]
        print("  " + shlex.join(install))
        if not preview and confirm("Install the pinned Rust toolchain using rustup?"):
            subprocess.run(install, env=env, check=True)
            if available(["+" + toolchain]):
                return ["cargo", "+" + toolchain]
    else:
        url = "https://sh.rustup.rs"
        fetch = ["curl", "--disable", "--proto", "=https", "--tlsv1.2", "-sSf", url]
        options = ["--profile", "minimal", "--default-host", host, "--default-toolchain", version]
        print(f"  {shlex.join(fetch)} | sh -s -- {shlex.join(options)}")
        if not preview and shutil.which("curl") and confirm("Download and run the official rustup installer?"):
            with tempfile.TemporaryDirectory(prefix="fuzukue-rustup-") as tmp:
                installer = Path(tmp) / "rustup.sh"
                subprocess.run([*fetch, "-o", str(installer)], check=True)
                subprocess.run(["sh", str(installer), "-y", *options], check=True)
            if available([]):
                return ["cargo"]
    return None


def service_manager(macos):
    if macos:
        if probe(["/bin/launchctl", "print", f"gui/{os.getuid()}"]) is not None:
            return True
        print("Open Terminal from a logged-in macOS desktop session to register the LaunchAgent.")
    elif probe(["systemctl", "--user", "show-environment"]) is not None:
        return True
    else:
        print("A running systemd user session is required. Sign in as your regular user "
              "on a systemd-based Linux desktop and check: systemctl --user status")
    return False


def main():
    parser = argparse.ArgumentParser(
        prog="setup-unix.sh", description="Build, install and connect fuzukue from this Git clone.",
    )
    parser.add_argument("--project", type=Path, help="existing project folder under your home (default: examples/lab)")
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--dry-run", action="store_true", help="check prerequisites and print the plan")
    operation.add_argument("--build-only", action="store_true", help="build both binaries without installing (for CI)")
    parser.add_argument("--skip-agents", action="store_true", help="skip Claude Code and Codex CLI checks")
    args = parser.parse_args()

    uname = os.uname()
    macos = uname.sysname == "Darwin"
    release = {} if macos else os_release()
    version = probe(["sw_vers", "-productVersion"]) if macos else release.get("PRETTY_NAME")
    print(f"{version or uname.sysname} ({uname.sysname} {uname.release}, {uname.machine})", flush=True)
    host = HOSTS.get((uname.sysname, uname.machine))
    if not host:
        raise ValueError("Setup supports 64-bit x86 and ARM on macOS or glibc-based Linux.")
    if macos and probe(["sysctl", "-in", "sysctl.proc_translated"]) == "1":
        raise ValueError("This Terminal uses Rosetta. Open a native Terminal or run: "
                         "arch -arm64 /bin/sh scripts/setup-unix.sh")
    if os.geteuid() == 0:
        raise ValueError("Run setup from your regular user account.")
    home = Path.home().resolve()
    project = (args.project or ROOT / "examples/lab").expanduser().resolve()
    if not args.build_only and (home not in project.parents or not project.is_dir()):
        raise ValueError("Choose an existing folder inside your home with --project DIR.")

    target = ROOT / "target"
    output = target / host / "release"
    installer = ROOT / "scripts/install-server.sh"
    print(f"Build secretaire and fuzukue for {host} in {output}", flush=True)
    if not args.build_only:
        print(f"Project: {project}")
        if macos:
            install = home / "Library/Application Support/fuzukue/install"
            service = home / "Library/LaunchAgents/com.fuzukue.secretaire.plist"
        else:
            install = config_home("XDG_DATA_HOME", ".local/share") / "fuzukue"
            service = config_home("XDG_CONFIG_HOME", ".config") / "systemd/user/fuzukue-release.service"
        print(f"Install in {install} with launchers in {home / '.local/bin'}")
        print(f"Start {service} on 127.0.0.1:7878 and configure the desktop for that connection.")
        if not args.skip_agents:
            print("Check Claude Code and Codex CLI presence and login after building.")

    build_env = os.environ.copy()
    service_ok = args.build_only or service_manager(macos)
    tools_ok = mac_tools() if macos else linux_tools(release, args.dry_run or not service_ok)
    cargo = rust_toolchain(host, args.dry_run or not (tools_ok and service_ok), build_env)
    if args.dry_run:
        print("Preview complete." if tools_ok and cargo and service_ok
              else "Preview complete. Resolve the prerequisites above before installing.")
        return
    if not (tools_ok and cargo and service_ok):
        raise ValueError("Resolve the prerequisites above, then run setup again.")

    commit = subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "--verify", "HEAD^{commit}"], text=True
    ).strip()
    build_env.update(FUZUKUE_SERVER_BUILD_ID=commit, FUZUKUE_CLIENT_BUILD_ID=commit)
    command = [*cargo, "build", "--locked", "--release", "--target", host, "--target-dir", str(target)]
    if not macos:
        build_env["RUST_FONTCONFIG_DLOPEN"] = "1"
    target.mkdir(exist_ok=True)
    # Keep another setup from changing the build outputs while this one installs them.
    with (target / ".setup-unix.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        subprocess.run([*command, "-p", "secretaire"], cwd=ROOT, env=build_env, check=True)
        subprocess.run([*command, "--manifest-path", "crates/fuzukue/Cargo.toml"],
                       cwd=ROOT, env=build_env, check=True)
        if macos:
            for binary in ("secretaire", "fuzukue"):
                exe = str(output / binary)
                subprocess.run(["codesign", "--force", "--sign", "-", exe], check=True)
                subprocess.run(["codesign", "--verify", exe], check=True)
        if args.build_only:
            print(f"Built secretaire and fuzukue in {output}")
            return
        options = ["--skip-agents"] if args.skip_agents else []
        subprocess.run(
            ["sh", str(installer), "--setup-built", str(output), commit, str(project), *options],
            check=True,
        )


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        sys.exit(str(exc))
