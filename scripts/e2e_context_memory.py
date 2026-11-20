#!/usr/bin/env python3
"""Exercise wire-7 context memory using the existing isolated real PTY fixture.

Requires newly built client AND relay binaries with the context APIs. This never
tests or deploys the shared relay. Uses one durable JSONL history and private
@file image references; no inline UserImage, model, browser, or real clipboard.
Run with --bin-dir DIR --report FILE; --out DIR stores actual terminal snapshots.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import tempfile
import time

from e2e_input_history import InputHistory, png_fixture
from e2e_client_ui import empty_composer_ready
from e2e_dashboard import sha256, text


class ContextMemory(InputHistory):
    def native(self, name, arguments=None):
        """Keep the actual MCP envelope; callers inspect failure and side metadata."""
        response = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments or {}),
        ))
        native = response["structuredContent"]["result"]
        if not isinstance(native, dict):
            raise AssertionError(f"{name} returned no native MCP envelope")
        return response, native

    def wait_queue_state(self, front, message_id, expected):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            record = self.record(self.queue(), message_id=message_id)
            if record["status"] == expected:
                return record
        raise AssertionError(f"request {message_id} did not become {expected}")

    def prepare_reference_draft(self, front):
        source = self.workspace / "context-fixture.png"
        source.write_bytes(png_fixture())
        front.send("/attach context-fixture.png\r")
        pattern = r'\n@"(\.ssmcp/inputs/image-[0-9a-f]{32}\.png)"\n'
        state = self.wait_state(
            front, lambda value: re.fullmatch(pattern, value["draft"]["text"]) is not None,
            "the context fixture's private image reference",
        )
        reference = state["draft"]["text"]
        self.context_reference_path = re.fullmatch(pattern, reference).group(1)
        image = self.workspace / self.context_reference_path
        self.check(image.read_bytes() == png_fixture() and image.stat().st_mode & 0o777 == 0o600,
                   "the context draft uses a private workspace PNG reference with unchanged bytes")
        _, native = self.native("ssmcp.fs.read_image", {"path": self.context_reference_path})
        images = [item for item in native.get("content", []) if item.get("type") == "image"]
        self.check(not native.get("isError") and len(images) == 1
                   and images[0]["mimeType"] == "image/png"
                   and base64.b64decode(images[0]["data"], validate=True) == png_fixture(),
                   "the referenced PNG is readable through the existing rooted MCP image tool")
        draft = reference + "CONTEXT_DRAFT 保留文字与图片引用"
        front.send("CONTEXT_DRAFT 保留文字与图片引用")
        state = self.wait_state(front, lambda value: value["draft"]["text"] == draft,
                                "the ordinary text-and-reference draft")
        self.check(state["draft"]["cursor"] == len(draft.encode("utf-8"))
                   and "images" not in state["draft"],
                   "wire-7 composition stores its image as text and preserves the UTF-8 cursor")
        self.save_shot("context-draft", front)
        return draft, {key: state[key] for key in ("draft", "target", "editing")}

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
  return {rejected: result.isError === true,
    error: (result.content || []).filter(x => x.type === "text").map(x => x.text).join("\\n")};
} catch (error) {
  return {rejected: true, error: String(error)};
}
""" % (json.dumps(self.client_id), json.dumps("ssmcp.context." + action),
       json.dumps(arguments))
        result = self.response(program)["structuredContent"]["result"]
        self.check(result.get("rejected") is True and "revision" in result.get("error", "").lower(),
                   "a stale workspace revision is rejected with an explicit revision error")

    def wait_context(self, front, predicate, description):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            value = self.current()
            if predicate(value):
                return value
        raise AssertionError(description)

    def work_with_only_stdout(self):
        # Two registered native tool families must yield one context envelope.
        return self.response(
            "const stat=await call(%s,'ssmcp.fs.stat',%s);"
            "if(stat.isError)throw new Error('context fixture stat failed');"
            "const response=await exec.run(%s,%s);return {stdout:response.stdout};" % (
                json.dumps(self.client_id), json.dumps({"path": self.context_reference_path}),
                json.dumps(self.client_id), json.dumps({
                    "command": ["/usr/bin/printf", "CONTEXT_WORK_OK\n"], "wait_ms": 1000,
                }),
            )
        )

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
        self.check(len(memories) == 1
                   and len(json.dumps(matching[0], ensure_ascii=False, separators=(",", ":")).encode()) <= 12 * 1024
                   and len(json.dumps(memories, ensure_ascii=False, separators=(",", ":")).encode()) <= 64 * 1024,
                   "multiple native tool families yield one context within the 12 KiB item and 64 KiB total budgets")
        self.check(matching[0]["context"]["provenance"]["model_invoked"] is False
                   and matching[0]["context"]["provenance"]["queue_replayed"] is False,
                   "the automatic memory describes local historical text, not a new model run or request")


    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.frontend("context-memory", self.config("context-memory"))
        self.oauth_login(front.credentials)
        front.until(empty_composer_ready,
                    "the real composer in an isolated context workspace")
        initial = self.current()
        original_id = initial["branch"]["id"]
        self.check(initial["active"] is True
                   and Path(initial["workspace"]["path"]) == self.workspace.resolve(),
                   "context is initialized in the exact startup workspace")

        # The source is historical assistant context, not a final-answer request.
        # Keep the JSONL identity/digest and retry checks independent of completion grants.
        source_text = "CONTEXT_SOURCE: implement the native conversation and preserve Unicode 界面."
        _, answer = self.native("ssmcp.ui.message", {
            "text": source_text, "kind": "progress", "idempotency_key": "context_source",
        })
        receipt = answer["structuredContent"]
        self.check(not answer.get("isError") and receipt["accepted"] is True
                   and receipt["kind"] == "progress"
                   and receipt["history_saved"] is True and receipt["duplicate"] is False,
                   "a real assistant message enters the shared durable conversation through its native tool")
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        current_session = next(session for session in sessions if session["current"])
        history_path = self.state_home / "ssmcp" / "conversations" / (current_session["id"] + ".jsonl")
        source_journal = history_path.read_bytes()
        entries = [json.loads(line) for line in source_journal.splitlines()]
        self.check(entries[0]["schema"] == "ssmcp.conversation.v1"
                   and sum(entry.get("id") == receipt["message_id"] and entry.get("content") == source_text
                           and entry.get("role") == "assistant" for entry in entries) == 1,
                   "the context source is an identified record in the existing versioned JSONL")
        captured = self.current()
        self.check(any(row.get("role") == "assistant" and row.get("text") == source_text
                       and row["source"]["conversation_id"] == current_session["id"]
                       and row["source"]["message_id"] == receipt["message_id"]
                       and row["source"]["sha256"] == hashlib.sha256(source_text.encode()).hexdigest()
                       for row in captured["recent"]),
                   "assistant context carries the actual JSONL conversation, message identity and text digest")
        count = len(captured["recent"])
        self.native("ssmcp.ui.message", {
            "text": source_text, "kind": "progress", "idempotency_key": "context_source",
        })
        self.check(len(self.current()["recent"]) == count and history_path.read_bytes() == source_journal,
                   "idempotent message retry duplicates neither the context source nor its shared JSONL")

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
        self.check(saved_history.get("isError") is not True and history_path.read_bytes() == source_journal,
                   "local extractive compaction leaves the original durable JSONL unchanged")

        queued_text = "CONTEXT_PENDING_DO_NOT_BROADCAST"
        submitted = self.observe("ssmcp.ui.queue", {"action": "submit", "text": queued_text})
        queued = self.wait_queue_state(front, submitted["id"], "queued")
        for action in ("status", "read", "list"):
            response, value = self.context(action)
            self.check(not self.messages(response)
                       and queued_text not in json.dumps(response, ensure_ascii=False),
                       f"context {action} neither claims nor automatically broadcasts a pending requirement")
            projected_response, projected_native = self.native(
                "ssmcp.ui.get", {"path": "/context/" + action},
            )
            projection = projected_native["structuredContent"]
            self.check(not projected_native.get("isError")
                       and projection["schema_version"] == 1
                       and projection["path"] == "/context/" + action
                       and projection["value"] == value,
                       f"ui.get /context/{action} matches the registered native context API")
            self.check(not self.messages(projected_response)
                       and queued_text not in json.dumps(projected_response, ensure_ascii=False),
                       f"unbound /context/{action} projection preserves Next caller requirements")

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

        delivered = self.wait_context(front, lambda value: any(
            row.get("role") == "user" and row.get("text") == queued_text
            and row.get("delivery", {}).get("request_id") == queued["id"]
            and row.get("delivery", {}).get("target") == "next"
            for row in value["recent"]), "confirmed Next caller delivery did not enter memory")
        self.check(bool(delivered["recent"]),
                   "only confirmed Next caller delivery makes the queued source eligible for shared memory")

        # Use the current attachment path: an ordinary draft containing an @file.
        draft, composition = self.prepare_reference_draft(front)
        branch_journal = history_path.read_bytes()

        current = self.current()
        _, forked = self.context("fork", {
            "name": "branch-alpha", "expected_revision": current["revision"],
        })
        fork_id = forked["branch"]["id"]
        self.check(fork_id != original_id and forked["branch"]["parent_id"] == original_id
                   and forked["active"] and forked["summary"] == summary,
                   "fork creates and activates an independent branch with its parent's checkpoint")
        self.check(self.unchanged_composition(composition),
                   "fork preserves the ordinary draft, UTF-8 cursor, image reference and recipient")

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
                   "switch and compact leave the text-and-reference draft intact")
        self.check_memory_envelope(self.work_with_only_stdout(), original_id, summary)
        self.check(history_path.read_bytes() == branch_journal,
                   "context forks, switches and provided summaries do not rewrite original JSONL records")

        # Activate the fork and reexec the real client under the same account grant.
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
        self.check(status == 200 and self.secret == old_token,
                   "reload preserves the independent account OAuth grant")
        listed = self.public_call("list", secret=old_token)["clients"]
        self.check(any(item["id"] == self.client_id for item in listed)
                   and all(item["id"] != old_id for item in listed),
                   "the account grant sees the replacement without a stale client route")
        restored = self.current()
        self.check(restored["branch"]["id"] == fork_id
                   and restored["summary"] == fork_summary
                   and restored["mode"] == "provided_summary",
                   "startup automatically restores the active fork and durable checkpoint")
        front.until(lambda shot: "CONTEXT_DRAFT" in text(shot), "the draft marker after actual reexec")
        restored_composition = self.wait_state(
            front, lambda value: value["draft"]["text"] == draft,
            "the restored ordinary draft",
        )
        self.check({key: restored_composition[key] for key in ("draft", "target", "editing")} == composition
                   and (self.workspace / self.context_reference_path).read_bytes() == png_fixture()
                   and history_path.read_bytes() == branch_journal,
                   "reexec preserves the ordinary @file draft and original JSONL without inline images or replay")
        self.save_shot("context-restored", front)
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
    parser.add_argument("--out", type=Path, help="actual VT snapshots; defaults beside the report")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for path in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not path.is_file():
            parser.error(f"missing {path}; build the client, server and vt_snapshot first")
    root = Path(__file__).resolve().parents[1]
    started = time.monotonic()
    report = {
        "schema": "ssmcp.context-e2e.v1", "status": "failed",
        "source_commit": subprocess.check_output(["/usr/bin/git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "platform": platform.system(),
        "scope": "new isolated wire-7 client/relay, shared JSONL, @file images, context native/projection APIs and bounded cross-tool memory; no models",
        "shared_relay_tested": True,
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"),
            ("vt_snapshot", decoder),
        )},
        "sources": {name: sha256(root / name) for name in (
            "scripts/e2e_context_memory.py", "scripts/e2e_input_history.py",
            "scripts/e2e_composer.py",
            "scripts/e2e_dashboard.py", "scripts/e2e.py",
            "src/client/context_memory.rs", "src/client/interface/history.rs",
            "src/code_mode.rs", "src/http.rs", "src/runtime.rs",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-context-e2e-") as directory:
        output = args.out or args.report.parent / args.report.stem
        harness = ContextMemory(Path(directory), args.bin_dir, decoder, output)
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
                try:
                    harness.save_shot("context-failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
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
