#!/usr/bin/env python3
"""Focused PTY acceptance; import the frozen checkout's existing harness.

Run with PYTHONPATH=<frozen-checkout>/scripts and the existing
e2e_interactions.py CLI arguments. This runner never builds any executable.
"""

import concurrent.futures
import os
from pathlib import Path
import subprocess
import threading
import time

import e2e_interactions as suite
from e2e_client_ui import body_text, prompt_text
from e2e_composer import ENTER


def focused_run(self):
    runner_hash = suite.digest(Path(__file__))
    self.prepare()
    self.workspace = self.directory / "workspace"
    self.workspace.mkdir()
    subprocess.run(
        ["git", "-c", "init.defaultBranch=main", "init", "-q"],
        cwd=self.workspace,
        env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
    )
    front = self.start_front("PTY-focused-latency")
    samples = []
    pending_send = {"text": None}
    last_input = {"ns": None, "before": None}
    epoch = time.monotonic_ns()

    def wait_frame(label, started, predicate, *, budget_ms=500):
        # Only physical PTY/VT observation occurs between input and this frame.
        # The normal snapshot helper intentionally pumps 50 ms; use 5 ms here.
        if last_input["ns"] != started:
            raise AssertionError("the physical input timestamp changed before " + label)
        if predicate(last_input["before"]):
            raise AssertionError("the target frame was already true before " + label)
        deadline = started + 9_000_000_000
        before = front.input_bytes
        while time.monotonic_ns() < deadline:
            front.pump(0.005)
            shot = front.decoder.request(op="snapshot", cells=False)
            if predicate(shot):
                ended = time.monotonic_ns()
                self.check(front.input_bytes == before,
                           label + ": no extra input during physical observation")
                samples.append({
                    "action": label,
                    "input_ms": (started - epoch) / 1_000_000,
                    "visible_ms": (ended - epoch) / 1_000_000,
                    "latency_ms": (ended - started) / 1_000_000,
                    "budget_ms": budget_ms,
                    "within_budget": ended - started < budget_ms * 1_000_000,
                })
                return shot
            if front.process.poll() is not None:
                raise AssertionError("fixture client exited during " + label)
        raise AssertionError("physical terminal timed out during " + label)

    original_send = front.send

    def send(data):
        raw = data.encode() if isinstance(data, str) else data
        # Record the physical precondition before starting the input timer.
        # This avoids measuring an already-satisfied predicate as a fast action.
        front.pump(0.005)
        before = front.decoder.request(op="snapshot", cells=False)
        started = time.monotonic_ns()
        original_send(data)
        last_input["ns"] = started
        last_input["before"] = before
        source = pending_send["text"]
        if raw == ENTER and source is not None:
            wait_frame(
                "send_accepted_frame", started,
                lambda shot: source in body_text(shot)
                and source not in prompt_text(shot)
                and shot["rows"][-1].lstrip().startswith("Queued ·"),
            )

    front.send = send
    original_queue_current = self.queue_current

    def queue_current(frontend, source):
        pending_send["text"] = source
        try:
            return original_queue_current(frontend, source)
        finally:
            pending_send["text"] = None

    self.queue_current = queue_current
    original_quiet_until = self.quiet_until
    timed_descriptions = {
        "server-confirmed Up recall": "up_confirmed_takeback_frame",
        "resubmitting the edit": "edited_send_accepted_frame",
        "Up after delivery": "up_delivered_input_history_frame",
    }

    def quiet_until(frontend, predicate, description, *, timeout=9):
        label = next((value for key, value in timed_descriptions.items()
                      if description.startswith(key)), None)
        if label is None:
            return original_quiet_until(frontend, predicate, description, timeout=timeout)
        shot = wait_frame(label, last_input["ns"], predicate)
        self.quiet_checks.append(description)
        return shot

    self.quiet_until = quiet_until
    recalled = self.pending_recall(front)

    # Exercise local mouse/input response while real history-producing native
    # calls are in flight. This is workload overlap, not proof of the exact
    # storage mutex acquisition instant; the Rust lock test proves that part.
    writes = []
    ready = threading.Event()
    stop = threading.Event()

    def write_history():
        for index in range(8):
            if stop.is_set():
                break
            started = time.monotonic_ns()
            if index == 1:
                ready.set()
            result = self.observe("ssmcp.ui.message", {
                "text": "PTY_WRITE_LOAD_" + str(index) + " " + "x" * 1024,
                "kind": "progress", "idempotency_key": "pty-latency-" + str(index),
            })
            ended = time.monotonic_ns()
            if not result.get("history_saved"):
                raise AssertionError("fixture progress message was not saved to history")
            writes.append({"start_ms": (started - epoch) / 1_000_000,
                           "end_ms": (ended - epoch) / 1_000_000,
                           "history_saved": True})

    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        writer = executor.submit(write_history)
        try:
            ready_deadline = time.monotonic() + 15
            while not ready.is_set() and not writer.done() and time.monotonic() < ready_deadline:
                front.pump(0.01)
            if not ready.is_set():
                writer.result(timeout=1)
                raise AssertionError("history writer did not reach its second real call")
            before_click = len(samples)
            front.send(b"\x1b[<0;4;8M\x1b[<0;4;8m")
            wait_frame("mouse_content_focus", last_input["ns"],
                       lambda shot: shot["hide_cursor"], budget_ms=250)
            rows = front.decoder.request(op="snapshot")["size"][0]
            row = rows - 2
            front.send((f"\x1b[<0;4;{row}M\x1b[<0;4;{row}m").encode())
            wait_frame("mouse_composer_focus", last_input["ns"],
                       lambda shot: not shot["hide_cursor"], budget_ms=250)
            marker = "PTY_LOCAL_INPUT_RESPONSE"
            front.send(marker)
            wait_frame("typed_draft_frame", last_input["ns"],
                       lambda shot: marker in prompt_text(shot), budget_ms=250)
        finally:
            stop.set()
            finish_deadline = time.monotonic() + 15
            while not writer.done() and time.monotonic() < finish_deadline:
                front.pump(0.01)
            writer.result(timeout=1)

    for sample in samples[before_click:]:
        sample["overlapped_history_call"] = any(
            write["start_ms"] <= sample["input_ms"] < write["end_ms"]
            for write in writes
        )
    self.check(any(item["overlapped_history_call"] for item in samples[before_click:]),
               "at least one physical input arrived during a real history-producing call")
    self.check(suite.digest(Path(__file__)) == runner_hash,
               "the focused acceptance runner remained unchanged")
    return {
        "status": "passed" if all(item["within_budget"] for item in samples)
        else "latency_budget_exceeded",
        "checks": len(self.checks), "passed_checks": self.checks,
        "frames": self.frames, "quiet_checks": self.quiet_checks,
        "pending_recall": recalled, "latency_samples": samples,
        "history_write_calls": writes,
        "focused_runner_sha256": runner_hash,
        "timing": "monotonic physical PTY write to first matching VT frame; 5 ms pump plus decoder overhead",
        "budgets": "diagnostic targets: 250 ms local input; 500 ms loopback accepted send/takeback",
        "transport": "existing production account router, native tools, WebSocket and owned PTY",
        "models_started": False, "existing_clients_reloaded": False,
        "existing_services_touched": False, "real_user_credentials_used": False,
    }


def main():
    run_status = None

    def run(self):
        nonlocal run_status
        report = focused_run(self)
        run_status = report["status"]
        return report

    suite.Interactions.run = run
    suite.main()
    # Preserve the existing report/hash checks and cleanup before failing a
    # diagnostic latency budget; the base suite does not enforce report.status.
    return 0 if run_status == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
