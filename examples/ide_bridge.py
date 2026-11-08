#!/usr/bin/env python3
"""Generic localhost HTTP bridge for Swarm Command Center.

Speaks the contract the Rust backend expects:

    POST /
    { "model": "...", "system": "...", "prompt": "...", "stream": false }
    -> { "content": "..." }

This example forwards to an OpenAI-compatible local server (Ollama by default).
Replace `call_ide()` with a call to your IDE's own HTTP API if it exposes one.

Usage:
    ollama serve
    python3 examples/ide_bridge.py
    # UI: IDE -> Custom HTTP köprüsü, Endpoint -> http://127.0.0.1:4319
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen

UPSTREAM = os.environ.get("BRIDGE_UPSTREAM", "http://127.0.0.1:11434/v1/chat/completions")
DEFAULT_MODEL = os.environ.get("BRIDGE_MODEL", "qwen2.5-coder:7b")
PORT = int(os.environ.get("BRIDGE_PORT", "4319"))
HOST = os.environ.get("BRIDGE_HOST", "127.0.0.1")
TIMEOUT = int(os.environ.get("BRIDGE_TIMEOUT", "600"))


def call_ide(model: str, system: str, prompt: str) -> str:
    """Forward the request to the upstream OpenAI-compatible endpoint."""
    messages = []
    if system.strip():
        messages.append({"role": "system", "content": system})
    messages.append({"role": "user", "content": prompt})

    body = json.dumps(
        {
            "model": model or DEFAULT_MODEL,
            "messages": messages,
            "temperature": 0.2,
            "stream": False,
        }
    ).encode("utf-8")

    request = Request(
        UPSTREAM,
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    with urlopen(request, timeout=TIMEOUT) as response:
        payload = json.loads(response.read().decode("utf-8"))

    if "error" in payload:
        raise RuntimeError(payload["error"])

    return payload["choices"][0]["message"]["content"]


class Handler(BaseHTTPRequestHandler):
    def _send(self, status: int, payload: dict) -> None:
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):  # noqa: N802 - stdlib naming
        # The UI calls GET /models to fill its dropdown. Serve a one-entry list.
        if self.path.rstrip("/").endswith("/models"):
            self._send(200, {"data": [{"id": DEFAULT_MODEL, "name": DEFAULT_MODEL}]})
            return
        self._send(200, {"ok": True, "upstream": UPSTREAM, "model": DEFAULT_MODEL})

    def do_POST(self):  # noqa: N802 - stdlib naming
        try:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            payload = json.loads(raw.decode("utf-8"))
            text = call_ide(
                payload.get("model") or DEFAULT_MODEL,
                payload.get("system") or "",
                payload.get("prompt") or "",
            )
            self._send(200, {"content": text})
        except Exception as exc:  # surface the real error to the agent log
            print(f"[bridge] error: {exc}", file=sys.stderr)
            self._send(502, {"error": str(exc)})

    def log_message(self, fmt, *args):  # noqa: N802 - stdlib naming
        print("[bridge] " + (fmt % args), file=sys.stderr)


if __name__ == "__main__":
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"[bridge] listening on http://{HOST}:{PORT} -> {UPSTREAM}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[bridge] stopped", file=sys.stderr)
