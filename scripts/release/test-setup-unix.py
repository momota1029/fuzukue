"""Offline source-setup fixtures for both service managers."""

from contextlib import redirect_stdout
import importlib.util
import io
import os
from pathlib import Path
import plistlib
import runpy
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


HERE = Path(__file__).resolve().parent
INSTALLER = runpy.run_path(str(HERE / "test-macos-update.py"))["SCRIPT"]
spec = importlib.util.spec_from_file_location("setup_unix", HERE.parent / "setup-unix.py")
SETUP = importlib.util.module_from_spec(spec)
sys.dont_write_bytecode = True
spec.loader.exec_module(SETUP)


class SourceSetup(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="fuzukue-setup-test-")
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name).resolve() / "Test Home"
        self.project = self.home / "Project & Notes"
        self.project.mkdir(parents=True)
        self.build = self.home / "build output"
        self.build.mkdir()
        self.log = self.home / "fixture.log"
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_DATA_HOME": str(self.home / "xdg"),
            "FIXTURE_LOG": str(self.log),
            "FIXTURE_CLIENT_ERROR": "0",
        }).start()
        self.output = io.StringIO()
        redirect = redirect_stdout(self.output)
        redirect.__enter__()
        self.addCleanup(redirect.__exit__, None, None, None)
        for binary, text in (
            ("secretaire", "#!/bin/sh\nexit 0\n"),
            ("fuzukue", "#!/bin/sh\nset -eu\ntest \"$1\" = --configure-external\n"
             "test \"$FIXTURE_CLIENT_ERROR\" = 0\nprintf 'configured\\n' >> \"$FIXTURE_LOG\"\n"),
        ):
            exe = self.build / binary
            exe.write_text(text, encoding="utf-8")
            exe.chmod(0o755)

    def setup(self, commit="a" * 40):
        INSTALLER.setup_built([str(self.build), commit, str(self.project), "--skip-agents"])

    def test_preview_identifies_native_mac_targets_without_writing_files(self):
        patch.object(SETUP, "mac_tools", return_value=True).start()
        patch.object(SETUP, "service_manager", return_value=True).start()
        rust = patch.object(SETUP, "rust_toolchain", return_value=["cargo"]).start()
        patch.object(SETUP, "probe", side_effect=lambda command: "15.7" if command[0] == "sw_vers" else None).start()
        for machine, target in (("arm64", "aarch64-apple-darwin"), ("x86_64", "x86_64-apple-darwin")):
            with self.subTest(machine=machine), patch.object(
                SETUP.os, "uname", return_value=SimpleNamespace(sysname="Darwin", machine=machine, release="24.6.0")
            ), patch.object(sys, "argv", ["setup-unix.py", "--dry-run", "--project", str(self.project)]):
                SETUP.main()
                self.assertIn(f"15.7 (Darwin 24.6.0, {machine})", self.output.getvalue())
                self.assertIn(target, self.output.getvalue())
                self.assertEqual(rust.call_args.args[:2], (target, True))
        self.assertFalse((self.home / "Library").exists())
        self.assertFalse((self.home / ".local").exists())
        self.assertFalse(self.log.exists())

    def test_rust_preview_cannot_trigger_rustup_install(self):
        with patch.object(SETUP, "probe", return_value=None) as probe, patch.object(
            SETUP.shutil, "which", return_value="rustup"
        ), patch.object(SETUP, "confirm") as confirm:
            self.assertIsNone(SETUP.rust_toolchain("x86_64-unknown-linux-gnu", True, {}))
        confirm.assert_not_called()
        self.assertTrue(all(call.args[1]["RUSTUP_AUTO_INSTALL"] == "0" for call in probe.call_args_list))

    def test_linux_preview_suggests_packages_without_running_sudo(self):
        with patch.object(SETUP.shutil, "which", return_value=None), patch.object(
            SETUP, "probe", return_value=None
        ), patch.object(SETUP, "confirm") as confirm, patch.object(SETUP.subprocess, "run") as run:
            self.assertFalse(SETUP.linux_tools({"ID": "ubuntu", "ID_LIKE": "debian"}, preview=True))
        self.assertIn("sudo apt-get install build-essential", self.output.getvalue())
        self.assertIn("libxkbcommon-x11-dev", self.output.getvalue())
        confirm.assert_not_called()
        run.assert_not_called()

    def test_systemd_install_update_idempotence_and_unmanaged_launcher(self):
        patch.object(INSTALLER.os, "uname", return_value=SimpleNamespace(sysname="Linux")).start()
        root = self.home / "xdg/fuzukue"
        unit = self.home / "config/systemd/user" / INSTALLER.UNIT
        launcher = self.home / ".local/bin/fuzukue"
        active = enabled = False
        commands = []

        def systemctl(action, *args, **kwargs):
            nonlocal active, enabled
            commands.append(action)
            if action == "is-active":
                return SimpleNamespace(returncode=int(not active))
            if action == "is-enabled":
                return SimpleNamespace(returncode=int(not enabled))
            if action in ("start", "stop"):
                active = action == "start"
            if action in ("enable", "disable"):
                enabled = action == "enable"
            return SimpleNamespace(returncode=0)

        with patch.object(INSTALLER, "systemctl", side_effect=systemctl), patch.object(
            INSTALLER, "check_health"
        ) as health:
            self.setup()
            first = os.readlink(root / "current")
            self.assertTrue(active and enabled)
            self.assertIn(f'WorkingDirectory="{self.project}"', unit.read_text(encoding="utf-8"))
            self.assertIn('"FUZUKUE_ADDR=127.0.0.1:7878"', unit.read_text(encoding="utf-8"))
            health.assert_called_once_with(7878, root / first)
            self.assertEqual(self.log.read_text(encoding="utf-8"), "configured\n")
            self.setup()
            self.assertEqual(os.readlink(root / "current"), first)
            self.assertFalse((root / "previous").is_symlink())
            self.assertNotIn("stop", commands)
            self.setup(commit="b" * 40)
            self.assertEqual(os.readlink(root / "previous"), first)
            self.assertTrue(os.readlink(root / "current").startswith("local-" + "b" * 40))
            self.assertIn("stop", commands)
            launcher.write_text("#!/bin/sh\n# user launcher\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Unmanaged file"):
                self.setup()

    def test_launchd_reuses_release_layout_and_rolls_back_if_client_setup_fails(self):
        patch.object(INSTALLER.os, "uname", return_value=SimpleNamespace(sysname="Darwin", machine="arm64")).start()
        base = INSTALLER.mac_base()
        unit = INSTALLER.agent_path(INSTALLER.AGENT)
        active = False

        def launchctl(action, *args):
            nonlocal active
            active = action == "bootstrap"

        def agent_status(path, domain, label=INSTALLER.AGENT):
            return "pid = 123" if active else None

        def check_health(path, domain, port):
            self.assertTrue(active)
            self.assertEqual(path, unit)
            self.assertEqual(port, 7878)

        for kind, binary in (("server", "secretaire"), ("client", "fuzukue")):
            root = base / kind
            root.mkdir(parents=True)
            target = INSTALLER.install_version(
                root, "v0.0.0-beta.1", binary, "1" * 64, lambda dest: dest.touch(mode=0o755),
            )
            INSTALLER.set_link(root, "current", target)
            launcher, _ = INSTALLER.managed_launcher(root, binary, True)
            header = f"# Managed by fuzukue release installer ({binary})\n"
            INSTALLER.write_file(launcher, INSTALLER.launcher_text(header, root, binary, True, legacy=True), 0o755)

        with patch.object(INSTALLER, "launch_domain", return_value="gui/1000"), patch.object(
            INSTALLER, "agent_status", side_effect=agent_status
        ), patch.object(INSTALLER, "launchctl", side_effect=launchctl), patch.object(
            INSTALLER, "check_agent_health", side_effect=check_health
        ):
            self.setup()
            plist = unit.read_bytes()
            config = plistlib.loads(plist)
            self.assertEqual(config["WorkingDirectory"], str(self.project))
            self.assertEqual(config["EnvironmentVariables"]["FUZUKUE_ADDR"], "127.0.0.1:7878")
            self.assertNotIn("FUZUKUE_MACOS_PACKAGE", config["EnvironmentVariables"])
            server, client = (base / "server/current", base / "client/current")
            first = (os.readlink(server), os.readlink(client))
            launcher = self.home / ".local/bin/fuzukue"
            self.assertEqual(subprocess.check_output([str(launcher), "--version"], text=True).strip(),
                             f"fuzukue {Path(first[1]).parent}")
            with patch.dict(os.environ, {"FIXTURE_CLIENT_ERROR": "1"}):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.setup(commit="b" * 40)
            self.assertEqual((os.readlink(server), os.readlink(client)), first)
            self.assertTrue(active)
            self.assertEqual(unit.read_bytes(), plist)
            self.assertEqual(os.readlink(server.with_name("previous")), "v0.0.0-beta.1/secretaire")

            # A verified release can take over the source installation using
            # the same launchers and LaunchAgent.
            tag = "v0.0.0-beta.2"
            with patch.object(INSTALLER, "download", return_value=(tag, "c" * 40, None, "2" * 64)), patch.object(
                INSTALLER, "extract", side_effect=lambda archive, binary, dest: shutil.copy2(self.build / binary, dest)
            ), patch.object(INSTALLER, "update_schedule") as schedule:
                options = ["--version", tag]
                for flags in (["--launchd", str(self.project), "--skip-agents"], ["--client"]):
                    with patch.object(sys, "argv", ["install-server.sh", *flags, *options]):
                        INSTALLER.main()
            schedule.assert_called_once_with(base)
            self.assertEqual(os.readlink(server), f"{tag}/secretaire")
            self.assertEqual(os.readlink(client), f"{tag}/fuzukue")
            self.assertEqual(plistlib.loads(unit.read_bytes())["EnvironmentVariables"]["FUZUKUE_MACOS_PACKAGE"], "1")


if __name__ == "__main__":
    unittest.main()
