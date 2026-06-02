#!/usr/bin/env python3

# based on zbirenbaum's copilot.lua neovim plugin
# https://github.com/zbirenbaum/copilot.lua/
#
# idea:
#   - subscribes to shed via $SHED_SOCK and watches buffer/cursor events.
#   - spawns the copilot language server (node language-server.js --stdio)
#     and communicates with it using json-rpc
#   - on receiving 'msg>>copilot>>*', dispatches on the last argument
#     'trigger' -> override line editor hint with copilot autocompletion
#     'dismiss' -> unset hint
#   - on shed's end, 'msg -b msg>>copilot>>*' is used to communicate.
#     this can be set to a keymap, like "keymap -i '<C-p>' '<CMD>!msg -b msg>>copilot>>trigger'"
#     which makes it so that Ctrl+P triggers autocompletion
#
# requires:
#   - python 3.10+
#   - node 22+ on PATH
#   - COPILOT_SERVER env var pointing at language-server.js (or pass --server <path>)
#       I got it from here: https://github.com/zbirenbaum/copilot.lua/blob/master/copilot/js/language-server.js
#
# usage:
#   COPILOT_SERVER=/path/to/language-server.js ./copilot.py --server $SHED_SOCK &
#
# To sign in for the first time, run with --signin and follow the device-code
# flow printed to the terminal.

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Any, Awaitable, Callable

EDITOR_INFO = {"name": "shed", "version": "0.1"}
PLUGIN_INFO = {"name": "shed-copilot.py", "version": "0.1"}

VERBOSE = False
def vlog(*a):
    if VERBOSE:
        print("[copilot]", *a, file=sys.stderr, flush=True)


# LSP framing

