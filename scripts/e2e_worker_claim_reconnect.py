#!/usr/bin/env python3
"""Reconcile an interrupted external worker using two private, headless pools."""
import argparse
import json
import selectors
from pathlib import Path
import subprocess
import tempfile

from e2e import wait_for
from e2e_worker_windows import WorkerWindows


def git(workspace, *args):
    return subprocess.check_output(["git", "-C", str(workspace), *args],
                                   text=True).strip()


def run(harness):
    harness.prepare()
    workspace = harness.directory / "workspace"
    workspace.mkdir()
    git(workspace, "init", "--quiet")
    environment = harness.environment("pool", workspace, terminal=False)
    home = harness.directory / "home"
    home.mkdir()
    environment["HOME"] = str(home)
    command = harness.client_command(name="fixture-pool", extra_args=(
        "--worker", "1", "--input", "none", "--no-watchdog"))

    def connect(label, previous=None):
        process = harness.spawn(label, command, cwd=workspace, env=environment, stdout=subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=15):
                raise AssertionError("headless pool did not publish its connection record")
        record = harness.validate_connection_record(json.loads(process.stdout.readline()))
        client_id = record["client_id"]
        visible = next((c for c in harness.sessions() if c["id"] == client_id), None)
        harness.check(visible is not None and visible["cwd"] == str(workspace) and client_id != previous,
                      f"{label} process receipt matches the account inventory and project directory")
        return process, client_id

    original_process, original = connect("original")
    harness.oauth_login({"client_id": original})
    manager = harness.join(original, "coordinator")
    worker = harness.join(original, "executor", role="worker")
    harness.bound(original, manager, "ssmcp.worker.attach")
    pending = {"request_id": "fixture-original-wait", "wait_ms": 0}
    first = harness.bound(original, worker, "ssmcp.worker.wait", pending)
    harness.check(first["status"] == "waiting", "original member registers a standby station")
    task = harness.bound(original, manager, "ssmcp.worker.submit",
                         {"request_id": "fixture-task", "text": "Commit one fixture effect."})["task"]
    assigned = harness.bound(original, worker, "ssmcp.worker.wait", pending)
    claim = assigned["claim"]
    harness.check(assigned["status"] == "assigned" and claim["task_id"] == task["task_id"],
                  "worker receives the original task and exact claim")
    same = harness.bound(original, worker, "ssmcp.worker.wait", pending)
    harness.check(same["claim"] == claim, "original member binds again and recovers the exact claim")
    argv = ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", task["task_id"]]
    bound_worker = {"client_id": original, "id": worker["agent_id"], "token": worker["token"]}
    code = "return await exec.run(%s,%s)" % (json.dumps(original),
            json.dumps({"command": argv, "cwd": str(workspace), "wait_ms": 5000}))
    effect = harness.public_call("code_mode", {"agent": bound_worker, "code": code})["result"]
    harness.check(effect["exit_code"] == 0 and not effect["running"], "owned worker creates one fixture commit")
    commit = git(workspace, "rev-parse", "HEAD")
    # The process dies after the durable effect and before worker.complete.
    original_process.kill()
    original_process.wait(timeout=5)
    wait_for(lambda: all(c["id"] != original for c in harness.sessions()), timeout=15,
             description="original connection removed")
    _, replacement = connect("replacement", original)
    status, rpc, _ = harness.rpc("tools/call", {"name": "collab", "arguments": {
        "action": "receive", "client_id": replacement,
        "agent_id": worker["agent_id"], "token": worker["token"]}}, modern=True)
    denied = rpc.get("error") or (rpc.get("result") if rpc.get("result", {}).get("isError") else {})
    harness.check(status == 200 and "agent credentials are invalid" in json.dumps(denied), "replacement rejects the original member credentials")
    fresh = harness.join(replacement, "executor", role="worker")
    fresh_manager = harness.join(replacement, "coordinator")
    state = harness.bound(replacement, fresh_manager, "ssmcp.worker.attach")["worker"]
    harness.check(fresh["agent_id"] != worker["agent_id"] and state["generation"] != claim["generation"],
                  "same worker role rejoins with a new member and pool generation")
    harness.check(state["working"] == 0 and state["queued"] == 0, "replacement starts with an empty pool")
    missing = harness.bound(replacement, fresh_manager, "ssmcp.worker.result",
                            {"task_id": task["task_id"]}, allow_error=True,
                            expected_error="worker task is not retained for this authenticated caller")
    harness.check(missing.get("isError") is True, "replacement has no completion receipt for the original task")
    after = harness.bound(replacement, fresh, "ssmcp.worker.wait", pending)
    harness.check(after["status"] == "waiting" and not after.get("claim"),
                  "replacement member binds and the prior pending ID does not replay the original claim")
    harness.check(git(workspace, "rev-parse", "HEAD") == commit
                  and git(workspace, "rev-list", "--count", "HEAD") == "1"
                  and git(workspace, "log", "-1", "--format=%s") == claim["task_id"],
                  "original task and claim reconcile to exactly one unchanged commit")
    harness.check(not harness.external_invocations("pool"), "no external model was invoked")
    return {"task_id": task["task_id"], "claim": claim, "commit": commit,
            "effect": "verified", "completion_receipt": None, "completion_outcome": "unknown"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ssmcp-worker-reconnect-") as tmp:
        directory = Path(tmp)
        harness = WorkerWindows(directory, args.bin_dir, None, args.fixture_binary.resolve(), directory, [])
        try:
            evidence = run(harness)
        finally:
            harness.cleanup()
        print(json.dumps(evidence, sort_keys=True))


if __name__ == "__main__":
    main()
