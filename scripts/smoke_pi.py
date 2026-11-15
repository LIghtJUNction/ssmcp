#!/usr/bin/env python3
"""Verify an installed Pi RPC CLI without model calls or existing credentials.

Tested with @earendil-works/pi-coding-agent 1.5.2. Run:
    python3 scripts/smoke_pi.py --pi /path/to/pi --report /tmp/pi-smoke.json

The child has an empty temporary configuration and working directory, a minimal
environment, offline mode, and only the temporary probe extension. Every prompt
names a discovered probe command that completes locally, without starting an
agent run. This checks Pi itself; tests/pi_backend.rs checks the Rust adapter.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time


EXTENSION = """
export default function(pi) {
  pi.registerCommand("ssmcp-probe-tools", {
    description: "Inspect tool metadata without model work",
    handler: async (_args, ctx) => {
      ctx.ui.notify(JSON.stringify({
        probe: "tools", active: pi.getActiveTools(),
        tools: pi.getAllTools().map(tool => ({
          name: tool.name, hasParameters: typeof tool.parameters === "object"
        }))
      }), "info");
    }
  });
  pi.registerCommand("ssmcp-probe-confirm", {
    description: "Exercise RPC dialogs without model work",
    handler: async (_args, ctx) => {
      const confirmed = await ctx.ui.confirm("ssmcp probe", "Local test only");
      ctx.ui.notify(JSON.stringify({probe: "confirmation", confirmed}), "info");
    }
  });
}
"""


class Probe:
    def __init__(self, executable: str, directory: Path):
        config = directory / "config"
        work = directory / "work"
        config.mkdir()
        work.mkdir()
        extension = directory / "probe.mjs"
        extension.write_text(EXTENSION)
        environment = {
            "PATH": os.environ.get("PATH", os.defpath),
            "LANG": "C.UTF-8",
            "PI_CODING_AGENT_DIR": str(config),
            "PI_OFFLINE": "1",
            "PI_SKIP_VERSION_CHECK": "1",
            "PI_TELEMETRY": "0",
        }
        self.version = subprocess.check_output(
            [executable, "--version"], env=environment, cwd=work, text=True, timeout=20
        ).strip()
        self.process = subprocess.Popen(
            [
                executable,
                "--mode", "rpc",
                "--no-session",
                "--no-extensions", "--extension", str(extension),
                "--no-skills", "--no-prompt-templates", "--no-themes",
                "--no-context-files", "--no-approve", "--offline",
            ],
            env=environment,
            cwd=work,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
        self.selector.register(self.process.stderr, selectors.EVENT_READ, "stderr")
        self.buffer = b""
        self.stderr = ""
        self.records = []
        self.checks = []

    def check(self, condition: bool, description: str):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print("PASS", description, flush=True)

    def send(self, value: dict):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def wait(self, predicate, *, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for key, _ in self.selector.select(0.2):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    self.selector.unregister(key.fileobj)
                    continue
                if key.data == "stderr":
                    self.stderr = (self.stderr + chunk.decode(errors="replace"))[-8192:]
                    continue
                self.buffer += chunk
                matched = None
                while b"\n" in self.buffer:
                    line, self.buffer = self.buffer.split(b"\n", 1)
                    if not line.strip():
                        continue
                    record = json.loads(line)
                    self.records.append(record)
                    if record.get("type") == "agent_start":
                        raise AssertionError("Probe must never start model work")
                    if predicate(record) and matched is None:
                        matched = record
                if matched is not None:
                    return matched
            if self.process.poll() is not None and not self.selector.get_map():
                break
        raise AssertionError(f"Pi response did not arrive; stderr: {self.stderr}")

    def request(self, request_id: str, kind: str, **args):
        self.send({"id": request_id, "type": kind, **args})
        return self.wait(lambda record: record.get("type") == "response" and record.get("id") == request_id)

    def run(self):
        state = self.request("state", "get_state")
        self.check(state["success"] and not state["data"]["isStreaming"], "RPC get_state is ready and idle")
        commands = self.request("commands", "get_commands")
        names = {command["name"] for command in commands["data"]["commands"]}
        self.check({"ssmcp-probe-tools", "ssmcp-probe-confirm"} <= names, "get_commands discovers the isolated extension commands")

        prompt = self.request("tools", "prompt", message="/ssmcp-probe-tools")
        self.check(prompt["success"] and prompt["data"]["disposition"] == "handled", "extension prompt is handled without model work")
        notifications = [
            json.loads(record["message"])
            for record in self.records
            if record.get("type") == "extension_ui_request" and record.get("method") == "notify"
        ]
        tools = next(record for record in notifications if record.get("probe") == "tools")
        self.check({"read", "bash", "edit", "write"} <= set(tools["active"]), "default model tools remain available inside Pi")
        self.check(all(tool["hasParameters"] for tool in tools["tools"]), "extension API reports native tool parameter schemas")

        self.send({"id": "confirm", "type": "prompt", "message": "/ssmcp-probe-confirm"})
        dialog = self.wait(lambda record: record.get("type") == "extension_ui_request" and record.get("method") == "confirm")
        self.send({"type": "extension_ui_response", "id": dialog["id"], "confirmed": False})
        response = self.wait(lambda record: record.get("type") == "response" and record.get("id") == "confirm")
        self.check(response["success"] and response["data"]["disposition"] == "handled", "explicit negative dialog response unblocks the extension")
        confirmation = [
            json.loads(record["message"])
            for record in self.records
            if record.get("type") == "extension_ui_request" and record.get("method") == "notify"
        ]
        self.check({"probe": "confirmation", "confirmed": False} in confirmation, "Pi receives confirmed:false unchanged")

        bash = self.request(
            "bash", "bash",
            command="printf 'ssmcp-pi-one\\n'; sleep 0.1; printf 'ssmcp-pi-two\\n'",
            excludeFromContext=True,
        )
        updates = [record for record in self.records if record.get("type") == "bash_execution_update" and record.get("id") == "bash"]
        expected = "ssmcp-pi-one\nssmcp-pi-two\n"
        self.check(bash["success"] and bash["data"]["output"] == expected, "native bash returns the actual local output")
        self.check("".join(record["delta"] for record in updates) == expected, "bash output streams with the originating request ID")
        self.check(all(self.records.index(record) < self.records.index(bash) for record in updates), "streamed output precedes the final bash response")
        self.check(self.request("queue", "clear_queue")["success"], "clear_queue succeeds")
        self.check(self.request("abort", "abort")["success"], "abort succeeds while idle")
        self.check(self.request("new", "new_session")["data"]["cancelled"] is False, "new_session succeeds without model work")
        unknown = self.request("unknown", "ssmcp_unknown_probe_command")
        self.check(unknown["success"] is False and isinstance(unknown["error"], str), "unknown commands retain native error responses")

        self.process.stdin.close()
        self.process.wait(timeout=10)
        self.check(self.process.returncode == 0, "stdin EOF shuts Pi down cleanly")
        self.check(not any(record.get("type") == "agent_start" for record in self.records), "no agent run or provider request was started")

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.selector.close()
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            if pipe and not pipe.closed:
                pipe.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pi", default="pi", help="Installed Pi executable (tested: 1.5.2)")
    parser.add_argument("--report", type=Path, help="Optional JSON report path")
    args = parser.parse_args()
    executable = shutil.which(args.pi)
    if executable is None:
        parser.error(f"Pi executable not found: {args.pi}")
    with tempfile.TemporaryDirectory(prefix="ssmcp-pi-smoke-") as temporary:
        probe = Probe(str(Path(executable).resolve()), Path(temporary))
        try:
            print(f"Pi {probe.version}", flush=True)
            probe.run()
            if args.report:
                args.report.write_text(json.dumps({
                    "pi_version": probe.version,
                    "checks": probe.checks,
                    "records": probe.records,
                    "stderr": probe.stderr,
                }, indent=2) + "\n")
        finally:
            probe.close()


if __name__ == "__main__":
    main()
