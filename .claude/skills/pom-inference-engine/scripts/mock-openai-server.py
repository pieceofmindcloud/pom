#!/usr/bin/env python3
"""A stand-in engine server for checking an engine plugin before the real
engine runs: it takes llama-server style flags (only --host and --port are
read), answers GET /health, and streams a fixed answer, reasoning first, on
POST /v1/chat/completions and /v1/completions in the OpenAI SSE format.

    mock-openai-server.py --host 127.0.0.1 --port 8080 [any other flags]
"""
import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        print("mock-engine:", fmt % args, file=sys.stderr, flush=True)

    def send_json(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/health":
            self.send_json(200, {"status": "ok"})
        else:
            self.send_json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        request = json.loads(self.rfile.read(length) or b"{}")
        if self.path not in ("/v1/chat/completions", "/v1/completions"):
            return self.send_json(404, {"error": "not found"})
        thinking = request.get("chat_template_kwargs", {}).get("enable_thinking", True)
        chat = self.path == "/v1/chat/completions"
        events = []
        if thinking and chat:
            events.append({"choices": [{"delta": {"reasoning_content": "Thinking."}}]})
        for piece in ("Hello", " from", " the mock engine."):
            events.append({"choices": [{"delta": {"content": piece}} if chat else {"text": piece}]})
        events.append({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 4},
        })
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        for event in events + ["[DONE]"]:
            data = f"data: {event if isinstance(event, str) else json.dumps(event)}\n\n".encode()
            self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    args, _ = parser.parse_known_args()
    print(f"mock-engine: listening on {args.host}:{args.port}", file=sys.stderr, flush=True)
    ThreadingHTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
