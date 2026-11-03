#!/usr/bin/env python3
"""Minimal stand-in for Ollama, for the demo only.

Answers the calls gpuq makes (GET /api/ps, POST /api/generate with or without
keep_alive 0) and /api/chat. A loaded model holds pretend VRAM on every card in
$GPUQ_DEMO_CARDS, split evenly, through the files fake-vram.sh reads.
"""

import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEMO = os.environ.get("GPUQ_DEMO", "/tmp/gpuq-demo")
CARDS = os.environ.get("GPUQ_DEMO_CARDS", "gpu0,gpu1").split(",")
MODEL_MIB = 20480  # a 32B model at 4 bits, split over the cards
LOAD_S = 1.5
UNLOAD_S = 1.0
loaded = set()


def tag(m):
    return m if ":" in m.rsplit("/", 1)[-1] else m + ":latest"


def hold(model, on):
    for card in CARDS:
        d = os.path.join(DEMO, "vram", card)
        os.makedirs(d, exist_ok=True)
        f = os.path.join(d, f"{os.getpid()}.{model.replace('/', '_')}")
        if on:
            with open(f, "w") as fh:
                fh.write(str(MODEL_MIB // len(CARDS)))
        elif os.path.exists(f):
            os.remove(f)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/api/ps":
            per = MODEL_MIB * 1024 * 1024
            self.reply({"models": [{"name": m, "model": m, "size_vram": per} for m in sorted(loaded)]})
        elif self.path in ("/", "/api/version"):
            self.reply({"version": "mock"})
        else:
            self.reply({"error": "not found"}, 404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        model = tag(req.get("model", ""))
        if self.path == "/api/generate" and req.get("keep_alive") in (0, "0", "0s"):
            time.sleep(UNLOAD_S)
            loaded.discard(model)
            hold(model, False)
            self.reply({"model": model, "done": True, "done_reason": "unload"})
            return
        if self.path not in ("/api/generate", "/api/chat"):
            self.reply({"error": "not found"}, 404)
            return
        if model not in loaded:
            time.sleep(LOAD_S)
            loaded.add(model)
            hold(model, True)
        msg = "Hello! How can I help?" if (req.get("prompt") or req.get("messages")) else ""
        if self.path == "/api/chat":
            self.reply({"model": model, "message": {"role": "assistant", "content": msg}, "done": True})
        else:
            self.reply({"model": model, "response": msg, "done": True})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 11435
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
