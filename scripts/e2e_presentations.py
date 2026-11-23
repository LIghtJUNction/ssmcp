#!/usr/bin/env python3
"""Verify structured presentations across the real MCP, client, PTY and clipboard.

Run after building ssmcp, ssmcpd and examples/vt_snapshot:
  python3 scripts/e2e_presentations.py --bin-dir target/debug \
    --report /tmp/presentations.json --out /tmp/presentation-snapshots

Reuses InputHistory's isolated state and owned account-router/OAuth/PTY fixture.
A private clipboard writer captures exact /copy bytes. No model, real display,
synthetic message buffer, or production server is used.
The two bounded VT cell snapshots can be rendered by render_ui_snapshot.py.
"""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time

from e2e_composer import journal_value
from e2e_input_history import InputHistory
from e2e_dashboard import sha256, text


def document(label):
    full_cell = f"{label}_COMPLETE_CELL_完整说明_must_survive_visual_shortening_and_copy"
    return {
        "title": f"PRESENT_{label}",
        "subtitle": "Native structured fixture",
        "blocks": [
            {"type": "text", "text": f"TEXT_{label} · 界面 content"},
            {"type": "card", "title": f"CARD_{label}",
             "body": "Card body remains readable.", "tone": "success"},
            {"type": "metrics", "items": [
                {"label": f"METRIC_{label}", "value": "42", "detail": "passed"},
                {"label": "Version", "value": "0.9.0", "detail": "fixture"},
            ]},
            {"type": "table", "columns": ["Name", "State", "Detail", "Scope", "Result"],
             "rows": [[f"ROW_{label}", "ready", full_cell, "local", "passed"]]},
            {"type": "progress", "label": f"PROGRESS_{label}", "value": 62.5,
             "detail": "Source and artifact checked"},
            {"type": "steps", "items": [
                {"label": f"STEP_{label}", "status": "done"},
                {"label": "Install", "status": "running"},
                {"label": "Probe", "status": "pending"},
            ]},
            {"type": "bar_chart", "title": f"BAR_{label}",
             "items": [{"label": "Rust", "value": 8}, {"label": "UI", "value": 3}],
             "unit": "checks"},
        ],
    }


def expected_text(label):
    # A concrete fixture expectation, independent of terminal cell truncation.
    return (
        f"PRESENT_{label}\nNative structured fixture\n\n"
        f"TEXT_{label} · 界面 content\n\n"
        f"[SUCCESS] CARD_{label}\nCard body remains readable.\n\n"
        f"METRIC_{label}: 42 (passed)\nVersion: 0.9.0 (fixture)\n\n"
        "Name | State | Detail | Scope | Result\n"
        f"ROW_{label} | ready | {label}_COMPLETE_CELL_完整说明_"
        "must_survive_visual_shortening_and_copy | local | passed\n\n"
        f"PROGRESS_{label}: 62.5%\nSource and artifact checked\n\n"
        f"[done] STEP_{label}\n[running] Install\n[pending] Probe\n\n"
        f"BAR_{label}\nRust: 8 checks\nUI: 3 checks"
    )


