#!/usr/bin/env python3
"""Measure both fuzukue partner harnesses with the same Japanese requests."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
LANGUAGES = ("en", "ja")
SKILL = """---
name: japanese-paper-style
description: 日本語の論文の文体を読み、具体的に直せる箇所を示す
when: 論文の文体を見てほしいと頼まれたとき
---
原文を読み、論旨を追いにくい箇所と、意味を変えずに直せる理由を示す。
一つの箇所について原文、指摘、修正案を短く書く。原稿は変更しない。
"""


def capture_claude():
    """Record the partner's stream and forward it unchanged to the server."""
    binary = os.environ["FUZUKUE_AB_CLAUDE"]
    args = sys.argv[1:]
    if "--permission-prompt-tool" not in args or "--no-session-persistence" in args:
        os.execv(binary, [binary, *args])
    with open(os.environ["FUZUKUE_AB_STREAM"], "ab", buffering=0) as capture:
        with subprocess.Popen([binary, *args], stdout=subprocess.PIPE) as claude:
            for line in claude.stdout:
                capture.write(line)
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
            return claude.wait()


def copy_project(source, target, add_sample_skill):
    shutil.copytree(source, target, ignore=shutil.ignore_patterns(".git", ".fuzukue", "target"))
    if add_sample_skill:
        skill = target / "skills/japanese-paper-style/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text(SKILL, encoding="utf-8")


def size(s):
    # UTF-8 bytes are the tokenizer proxy. No encoder downloads are needed.
    return {"characters": len(s), "bytes": len(s.encode("utf-8"))}


def static_sizes(server, project, env):
    for lang in LANGUAGES:
        output = subprocess.check_output(
            [server, "harness-ab", "snapshot", project],
            env=dict(env, FUZUKUE_HARNESS_LANG=lang),
        )
        snapshot = json.loads(output)
        system = snapshot["system"]
        tools = json.dumps(snapshot["toolList"], ensure_ascii=False, separators=(",", ":"))
        print(json.dumps({
            "type": "static", "lang": lang, "system": size(system),
            "tool_list": size(tools), "total": size(system + tools),
        }), flush=True)


def metrics(stream):
    assistant = []
    tools = {}
    result = None
    for line in stream.read_bytes().splitlines() if stream.exists() else ():
        try:
            event = json.loads(line)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if event.get("type") == "result":
            result = event
            break
        if event.get("type") == "assistant":
            for item in event["message"]["content"]:
                if item.get("type") == "text":
                    assistant.append(item["text"])
                if item.get("type") == "tool_use":
                    tools[item["id"]] = item
        elif event.get("type") == "stream_event":
            block = event["event"].get("content_block", {})
            if block.get("type") == "tool_use":
                tools.setdefault(block["id"], block)
    reply = "\n".join(assistant) or (result or {}).get("result", "")
    usage = (result or {}).get("usage") or {}
    input_tokens = usage.get("input_tokens")
    if input_tokens is not None:
        input_tokens += usage.get("cache_read_input_tokens", 0) + usage.get("cache_creation_input_tokens", 0)
    return {
        "reply": reply,
        "reply_language": "ja" if re.search(r"[\u3041-\u3096\u30a1-\u30fa]", reply) else "undetected",
        "tone_markers": {marker: marker in reply for marker in ("だよ", "だね", "私")},
        "delegate_called": any(tool.get("name") == "mcp__fuzukue__delegate" for tool in tools.values()),
        "delegated_agents": [tool.get("input", {}).get("agent") for tool in tools.values()
                             if tool.get("name") == "mcp__fuzukue__delegate"],
        "input_tokens": input_tokens,
        "output_tokens": usage.get("output_tokens"),
        "usage": usage,
        "result_error": None if result is None else bool(result.get("is_error")),
    }


def live_case(server, project, request, stream, env, timeout):
    with subprocess.Popen(
        [server, "harness-ab", "turn", project],
        env=dict(env, FUZUKUE_AB_STREAM=str(stream)),
        stdin=subprocess.PIPE, start_new_session=True,
    ) as process:
        try:
            process.communicate(request.encode("utf-8"), timeout=timeout)
            status = process.returncode
        except subprocess.TimeoutExpired:
            status = "timeout"
        finally:
            # The streaming CLI and any worker processes belong to this run.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    return {"status": status, **metrics(stream)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="run authenticated Claude CLI turns")
    parser.add_argument("--server", type=Path, default=ROOT / "target/debug/secretaire")
    parser.add_argument("--project", type=Path, help="workspace to copy for each trial")
    parser.add_argument("--user-instructions", type=Path, default=ROOT / "AGENTS.md")
    parser.add_argument("--samples", type=Path, default=HERE / "samples.json")
    parser.add_argument("--timeout", type=float, default=300, help="seconds per live trial")
    args = parser.parse_args()

    server = args.server.resolve()
    source = (args.project or ROOT / "examples/lab").resolve()
    env = dict(os.environ, FUZUKUE_USER_INSTRUCTIONS=str(args.user_instructions.resolve()))
    with tempfile.TemporaryDirectory(prefix="fuzukue-harness-ab-", dir=Path.home() if args.live else None) as temp:
        temp = Path(temp)
        baseline = temp / "baseline"
        copy_project(source, baseline, args.project is None)
        static_sizes(server, baseline, env)
        if not args.live:
            return 0

        binary = shutil.which("claude")
        if binary is None:
            parser.error("claude is required for --live")
        wrappers = temp / "bin"
        wrappers.mkdir()
        (wrappers / "claude").symlink_to(Path(__file__).resolve())
        env.update(FUZUKUE_AB_CLAUDE=str(Path(binary).absolute()),
                   PATH=f"{wrappers}{os.pathsep}{env['PATH']}")
        samples = json.loads(args.samples.read_text(encoding="utf-8"))
        failed = False
        for index, (name, request) in enumerate(samples.items()):
            for lang in LANGUAGES:
                target = temp / f"{index}-{lang}"
                shutil.copytree(baseline, target)
                stream = temp / f"{index}-{lang}.jsonl"
                record = live_case(server, target, request, stream,
                                   dict(env, FUZUKUE_HARNESS_LANG=lang), args.timeout)
                print(json.dumps({"type": "live", "case": name, "lang": lang, **record},
                                 ensure_ascii=False), flush=True)
                failed |= record["status"] != 0 or record["result_error"] is not False
        return int(failed)


if __name__ == "__main__":
    sys.exit(capture_claude() if Path(sys.argv[0]).name == "claude" else main())
