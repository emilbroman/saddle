#!/usr/bin/env python3
"""A scripted language server for Saddle's LSP client tests.

- Picks utf-16 (ignores the client's offer) so the client must convert.
- Asks the client for `workspace/configuration` and registers a file watcher
  after `initialized`, and reports the answers via window/logMessage.
- Publishes one diagnostic per line containing `ERROR` on open/change, and
  clears them on close.
- `definition` returns the first occurrence of the word `target` in the file.
- Exits with status 3 on opening or changing a document that contains `CRASH`.
"""

import json
import sys

docs = {}
next_id = 1000


def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        name, value = line.split(b":", 1)
        if name.lower() == b"content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def send(msg):
    msg["jsonrpc"] = "2.0"
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


def request(method, params):
    global next_id
    next_id += 1
    send({"id": next_id, "method": method, "params": params})
    return next_id


def notify(method, params):
    send({"method": method, "params": params})


def utf16_len(s):
    return len(s.encode("utf-16-le")) // 2


def publish(uri):
    text = docs.get(uri)
    diagnostics = []
    if text is not None:
        for number, line in enumerate(text.split("\n")):
            col = line.find("ERROR")
            if col >= 0:
                start = utf16_len(line[:col])
                diagnostics.append(
                    {
                        "range": {
                            "start": {"line": number, "character": start},
                            "end": {"line": number, "character": start + 5},
                        },
                        "severity": 1,
                        "source": "fake",
                        "message": "found ERROR",
                    }
                )
    notify("textDocument/publishDiagnostics", {"uri": uri, "diagnostics": diagnostics})


def check_crash(uri):
    if "CRASH" in docs.get(uri, ""):
        sys.stderr.write("fake_lsp: crashing on request\n")
        sys.stderr.flush()
        sys.exit(3)


pending = {}
shutdown = False
while True:
    msg = read()
    method = msg.get("method")
    if method is None:
        kind = pending.pop(msg.get("id"), None)
        if kind == "configuration":
            notify("window/logMessage", {"type": 3, "message": "configuration=" + json.dumps(msg.get("result"))})
        elif kind == "register":
            notify("window/logMessage", {"type": 3, "message": "registered"})
        continue
    params = msg.get("params") or {}
    if method == "initialize":
        offered = params["capabilities"]["general"]["positionEncodings"]
        send(
            {
                "id": msg["id"],
                "result": {
                    "capabilities": {
                        "positionEncoding": "utf-16",
                        "textDocumentSync": {"openClose": True, "change": 1, "save": {"includeText": False}},
                        "definitionProvider": True,
                    },
                    "serverInfo": {"name": "fake_lsp", "version": "offered " + ",".join(offered)},
                },
            }
        )
    elif method == "initialized":
        pending[request("workspace/configuration", {"items": [{"section": "fake"}, {"section": "other"}]})] = "configuration"
        pending[
            request(
                "client/registerCapability",
                {
                    "registrations": [
                        {
                            "id": "watch",
                            "method": "workspace/didChangeWatchedFiles",
                            "registerOptions": {"watchers": [{"globPattern": "**/*.toml"}]},
                        }
                    ]
                },
            )
        ] = "register"
        token = "fake/indexing"
        request("window/workDoneProgress/create", {"token": token})
        notify("$/progress", {"token": token, "value": {"kind": "begin", "title": "Indexing"}})
        notify("$/progress", {"token": token, "value": {"kind": "end"}})
    elif method == "textDocument/didOpen":
        doc = params["textDocument"]
        docs[doc["uri"]] = doc["text"]
        check_crash(doc["uri"])
        publish(doc["uri"])
    elif method == "textDocument/didChange":
        uri = params["textDocument"]["uri"]
        docs[uri] = params["contentChanges"][-1]["text"]
        check_crash(uri)
        publish(uri)
    elif method == "textDocument/didSave":
        notify("window/logMessage", {"type": 3, "message": "saved " + params["textDocument"]["uri"]})
    elif method == "textDocument/didClose":
        uri = params["textDocument"]["uri"]
        docs.pop(uri, None)
        publish(uri)
    elif method == "workspace/didChangeWatchedFiles":
        for change in params["changes"]:
            notify("window/logMessage", {"type": 3, "message": "watched %d %s" % (change["type"], change["uri"])})
    elif method == "textDocument/definition":
        uri = params["textDocument"]["uri"]
        result = None
        for number, line in enumerate(docs.get(uri, "").split("\n")):
            col = line.find("target")
            if col >= 0:
                start = utf16_len(line[:col])
                result = {
                    "uri": uri,
                    "range": {
                        "start": {"line": number, "character": start},
                        "end": {"line": number, "character": start + 6},
                    },
                }
                break
        send({"id": msg["id"], "result": result})
    elif method == "shutdown":
        shutdown = True
        send({"id": msg["id"], "result": None})
    elif method == "exit":
        sys.exit(0 if shutdown else 1)
    elif "id" in msg:
        send({"id": msg["id"], "error": {"code": -32601, "message": "unsupported " + method}})
