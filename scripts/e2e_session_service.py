#!/usr/bin/env python3
"""Exercise persistent sessions through actual SSH, OAuth, MCP and local CLI.

Only a private temporary session home, loopback fixture server and owned child
processes are used. No system unit, existing session or model account is touched.
Linux fixture requirements match scripts/e2e.py. Platform service installation
and portable lifecycle validation are recorded separately.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

from e2e import Harness, PUBLIC_URL, wait_for


class Sessions(Harness):
    def __init__(self, directory, binary_dir, artifact_dir):
        super().__init__(directory, binary_dir)
        self.workspace = directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        (self.workspace / "child").mkdir()
        self.home = directory / "sessions"
        self.home.mkdir(mode=0o700)
        self.env = self.client_environment("session-service")
        self.env["SSMCP_SESSION_HOME"] = str(self.home)
        self.manager = None
        self.names = set()
        self.artifact_dir = artifact_dir
        self.http_timeout = 45

    def cli(self, args, *, expect=0, private=False):
        result = subprocess.run(
            [str(self.binary_dir / "ssmcp"), *args], cwd=self.workspace,
            env=self.env, stdin=subprocess.DEVNULL, capture_output=True,
            text=True, timeout=55,
        )
        if expect == 0 and result.returncode != 0:
            raise AssertionError(f"CLI {args[0]} failed: {result.stderr[-1200:]}")
        if expect == "failure" and result.returncode == 0:
            raise AssertionError(f"CLI {args[0]} unexpectedly succeeded")
        if private and result.returncode == 0:
            value = json.loads(result.stdout)
            if value.get("login_code"):
                self.sensitive.append(value["login_code"])
        return result

    def action(self, action, name):
        return json.loads(self.cli(["sessions", action, name, "--json"], private=action == "auth").stdout)

    def launch_manager(self):
        self.manager = self.spawn(
            "session-manager-" + str(len(self.processes)),
            [str(self.binary_dir / "ssmcp"), "service", "run"],
            cwd=self.workspace, env=self.env,
        )
        def ready():
            if self.manager.poll() is not None:
                raise AssertionError("session manager exited before its private endpoint was ready")
            return (self.home / "service.json").is_file()
        wait_for(ready, timeout=10, description="private manager endpoint")

    def stop_manager(self):
        if self.manager is None or self.manager.poll() is not None:
            return
        self.manager.send_signal(signal.SIGTERM)
        self.manager.wait(timeout=35)
        self.check(self.manager.returncode == 0, "service termination cleans owned workers and exits successfully")

    def create(self, name, *, control=False, expected=0):
        self.names.add(name)
        args = ["sessions", "create", name, "--json", "--ssh", "--watchdog", "--reconnect-delay", "1",
                "--title", "session lifecycle acceptance", "-F", str(self.directory / "ssh_config"),
                "--shell", "/bin/sh"]
        if control:
            args.append("--session-control")
        args.append(f"{self.user}@ssmcp-e2e")
        return self.cli(args, expect=expected)

    def connected(self, name, old_generation=None):
        def observe():
            value = self.action("status", name)
            if value.get("connection_state") != "connected":
                return False
            if old_generation and value.get("generation") == old_generation:
                return False
            self.check("login_code" not in json.dumps(value), "public session status excludes login credentials")
            return value
        return wait_for(observe, timeout=30, description=f"observed connection for {name}")

    def authenticate_manager(self, status):
        credentials = self.action("auth", status["name"])
        self.client_id = status["connection"]["client_id"]
        self.oauth_login({
            "url": f"{PUBLIC_URL}/{self.user}",
            "login_code": credentials["login_code"], "login_expires_in": 1800,
        })

    def expect_native_failure(self, tool, arguments, needle):
        code = "try { await call(%s,%s,%s); } catch(error) { return {error:String(error)}; }" % (
            json.dumps(self.client_id), json.dumps(tool), json.dumps(arguments),
        )
        result = self.public_call("code_mode", {"code": code})["result"]
        self.check(needle in result.get("error", ""), f"{tool} rejects {needle}")

    def run(self):
        self.prepare()
        self.launch_manager()
        self.check(json.loads(self.cli(["attach", "--json"]).stdout) == [], "bare attach lists an empty manager without connecting")
        self.create("manager", control=True)
        manager = self.connected("manager")
        self.authenticate_manager(manager)
        listed = self.public_call("code_mode", {"code": f"return await sessions.list({json.dumps(self.client_id)});"})["result"]
        self.check(isinstance(listed.get("sessions"), list) and any(item["name"] == "manager" for item in listed["sessions"]), "sessions helper dispatches through authenticated MCP")

        discovery = self.public_call("code_mode", {"code": """
const found = [];
let cursor = null;
do {
  const page = await tools.search(%s, {
    name_prefix: 'SSMCP.EXEC.', include_schemas: true, limit: 2, cursor
  });
  if (page.name_prefix !== 'ssmcp.exec.') throw new Error('missing prefix metadata');
  found.push(...page.tools);
  cursor = page.nextCursor;
  if (found.length > 50) throw new Error('unexpected execution catalog size');
} while (cursor);
return {names: found.map(tool => tool.name),
        schemas: found.every(tool => tool.inputSchema && tool.inputSchema.type === 'object')};
