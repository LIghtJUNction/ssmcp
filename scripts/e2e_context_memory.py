#!/usr/bin/env python3
"""Exercise native context checkpoints, forks and automatic restore with an owned real PTY.

Uses the existing isolated SSH/OAuth/client fixture. No external model is invoked,
no user clipboard is read, and only synthetic context is persisted in the fixture.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time

from e2e_presentations import Presentations
from e2e_dashboard import sha256, text


class ContextMemory(Presentations):
    def context(self, action, arguments=None):
        outer, native = self.native("ssmcp.context." + action, arguments or {})
        if native.get("isError"):
            raise AssertionError(f"context {action} failed: {native}")
        return outer, native["structuredContent"]

    def current(self):
        return self.context("read")[1]

    def unchanged_composition(self, expected):
        state = self.ui("/display/conversation")
        return {key: state[key] for key in ("draft", "target", "editing")} == expected

    def rejected(self, action, arguments):
        program = """
try {
  const result = await call(%s, %s, %s);
  return {rejected: result.isError === true};
} catch (error) {
  return {rejected: true, error: String(error)};
}
""" % (json.dumps(self.client_id), json.dumps("ssmcp.context." + action),
       json.dumps(arguments))
        result = self.response(program)["structuredContent"]["result"]
        self.check(result.get("rejected") is True,
                   "a stale workspace revision is rejected without changing the checkpoint")

    def wait_context(self, front, predicate, description):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            value = self.current()
            if predicate(value):
                return value
        raise AssertionError(description)

    def work_with_only_stdout(self):
        return self.response("""
