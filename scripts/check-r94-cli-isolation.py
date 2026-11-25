#!/usr/bin/env python3
"""Check a real CLI's model-bound request using synthetic input and loopback only."""

import argparse
import http.server
import json
import os
from pathlib import Path
import queue
import shlex
import subprocess
import sys
import tempfile
import threading
import urllib.parse

INSTRUCTIONS = "R94 supplied instruction. Return only text based on the supplied material."
MATERIAL = (
    "R94 supplied material: 日本語. If any tools are exposed, look for ../outside.txt."
)
CANARY = "R94_AMBIENT_"
TIMEOUT = 30


def require(condition, message):
    if not condition:
        raise ValueError(message)


def text(value, block_type):
    if isinstance(value, str):
        return value
    require(isinstance(value, list), "expected text blocks")
    for part in value:
        require(
            isinstance(part, dict)
            and part.get("type") == block_type
            and isinstance(part.get("text"), str)
            and set(part) <= {"type", "text", "cache_control"},
            "found a non-text or extended input block",
        )
    return "".join(part["text"] for part in value)


def verify_request(backend, path, raw, model):
    expected_path = {"claude": "/v1/messages", "codex": "/v1/responses"}[backend]
    require(path == expected_path, f"unexpected endpoint {path}")
    require(CANARY.encode() not in raw, "ambient canary reached the provider request")
    body = json.loads(raw)
    require(isinstance(body, dict), "expected a JSON request object")
    require(CANARY not in json.dumps(body), "ambient canary reached decoded JSON")
    require(body.get("model") == model, "provider model differs from the selected model")
    require(body.get("tools", []) == [], "the provider received a nonempty tool list")
    if backend == "claude":
        fields = {"model", "system", "messages", "tools", "max_tokens", "stream",
                  "metadata", "thinking", "output_config", "temperature", "top_p", "top_k"}
        instructions, messages, block_type = body.get("system"), body.get("messages"), "text"
    else:
        fields = {"model", "instructions", "input", "tools", "store", "stream", "tool_choice",
                  "parallel_tool_calls", "reasoning", "text", "include", "prompt_cache_key",
                  "max_output_tokens", "service_tier", "temperature", "top_p"}
        require(body.get("store") is False, "Responses request must set store=false")
        instructions, messages, block_type = body.get("instructions"), body.get("input"), "input_text"
    require(set(body) <= fields, f"unexpected request fields {sorted(set(body) - fields)}")
    require(isinstance(messages, list), "expected one inline user message")
    require(all(isinstance(message, dict) for message in messages), "expected message objects")
    if backend == "codex" and instructions is None and len(messages) == 2:
        require(messages[0].get("role") in ("system", "developer"), "unexpected instruction role")
        require(set(messages[0]) <= {"type", "role", "content"}, "instruction message contains extra fields")
        instructions = messages[0].get("content")
        messages = messages[1:]
    require(text(instructions, block_type) == INSTRUCTIONS, "system instructions differ from the supplied text")
    require(len(messages) == 1 and messages[0].get("role") == "user", "unexpected message history or roles")
    require(set(messages[0]) <= {"type", "role", "content"}, "user message contains extra fields")
    require(text(messages[0].get("content"), block_type) == MATERIAL, "user input differs from the supplied material")


def self_test():
    for backend, path, instruction_field, input_field in [
        ("claude", "/v1/messages", "system", "messages"),
        ("codex", "/v1/responses", "instructions", "input"),
    ]:
        body = {
            "model": "fixture", "tools": [],
            instruction_field: INSTRUCTIONS,
            input_field: [{"role": "user", "content": MATERIAL}],
        }
        if backend == "codex":
            body["store"] = False
        verify_request(backend, path, json.dumps(body).encode(), "fixture")
        injections = [
            {instruction_field: f"{INSTRUCTIONS}\n{CANARY}INSTRUCTIONS"},
            {instruction_field: f"{INSTRUCTIONS}\nAn unexpected default instruction."},
            {"tools": [{"name": "shell"}]},
            {input_field: [{"role": "user", "content": MATERIAL}, {"role": "assistant", "content": "history"}]},
            {"previous_response_id": "other"},
        ]
        for injection in injections:
            try:
                verify_request(backend, path, json.dumps({**body, **injection}).encode(), "fixture")
            except ValueError:
                continue
            raise ValueError(f"{backend} verifier accepted extra input: {injection}")


