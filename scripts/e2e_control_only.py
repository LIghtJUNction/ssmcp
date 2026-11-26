#!/usr/bin/env python3
"""Real CLI, disposable loopback production router; no service manager or model."""
import argparse
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile

from e2e import Harness, wait_for


def connection(process):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        if not selector.select(20):
            raise AssertionError("control-only CLI did not become ready")
    line = process.stdout.readline()
    if not line:
        raise AssertionError("control-only CLI exited before registration")
    return json.loads(line)


def run(h):
    h.prepare()
    environment = h.client_environment("control-only")
    command = h.client_command(extra_args=("--no-watchdog", "--daemon", "--control-only"))
    process = h.spawn("control-only", command, stdout=subprocess.PIPE, cwd=h.directory, env=environment)
    record = h.validate_connection_record(connection(process))
    h.client_id = record["client_id"]
    h.check(record["mode"] == "control-only" and record["chat_enabled"] is False,
            "real CLI starts with null stdin and no TTY in explicit control-only mode")
    h.oauth_login(record)
    catalog = h.native_catalog(h.client_id)
    names = {tool["name"] for tool in catalog}
    h.check("ssmcp.exec.start" in names and "terminal.status" in names
            and all(name == "ssmcp.client.capabilities" or name.startswith("terminal.")
                    or (name.startswith("ssmcp.exec.") and not name.endswith("events")) for name in names),
            "catalog contains only native process/terminal control and capability reads")
    manifest = h.call("ssmcp.client.capabilities")
    h.check(manifest["chat_enabled"] is False and manifest["control_only"] is True
            and "listener" not in manifest and "timing" not in manifest,
            "capability manifest advertises no chat, lifecycle, subscriptions or timed workflow")
    job = h.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", "printf CONTROL_ONLY_OK"], "timeout_ms": 5000})
    h.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
    output = h.call("ssmcp.exec.read", {"job_id": job["job_id"], "max_bytes": 4096})
    h.check("CONTROL_ONLY_OK" in json.dumps(output), "authorized minimal command runs through the public native call entry")
    status, _, _ = h.rpc("tools/call", {"name": "list", "arguments": {}}, secret="")
    h.check(status == 401, "unauthenticated HTTP controller is refused")
    for name in ("ssmcp.exec.events", "ssmcp.worker.submit", "ssmcp.ui.wait", "ssmcp.harnesses.load"):
        code = "try { const r=await call(%s,%s,{}); return {denied:!!r.isError}; } catch (_) { return {denied:true}; }" % (json.dumps(h.client_id), json.dumps(name))
        result = h.public_call("code_mode", {"code": code})["result"]
        h.check(result.get("denied") is True or result.get("structuredContent", {}).get("denied") is True,
                "hard-coded unavailable tool is refused: " + name)
    pid_file = h.directory / "owned-command.pid"
    h.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", 'echo $$ > "$1"; exec sleep 120', "fixture", str(pid_file)]})
    wait_for(pid_file.exists, "owned command pid")
    pid = int(pid_file.read_text())
    h.check(pid > 1 and Path(f"/proc/{pid}").exists(), "owned native process is live before shutdown")
    revoked = subprocess.run([str(h.binary_dir / "ssmcp"), "controllers", "revoke", h.origin,
                              h.identity["provider"], h.identity["subject"]],
                             stdin=subprocess.DEVNULL, capture_output=True, env=environment, timeout=10)
    h.check(revoked.returncode == 0, "existing local controller approval command revokes the exact fixture subject")
    marker = h.directory / "must-not-execute"
    code = "return await call(%s,'ssmcp.exec.start',{command:['/bin/sh','-c',%s]});" % (json.dumps(h.client_id), json.dumps("touch " + str(marker)))
    status, result, _ = h.rpc("tools/call", {"name": "code_mode", "arguments": {"code": code}})
    h.check(status == 200 and ("error" in result or result.get("result", {}).get("isError")
            or result.get("result", {}).get("structuredContent", {}).get("result", {}).get("isError"))
            and not marker.exists(), "revoked local controller cannot execute a new native command")
    process.send_signal(signal.SIGTERM)
    h.check(process.wait(timeout=15) == 0, "SIGTERM exits successfully with null stdin")
    wait_for(lambda: not Path(f"/proc/{pid}").exists(), "owned command cleanup", timeout=10)
    h.check(not list(Path(environment["SSMCP_SESSION_HOME"]).iterdir()),
            "control-only creates no saved session or service-manager definition")
    h.check(not h.external_invocations("control-only"), "no agent executable was invoked")
    log = (h.directory / "control-only.log").read_bytes()
    h.check(b"\x1b[" not in log, "control-only emits no TUI or ANSI dashboard output")
    for label, stop in (("interrupt", "signal"), ("disconnect", "relay")):
        isolated = h.client_environment(label)
        child = h.spawn(label, command, stdout=subprocess.PIPE, cwd=h.directory, env=isolated)
        child_record = h.validate_connection_record(connection(child))
        if stop == "signal":
            child.send_signal(signal.SIGINT)
            h.check(child.wait(timeout=15) == 0, "SIGINT exits the unsupervised process cleanly")
        else:
            h.account_gateway.disconnect_client(child_record["client_id"])
            h.check(child.wait(timeout=15) != 0, "relay disconnect exits nonzero without reconnect or replay")
        h.check(not list(Path(isolated["SSMCP_SESSION_HOME"]).iterdir()),
                label + " leaves no saved session state")
    normal, credentials = h.start_client("default-regression", name="normal", cwd=h.directory)
    h.check(credentials["client_id"] != h.client_id and normal.poll() is None,
            "unchanged default client connects through the real CLI without control-only")
    h.stop(normal)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ssmcp-control-only-") as root:
        h = Harness(Path(root), args.bin_dir)
        try:
            run(h)
        except Exception:
            h.diagnostics()
            raise
        finally:
            h.cleanup()
        print(json.dumps({"status": "passed", "checks": h.checks}))


if __name__ == "__main__":
    main()