const response = await exec.run(%s, {command:["printf","CONTEXT_WORK_OK\\n"],wait_ms:1000});
return {stdout:response.stdout};
""" % json.dumps(self.client_id))

    def check_memory_envelope(self, response, branch_id, summary):
        content = response["structuredContent"]
        self.check(content["result"] == {"stdout": "CONTEXT_WORK_OK\n"},
                   "automatic memory leaves the caller's stdout-only result unchanged")
        memories = content.get("context_memories", [])
        matching = [item for item in memories if item.get("client_id") == self.client_id]
        self.check(len(matching) == 1
                   and matching[0]["context"]["branch"]["id"] == branch_id
                   and matching[0]["context"].get("summary") == summary,
                   "the outer response retains the active branch memory even when JavaScript returns only stdout")
        self.check(any(block.get("type") == "text"
                       and "SESSION CONTEXT" in block.get("text", "")
                       for block in response["content"]),
                   "restored context is a separate labelled historical-reference content block")
        self.check(not self.messages(response),
                   "restored memory is never fabricated as a newly delivered user requirement")
        self.check(len(json.dumps(matching[0], ensure_ascii=False).encode()) <= 12 * 1024,
                   "the automatic context envelope stays within its fixed byte budget")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.frontend("context-memory", self.config("context-memory"))
        self.oauth_login(front.credentials)
        front.until(lambda shot: shot["alternate_screen"] and "To · next caller" in text(shot),
                    "the real composer in an isolated context workspace")
        initial = self.current()
        original_id = initial["branch"]["id"]
        self.check(initial["active"] is True
                   and Path(initial["workspace"]["path"]) == self.workspace.resolve(),
                   "context is initialized in the exact startup workspace")

        source_text = "CONTEXT_SOURCE: implement the native conversation and preserve Unicode 界面."
        _, answer = self.native("ssmcp.ui.message", {
            "text": source_text, "kind": "answer", "idempotency_key": "context_source",
        })
        self.check(answer["structuredContent"]["accepted"] is True,
                   "a real assistant message enters the conversation through its native tool")
        captured = self.current()
        self.check(any(row.get("role") == "assistant" and row.get("text") == source_text
                       for row in captured["recent"]),
                   "confirmed assistant history is retained as source-backed context")
        count = len(captured["recent"])
        self.native("ssmcp.ui.message", {
            "text": source_text, "kind": "answer", "idempotency_key": "context_source",
        })
        self.check(len(self.current()["recent"]) == count,
                   "idempotent message retry does not duplicate the context source record")

        # The slash command compresses local context without publishing a requirement.
        before_revision = self.current()["revision"]
        front.send(b"/compact\r")
        extracted = self.wait_context(
            front, lambda value: value["revision"] > before_revision
            and value["mode"] == "local_extractive", "the local compact command did not finish",
        )
        self.check(extracted["checkpoint"] is not None and extracted["summary"],
                   "/compact creates a labelled local extractive checkpoint")
        self.check(not self.queue()["queue"],
                   "/compact never submits its command text as an Agent requirement")
        _, saved_history = self.native("ssmcp.ui.history", {"action": "list", "limit": 8})
        self.check(saved_history.get("isError") is not True,
                   "the original durable conversation remains readable after compaction")

        queued_text = "CONTEXT_PENDING_DO_NOT_BROADCAST"
        submitted = self.observe("ssmcp.ui.queue", {"action": "submit", "text": queued_text})
        queued = self.wait_queue_state(front, submitted["id"], "queued")
        for action in ("status", "read", "list"):
            response, _ = self.context(action)
            self.check(not self.messages(response)
                       and queued_text not in json.dumps(response, ensure_ascii=False),
                       f"context {action} neither claims nor automatically broadcasts a pending requirement")
        self.check(self.record(self.queue(), message_id=queued["id"])["status"] == "queued",
                   "context observations keep the pending requirement editable")

        summary = "CONTEXT_CHECKPOINT_ALPHA: next install the validated client; keep exact source identity."
        current = self.current()
        response, compacted = self.context("compact", {
            "summary": summary, "expected_revision": current["revision"],
        })
        self.check(compacted["mode"] == "provided_summary" and compacted["summary"] == summary,
                   "explicit Agent compaction preserves the complete supplied summary and mode")
        self.expect_delivery(response, queued)
        self.wait_queue_state(front, queued["id"], "delivered")

        # Use the normal attachment path; context operations must not consume this draft.
        self.attach(front)
        draft = "CONTEXT_DRAFT 保留文字与图片"
        front.send(draft.encode())
        state = self.wait_ui(front, lambda value: value["draft"]["text"] == draft,
                             "the ordinary text-and-image draft")
        composition = {key: state[key] for key in ("draft", "target", "editing")}
        self.metadata(state["draft"]["images"], "context draft image")

        current = self.current()
        _, forked = self.context("fork", {
            "name": "branch-alpha", "expected_revision": current["revision"],
        })
        fork_id = forked["branch"]["id"]
        self.check(fork_id != original_id and forked["branch"]["parent_id"] == original_id
                   and forked["active"] and forked["summary"] == summary,
                   "fork creates and activates an independent branch with its parent's checkpoint")
        self.check(self.unchanged_composition(composition),
                   "fork preserves the ordinary draft, cursor, image and selected recipient")

        stale = current["revision"]
        before_stale = self.current()
        self.rejected("compact", {"summary": "STALE_MUST_NOT_WIN", "expected_revision": stale})
        self.check(self.current() == before_stale,
                   "a rejected stale compaction preserves all current branch state")

        fork_summary = "CONTEXT_CHECKPOINT_BETA: this belongs only to the fork."
        _, forked = self.context("compact", {
            "summary": fork_summary, "expected_revision": before_stale["revision"],
        })
        _, parent = self.context("read", {"id": original_id})
        self.check(parent["summary"] == summary and parent["active"] is False,
                   "compacting the fork leaves the parent checkpoint unchanged")
        _, switched = self.context("switch", {
            "id": original_id, "expected_revision": self.current()["revision"],
        })
        self.check(switched["branch"]["id"] == original_id and switched["summary"] == summary,
                   "switch restores the exact original branch checkpoint")
        self.check(self.unchanged_composition(composition),
                   "switch and compact leave the text-and-image draft intact")
        self.check_memory_envelope(self.work_with_only_stdout(), original_id, summary)

        # Activate the fork and reexec the real client; normal OAuth login is repeated.
        _, active = self.context("switch", {
            "id": fork_id, "expected_revision": self.current()["revision"],
        })
        self.check(active["summary"] == fork_summary, "the fork retains its independent summary")
        old_id, old_token, old_pid = self.client_id, self.secret, front.process.pid
        self.native("ssmcp.client.reload", {})
        credentials = self.new_credentials(front)
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "real reload reexecs the client and gives it a new connection identity")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        status, _, _ = self.rpc("ping", modern=True, secret=old_token)
        self.check(status == 401, "reload revokes the old login-code grant")
        self.oauth_login(credentials)
        restored = self.current()
        self.check(restored["branch"]["id"] == fork_id
                   and restored["summary"] == fork_summary
                   and restored["mode"] == "provided_summary",
                   "startup automatically restores the active fork and durable checkpoint")
        front.until(lambda shot: draft in text(shot), "the draft after actual reexec")
        restored_composition = self.wait_ui(
            front, lambda value: value["draft"]["text"] == draft,
            "the restored ordinary draft",
        )
        self.check(restored_composition["draft"]["images"] == composition["draft"]["images"],
                   "automatic memory recovery preserves the separate draft image handoff")
        self.check_memory_envelope(self.work_with_only_stdout(), fork_id, fork_summary)
        _, branches = self.context("list")
        self.check(branches["active_id"] == fork_id
                   and {row["branch"]["id"] for row in branches["branches"]} >= {original_id, fork_id},
                   "both context branches survive reexec with the correct active selection")
        final, termios = front.disconnect()
        self.check(not final["alternate_screen"] and termios == front.original_termios
                   and front.process.returncode == 0,
                   "the context scenario exits cleanly and restores the owned physical terminal")
        self.metrics["context"] = {
            "pty_bytes": len(front.transcript),
            "pty_sha256": hashlib.sha256(front.transcript).hexdigest(),
            "branch_count": len(branches["branches"]),
            "memory_mode": restored["mode"],
        }
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("context E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for path in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not path.is_file():
            parser.error(f"missing {path}; build the client, server and vt_snapshot first")
    root = Path(__file__).resolve().parents[1]
    started = time.monotonic()
    report = {
        "schema": "ssmcp.context-e2e.v1", "status": "failed",
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "platform": platform.system(),
        "scope": "local isolated real client/server, SSH/OAuth and PTY; synthetic context; no models",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"),
            ("vt_snapshot", decoder),
        )},
        "sources": {name: sha256(root / name) for name in (
            "scripts/e2e_context_memory.py", "scripts/e2e_presentations.py",
            "scripts/e2e_attachments.py", "scripts/e2e_composer.py",
            "scripts/e2e_dashboard.py", "scripts/e2e.py",
            "src/client/context_memory.rs", "src/code_mode.rs", "src/http.rs", "src/runtime.rs",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-context-e2e-") as directory:
        harness = ContextMemory(Path(directory), args.bin_dir, decoder)
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
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} context checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