class LspClient:
    # json-rpc client thing for LSP

    def __init__(self, proc: asyncio.subprocess.Process):
        self.proc = proc
        self._next_id = 1
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._notif_handlers: dict[str, Callable[[dict], Awaitable[None]]] = {}
        self._reader_task = asyncio.create_task(self._read_loop())

    def on_notification(self, method: str, handler):
        self._notif_handlers[method] = handler

    async def request(self, method: str, params: dict | None = None) -> Any:
        rid = self._next_id
        self._next_id += 1

        fut: asyncio.Future[Any] = asyncio.get_event_loop().create_future()
        self._pending[rid] = fut

        await self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})

        return await fut

    async def notify(self, method: str, params: dict | None = None) -> None:
        await self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    async def _send(self, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")

        if VERBOSE and payload.get("method") in ("getCompletions", "textDocument/didOpen", "textDocument/didChange"):
            vlog(f"send {payload.get('method')}: {body[:400].decode()}")

        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")

        assert self.proc.stdin is not None

        self.proc.stdin.write(header + body)
        await self.proc.stdin.drain()

    async def _read_loop(self) -> None:
        assert self.proc.stdout is not None
        reader = self.proc.stdout

        while True:
            headers: dict[str, str] = {}

            while True:
                line = await reader.readline()

                if not line:
                    return  # server died

                line = line.rstrip(b"\r\n")

                if not line:
                    break

                k, _, v = line.decode("ascii").partition(":")
                headers[k.strip().lower()] = v.strip()

            n = int(headers.get("content-length", "0"))
            body = await reader.readexactly(n)

            try:
                msg = json.loads(body)
            except json.JSONDecodeError:
                continue

            if "id" in msg and ("result" in msg or "error" in msg):
                fut = self._pending.pop(msg["id"], None)

                if fut and not fut.done():
                    if "error" in msg:
                        fut.set_exception(RuntimeError(msg["error"]))
                    else:
                        fut.set_result(msg.get("result"))
            elif "method" in msg and "id" not in msg:
                handler = self._notif_handlers.get(msg["method"])
                if handler:
                    asyncio.create_task(handler(msg.get("params") or {}))


# shed socket

class ShedSocket:
    # $SHED_SOCK subscriber/dispatcher thing

    def __init__(self, path: str):
        self.path = path

    async def send_oneshot(self, msg: str) -> str:
        r, w = await asyncio.open_unix_connection(self.path)
        w.write(msg.encode("utf-8"))
        await w.drain()

        if w.can_write_eof():
            w.write_eof()

        try:
            resp = await asyncio.wait_for(r.readline(), timeout=2.0)
        except asyncio.TimeoutError:
            resp = b""

        w.close()

        return resp.decode("utf-8", "replace").rstrip("\n")

    async def subscribe(self):
        """Yields (topic, data) tuples until the connection closes."""
        r, w = await asyncio.open_unix_connection(self.path)
        w.write(b"subscribe")
        await w.drain()

        if w.can_write_eof():
            w.write_eof()

        while True:
            line = await r.readline()

            if not line:
                return

            text = line.decode("utf-8", "replace").rstrip("\n")
            # events are of the form "namespace>>event>>data" or "namespace>>event"
            head, sep, rest = text.partition(">>")

            if not sep:
                continue

            event, sep2, data = rest.partition(">>")

            yield (head, event, data)


# document state

class DocState:
    # used for dummy document-state
    # that we send to copilot

    URI = "file:///tmp/shed-buffer.sh"
    LANG = "shellscript"

    # seeded content for context in small buffers
    SEED = "#!/bin/bash\n# Interactive shell command:\n"

    def __init__(self):
        self.version = 0
        self.text = ""           # the buffer as the user sees it
        self.cursor_byte = 0     # cursor index into self.text

    def _full_text(self) -> str:
        return self.SEED + self.text

    def _position(self) -> dict[str, int]:
        full = self._full_text()
        cursor = len(self.SEED) + self.cursor_byte
        prefix = full[:cursor]
        line = prefix.count("\n")
        col = len(prefix) - (prefix.rfind("\n") + 1)

        return {"line": line, "character": col}

    def did_open_params(self) -> dict:
        return {
            "textDocument": {
                "uri": self.URI,
                "languageId": self.LANG,
                "version": self.version,
                "text": self._full_text(),
            }
        }

    def did_change_params(self) -> dict:
        self.version += 1
        return {
            "textDocument": {"uri": self.URI, "version": self.version},
            "contentChanges": [{"text": self._full_text()}],
        }

    def completion_params(self) -> dict:
        pos = self._position()
        doc = {
            "uri": self.URI,
            "version": self.version,
            "relativePath": "shed-buffer.sh",
            "insertSpaces": True,
            "tabSize": 4,
            "indentSize": 4,
            "position": pos,
        }
        return {
            "doc": doc,
            "textDocument": {
                "uri": self.URI,
                "version": self.version,
                "relativePath": "shed-buffer.sh",
            },
            "position": pos,
            "_": True,  # force json object, never array
        }


# main loop

async def run(server_path: str, sock_path: str, signin: bool, signout: bool) -> None:
    if not sock_path and not signout:
        sys.exit("no socket path; set SHED_SOCK, pass --socket, or run inside a shed session.")

    if not Path(server_path).is_file():
        sys.exit(f"Copilot language server not found at: {server_path}")

    proc = await asyncio.create_subprocess_exec(
        "node", server_path, "--stdio",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE if VERBOSE else asyncio.subprocess.DEVNULL,
    )
    lsp = LspClient(proc)

    async def drain_stderr():
        assert proc.stderr is not None

        while True:
            line = await proc.stderr.readline()

            if not line:
                return
            print(f"[server] {line.decode('utf-8', 'replace').rstrip()}", file=sys.stderr, flush=True)

    if VERBOSE:
        asyncio.create_task(drain_stderr())

    async def on_log(params):
        vlog(f"server log: {params}")
    lsp.on_notification("window/logMessage", on_log)
    lsp.on_notification("window/showMessage", on_log)
    lsp.on_notification("$/logTrace", on_log)

    # handshake
    await lsp.request("initialize", {
        "processId": os.getpid(),
        "rootUri": f"file://{os.getcwd()}",
        "capabilities": {"workspace": {"workspaceFolders": True}},
        "initializationOptions": {
            "editorInfo": EDITOR_INFO,
            "editorPluginInfo": PLUGIN_INFO,
        },
    })
    await lsp.notify("initialized", {})
    if VERBOSE:
        await lsp.notify("$/setTrace", {"value": "verbose"})

    await lsp.notify("workspace/didChangeConfiguration", {
        "settings": {
            "github": {
                "copilot": {
                    "editor": {
                        "enableAutoCompletions": True,
                        "showEditorCompletions": True,
                        "delayCompletions": False,
                        "filterCompletions": False,
                    },
                },
            },
        },
    })

    # auth
    status = await lsp.request("checkStatus", {})

    if signout:
        if not status.get("user"):
            print("[copilot] not signed in", file=sys.stderr)
        else:
            user = status["user"]
            await lsp.request("signOut", {})
            print(f"[copilot] signed out {user}", file=sys.stderr)
        return

    if signin or not status.get("user"):
        signin_data = await lsp.request("signInInitiate", {})

        # response shape branches on status:
        #   AlreadySignedIn    -> {status, user}
        #   PromptUserDeviceFlow -> {status, userCode, verificationUri, ...}
        signin_status = str(signin_data.get("status", "")).lower()
        if signin_status == "alreadysignedin":
            print(f"[copilot] already signed in as {signin_data.get('user')}", file=sys.stderr)
        else:
            print(f"[copilot] open {signin_data['verificationUri']}", file=sys.stderr)
            print(f"[copilot] enter code: {signin_data['userCode']}", file=sys.stderr)

            confirm = await lsp.request("signInConfirm", {"userCode": signin_data["userCode"]})

            if str(confirm.get("status", "")).lower() != "ok":
                sys.exit(f"[copilot] sign-in failed: {confirm.get('error')}")

            print(f"[copilot] signed in as {confirm.get('user')}", file=sys.stderr)
    else:
        print(f"[copilot] signed in as {status['user']}", file=sys.stderr)

    sock = ShedSocket(sock_path)
    doc = DocState()

    await lsp.notify("textDocument/didOpen", doc.did_open_params())

    inflight: asyncio.Task | None = None

    async def push_hint() -> None:
        vlog(f"requesting completion for buffer={doc.text!r} cursor={doc.cursor_byte}")

        await lsp.notify("textDocument/didChange", doc.did_change_params())

        try:
            result = await lsp.request("getCompletions", doc.completion_params())
        except RuntimeError as e:
            print(f"[copilot] getCompletions failed: {e}", file=sys.stderr)
            return

        vlog(f"raw result: {json.dumps(result)[:400]}")
        completions = (result or {}).get("completions") or []
        vlog(f"got {len(completions)} completion(s)")

        if not completions:
            return

        text = completions[0].get("text") or ""

        if not text:
            return

        vlog(f"sending hint: {text!r}")
        await sock.send_oneshot(f"line::set::hint::{text}")

    def trigger() -> None:
        nonlocal inflight

        if inflight and not inflight.done():
            inflight.cancel()

        inflight = asyncio.create_task(push_hint())

    async def dismiss() -> None:
        if inflight and not inflight.done():
            inflight.cancel()
        await sock.send_oneshot("line::set::hint::")

    # event loop
    print("[copilot] listening on shed socket (trigger with: msg -b copilot::trigger)",
          file=sys.stderr)
    async for ns, event, data in sock.subscribe():
        vlog(f"event {ns}>>{event}>>{data!r}")

        # update our internal buffer tracking
        if ns == "line":
            if event == "buffer":
                doc.text = data
                doc.cursor_byte = min(doc.cursor_byte, len(doc.text))
            elif event == "cursor":
                try:
                    doc.cursor_byte = int(data)
                except ValueError:
                    pass

        elif ns == "msg" and event.startswith("copilot::"):
            cmd = event[len("copilot::"):]

            # fire
            if cmd == "trigger":
                trigger()
            elif cmd == "dismiss":
                await dismiss()
            else:
                vlog(f"unknown copilot command: {cmd!r}")


def main() -> None:
    ap = argparse.ArgumentParser(description="GitHub Copilot subscriber for shed")

    ap.add_argument("--server", default=os.environ.get("COPILOT_SERVER", ""),
                    help="path to language-server.js")
    ap.add_argument("--socket", default=os.environ.get("SHED_SOCK", ""),
                    help="path to a shed unix socket (defaults to $SHED_SOCK)")
    ap.add_argument("--signin", action="store_true",
                    help="force the device-code sign-in flow")
    ap.add_argument("--signout", action="store_true",
                    help="sign out via the language server and exit")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="log every event, request, and response to stderr")

    args = ap.parse_args()

    global VERBOSE
    VERBOSE = args.verbose

    if not args.server:
        sys.exit("pass --server or set COPILOT_SERVER")

    try:
        asyncio.run(run(args.server, args.socket, args.signin, args.signout))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
