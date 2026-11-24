"""Offline checks for the server installer and its GitHub update scheduler."""

from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import patch
from urllib.error import HTTPError


def load_installer():
    installer = Path(__file__).resolve().parents[1] / "install-server.sh"
    source = installer.read_text(encoding="utf-8").split("<<'PY'\n", 1)[1].rsplit("\nPY\n", 1)[0]
    module = types.ModuleType("installer")
    with patch.object(sys, "argv", ["-", str(installer)]):
        exec(compile(source, str(installer), "exec"), module.__dict__)
    return module


SCRIPT = load_installer()
OLD = "v1.2.3-beta.1"
NEW = "v1.2.3-beta.2"
PLATFORM = "macos-aarch64"
ROW = {"tag_name": NEW, "draft": False, "prerelease": True,
       "assets": [{"name": name} for name in SCRIPT.mac_names(NEW, PLATFORM)]}


class UpdateTests(unittest.TestCase):
    def test_conditional_request_and_backoff(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            response = io.BytesIO(json.dumps([ROW]).encode())
            response.headers = {"ETag": '"response"'}
            with patch.object(SCRIPT.time, "time", return_value=10_000), patch.object(
                SCRIPT.urllib.request, "urlopen", return_value=response
            ) as get:
                self.assertEqual(SCRIPT.cached_releases(base), [ROW])
                request = get.call_args.args[0]
                self.assertNotIn("Authorization", request.headers)
                self.assertTrue(request.full_url.startswith("https://api.github.com/"))
            with patch.object(SCRIPT.time, "time", return_value=10_001), patch.object(
                SCRIPT.urllib.request, "urlopen"
            ) as get:
                self.assertEqual(SCRIPT.cached_releases(base), [ROW])
                get.assert_not_called()
            with patch.object(SCRIPT.time, "time", return_value=14_000), patch.object(
                SCRIPT.urllib.request, "urlopen", side_effect=HTTPError("", 304, "", {}, None)
            ) as get:
                self.assertEqual(SCRIPT.cached_releases(base), [ROW])
                self.assertEqual(get.call_args.args[0].get_header("If-none-match"), '"response"')
            with patch.object(SCRIPT.time, "time", return_value=18_000), patch.object(
                SCRIPT.urllib.request, "urlopen", side_effect=HTTPError("", 429, "", {"Retry-After": "1800"}, None)
            ):
                self.assertEqual(SCRIPT.cached_releases(base), [])
            with patch.object(SCRIPT.time, "time", return_value=19_700), patch.object(
                SCRIPT.urllib.request, "urlopen"
            ) as get:
                self.assertEqual(SCRIPT.cached_releases(base), [])
                get.assert_not_called()

    def test_scheduler_defers_client_until_matching_server(self):
        mac = types.SimpleNamespace(sysname="Darwin", machine="arm64")
        with tempfile.TemporaryDirectory() as tmp, patch.object(SCRIPT, "mac_base", return_value=Path(tmp)), (
            patch.object(SCRIPT.os, "uname", return_value=mac)
        ), patch.object(SCRIPT, "cached_releases", return_value=[ROW]) as catalog, (
            patch.object(SCRIPT, "read_link")
        ) as current, patch.object(SCRIPT.subprocess, "run") as install:
            old_server = f"{OLD}/secretaire"
            old_client = f"{OLD}/fuzukue"
            current.side_effect = [old_server, old_client, old_server]
            SCRIPT.scheduled_update()
            self.assertEqual(install.call_count, 1)
            self.assertNotIn("--client", install.call_args.args[0])

            install.reset_mock()
            current.side_effect = [old_server, old_client, f"{NEW}/secretaire"]
            SCRIPT.scheduled_update()
            self.assertEqual(install.call_count, 2)
            self.assertIn("--client", install.call_args.args[0])
            args = install.call_args.args[0]
            self.assertEqual(args[args.index("--version") + 1], NEW)

            install.reset_mock()
            install.side_effect = SCRIPT.subprocess.CalledProcessError(1, "installer")
            current.side_effect = [old_server, old_client]
            with self.assertRaises(SCRIPT.subprocess.CalledProcessError):
                SCRIPT.scheduled_update()
            self.assertEqual(install.call_count, 1)
            install.reset_mock()
            current.side_effect = [old_server, old_client]
            SCRIPT.scheduled_update()
            install.assert_not_called()

            catalog.reset_mock()
            (Path(tmp) / "auto-update-off").touch()
            SCRIPT.scheduled_update()
            catalog.assert_not_called()
            install.assert_not_called()


class AgentTests(unittest.TestCase):
    def test_finds_path_and_known_install_dir_without_showing_auth_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            claude = home / ".local/bin/claude"
            codex = home / "custom-path/codex"
            for executable, version, status in [(claude, "2.1.123", 0), (codex, "0.54.1", 1)]:
                executable.parent.mkdir(parents=True)
                executable.write_text(
                    "#!/bin/sh\n"
                    f'if [ "$1" = --version ]; then printf "%s\\n" "{version}"; exit 0; fi\n'
                    f'printf "%s\\n" "private probe output"; exit {status}\n',
                    encoding="utf-8",
                )
                executable.chmod(0o700)
            output = io.StringIO()
            with patch.dict(os.environ, HOME=tmp, PATH=str(codex.parent)), redirect_stdout(output):
                SCRIPT.check_agents()
                self.assertIn(str(claude.parent), SCRIPT.agent_search_path().split(os.pathsep))
                self.assertIn(str(codex.parent), SCRIPT.agent_search_path().split(os.pathsep))
            report = output.getvalue()
            self.assertIn("Claude Code: 2.1.123, signed in", report)
            self.assertIn("Codex CLI: 0.54.1, not signed in", report)
            self.assertIn("codex login", report)
            self.assertNotIn("private probe output", report)
            self.assertNotIn("Sign in to either CLI", report)

    def test_missing_cli_report_does_not_block_server(self):
        output = io.StringIO()
        with patch.object(SCRIPT.shutil, "which", return_value=None), redirect_stdout(output):
            SCRIPT.check_agents()
        report = output.getvalue()
        self.assertIn("https://claude.ai/install.sh", report)
        self.assertIn("https://chatgpt.com/codex/install.sh", report)
        self.assertIn("The server can start", report)

    def test_vendor_installer_requires_consent(self):
        for _, _, _, _, url, shell in SCRIPT.AGENT_CLI:
            with patch.object(SCRIPT, "confirmation", return_value=False), patch.object(
                SCRIPT.subprocess, "run"
            ) as run, redirect_stdout(io.StringIO()) as output:
                SCRIPT.offer_agent_install(url, shell, explicit=False)
                run.assert_not_called()
                self.assertIn(url, output.getvalue())
                SCRIPT.offer_agent_install(url, shell, explicit=True)
                self.assertEqual(run.call_count, 2)
                self.assertEqual(run.call_args_list[0].args[0][-1], url)
                self.assertEqual(run.call_args_list[1].args[0][0], shell)


if __name__ == "__main__":
    unittest.main()
