#!/usr/bin/env python3
"""Deterministic stdio language server for tests and the editor shot."""

import json
import os
import sys
from pathlib import Path

TRACE = Path(sys.argv[1]) if len(sys.argv) > 1 else None
DOCUMENTS = {}
DOC_RANGE = {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}}
TARGET = {"uri": (Path.cwd() / "helper.rs").as_uri(), "range": DOC_RANGE}
OUTSIDE = {"uri": "file:///etc/passwd", "range": DOC_RANGE}


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        name, value = line.split(b":", 1)
        headers[name.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))


def send(**message):
    body = json.dumps({"jsonrpc": "2.0", **message}, ensure_ascii=False).encode()
    # Deliberately split header and body across writes.
    for data in (b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n",
                 f"Content-Length: {len(body)}\r\n\r\n".encode(), body):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()


def diagnostics(uri, version, text):
    send(method="textDocument/publishDiagnostics", params={
        "uri": uri, "version": version,
        "diagnostics": [{"range": {
            "start": {"line": 2, "character": 22}, "end": {"line": 2, "character": 34},
        }, "severity": 1, "message": f'Expected u32, found &str — {len(text)} chars', "source": "fake-lsp"}],
    })


def handle(message):
    method = message.get("method")
    params = message.get("params") or {}
    document = params.get("textDocument", {})
    uri = document.get("uri")
    if TRACE is not None:
        with TRACE.open("a") as trace:
            trace.write(json.dumps(message, ensure_ascii=False) + "\n")

    if method == "exit":
        return False
    if method == "textDocument/didOpen":
        DOCUMENTS[uri] = document["text"]
        diagnostics(uri, document["version"], DOCUMENTS[uri])
    if method == "textDocument/didChange":
        edit = params["contentChanges"][0]
        assert "range" in edit
        diagnostics(uri, document["version"] - 1, "STALE")
        DOCUMENTS[uri] = edit["text"]
        diagnostics(uri, document["version"], DOCUMENTS[uri])
    if method == "textDocument/didSave":
        assert params["text"] == DOCUMENTS[uri]
    if method == "textDocument/didClose":
        DOCUMENTS.pop(uri, None)
    if "id" not in message or method is None:
        return True

    result = None
    item = {"name": "helper", "kind": 12, "selectionRange": DOC_RANGE, **TARGET}
    if method == "initialize":
        root = params["rootUri"]
        assert root == Path.cwd().as_uri()
        send(id="config", method="workspace/configuration", params={"items": [{}]})
        config = read()
        assert config["id"] == "config" and config["result"] == [None]
        result = {"capabilities": {"textDocumentSync": {
            "openClose": True, "change": 2, "save": {"includeText": True},
        }}}
    elif method == "textDocument/hover":
        if params["position"]["character"] == 98:
            os._exit(9)
        if uri not in DOCUMENTS:
            send(id=message["id"], error={"code": -32602, "message": "file is closed"})
            return True
        result = {"contents": {"kind": "markdown", "value": "```rust\nfn helper() -> u32\n```\n\nReturns the sample value."}}
    elif method == "textDocument/completion":
        result = {"isIncomplete": False, "items": [{"label": "helper"}]}
    elif method == "textDocument/definition":
        result = [TARGET, OUTSIDE]
    elif method == "textDocument/references":
        result = [TARGET, OUTSIDE]
    elif method == "textDocument/documentSymbol":
        result = [{"name": "helper", "kind": 12, "range": DOC_RANGE, "selectionRange": DOC_RANGE}]
    elif method == "workspace/symbol":
        result = [{"name": "helper", "kind": 12, "location": TARGET}, {"name": "outside", "kind": 12, "location": OUTSIDE}]
    elif method == "textDocument/prepareCallHierarchy":
        result = [item]
    elif method in ("callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"):
        assert params["item"]["uri"] == TARGET["uri"]
        field = "from" if method.endswith("incomingCalls") else "to"
        result = [{field: item, "fromRanges": [DOC_RANGE]}, {field: {**item, **OUTSIDE}, "fromRanges": []}]
    elif method == "$/lean/plainGoal":
        result = {"rendered": "```lean\n⊢ True\n```", "goals": ["⊢ True"]}
    elif method == "$/lean/plainTermGoal":
        result = {"goal": "⊢ Nat", "range": DOC_RANGE}
    send(id=message["id"], result=result)
    return True


while (message := read()) is not None and handle(message):
    pass