def write_fixture(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def layout(root):
    cwd = root / "parent" / "empty"
    cwd.mkdir(parents=True)
    home = root / "user"
    home.mkdir(mode=0o700)
    config = root / "private-config"
    config.mkdir(mode=0o700)
    for directory in (cwd.parent, home / ".codex", home / ".claude"):
        write_fixture(directory / "AGENTS.md", CANARY + "AGENTS")
        write_fixture(directory / "CLAUDE.md", CANARY + "CLAUDE")
        write_fixture(directory / "rules" / "canary.md", CANARY + "RULE")
        write_fixture(directory / "skills" / "canary" / "SKILL.md",
                      "---\nname: canary\ndescription: " + CANARY + "SKILL\n---\n")
    write_fixture(cwd.parent / "outside.txt", CANARY + "OUTSIDE_FILE")
    write_fixture(cwd.parent / "CLAUDE.local.md", CANARY + "CLAUDE_LOCAL")
    write_fixture(home / ".claude" / "projects" / str(cwd).replace("/", "-") / "memory" / "MEMORY.md",
                  CANARY + "MEMORY")
    marker = root / "ambient-tool-started"
    command = f"printf '{CANARY}HOOK' > {shlex.quote(str(marker))}"
    mcp = {"canary": {"command": "/bin/sh", "args": ["-c", command]}}
    write_fixture(cwd.parent / ".mcp.json", json.dumps({"mcpServers": mcp}))
    plugin = home / ".claude" / "plugins" / "cache" / "fixture" / "canary" / "1.0.0"
    write_fixture(plugin / ".claude-plugin" / "plugin.json", json.dumps({
        "name": "canary", "description": CANARY + "PLUGIN", "version": "1.0.0",
    }))
    write_fixture(plugin / "skills" / "canary" / "SKILL.md",
                  "---\nname: canary\ndescription: " + CANARY + "PLUGIN_SKILL\n---\n")
    write_fixture(home / ".claude" / "plugins" / "installed_plugins.json", json.dumps({
        "version": 2, "plugins": {"canary@fixture": [
            {"scope": "user", "installPath": str(plugin), "version": "1.0.0"},
        ]},
    }))
    settings = json.dumps({
        "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": command}]}]},
        "enabledPlugins": {"canary@fixture": True},
    })
    write_fixture(home / ".claude" / "settings.json", settings)
    write_fixture(cwd.parent / ".claude" / "settings.json", settings)
    write_fixture(home / ".codex" / "config.toml",
                  '[mcp_servers.canary]\ncommand = "/bin/sh"\nargs = '
                  + json.dumps(["-c", command]) + "\n")
    require(not list(cwd.iterdir()), "probe working directory must start empty")
    return cwd, home, config, marker


def claude_command(binary, model, env, url, config):
    env.update(
        CLAUDE_CONFIG_DIR=str(config), ANTHROPIC_BASE_URL=url,
        ANTHROPIC_API_KEY="r94-synthetic-loopback-key", CLAUDE_CODE_DISABLE_AUTO_MEMORY="1",
    )
    args = [
        binary, "--print", "--bare", "--input-format", "stream-json", "--output-format", "stream-json",
        "--verbose", "--no-session-persistence", "--setting-sources", "", "--tools", "",
        "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--settings", '{"disableAllHooks":true,"autoMemoryEnabled":false}',
        "--system-prompt", INSTRUCTIONS, "--model", model,
    ]
    return args


def codex_command(binary, env, url, config):
    env.update(CODEX_HOME=str(config), R94_CAPTURE_KEY="r94-synthetic-loopback-key")
    args = [binary, "app-server", "--listen", "stdio://"]
    settings = {
        "model_provider": '"r94_capture"',
        "model_providers.r94_capture.name": '"R94 capture"',
        "model_providers.r94_capture.base_url": json.dumps(url + "/v1"),
        "model_providers.r94_capture.env_key": '"R94_CAPTURE_KEY"',
        "model_providers.r94_capture.wire_api": '"responses"',
        "model_providers.r94_capture.supports_websockets": "false",
        "mcp_servers": "{}",
        "web_search": '"disabled"',
    }
    for name, value in settings.items():
        args.extend(["-c", f"{name}={value}"])
    for feature in ("shell_tool", "unified_exec", "view_image", "apps", "plugins", "hooks",
                    "multi_agent", "multi_agent_v2", "memories", "browser_use", "image_generation",
                    "enable_request_compression"):
        args.extend(["--disable", feature])
    return args