""" % json.dumps(self.client_id)})["result"]
        names = discovery["names"]
        self.check(names and all(name.startswith("ssmcp.exec.") for name in names)
                   and len(names) == len(set(names)),
                   "authenticated prefix discovery pages through native execution tools without duplicates")
        self.check({"ssmcp.exec.start", "ssmcp.exec.wait", "ssmcp.exec.read"}.issubset(names)
                   and discovery["schemas"],
                   "prefix discovery preserves the real execution tool schemas")

        created = self.call("ssmcp.session.create", {"name":"worker", "cwd":"child", "watchdog":True, "reconnect_delay_secs":1})
        self.names.add("worker")
        self.check(created["name"] == "worker" and Path(created["cwd"]) == self.workspace / "child", "atomic create preserves stable name and authorized child cwd")
        worker = self.connected("worker")
        self.check(worker.get("generation") and worker.get("manager_active"), "worker status reports generation and observed manager state")
        self.expect_native_failure("ssmcp.session.create", {"name":"escape", "cwd":".."}, "parent traversal")
        self.expect_native_failure("ssmcp.session.create", {"name":"escape", "allow":["other@github"]}, "unknown field")
        self.expect_native_failure("ssmcp.session.close", {"name":"manager"}, "different management connection")

        before = worker["generation"]
        changed = self.call("ssmcp.session.restart", {"name":"worker", "expected_generation":before})
        self.check(changed["generation"] != before, "restart replaces the worker generation after confirming old worker exit")
        worker = self.connected("worker", before)
        self.expect_native_failure("ssmcp.session.close", {"name":"worker", "expected_generation":before}, "generation")
        self.check(self.action("status", "worker")["generation"] == worker["generation"], "stale lifecycle request cannot change the current worker")

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: self.create("race", expected=None), range(2)))
        self.check(sum(result.returncode == 0 for result in results) == 1, "concurrent duplicate creation commits exactly one saved session")
        self.connected("race")
        self.check(
            [item["name"] for item in json.loads(self.cli(["attach", "--json"]).stdout)]
            == [item["name"] for item in json.loads(self.cli(["attach", "list", "--json"]).stdout)],
            "bare attach and explicit attach list return the same saved identities",
        )
        self.cli(["attach", "missing-session", "--read-only"], expect="failure")
        self.check(not any(item["name"] == "missing-session" for item in json.loads(self.cli(["attach", "--json"]).stdout)), "attach never creates an absent session")

        from e2e_attach_service import exercise_attach
        attach = exercise_attach(
            self.binary_dir / "ssmcp", self.env, self.workspace / "child", "worker",
            lambda: self.action("status", "worker"),
            artifacts_dir=self.artifact_dir, browser=os.environ.get("SSMCP_PREVIEW_BROWSER"),
        )
        for description in attach["checks"]:
            self.check(True, description)
        if attach.get("transcript"):
            (self.artifact_dir / "attach-session.ansi").write_bytes(attach["transcript"])
        if attach.get("snapshot"):
            (self.artifact_dir / "attach-session.txt").write_text(attach["snapshot"])
        if attach.get("frame"):
            (self.artifact_dir / "attach-session-frame.json").write_text(json.dumps(attach["frame"]))

        closed = self.call("ssmcp.session.close", {"name":"worker", "expected_generation":worker["generation"]})
        self.check(not closed["manager_active"] and closed["stop_requested"], "atomic close confirms process exit and persists stopped intent")
        old_manager = manager["generation"]
        self.stop_manager()
        self.launch_manager()
        manager = self.connected("manager", old_manager)
        self.connected("race")
        time.sleep(1.2)
        stopped = self.action("status", "worker")
        self.check(not stopped["manager_active"] and stopped["stop_requested"], "closed watchdog session stays stopped after manager restart")
        self.check(manager["generation"] != old_manager, "manager restart restores desired-running session as a fresh generation")
        self.action("start", "worker")
        self.connected("worker")
        self.check(True, "explicit start restores a retained closed session")
        for name in sorted(self.names):
            self.action("remove", name)
        self.names.clear()
        self.check(json.loads(self.cli(["attach", "--json"]).stdout) == [], "explicit remove clears only the owned test definitions")
        self.stop_manager()

    def cleanup(self):
        if self.manager is not None and self.manager.poll() is None:
            for name in sorted(self.names):
                try:
                    self.action("remove", name)
                except Exception:
                    pass
            try:
                self.stop_manager()
            except Exception:
                pass
        super().cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--revision", default="working-tree")
    args = parser.parse_args()
    args.report.parent.mkdir(parents=True, exist_ok=True)
    report = {"revision":args.revision, "platform":sys.platform, "status":"failed", "checks":[]}
    if sys.platform != "linux" or os.geteuid() == 0:
        report.update(status="blocked", error="isolated SSH fixture requires a non-root Linux user")
        args.report.write_text(json.dumps(report, indent=2) + "\n")
        return 2
    with tempfile.TemporaryDirectory(prefix="ssmcp-service-e2e-") as directory:
        harness = Sessions(Path(directory), args.bin_dir, args.report.parent)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            message = str(error)
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[redacted]")
            report["error"] = message
            harness.diagnostics()
            print(f"FAIL {message}", file=sys.stderr)
        finally:
            harness.cleanup()
            report["checks"] = harness.checks
            args.report.write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