class Presentations(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        # InputHistory's image launcher removes NO_COLOR. Use the real binary
        # directly so Frontend's NO_COLOR=1 overrides this suite's color=true.
        self.binary_dir = Path(binary_dir).resolve()
        self.copied = self.directory / "copied.txt"
        self.snapshots = []
        self.presenter = None

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET",
                    "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"):
            environment.pop(key, None)
        environment["WAYLAND_DISPLAY"] = "ssmcp-presentation-private-fixture"
        environment["SSMCP_PRESENT_COPY"] = str(self.copied)
        writer = Path(environment["PATH"]) / "wl-copy"
        writer.write_text(
            "#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\n"
            "data=sys.stdin.buffer.read(16385)\n"
            "if len(data)>16384: raise SystemExit(2)\n"
            "Path(os.environ['SSMCP_PRESENT_COPY']).write_bytes(data)\n",
            encoding="utf-8",
        )
        writer.chmod(0o700)
        return environment

    def config(self, name, **_options):
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=true\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=false\n', encoding="utf-8"
        )
        return path

    def presenter_binding(self):
        if self.presenter is None:
            raise AssertionError("the real presenter must be registered before native publication")
        return {"client_id": self.client_id, "id": self.presenter["agent_id"],
                "token": self.presenter["token"]}

    def native(self, name, arguments):
        result = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments),
        ), agent=self.presenter_binding())
        native = result["structuredContent"]["result"]
        return result, native

    def present(self, payload):
        result, native = self.native("ssmcp.ui.present", payload)
        if native.get("isError"):
            raise AssertionError(f"ui.present failed: {native}")
        acknowledgement = native["structuredContent"]
        self.check(acknowledgement.get("accepted") is True
                   and isinstance(acknowledgement.get("message_id"), str)
                   and acknowledgement.get("kind") == "answer"
                   and acknowledgement.get("finish_request_id") is None
                   and acknowledgement.get("recorded") is True
                   and acknowledgement.get("storage") == "memory"
                   and acknowledgement.get("format") == "intelligence_ui.v1"
                   and acknowledgement.get("block_count") == len(payload["blocks"]),
                   "a real final presentation requires no approval and retains its message ACK and structured metadata")
        self.check("user_messages" not in acknowledgement,
                   "queued requirements do not overwrite the original native acknowledgement")
        return result, acknowledgement

    def message_events(self):
        page = self.live_events(agent_id=self.presenter["agent_id"], limit=4)
        return [event["value"] for event in page["events"]
                if journal_value(event).get("type") == "ssmcp.message"]

    def message(self, message_id):
        matching = [row for row in self.message_events() if row.get("message_id") == message_id]
        if len(matching) != 1:
            raise AssertionError("expected one current-process presentation message")
        return matching[0]

    def wait_queue_state(self, front, message_id, state):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            queue = self.queue()
            matching = [row for row in queue["queue"] if row["id"] == message_id]
            if len(matching) == 1 and matching[0]["status"] == state:
                return matching[0]
            if front.process.poll() is not None:
                break
        raise AssertionError(f"fixture request did not reach {state}")

    def resize(self, front, rows, cols):
        front.resize(rows, cols)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            observed_rows = self.ui("/display/rows")
            observed_cols = self.ui("/display/cols")
            if observed_rows == rows and observed_cols == cols:
                return
        raise AssertionError("client did not observe the actual owned PTY resize")

    def snapshot(self, front, label):
        markers = [f"{prefix}_{label}" for prefix in
                   ("PRESENT", "TEXT", "CARD", "METRIC", "ROW", "PROGRESS", "STEP", "BAR")]
        front.until(lambda shot: all(marker in text(shot) for marker in markers),
                    f"all seven native blocks in the {label.lower()} terminal")
        shot = front.snapshot(cells=True)
        self.check(shot["alternate_screen"] and all(marker in text(shot) for marker in markers),
                   f"the {label.lower()} PTY actually displays all seven presentation block types")
        selected_rows = [index for index, row in enumerate(shot["rows"])
                         if any(marker in row for marker in markers)]
        self.check(all(cell["foreground"] is None and cell["background"] is None
                       for row in selected_rows for cell in shot["cells"][row] if cell["text"].strip()),
                   f"NO_COLOR keeps the {label.lower()} presentation cells uncolored despite color=true")
        encoded = (json.dumps(shot, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
        if len(encoded) > 2 * 1024 * 1024 or len(shot["rows"]) > 96:
            raise AssertionError("presentation snapshot exceeded its fixed artifact budget")
        for secret in [self.secret, *self.sensitive]:
            if secret and secret.encode() in encoded:
                raise AssertionError("refusing to save a snapshot containing fixture credentials")
        record = {"name": f"presentations-{label.lower()}.json", "bytes": len(encoded),
                  "sha256": hashlib.sha256(encoded).hexdigest(), "rows": len(shot["rows"])}
        if self.output is not None:
            self.output.mkdir(parents=True, exist_ok=True)
            (self.output / record["name"]).write_bytes(encoded)
            (self.output / f"presentations-{label.lower()}.txt").write_text(text(shot) + "\n", encoding="utf-8")
        self.snapshots.append(record)

    def rejected(self, arguments, description, expected):
        before = self.message_events()
        # Native errors reject call(), so code_mode returns its own error result.
        # Exercise that HTTP boundary directly instead of the success-only helper.
        code = "return await call(%s, 'ssmcp.ui.present', %s);" % (
            json.dumps(self.client_id), json.dumps(arguments),
        )
        status, body, _ = self.rpc(
            "tools/call", {"name": "code_mode", "arguments": {
                "code": code, "agent": self.presenter_binding()}}, modern=True,
        )
        if status != 200 or not isinstance(body, dict) or "error" in body:
            raise AssertionError(f"presentation rejection HTTP boundary failed: {status}, {body}")
        response = body["result"]
        error_text = "\n".join(
            block.get("text", "") for block in response.get("content", [])
            if block.get("type") == "text"
        )
        self.check(response.get("isError") is True and expected in error_text, description)
        self.check(self.message_events() == before and not self.messages(response),
                   "rejected presentation input adds no retained message or unexpected requirement")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.start_front("presentations")
        self.presenter = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Presentation fixture", "role": "presenter"})
        self.sensitive.append(self.presenter["token"])
        self.resize(front, 64, 120)

        wide_document = document("WIDE")
        wide = {**wide_document, "kind": "answer", "idempotency_key": "presentation_wide"}
        response, acknowledgement = self.present(wide)
        self.check(not acknowledgement["duplicate"] and not self.messages(response),
                   "the first presentation is accepted once without inventing a user request")
        self.snapshot(front, "WIDE")
        row = self.message(acknowledgement["message_id"])
        self.check(row["text"] == expected_text("WIDE") and row["kind"] == "answer",
                   "current process ui.events retains the complete unshortened presentation text")
        before = self.message_events()
        self.check(sum(entry.get("message_id") == acknowledgement["message_id"] for entry in before) == 1,
                   "the presentation ACK identifies exactly one live message event")
        _, repeated = self.present(wide)
        self.check(repeated["message_id"] == acknowledgement["message_id"]
                   and repeated["duplicate"] is True and self.message_events() == before,
                   "an identical presentation retry returns the same identity without another live event")
        conflict = copy.deepcopy(wide)
        conflict["title"] = "DIFFERENT_PRESENTATION"
        self.rejected(conflict, "a changed payload cannot reuse a retained presentation idempotency key",
                      "idempotency_key already identifies")
        layout = {"title": "LAYOUT_IDENTITY", "kind": "answer", "idempotency_key": "same_plain",
                  "blocks": [{"type": "text", "text": "[SUCCESS] Status\nComplete"}]}
        _, layout_ack = self.present(layout)
        self.check(self.message(layout_ack["message_id"])["text"]
                   == "LAYOUT_IDENTITY\n\n[SUCCESS] Status\nComplete",
                   "the layout-conflict fixture has exact known plain text")
        # Card and text deliberately have identical plain text but different
        # rendering semantics; retry identity must include the full document.
        layout["blocks"] = [{"type": "card", "title": "Status", "body": "Complete", "tone": "success"}]
        self.rejected(layout, "identical plain text with a different layout cannot reuse the same key",
                      "idempotency_key already identifies")
        self.rejected({"title": "INVALID_PRESENTATION", "blocks": []},
                      "an invalid native presentation is rejected before message recording", "blocks")
        self.rejected({**wide_document, "author": "forged identity"},
                      "presentation fields cannot spoof a caller identity", "author")
        self.rejected({"title": "INVALID_CONTROL", "blocks": [{"type": "text", "text": "\x1b[2J"}]},
                      "terminal escape input is rejected before display or event recording", "control characters")

        draft = "PRESENT_DRAFT 保留输入界"
        front.send(draft.encode())
        original = self.wait_state(front, lambda value: value["draft"]["text"] == draft,
                                "the independent ordinary draft")
        composition = {key: original[key] for key in ("draft", "target", "editing")}
        request_text = "PRESENT_NEXT_REQUIREMENT 实际送达"
        submitted = self.observe("ssmcp.ui.queue", {"action": "submit", "text": request_text})
        queued = self.wait_queue_state(front, submitted["id"], "queued")
        self.observe("ssmcp.ui.events", {
            "action": "thread", "client_id": self.client_id, "limit": 1,
        })
        self.observe("ssmcp.ui.questions", {})
        self.observe("ssmcp.ui.events", {
            "action": "thread", "client_id": self.client_id,
            "agent_id": self.presenter["agent_id"], "limit": 1,
        })
        self.check(self.record(self.queue(), message_id=queued["id"])["status"] == "queued",
                   "process, thread and question observations leave the next request queued")

        self.resize(front, 80, 36)
        narrow_document = document("NARROW")
        narrow = {**narrow_document, "kind": "answer", "idempotency_key": "presentation_narrow"}
        # Retained output reflects its publication width. Publish a fresh document
        # after the real resize instead of assuming historical output is reflowed.
        delivered_response, narrow_ack = self.present(narrow)
        self.expect_delivery(delivered_response, queued, agent_id=self.presenter["agent_id"])
        delivered = self.wait_queue_state(front, queued["id"], "delivered")
        self.check(delivered["revision"] == queued["revision"] + 1,
                   "a successful ui.present call claims the actual queued revision exactly once")
        self.snapshot(front, "NARROW")
        row = self.message(narrow_ack["message_id"])
        self.check(row["text"] == expected_text("NARROW") and row["kind"] == "answer",
                   "narrow rendering retains every original table cell in the current presenter event scope")
        after = self.ui("/display/conversation")
        self.check({key: after[key] for key in composition} == composition,
                   "structured display and request delivery preserve the ordinary draft, cursor and target")
        repeat_response, repeated = self.present(narrow)
        self.check(repeated["duplicate"] is True and repeated["message_id"] == narrow_ack["message_id"]
                   and not self.messages(repeat_response),
                   "a duplicate presentation neither duplicates the live event nor redelivers the user request")

        front.send(b"\x01\x0b")
        self.wait_state(front, lambda value: not value["draft"]["text"], "clearing only the fixture draft")
        front.send("/copy\r")
        deadline = time.monotonic() + 8
        copied = None
        while time.monotonic() < deadline:
            front.pump(0.05)
            try:
                copied = self.copied.read_bytes()
            except (OSError, ConnectionError):
                continue
            if copied == expected_text("NARROW").encode():
                break
        self.check(copied == expected_text("NARROW").encode(),
                   "/copy writes the complete latest presentation text to the private clipboard writer")
        entries = self.message_events()
        self.check(sum(entry.get("message_id") == narrow_ack["message_id"] for entry in entries) == 1,
                   "a duplicate narrow presentation remains one live message event")
        (self.output / "presentations-events.json").write_text(
            json.dumps(entries, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        state = self.state_home / "ssmcp"
        self.check(not any(path.is_file() for namespace in ("conversations", "context-memory")
                           for path in (state / namespace).rglob("*")),
                   "presentation publication creates no conversation or memory-state files")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios
                   and front.process.returncode == 0,
                   "the isolated presentation client restores its owned terminal and exits successfully")
        self.metrics["presentation"] = {
            "block_types": [block["type"] for block in wide_document["blocks"]],
            "transcript_bytes": len(front.transcript),
            "transcript_sha256": hashlib.sha256(front.transcript).hexdigest(),
            "snapshots": self.snapshots,
            "clipboard": "private native-writer fixture; no real display endpoint",
        }
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("presentation E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--fixture-binary", type=Path, default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"),
                        help="prebuilt library fixture; overrides SSMCP_E2E_FIXTURE_BINARY")
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--report", type=Path, help="write checks and hashes without credentials")
    parser.add_argument("--out", type=Path, required=True, help="write JSON report, explicit live-event evidence and bounded actual VT cell snapshots")
    args = parser.parse_args()
    if args.fixture_binary is not None:
        fixture = args.fixture_binary.resolve()
        if not fixture.is_file() or not os.access(fixture, os.X_OK):
            parser.error(f"prebuilt fixture is not executable: {fixture}")
        os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(fixture)
    binary_dir = args.bin_dir.resolve()
    decoder = (args.decoder or binary_dir / "examples/vt_snapshot").resolve()
    for executable in (binary_dir / "ssmcp", binary_dir / "ssmcpd", decoder):
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    root = Path(__file__).resolve().parent.parent
    source_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    sources = [Path(__file__).resolve(), *(root / "scripts" / name for name in (
        "e2e.py", "e2e_dashboard.py", "e2e_composer.py", "e2e_input_history.py")),
        *(root / name for name in ("src/client/presentation.rs", "src/client/interface/messages.rs",
                                  "src/client/interface.rs", "src/code_mode.rs"))]
    started = time.monotonic()
    report = {
        "schema": "ssmcp.presentation-e2e.v1", "status": "failed",
        "source_commit": source_commit, "platform": platform.system(),
        "transport": "isolated account OAuth + owned PTY + independent vt100 decoder",
        "scope": "native structured display, complete live events/copy, idempotency and next-request delivery",
        "models_started": False, "production_contacted": False,
        "display_mode": "NO_COLOR owned terminal; native wide and narrow block layout",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", binary_dir / "ssmcp"), ("ssmcpd", binary_dir / "ssmcpd"),
            ("vt_snapshot", decoder))},
        "sources": {str(path.relative_to(root)): sha256(path) for path in sources},
    }
    if args.fixture_binary is not None:
        report["binaries"]["fixture"] = sha256(args.fixture_binary.resolve())
    with tempfile.TemporaryDirectory(prefix="ssmcp-presentations-e2e-") as directory:
        harness = Presentations(Path(directory), binary_dir, decoder, args.out)
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
                    harness.save_shot("presentation-failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          snapshots=harness.snapshots,
                          duration_seconds=round(time.monotonic() - started, 3))
            encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
            (harness.output / "report.json").write_text(encoded, encoding="utf-8")
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(encoded, encoding="utf-8")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} presentation checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
