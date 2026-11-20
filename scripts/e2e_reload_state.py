#!/usr/bin/env python3
"""Verify actual client reexec preserves the local conversation, draft and UI.

Build ssmcp, ssmcpd and examples/vt_snapshot first. This uses the existing
temporary account/PTY fixture, private HOME/XDG paths, and no model or harness.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time

from e2e_client_ui import prompt_start
from e2e_composer import DOWN, HOME, LEFT, UP
from e2e_dashboard import sha256, text
from e2e_input_history import InputHistory


class ReloadState(InputHistory):
    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.start_front("reload-state")
        accepted = "RELOAD_ACCEPTED_REQUEST 界"
        record = self.submit_source(front, accepted)
        self.expect_delivery(self.work(), record, agent_id=None)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.record(self.queue(), message_id=record["id"])["status"] == "delivered":
                break
        else:
            raise AssertionError("initial delivery was not observed before reload")
        reply = "RELOAD_RETAINED_REPLY 界"
        self.observe("ssmcp.ui.message", {
            "text": reply, "kind": "progress", "idempotency_key": "reload-retained-reply",
        })
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        current = next(session for session in sessions if session["current"])
        history_path = self.state_home / "ssmcp" / "conversations" / (current["id"] + ".jsonl")
        history_before = history_path.read_bytes()
        messages_before = self.observe("ssmcp.ui.history", {
            "action": "read", "id": current["id"], "limit": 64,
        })["messages"]

        # A non-default column and input height make a silently recreated UI fail.
        self.activate_column(front, "conversation")
        original_columns = len(self.ui("/display/conversation")["canvas"]["columns"])
        front.send("/new chat\r")
        self.wait_state(front, lambda state: len(state["canvas"]["columns"]) == original_columns + 1,
                        "the additional conversation column did not open")
        draft = "RELOAD_UNSENT_DRAFT 界"
        front.send(draft)
        self.wait_state(front, lambda state: state["draft"]["text"] == draft,
                        "the Unicode reload draft did not appear")
        front.send(LEFT + b"\x1b[1;3A" * 2)
        before = self.wait_state(
            front, lambda state: state["draft"]["cursor"] == len(draft.encode()) - len("界".encode()),
            "the reload draft cursor did not move over the Unicode character",
        )
        shot_before = self.save_resized_shots("reload-before", front, "RELOAD_UNSENT_DRAFT")
        input_start = prompt_start(shot_before)
        old_id, old_pid = self.client_id, front.process.pid

        # Invoke the same native lifecycle as /reload without replacing the draft
        # with a slash command. e2e_composer independently covers parked /reload.
        acknowledgement = self.call("ssmcp.client.reload", {})
        self.check(acknowledgement["accepted"], "native reload accepted the prepared local state")
        credentials = self.new_credentials(front)
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "the owned process actually reexecutes and receives a fresh client identity")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        after = self.wait_state(front, lambda state: state["draft"] == before["draft"],
                                "reexec did not restore the draft and UTF-8 cursor")
        self.check(after["canvas"] == before["canvas"] and after["view"] == before["view"]
                   and after["focus"] == before["focus"],
                   "reexec preserves the exact column IDs, order, active panel and focus")
        shot_after = front.until(
            lambda shot: reply in text(shot) and prompt_start(shot) == input_start,
            "retained conversation text and input height in the restarted terminal",
        )
        self.save_resized_shots("reload-after", front, "RELOAD_UNSENT_DRAFT")
        self.check(accepted in text(shot_after),
                   "both retained user and assistant messages return to the conversation")
        resumed = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        self.check([session["id"] for session in resumed if session["current"]] == [current["id"]],
                   "the replacement resumes the exact active local conversation")
        self.check(history_path.read_bytes() == history_before,
                   "display restoration leaves the original JSONL bytes unchanged")
        self.check(not self.messages(self.work()),
                   "reexec never redelivers the already delivered requirement")

        # Exercise real Up/Down navigation, not only a serialized history field.
        front.send(HOME + UP)
        self.wait_state(front, lambda state: state["draft"]["text"] == accepted,
                        "accepted input history was not restored")
        front.send(DOWN)
        restored_draft = self.wait_state(front, lambda state: state["draft"]["text"] == draft,
                                         "history navigation lost the unsent draft")
        self.check(restored_draft["draft"]["cursor"] == 0,
                   "history navigation restores the parked Unicode draft and cursor")

        front.send(b"\x01\x0b")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "the owned draft did not clear before continuation")
        continuation = "RELOAD_CONTINUATION 界"
        continued = self.submit_source(front, continuation)
        self.expect_delivery(self.work(), continued, agent_id=None)
        history = self.observe("ssmcp.ui.history", {
            "action": "read", "id": current["id"], "limit": 64,
        })
        self.check(history["messages"][:-1] == messages_before
                   and history["messages"][-1]["content"] == continuation,
                   "new messages append once to the same retained conversation after reload")
        final, restored_termios = front.disconnect()
        self.check(not final["alternate_screen"] and restored_termios == front.original_termios
                   and front.process.returncode == 0,
                   "normal shutdown restores the owned terminal after reload")
        self.metrics["history_messages"] = len(history["messages"])
        self.metrics["canvas_columns"] = len(after["canvas"]["columns"])
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("reload E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd", "vt_snapshot": decoder}
    for executable in binaries.values():
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "scope": "actual reexec, conversation identity/contents, Unicode draft/input history and canvas; no models",
        "binaries": {name: sha256(path) for name, path in binaries.items()},
        "sources": {name: sha256(Path(__file__).with_name(name)) for name in (
            "e2e_reload_state.py", "e2e_input_history.py", "e2e_composer.py",
            "e2e_dashboard.py", "e2e.py",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-reload-state-e2e-") as directory:
        harness = ReloadState(Path(directory), args.bin_dir, decoder, args.out)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            description = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    description = description.replace(secret, "[redacted]")
            report["error"] = description
            print(description, flush=True)
            if harness.last_frontend and not harness.last_frontend.closed:
                harness.save_shot("reload-failure", harness.last_frontend, cells=True)
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(
                json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8",
            )
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"Passed {len(report['checks'])} reload state checks", flush=True)


if __name__ == "__main__":
    main()
