#!/usr/bin/env python3
"""OpenCode v2 HTTP/SSE fixture; stdlib only, without models or existing credentials.

The fixture owner supplies a private OPENCODE_PASSWORD. This process never prints or
persists it, and exits on stdin EOF just like the native v2 --stdio service.
"""

from __future__ import annotations

import argparse
import base64
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
from urllib.parse import parse_qs, unquote, urlsplit


EXTRA_OPERATIONS = 70  # Exercise real MCP pagination past its 64-tool boundary.
LIVE_MARKER = "SSMCP_OPENCODE_SSE_BEFORE_READ"


def document():
    paths = {
        "/api/info": {"get": {
            "operationId": "server.info",
            "summary": "Read fixture process information",
            "responses": {"200": {"description": "Fixture information"}},
        }},
        "/api/event": {"get": {
            "operationId": "event.subscribe",
            "summary": "Receive fixture live events",
            "responses": {"200": {
                "description": "Live events",
                "content": {"text/event-stream": {"schema": {"type": "string"}}},
            }},
        }},
        "/api/fixture/echo/{itemID}": {"post": {
            "operationId": "fixture.echo",
            "summary": "Preserve native path, query, headers and JSON body",
            "parameters": [
                {"in": "path", "name": "itemID", "required": True,
                 "schema": {"type": "string", "pattern": "^item_"}},
                {"in": "query", "name": "trace", "schema": {"type": "string"}},
                {"in": "header", "name": "X-Fixture-In", "schema": {"type": "string"}},
            ],
            "requestBody": {"required": True, "content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/EchoInput"},
            }}},
            "responses": {"201": {"description": "Echoed native payload"}},
            "x-fixture-extension": {"keep": [1, 2, 3]},
        }},
    }
    for index in range(EXTRA_OPERATIONS):
        paths[f"/api/fixture/extra/{index:02d}"] = {"get": {
            "operationId": f"fixture.extra{index:02d}",
            "summary": "A discoverable fixture operation",
            "responses": {"200": {"description": "Fixture result"}},
        }}
    return {
        "openapi": "3.1.0",
        "info": {"title": "ssmcp OpenCode fixture", "version": "2.3.0"},
        "paths": paths,
        "components": {"schemas": {"EchoInput": {
            "type": "object", "required": ["message"],
            "properties": {"message": {"type": "string"}, "future": {"type": "object"}},
            "additionalProperties": True,
        }}},
    }


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False

    def __init__(self, directory, password, child):
        super().__init__(("127.0.0.1", 0), Handler)
        self.directory = directory
        self.expected_auth = "Basic " + base64.b64encode(
            f"opencode:{password}".encode()
        ).decode()
        self.child = child
        self.document = document()
        self.stopping = threading.Event()
        self.lock = threading.Lock()
        self.subscribers = []
        self.sequence = 0

    def record(self, method, path, authenticated):
        # Deliberately omit headers and request bodies: neither private Basic
        # credentials nor OAuth credentials belong in test artifacts.
        record = {"method": method, "path": path, "authenticated": authenticated}
        with self.lock:
            with (self.directory / "requests.jsonl").open("a") as stream:
                stream.write(json.dumps(record) + "\n")

    def publish(self):
        with self.lock:
            self.sequence += 1
            event = {"type": "fixture.echoed", "properties": {
                "message": LIVE_MARKER, "sequence": self.sequence,
            }}
            for subscriber in self.subscribers:
                subscriber.put_nowait(event)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def authenticated(self):
        valid = hmac.compare_digest(
            self.headers.get("Authorization", ""), self.server.expected_auth
        )
        self.server.record(self.command, self.path, valid)
        if not valid:
            self.reply(401, {"error": "fixture requires private Basic authentication"})
        return valid

    def reply(self, status, payload, *, headers=None):
        data = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def do_GET(self):
        if not self.authenticated():
            return
        path = urlsplit(self.path).path
        if path == "/openapi.json":
            self.reply(200, self.server.document)
        elif path == "/api/info":
            # Native v2.3.0 HTTP ServerInfo is flat; SDK response wrappers are
            # not part of the wire body (packages/server/test/process.test.ts).
            self.reply(200, {
                "version": "2.3.0", "pid": os.getpid(),
                "child_pid": self.server.child.pid, "authenticated": True,
            }, headers={"X-Fixture-Result": "preserved"})
        elif path == "/api/event":
            self.stream()
        elif path.startswith("/api/fixture/extra/"):
            try:
                index = int(path.rsplit("/", 1)[1])
            except ValueError:
                index = -1
            if 0 <= index < EXTRA_OPERATIONS:
                self.reply(200, {"index": index})
            else:
                self.reply(404, {"error": "unknown fixture operation"})
        else:
            self.reply(404, {"error": "unknown fixture route"})

    def do_POST(self):
        if not self.authenticated():
            return
        parsed = urlsplit(self.path)
        if not parsed.path.startswith("/api/fixture/echo/"):
            self.reply(404, {"error": "unknown fixture route"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= 1024 * 1024:
                raise ValueError("invalid body size")
            payload = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            self.reply(400, {"error": "invalid fixture JSON"})
            return
        self.server.publish()
        self.reply(201, {
            "item": unquote(parsed.path.rsplit("/", 1)[1]),
            "query": parse_qs(parsed.query),
            "header": self.headers.get("X-Fixture-In"),
            "payload": payload,
            "futureResult": {"keep": [1, {"nested": True}]},
        }, headers={"X-Fixture-Result": "preserved"})

    def stream(self):
        subscriber = queue.Queue()
        with self.server.lock:
            self.server.subscribers.append(subscriber)
        (self.server.directory / "subscriber-ready").write_text("ready\n")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        subscriber.put({"type": "fixture.ready", "properties": {"pid": os.getpid()}})
        try:
            while not self.server.stopping.is_set():
                try:
                    event = subscriber.get(timeout=0.25)
                    data = json.dumps(event, separators=(",", ":")).encode()
                    self.wfile.write(b"data: " + data + b"\n\n")
                except queue.Empty:
                    self.wfile.write(b": keep-alive\n\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            self.close_connection = True
            with self.server.lock:
                self.server.subscribers.remove(subscriber)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["serve"])
    parser.add_argument("--stdio", action="store_true", required=True)
    parser.add_argument("--hostname", choices=["127.0.0.1"], required=True)
    parser.add_argument("--port", type=int, choices=[0], required=True)
    parser.add_argument("--fixture-dir", type=Path, required=True)
    args = parser.parse_args()
    password = os.environ.get("OPENCODE_PASSWORD")
    if not password:
        parser.error("OPENCODE_PASSWORD must be supplied by the fixture owner")
    args.fixture_dir.mkdir(parents=True, exist_ok=True)
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(90)"])
    server = None

    def terminate(*_):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, terminate)
    try:
        server = FixtureServer(args.fixture_dir, password, child)
        (args.fixture_dir / "pid").write_text(str(os.getpid()))
        (args.fixture_dir / "child-pid").write_text(str(child.pid))
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        thread.start()
        print(json.dumps({"url": f"http://127.0.0.1:{server.server_port}"}), flush=True)
        sys.stdin.buffer.read()
    finally:
        if server is not None:
            server.stopping.set()
            server.shutdown()
            server.server_close()
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=2)


if __name__ == "__main__":
    main()
