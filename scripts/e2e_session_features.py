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
OUTPUT_READ_BYTES = 32768
OUTPUT_RETENTION_SECS = 900
OUTPUT_MAX_BYTES = 8 * 1024 * 1024
OUTPUT_RETAINED_BYTES = 32 * 1024 * 1024


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
        self.artifacts = {}
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

    def artifact_metadata(self, metadata, *, client_id=None):
        artifact_id = metadata.get("id", "")
        size = metadata.get("bytes")
        if (metadata.get("schema") != "ssmcp.output.v1"
                or not isinstance(artifact_id, str) or not re.fullmatch(r"[a-f0-9]{32}", artifact_id)
                or metadata.get("storage") != "memory" or "path" not in metadata or metadata["path"] is not None
                or not isinstance(size, int) or not 0 <= size <= OUTPUT_MAX_BYTES
                or metadata.get("observed_bytes") != size or metadata.get("error") is not None
                or not isinstance(metadata.get("complete"), bool)
                or not isinstance(metadata.get("lines"), int) or metadata["lines"] < 0
                or not re.fullmatch(r"[a-f0-9]{64}", str(metadata.get("sha256", "")))
                or metadata.get("sha256_scope") != "first_bytes_snapshot"
                or metadata.get("retention_secs") != OUTPUT_RETENTION_SECS
                or metadata.get("max_bytes") != OUTPUT_MAX_BYTES):
            raise AssertionError("output artifact is not an intact bounded memory snapshot")
        owner = client_id or self.client_id
        if not owner or self.artifacts.setdefault(artifact_id, owner) != owner:
            raise AssertionError("output snapshot was attributed to a different owning connection")
        return metadata

    def artifact_page(self, metadata, page, offset, maximum):
        chunk = base64.b64decode(page["data_base64"], validate=True)
        end = offset + len(chunk)
        if (page.get("id") != metadata["id"] or page.get("storage") != "memory"
                or "path" not in page or page["path"] is not None
                or page.get("offset") != offset or page.get("bytes") != len(chunk)
                or page.get("next_offset") != end or not 0 <= len(chunk) <= maximum
                or end > metadata["bytes"] or (not chunk and offset < metadata["bytes"])
                or page.get("size") != metadata["bytes"]
                or page.get("observed_bytes") != metadata["observed_bytes"]
                or page.get("complete") is not metadata["complete"]
                or page.get("truncated") is not False
                or page.get("eof") is not (end == metadata["bytes"])
                or page.get("sha256") != hashlib.sha256(chunk).hexdigest()
                or page.get("sha256_scope") != "returned_chunk"):
            raise AssertionError("output page changed its memory snapshot, bytes, cursor or digest")
        return chunk

    def read_artifact_page(self, metadata, offset, maximum, *, client_id=None, secret=None):
        self.artifact_metadata(metadata, client_id=client_id)
        page = super().call("ssmcp.output.read", {
            "id": metadata["id"], "offset": offset, "max_bytes": maximum,
        }, client_id=client_id, secret=secret)
        return self.artifact_page(metadata, page, offset, maximum)

    def artifact_bytes(self, metadata, *, client_id=None, secret=None):
        self.artifact_metadata(metadata, client_id=client_id)
        result = bytearray()
        while len(result) < metadata["bytes"]:
            result.extend(self.read_artifact_page(
                metadata, len(result), min(OUTPUT_READ_BYTES, metadata["bytes"] - len(result)),
                client_id=client_id, secret=secret))
        if not result:
            self.read_artifact_page(metadata, 0, 1, client_id=client_id, secret=secret)
        lines = result.count(b"\n") + int(bool(result) and not result.endswith(b"\n"))
        if (len(result) != metadata["bytes"] or lines != metadata["lines"]
                or hashlib.sha256(result).hexdigest() != metadata["sha256"]):
            raise AssertionError("output pages do not reconstruct the full advertised byte snapshot")
        return bytes(result)

    def output_unavailable(self, metadata, *, client_id=None, secret=None):
        selected = client_id or self.client_id
        code = """
            return await Promise.all(['ssmcp.output.read','ssmcp.output.stat'].map(async name => {
                try {
                    const value = await call(%s, name, {id:%s});
                    return {name, rejected:value.isError === true, detail:JSON.stringify(value)};
                } catch (error) { return {name, rejected:true, detail:String(error)}; }
            }));
        """ % (json.dumps(selected), json.dumps(metadata["id"]))
        rows = self.public_call("code_mode", {"code": code}, secret=secret)["result"]
        if (len(rows) != 2 or {row["name"] for row in rows} != {"ssmcp.output.read", "ssmcp.output.stat"}
                or not all(row.get("rejected") is True
                           and "output is unavailable in this connection" in row.get("detail", "") for row in rows)):
            raise AssertionError("the new or foreign connection did not reject the actual old output ID")
        return rows

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
            tail = self.read_artifact_page(metadata, max(0, metadata["bytes"] - 1024), 1024,
                                           client_id=client_id)
            return status if marker in tail else False

        return wait_for(settled, timeout=20.0, description="complete stable terminal output")

    def inspect_spill(self, client_id, *, marker, minimum):
        status = self.settled_output(client_id, marker, minimum)
        result = self.call("terminal.read", {"cursor": 0, "max_bytes": 16384}, client_id=client_id)
        self.check(result.get("spilled") is True and result["text"] == ""
                   and result["data_base64"] == "" and not result["truncated"],
                   "long terminal read returns metadata without inline body or lost prefix")
        metadata = result["artifact"]
        data = self.artifact_bytes(metadata, client_id=client_id)
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
        return metadata, data

    def verify_pagination(self, client_id, metadata):
        self.artifact_metadata(metadata, client_id=client_id)
        digest = hashlib.sha256()
        cursor = 0
        pages = 0
        chunk_size = OUTPUT_READ_BYTES
        while cursor < metadata["bytes"]:
            offsets = list(range(cursor, min(metadata["bytes"], cursor + 8 * chunk_size), chunk_size))
            # Eight bounded pages remain below code_mode's 1 MiB result limit.
            code = "return await Promise.all(%s.map(offset => outputs.read(%s, {id:%s, offset, max_bytes:Math.min(%d,%d-offset)})));" % (
                json.dumps(offsets), json.dumps(client_id), json.dumps(metadata["id"]), chunk_size, metadata["bytes"],
            )
            results = self.public_call("code_mode", {"code": code})["result"]
            if len(results) != len(offsets):
                raise AssertionError("outputs.read returned a missing or duplicate pagination result")
            for result in results:
                data = self.artifact_page(metadata, result, cursor, min(chunk_size, metadata["bytes"] - cursor))
                digest.update(data)
                cursor = result["next_offset"]
                pages += 1
        self.check(cursor == metadata["bytes"] and digest.hexdigest() == metadata["sha256"],
                   "outputs.read reconstructs the exact artifact across bounded WSS pages")
        return pages

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
        artifact, data = self.inspect_spill(self.client_id, marker=END, minimum=LARGE_BYTES)
        self.check(BEGIN in data and END in data and b"Z" * LARGE_BYTES in data
                   and len(data) > 1100000,
                   "real command output exceeding 1.1 MiB retains deterministic beginning, body and ending")
        pages = self.verify_pagination(self.client_id, artifact)
        code = "return await outputs.cleanup(%s);" % json.dumps(self.client_id)
        cleanup = self.public_call("code_mode", {"code": code})["result"]
        retained = self.call("ssmcp.output.stat", {"id": artifact["id"]})
        self.check(cleanup["storage"] == "memory" and cleanup["retention_secs"] == OUTPUT_RETENTION_SECS
                   and cleanup["max_retained_bytes"] == OUTPUT_RETAINED_BYTES
                   and retained == artifact and self.artifact_bytes(retained) == data,
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
            line_artifact, line_data = self.inspect_spill(line_id, marker=b"SSMCP_LINES_END", minimum=1200)
            self.check(line_artifact["lines"] > 500 and len(line_data) < 32768,
                       "more than 500 short lines trigger spilling below the byte threshold")
        finally:
            try:
                self.stop(line_client)
            finally:
                self.secret, self.oauth_grant = primary_token, primary_grant

        old_id, old_token, pid = self.client_id, self.secret, self.client.pid
        code = 'return await call(%s, "ssmcp.client.reload", {});' % json.dumps(old_id)
        reply = self.public_call("code_mode", {"code": code})["result"]
        self.check(reply.get("isError") is not True, "native reload returns no MCP error")
        accepted = reply["structuredContent"]
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
        self.output_unavailable(artifact)
        self.check(True, "the reloaded connection rejects the actual previous memory output ID through read and stat")
        self.quiet_terminal(self.client_id)
        self.call("terminal.write", {"data":
            "i=0; while [ \"$i\" -lt 600 ]; do printf 'r\\n'; i=$((i+1)); done; "
            "printf 'SSMCP_RELOADED_OUTPUT\\n'\n",
        })
        fresh, fresh_data = self.inspect_spill(self.client_id, marker=b"SSMCP_RELOADED_OUTPUT", minimum=1200)
        self.check(fresh["id"] != artifact["id"] and b"SSMCP_RELOADED_OUTPUT" in fresh_data,
                   "the replacement connection creates and reads its own fresh memory output")
        self.verify_pagination(self.client_id, fresh)
        self.metrics = {
            "large_output_bytes": artifact["bytes"], "large_output_lines": artifact["lines"],
            "large_output_sha256": artifact["sha256"], "artifact_read_pages": pages,
            "line_threshold_bytes": line_artifact["bytes"], "line_threshold_lines": line_artifact["lines"],
            "client_id_changed": True, "same_process_reconnected": True,
            "previous_oauth_status": 200, "artifact_survived_reload": False,
            "old_output_read_and_stat_rejected": True, "replacement_output_id": fresh["id"],
        }

    def remove_test_artifacts(self):
        # Output has no filesystem path. Its owning processes must finish before
        # the harness drops its ID bookkeeping; Harness.cleanup owns that shutdown.
        self.check(all(process.poll() is not None for process in self.processes),
                   "all owned fixture processes stop before memory-output bookkeeping is cleared")
        self.artifacts.clear()


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
            harness.remove_test_artifacts()
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