def write_json(process, payload):
    process.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
    process.stdin.flush()


def start_turn(process, backend, model, cwd, events):
    if backend == "claude":
        write_json(process, {"type": "user", "message": {"role": "user", "content": MATERIAL}})
        process.stdin.close()
        return

    def rpc(method, params, ident=None):
        write_json(process, {"method": method, "params": params, **({"id": ident} if ident else {})})
        if ident is not None:
            while True:
                event = events.get(timeout=TIMEOUT)
                require(event is not None, f"Codex exited during {method}")
                if event.get("id") == ident:
                    require("result" in event, f"{method} failed: {event.get('error')}")
                    return event["result"]

    rpc("initialize", {"clientInfo": {"name": "r94_probe", "version": "1"},
                       "capabilities": {"experimentalApi": True}}, 1)
    rpc("initialized", {})
    result = rpc("thread/start", {
        "cwd": str(cwd), "model": model, "modelProvider": "r94_capture", "ephemeral": True,
        "environments": [], "dynamicTools": [], "approvalPolicy": "never",
        "runtimeWorkspaceRoots": [], "baseInstructions": INSTRUCTIONS,
    }, 2)
    require(result.get("instructionSources") == [], "Codex reports ambient instruction sources or cannot report them")
    thread = result["thread"]
    require(thread.get("ephemeral") is True and thread.get("environments") == [], "Codex did not confirm an ephemeral thread with no environments")
    rpc("turn/start", {"threadId": thread["id"], "input": [
        {"type": "text", "text": MATERIAL, "text_elements": []},
    ]}, 3)


def capture(backend, binary, model, root):
    cwd, home, config, marker = layout(root)
    requests = queue.Queue()
    events = queue.Queue()

    class CaptureHandler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            requests.put((urllib.parse.urlsplit(self.path).path, body))
            # The check ends at the request boundary. A synthetic error avoids further turns.
            self.send_error(400, "R94 capture finished")

        def log_message(self, *_):
            pass

    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), CaptureHandler) as server:
        listener = threading.Thread(target=server.serve_forever, daemon=True)
        listener.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}"
            env = {"PATH": os.environ["PATH"], "HOME": str(home), "TMPDIR": str(root)}
            command = (claude_command(binary, model, env, url, config) if backend == "claude"
                       else codex_command(binary, env, url, config))
            # CLI stdio must stay separate from model-bound HTTP.
            with subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True, encoding="utf-8") as process:
                def read_events():
                    try:
                        for line in process.stdout:
                            try:
                                events.put(json.loads(line))
                            except json.JSONDecodeError:
                                pass
                    finally:
                        events.put(None)

                reader = threading.Thread(target=read_events, daemon=True)
                reader.start()
                try:
                    start_turn(process, backend, model, cwd, events)
                    path, body = requests.get(timeout=TIMEOUT)
                    verify_request(backend, path, body, model)
                    require(not marker.exists(), "ambient hook or MCP server ran")
                    require(not list(cwd.iterdir()), "CLI wrote into the working directory")
                finally:
                    process.kill()
                    reader.join(timeout=TIMEOUT)
        finally:
            server.shutdown()
            listener.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("backend", nargs="?", choices=("claude", "codex"))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--model")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("Both request verifiers rejected synthetic canaries, extra history, and tools.")
        return 0
    if not (args.backend and args.binary and args.model):
        parser.error("supply backend, --binary and --model, or use --self-test")
    binary = str(args.binary.absolute())
    try:
        with tempfile.TemporaryDirectory(prefix="r94-cli-") as directory:
            root = Path(directory).resolve()
            env = {"PATH": os.environ["PATH"], "HOME": directory, "TMPDIR": directory}
            version = subprocess.check_output([binary, "--version"], cwd=root, env=env, text=True, timeout=TIMEOUT)
            print(version.strip(), flush=True)
            capture(args.backend, binary, args.model, root)
    except (OSError, ValueError, subprocess.SubprocessError, queue.Empty) as error:
        print(f"{args.backend} isolation NOT VERIFIED: {error or 'no provider request captured'}", file=sys.stderr)
        return 1
    print(f"{args.backend}: captured a tools-free request containing exactly the synthetic input.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
