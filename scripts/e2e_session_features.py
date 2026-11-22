#!/usr/bin/env python3
"""Verify titles, long-output artifacts and reload through real local WSS/OAuth.

Uses the isolated account gateway and OAuth helpers from e2e.py. No real identity,
external service or model is used. Client PATH contains only test shell tools,
so automatic harness discovery cannot start an installed coding agent.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import tempfile
import time

from e2e import Harness, wait_for


LARGE_BYTES = 1152 * 1024
TITLE = "修复客户端：会话标题、重载与长输出护卫"
BEGIN = b"SSMCP_LARGE_BEGIN"
END = b"SSMCP_LARGE_END"


class ConnectionReader:
    """Read both credential records from the same stdout pipe across exec."""

    def __init__(self, process):
        self.process = process
        self.buffer = bytearray()

    def read(self, timeout=30.0):
        deadline = time.monotonic() + timeout
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise AssertionError("client did not publish connection credentials in time")
                chunk = os.read(self.process.stdout.fileno(), 4096)
                if not chunk:
                    raise AssertionError("client stdout closed before connection credentials")
                self.buffer.extend(chunk)
                if len(self.buffer) > 65536:
                    raise AssertionError("client connection record exceeded its bound")
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        try:
            credentials = json.loads(line)
        except (ValueError, UnicodeError) as error:
            # Never include the raw credential line in test diagnostics.
            raise AssertionError("client connection record is not valid JSON") from error
        if not isinstance(credentials, dict) or not credentials.get("client_id"):
            raise AssertionError("client connection record has no client ID")
        return credentials


class SessionFeatures(Harness):
    def __init__(self, directory, binary_dir):
        super().__init__(directory, binary_dir)
        self.metrics = {}

    def titled_client(self, label, title, cwd):
        command = self.client_command(name=label, extra_args=("--title", title))
        process = self.spawn(label, command, stdout=subprocess.PIPE, cwd=cwd,
                             env=self.client_environment(label))
        reader = ConnectionReader(process)
        credentials = reader.read()
        self.validate_connection_record(credentials)
        return process, reader, credentials

    def quiet_terminal(self, client_id):
        self.call("terminal.write", {
            "data": "stty -echo; printf '%s%s\\n' SSMCP_ READY\n",
        }, client_id=client_id)
        self.read_until(r"SSMCP_READY", client_id=client_id)

    def check_memory_artifact(self, metadata):
        if (not re.fullmatch(r"[a-f0-9]{32}", metadata["id"])
                or metadata["schema"] != "ssmcp.output.v1"
                or metadata["path"] is not None or metadata["storage"] != "memory"):
            raise AssertionError("expected a connection-scoped memory artifact without a disk path")

    def settled_output(self, client_id, marker, minimum):
        previous = None
        stable_since = time.monotonic()

        def settled():
            nonlocal previous, stable_since
            status = self.call("terminal.status", client_id=client_id)
            cursor = status["next_cursor"]
            if cursor != previous:
                previous, stable_since = cursor, time.monotonic()
                return False
            metadata = status.get("output_artifact")
            if cursor < minimum or not metadata or time.monotonic() - stable_since < 0.15:
                return False
            self.check_memory_artifact(metadata)
            tail = self.call("ssmcp.output.read", {
                "id": metadata["id"], "offset": max(0, metadata["bytes"] - 1024), "max_bytes": 1024,
            }, client_id=client_id)
            return status if marker in base64.b64decode(tail["data_base64"], validate=True) else False

        return wait_for(settled, timeout=20.0, description="complete stable terminal output")

    def inspect_spill(self, client_id, *, marker, minimum):
        status = self.settled_output(client_id, marker, minimum)
        result = self.call("terminal.read", {"cursor": 0, "max_bytes": 16384}, client_id=client_id)
        self.check(result.get("spilled") is True and result["text"] == ""
                   and result["data_base64"] == "" and not result["truncated"],
                   "long terminal read returns metadata without inline body or lost prefix")
        metadata = result["artifact"]
        data, pages = self.read_paginated_output(client_id, metadata)
        lines = data.count(b"\n") + int(bool(data) and not data.endswith(b"\n"))
        self.check(metadata["bytes"] == len(data)
                   and metadata["lines"] == lines
                   and metadata["sha256"] == hashlib.sha256(data).hexdigest()
                   and metadata["scope"] == "terminal_session"
                   and metadata["expires_at_unix"] > time.time(),
                   "artifact metadata matches actual bytes, SHA-256, logical lines and retention")
        self.check(result["next_cursor"] == status["next_cursor"] == len(data)
                   and result["range"] == {"start": 0, "end": len(data), "stored_end": len(data)},
                   "spilled terminal cursor and stored byte range cover the full session")
        return metadata, data, pages

    def read_paginated_output(self, client_id, metadata):
        self.check_memory_artifact(metadata)
        output = bytearray()
        cursor = 0
        pages = 0
        chunk_size = 32768
        while cursor < metadata["bytes"]:
            offsets = list(range(cursor, min(metadata["bytes"], cursor + 8 * chunk_size), chunk_size))
            # Eight bounded pages remain below code_mode's 1 MiB result limit.
            code = "return await Promise.all(%s.map(offset => outputs.read(%s, {id:%s, offset, max_bytes:%d})));" % (
                json.dumps(offsets), json.dumps(client_id), json.dumps(metadata["id"]), chunk_size,
            )
            results = self.public_call("code_mode", {"code": code})["result"]
            for result in results:
                data = base64.b64decode(result["data_base64"], validate=True)
                if (result["path"] is not None or result["storage"] != "memory"
                        or result["size"] != metadata["bytes"]
                        or result["eof"] != (result["next_offset"] == metadata["bytes"])
                        or result["offset"] != cursor or result["bytes"] != len(data)
                        or result["next_offset"] != cursor + len(data)
                        or not 0 < len(data) <= chunk_size
                        or result["sha256"] != hashlib.sha256(data).hexdigest()):
                    raise AssertionError("output pagination changed bytes or cursor semantics")
                output.extend(data)
                cursor = result["next_offset"]
                pages += 1
        self.check(cursor == metadata["bytes"] and hashlib.sha256(output).hexdigest() == metadata["sha256"],
                   "outputs.read reconstructs the exact artifact across bounded WSS pages")
        return bytes(output), pages

    def check_output_unavailable(self, artifact_id):
        rejected = {}
        for tool in ("ssmcp.output.stat", "ssmcp.output.read"):
            code = """
                try {
                    const value = await call(%s, %s, %s);
                    return {rejected: value.isError === true, message: value.content?.[0]?.text};
                } catch (error) {
                    return {rejected: true, message: String(error)};
                }
            """ % (json.dumps(self.client_id), json.dumps(tool), json.dumps({"id": artifact_id}))
            result = self.public_call("code_mode", {"code": code})["result"]
            self.check(result["rejected"] is True
                       and "output is unavailable in this connection" in result.get("message", ""),
                       f"{tool} rejects the previous connection's output after reload")
            rejected[tool] = result
        return rejected

    def run(self):
        self.prepare()
        workspace = self.directory / "workspace"
        workspace.mkdir()
        self.client, reader, original = self.titled_client("session-features", TITLE, workspace)
        self.oauth_login(original)
        self.client_id = original["client_id"]
        inventory = self.public_call("list")
        visible = next(client for client in inventory["clients"] if client["id"] == self.client_id)
        self.check(original["title"] == visible["title"] == TITLE
                   and visible["cwd"] == str(workspace.resolve()),
                   "Chinese session title and startup cwd are visible through authenticated list")
        self.quiet_terminal(self.client_id)
        command = (
            "printf 'SSMCP_LARGE_BEGIN\\n'; "
            f"head -c {LARGE_BYTES} /dev/zero | tr '\\000' Z; "
            "printf '\\nSSMCP_LARGE_END\\n'\n"
        )
        self.call("terminal.write", {"data": command})
        artifact, data, pages = self.inspect_spill(self.client_id, marker=END, minimum=LARGE_BYTES)
        self.check(BEGIN in data and END in data and b"Z" * LARGE_BYTES in data
                   and len(data) > 1100000,
                   "real command output exceeding 1.1 MiB retains deterministic beginning, body and ending")
        code = "return await outputs.cleanup(%s);" % json.dumps(self.client_id)
        cleanup = self.public_call("code_mode", {"code": code})["result"]
        current = self.call("ssmcp.output.stat", {"id": artifact["id"]})
        self.check(cleanup["storage"] == "memory" and cleanup["active_artifacts"] >= 1
                   and current["path"] is None and current["sha256"] == artifact["sha256"],
                   "retention cleanup preserves the active terminal artifact")

        line_client, _, line_credentials = self.titled_client("line-threshold", "验证行数阈值", workspace)
        line_id = line_credentials["client_id"]
        primary_token, primary_grant = self.secret, self.oauth_grant
        try:
            self.oauth_login(line_credentials)
            self.quiet_terminal(line_id)
            self.call("terminal.write", {"data":
                "i=0; printf 'SSMCP_LINES_BEGIN\\n'; "
                "while [ \"$i\" -lt 600 ]; do printf 'x\\n'; i=$((i+1)); done; "
                "printf 'SSMCP_LINES_END\\n'\n",
            }, client_id=line_id)
            line_artifact, line_data, _ = self.inspect_spill(line_id, marker=b"SSMCP_LINES_END", minimum=1200)
            self.check(line_artifact["lines"] > 500 and len(line_data) < 32768,
                       "more than 500 short lines trigger spilling below the byte threshold")
        finally:
            try:
                self.stop(line_client)
            finally:
                self.secret, self.oauth_grant = primary_token, primary_grant

        old_id, old_token, pid = self.client_id, self.secret, self.client.pid
        code = "return await reload(%s);" % json.dumps(old_id)
        accepted = self.public_call("code_mode", {"code": code})["result"]
        self.check(accepted.get("accepted") is True
                   and accepted["client_id"] == old_id
                   and accepted["oauth_login_required"] is False,
                   "code_mode reload returns its acknowledged lifecycle result over real WSS")
        replacement = reader.read()
        self.validate_connection_record(replacement)
        self.check(self.client.poll() is None and self.client.pid == pid
                   and replacement["client_id"] != old_id
                   and replacement["title"] == original["title"]
                   and replacement["cwd"] == original["cwd"]
                   and replacement["name"] == original["name"],
                   "same client process execs and reconnects with new ID while preserving title, name and cwd")
        self.check(self.rpc("ping", modern=True, secret=old_token)[0] == 200,
                   "device reload preserves the independent account OAuth grant")
        self.client_id = replacement["client_id"]
        self.check(self.call("terminal.status")["running"],
                   "the same account OAuth authorization controls the reloaded terminal")
        listed = self.public_call("list")["clients"]
        self.check(all(item["id"] != old_id for item in listed),
                   "the disconnected client ID is absent after reload")
        self.check_output_unavailable(artifact["id"])
        self.metrics = {
            "large_output_bytes": artifact["bytes"], "large_output_lines": artifact["lines"],
            "large_output_sha256": artifact["sha256"], "artifact_read_pages": pages,
            "line_threshold_bytes": line_artifact["bytes"], "line_threshold_lines": line_artifact["lines"],
            "client_id_changed": True, "same_process_reconnected": True,
            "previous_oauth_status": 200, "artifact_survived_reload": False,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--report", type=Path, help="write JSON validation results without credentials")
    args = parser.parse_args()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-session-e2e-") as directory:
        harness = SessionFeatures(Path(directory), args.bin_dir)
        try:
            harness.run()
        except Exception:
            harness.diagnostics()
            raise
        finally:
            harness.cleanup()
        report = {
            "status": "passed", "checks": harness.checks, "metrics": harness.metrics,
            "duration_seconds": round(time.monotonic() - started, 3),
            "transport": "real account WebSocket + OAuth PKCE + HTTP MCP + Linux PTY",
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} session feature checks in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()

