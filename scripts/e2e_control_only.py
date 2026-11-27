#!/usr/bin/env python3
"""Real CLI and disposable loopback router; no service manager or model launch."""
import argparse
import json
import hashlib
import os
import pty
import time
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


def native(h, name, arguments=None):
    code = "try {return await call(%s,%s,%s);} catch (error) {return {denied:true,error:String(error)};}" % (
        json.dumps(h.client_id), json.dumps(name), json.dumps(arguments or {}))
    response = h.public_call("code_mode", {"code": code})["result"]
    return response.get("structuredContent", response)


def run(h):
    blockers = []
    h.prepare()
    environment = h.client_environment("control-only")
    # Isolate the actual Linux installer destination, as well as saved sessions.
    config_home = h.directory / "control-only-config"
    config_home.mkdir(mode=0o700)
    environment["XDG_CONFIG_HOME"] = str(config_home)
    command = h.client_command(extra_args=("--no-watchdog", "--daemon", "--control-only"))
    index = command.index("--shell")
    del command[index:index + 2]
    process = h.spawn("control-only", command, stdout=subprocess.PIPE, cwd=h.directory, env=environment)
    record = h.validate_connection_record(connection(process))
    h.client_id = record["client_id"]
    h.check(record["mode"] == "control-only" and record["chat_enabled"] is False,
            "real CLI starts with null stdin and no TTY in explicit control-only mode")
    h.oauth_login(record)
    names = {tool["name"] for tool in h.native_catalog(h.client_id)}
    h.check("ssmcp.exec.start" in names and all(name == "ssmcp.client.capabilities"
            or (name.startswith("ssmcp.exec.") and not name.endswith("events")) for name in names),
            "catalog contains only native process control and capability reads")
    manifest = native(h, "ssmcp.client.capabilities")
    h.check(manifest["chat_enabled"] is False and manifest["control_only"] is True
            and "listener" not in manifest and "timing" not in manifest,
            "manifest advertises no chat, lifecycle, subscriptions or timed workflow")
    h.check(h.call("ssmcp.exec.list")["jobs"] == [],
            "authorized native control request observes no eagerly started shell or command")
    status, _, _ = h.rpc("tools/call", {"name": "list", "arguments": {}}, secret="")
    h.check(status == 401, "unauthenticated HTTP controller is refused")
    for name in ("ssmcp.exec.events", "ssmcp.worker.submit", "ssmcp.ui.wait", "ssmcp.harnesses.load", "terminal.open"):
        result = native(h, name)
        h.check(result.get("denied") is True or result.get("isError") is True,
                "hard-coded unavailable tool is refused: " + name)
    invalid = native(h, "ssmcp.exec.start", {"command": []})
    h.check("command must contain" in json.dumps(invalid)
            and "memory ceiling" not in json.dumps(invalid),
            "invalid start arguments are rejected before resource admission")
    pid = None
    started = native(h, "ssmcp.exec.start", {"command": ["/bin/sh", "-c", "printf CONTROL_ONLY_OK"], "timeout_ms": 5000})
    if "job_id" in started:
        completed = h.call("ssmcp.exec.wait", {"job_id": started["job_id"], "wait_ms": 5000})
        h.check(not completed["running"] and completed["exit_code"] == 0,
                "authorized native command finishes with exit code zero")
        output = h.call("ssmcp.exec.read", {"job_id": started["job_id"], "max_bytes": 4096})
        h.check(output["stdout"] == "CONTROL_ONLY_OK" and output["stderr"] == "",
                "authorized native command returns the exact expected output")
        pid_file = h.directory / "owned-command.pid"
        h.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", 'echo $$ > "$1"; exec sleep 120', "fixture", str(pid_file)]})
        wait_for(pid_file.exists, description="owned command pid")
        pid = int(pid_file.read_text())
        h.check(pid > 1 and Path(f"/proc/{pid}").exists(), "owned command is live before shutdown")
    else:
        h.check("memory ceiling" in json.dumps(started), "native execution fails closed without resource delegation")
        blockers.append("actual CLI execution and command cleanup require existing delegated cgroup memory admission")
    master, slave = pty.openpty()
    revoked = subprocess.Popen([str(h.binary_dir / "ssmcp"), "controllers", "revoke", h.origin,
                                h.identity["provider"], h.identity["subject"]],
                               stdin=slave, stdout=slave, stderr=slave, env=environment, start_new_session=True)
    h.processes.append(revoked)
    os.close(slave)
    observed = bytearray()
    confirmed = False
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(master, selectors.EVENT_READ)
            deadline = time.monotonic() + 10
            while revoked.poll() is None and time.monotonic() < deadline:
                if selector.select(0.05):
                    try:
                        observed.extend(os.read(master, 4096))
                    except OSError:
                        break
                if not confirmed and b"Type yes to confirm:" in observed:
                    # Only this disposable fixture identity, through the existing local prompt.
                    os.write(master, b"yes\n")
                    confirmed = True
        h.check(confirmed and revoked.wait(timeout=5) == 0,
                "existing interactive local approval prompt revokes the exact disposable fixture subject")
    finally:
        os.close(master)
    denied = native(h, "ssmcp.exec.list")
    h.check((denied.get("denied") is True or denied.get("isError") is True)
            and "approval" in json.dumps(denied), "revoked controller cannot issue even a read-only native control request")
    process.send_signal(signal.SIGTERM)
    h.check(process.wait(timeout=15) == 0, "SIGTERM exits successfully with null stdin")
    if pid is not None:
        wait_for(lambda: not Path(f"/proc/{pid}").exists(), description="owned command cleanup", timeout=10)
        h.check(True, "owned command was reaped on shutdown")
    h.check(not list(Path(environment["SSMCP_SESSION_HOME"]).iterdir()), "no saved session or service definition is created")
    h.check(not list(config_home.rglob("*")),
            "no persistent service definition or enablement registration in isolated XDG config")
    h.check(not h.external_invocations("control-only"), "no agent executable was invoked")
    log = (h.directory / "control-only.log").read_bytes()
    h.check(b"\x1b[" not in log, "no TUI or ANSI dashboard output")
    h.check(b"CONTROL_ONLY_OK" not in log, "native command output is returned only through control requests, without event presentation")
    for label, stop in (("interrupt", "signal"), ("disconnect", "relay")):
        isolated = h.client_environment(label)
        child = h.spawn(label, command, stdout=subprocess.PIPE, cwd=h.directory, env=isolated)
        child_record = h.validate_connection_record(connection(child))
        if stop == "signal":
            child.send_signal(signal.SIGINT)
            h.check(child.wait(timeout=15) == 0, "SIGINT exits cleanly")
        else:
            h.account_gateway.disconnect_client(child_record["client_id"])
            h.check(child.wait(timeout=15) != 0, "relay disconnect exits nonzero without reconnect or replay")
        h.check(not list(Path(isolated["SSMCP_SESSION_HOME"]).iterdir()), label + " leaves no saved session state")
    normal_env = h.client_environment("default-discovery")
    normal = subprocess.run([str(h.binary_dir / "ssmcp"), "--list-tools"],
                            stdin=subprocess.DEVNULL, capture_output=True, env=normal_env, timeout=30)
    h.check(normal.returncode == 0 and any(tool["name"] == "ssmcp.ui.wait"
            for tool in json.loads(normal.stdout)["tools"]), "default offline discovery retains foreground chat tools")
    return blockers


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--require-execution", action="store_true", help="fail if existing resource delegation is unavailable")
    parser.add_argument("--report", type=Path, help="write source and binary identities with the acceptance result")
    parser.add_argument("--revision", help="exact source revision supplied by the checkout owner")
    args = parser.parse_args()
    report = {"status": "failed", "revision": args.revision, "checks": [], "blockers": []}
    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp-control-only-") as root:
            h = Harness(Path(root), args.bin_dir)
            try:
                blockers = run(h)
                report.update(status="partial" if blockers else "passed", blockers=blockers)
            except Exception:
                h.diagnostics()
                raise
            finally:
                try:
                    h.cleanup()
                finally:
                    report["checks"] = h.checks
                    for label, binary in (("cli", h.binary_dir / "ssmcp"),
                                          ("router_fixture", getattr(h, "fixture_binary", None))):
                        if binary is not None and Path(binary).is_file():
                            report[label + "_sha256"] = sha256(Path(binary))
        print(json.dumps(report))
        return 2 if args.require_execution and report["blockers"] else 0
    except Exception:
        report["status"] = "failed"
        raise
    finally:
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
