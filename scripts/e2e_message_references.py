#!/usr/bin/env python3
"""Exercise missing @files and $skills through an authenticated relay and owned PTY.

Uses prebuilt product binaries, the existing account-router fixture, and the
independent VT decoder. No existing session, desktop, X server, or model is used.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time
import traceback

from e2e_composer import ENTER, HOME
from e2e_dashboard import text
from e2e_input_history import InputHistory, PASTE_END, PASTE_START


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class MessageReferences(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.frames = []
        self.frame_receipts = []
        self.queue_artifacts = []
        self.receipts = []

    def native_result(self, name, arguments):
        if name not in ("ssmcp.ui.queue", "ssmcp.output.read"):
            raise AssertionError("unexpected artifact observation tool")
        result = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)))
        if self.messages(result):
            raise AssertionError("queue/artifact observation consumed a user requirement")
        native = result["structuredContent"]["result"]
        if native.get("isError"):
            raise AssertionError(f"native observation failed: {native}")
        return native

    def read_queue_artifact(self, summary):
        meta = summary.get("artifact", {})
        size = meta.get("bytes")
        if (summary.get("format") != "mcp_tool_result_json" or summary.get("output_error")
                or meta.get("schema") != "ssmcp.output.v1"
                or meta.get("scope") != "native_tool_result_json"
                or meta.get("complete") is not True
                or not isinstance(size, int) or not 0 < size <= 1024 * 1024
                or meta.get("observed_bytes") != size
                or meta.get("sha256_scope") != "first_bytes_snapshot"
                or not re.fullmatch(r"[0-9a-f]{32}", str(meta.get("id", "")))
                or not re.fullmatch(r"[0-9a-f]{64}", str(meta.get("sha256", "")))):
            raise AssertionError("queue spill is not a complete bounded native JSON artifact")
        payload = bytearray()
        chunks = 0
        while len(payload) < size:
            offset = len(payload)
            native = self.native_result("ssmcp.output.read", {
                "id": meta["id"], "offset": offset, "max_bytes": min(8192, size - offset)})
            part = native["structuredContent"]
            chunk = base64.b64decode(part["data_base64"], validate=True)
            end = offset + len(chunk)
            if (not chunk or len(chunk) > 8192 or end > size
                    or part.get("id") != meta["id"] or part.get("offset") != offset
                    or part.get("next_offset") != end or part.get("bytes") != len(chunk)
                    or part.get("size") != size or part.get("complete") is not True
                    or part.get("eof") is not (end == size)
                    or part.get("sha256_scope") != "returned_chunk"
                    or part.get("sha256") != hashlib.sha256(chunk).hexdigest()):
                raise AssertionError("output.read returned an inconsistent or incomplete queue chunk")
            payload.extend(chunk)
            chunks += 1
        if hashlib.sha256(payload).hexdigest() != meta["sha256"]:
            raise AssertionError("complete queue artifact SHA-256 differs from its manifest")
        native = json.loads(payload)
        if not isinstance(native, dict) or not isinstance(native.get("structuredContent"), dict):
            raise AssertionError("queue artifact is not a complete native MCP result")
        self.queue_artifacts.append({
            "id": meta["id"], "bytes": size, "sha256": meta["sha256"],
            "complete": True, "chunks": chunks, "format": summary["format"]})
        return native

    def queue_headers(self):
        path = "/steering/queue"
        # This fixture retains its small first request; only fetch one row to
        # obtain the array length, then use bounded scalar projections.
        page = self.observe("ssmcp.ui.get", {"path": path, "limit": 1})
        total = page.get("total")
        if (page.get("schema_version") != 1 or page.get("path") != path
                or not isinstance(total, int) or not 0 <= total <= 64):
            raise AssertionError("invalid bounded queue projection")
        headers = []
        for index in range(total):
            prefix = f"{path}/{index}"
            headers.append({"id": self.ui(prefix + "/id"),
                            "status": self.ui(prefix + "/status"), "index": index})
        if len({item["id"] for item in headers}) != total:
            raise AssertionError("queue identities were duplicated while observing")
        return headers

    def queue(self):
        native = self.native_result("ssmcp.ui.queue", {"action": "get"})
        data = native.get("structuredContent", {})
        if data.get("spilled"):
            # output.read is eligible work in code_mode. Reading it while a
            # Next caller request is queued would consume that request.
            if any(item["status"] != "delivered" for item in self.queue_headers()):
                raise AssertionError("artifact reads require an entirely delivered queue")
            native = self.read_queue_artifact(data)
            data = native.get("structuredContent", {})
        if (native.get("isError") or data.get("schema_version") != 1
                or not isinstance(data.get("queue"), list)
                or not isinstance(data.get("agents"), list)):
            raise AssertionError("complete native result did not contain the queue schema")
        return data

    def capture(self, front, name, marker, *, record=None, body_marker=None,
                warning_marker=None):
        queued = None
        if record is not None:
            queued = self.record(self.queue(), message_id=record["id"])
            self.check(queued["status"] == "queued"
                       and queued["text"] == record["text"]
                       and body_marker and body_marker in queued["text"]
                       and warning_marker and any(
                           warning_marker in warning
                           for warning in queued.get("reference_warnings", [])),
                       name + " is tied to the exact accepted queue ID and its warning")
        for columns in (120, 48):
            front.resize(32, columns)
            shot = front.until(
                lambda value: value["size"] == [32, columns]
                and marker.lower() in text(value).lower()
                and (body_marker is None or body_marker in text(value))
                and (warning_marker is None or any(
                    marker.lower() in row.lower() and warning_marker in row
                    for row in value["rows"])),
                name + " at " + str(columns) + " columns", cells=True)
            stem = "message-references-" + name + "-" + str(columns)
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            if any(secret and secret in encoded for secret in self.sensitive):
                raise AssertionError("a private fixture credential appeared in the VT snapshot")
            (self.output / (stem + ".json")).write_text(encoded, encoding="utf-8")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
            self.frames.append(stem + ".json")
            if queued is not None:
                # The conversation renders source prose, not the complete ID.
                # Keep the untouched VT cells and bind their unique markers to
                # the separately observed exact queue identity in the report.
                self.frame_receipts.append({
                    "frame": stem + ".json", "message_id": queued["id"],
                    "queue_status": queued["status"], "body_marker": body_marker,
                    "warning_marker": warning_marker,
                    "queue_text_sha256": hashlib.sha256(
                        queued["text"].encode()).hexdigest(),
                    "reference_warnings": queued["reference_warnings"],
                })
        front.resize(32, 120)
        front.pump(0.15)

    def projection_matches(self, path, source):
        # A whole conversation projection is limited to 8 KiB. Page the string
        # so the maximum-size draft is still checked byte-for-byte before Enter.
        for offset in range(0, max(1, len(source)), 1024):
            page = self.observe("ssmcp.ui.get", {
                "path": path, "offset": offset, "limit": 1024})
            if page.get("schema_version") != 1 or page.get("path") != path:
                raise AssertionError("the paged draft did not return its exact UI projection")
            if (page.get("total") != len(source)
                    or page.get("value") != source[offset:offset + 1024]):
                return False
        return True

    def queue_large_draft(self, front, source):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.projection_matches("/display/conversation/draft/text", source):
                break
        else:
            raise AssertionError("the complete maximum-size draft was not entered\n"
                                 + front.failure_detail())
        self.check(True, "the complete maximum-size draft matches every original byte")
        before = {item["id"] for item in self.queue()["queue"]}
        front.send(ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            headers = self.queue_headers()
            created = [item for item in headers if item["id"] not in before]
            if (len(created) == 1 and created[0]["status"] == "queued"
                    and {item["id"] for item in headers} == before | {created[0]["id"]}):
                entry = created[0]
                path = f"/steering/queue/{entry['index']}"
                self.check(self.projection_matches(path + "/text", source),
                           "the exact new queue identity contains every submitted byte")
                warnings = self.ui(path + "/reference_warnings")
                self.wait_state(front, lambda state: not state["draft"]["text"],
                                "the accepted maximum-size draft did not clear")
                return {"id": entry["id"], "status": entry["status"],
                        "text": source, "reference_warnings": warnings}
        raise AssertionError("the maximum-size requirement was not acknowledged exactly once")

    def received(self, front, member, record, source):
        references = self.delivered_references(front, member, record, source)
        self.receipts.append({
            "message_id": record["id"], "source": source,
            "status": self.record(self.queue(), message_id=record["id"])["status"],
            "references": references,
        })
        return references

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        notes_path = "资料/说明.txt"
        notes = "REAL_REFERENCE_FILE\nUTF-8 内容 preserved.\n"
        skill_path = ".agents/skills/reference-fixture/SKILL.md"
        skill = (
            "---\nname: reference-fixture\n"
            "description: Local keyboard reference fixture.\n---\n\n"
            "Read the complete REFERENCE_SKILL_BODY before applying this skill.\n")
        for path, content in ((notes_path, notes), (skill_path, skill)):
            target = self.workspace / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")

        front = self.start_front("reference-keyboard")
        front.resize(32, 120)
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Reference recipient", "role": "owned keyboard acceptance fixture"})
        self.sensitive.append(member["token"])
        # Account-scoped members appear in the separate native inbox roster.
        # Reference expansion belongs to the steering queue: choose its fixed
        # Next caller row, then consume only through the bound fixture member.
        self.agents(front)
        front.send(HOME + ENTER)
        self.wait_state(front, lambda state: state["target"] is None
                        and state["native_recipient"] is None
                        and state["view"] == "activity" and state["focus"] == "input",
                        "keyboard selection of Next caller for the reference queue")

        source = "MISSING_DIRECT 请创建 @future-missing.txt"
        front.send(source)
        front.until(lambda shot: "No matching files" in text(shot),
                    "the completed no-match file search")
        self.capture(front, "missing-before-send", "No matching files")
        record = self.queue_current(front, source)
        front.until(lambda shot: "reference notice:" in text(shot).lower(),
                    "the warning rendered after the relay acknowledged the message")
        self.capture(front, "missing-accepted", "reference notice:",
                     record=record, body_marker="MISSING_DIRECT", warning_marker="@future-")
        references = self.received(front, member, record, source)
        self.check(len(references) == 1
                   and references[0]["path"] == "future-missing.txt"
                   and references[0]["status"] == "unavailable"
                   and references[0].get("warning")
                   and "read" not in references[0] and "preview" not in references[0],
                   "Enter after a no-match file search delivers the exact source with one warning")
        self.check(not (self.workspace / "future-missing.txt").exists(),
                   "referencing a missing file neither creates it nor fabricates its contents")

        source = ('MIXED @missing-later.txt，@"资料/说明.txt"；'
                  '$reference-fixture，$unavailable-fixture。 done')
        self.send_stream(front, PASTE_START + source.encode() + PASTE_END)
        self.wait_state(front, lambda state: state["draft"]["text"] == source,
                        "the mixed UTF-8 reference draft")
        record = self.queue_current(front, source)
        self.capture(front, "mixed-accepted", "reference notice:",
                     record=record, body_marker="MIXED", warning_marker="@missing-l")
        references = self.received(front, member, record, source)
        self.check(len(references) == 4
                   and references[0]["status"] == "unavailable"
                   and references[1]["path"] == notes_path
                   and references[1]["preview"] == notes
                   and references[2]["name"] == "reference-fixture"
                   and references[2]["root"] == "workspace"
                   and references[3]["name"] == "unavailable-fixture"
                   and references[3]["status"] == "unavailable",
                   "Chinese punctuation separates valid and unavailable files and skills")
        self.check(all("read" not in item and "preview" not in item
                       for item in references if item.get("status") == "unavailable"),
                   "unavailable references expose no attachment contents or read actions")
        file_data = self.call(references[1]["read"]["tool"], references[1]["read"]["arguments"])
        skill_data = self.call(references[2]["read"]["tool"], references[2]["read"]["arguments"])
        self.check(file_data["text"] == notes and skill_data["content"] == skill,
                   "the delivered valid file and skill actions retrieve complete real fixture contents")

        before = {item["id"] for item in self.queue()["queue"]}
        front.send("$reference-f")
        front.until(lambda shot: any(row.lstrip().startswith("› $reference-fixture  ")
                                     for row in shot["rows"]),
                    "the actual installed skill completion")
        self.capture(front, "skill-picker", "$reference-fixture")
        front.send(ENTER)
        source = "$reference-fixture "
        self.wait_state(front, lambda state: state["draft"]["text"] == source,
                        "the first Enter selecting the skill without sending")
        self.check({item["id"] for item in self.queue()["queue"]} == before,
                   "the first Enter only accepts the selected skill")
        record = self.queue_current(front, source)
        references = self.received(front, member, record, source)
        self.check(len(references) == 1 and references[0]["kind"] == "skill"
                   and references[0]["name"] == "reference-fixture"
                   and "warning" not in references[0],
                   "the second Enter delivers the selected skill exactly once")
        prefix = "FULL_BUDGET @missing-large.txt "
        source = prefix + "x" * (16384 - len(prefix.encode()))
        # Deliberate plain keys: a bracketed paste this large imports an attachment.
        self.send_stream(front, source.encode())
        record = self.queue_large_draft(front, source)
        self.check(record.get("reference_warnings")
                   and any("missing-large.txt" in item for item in record["reference_warnings"]),
                   "maximum-size missing-reference requests retain their local warning")
        front.until(lambda shot: any("reference notice:" in row.lower()
                                     and "@missing-large.txt" in row for row in shot["rows"]),
                    "the accepted full-budget message shows its own local reference notice")
        response = self.work(member=member)
        self.expect_delivery(response, record, agent_id=member["agent_id"])
        delivered = self.messages(response)[0]["text"]
        self.check(delivered == source and len(delivered.encode()) == 16384,
                   "all 16384 original bytes are delivered when warning metadata will not fit")
        status = record["status"]
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            matching = [item for item in self.queue_headers() if item["id"] == record["id"]]
            if len(matching) != 1:
                raise AssertionError("the exact full-budget queue identity disappeared")
            status = matching[0]["status"]
            if status == "delivered":
                break
        self.check(status == "delivered", "the exact full-budget queue ID is marked delivered")
        complete = self.record(self.queue(), message_id=record["id"])
        self.check(complete["status"] == "delivered" and complete["text"] == source
                   and complete["reference_warnings"] == record["reference_warnings"],
                   "the complete native queue artifact confirms the delivered identity and body")
        self.receipts.append({
            "message_id": record["id"], "source_bytes": len(source.encode()),
            "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
            "reference_warnings": record["reference_warnings"],
            "status": "delivered", "wire_metadata_omitted": True,
        })

        return {
            "status": "passed", "checks": self.checks, "frames": self.frames,
            "receipts": self.receipts, "frame_receipts": self.frame_receipts,
            "queue_artifacts": self.queue_artifacts,
            "transport": "real account router, authenticated WebSocket, native queue and owned PTY",
            "upstream_provider": "synthetic GitHub fixture",
            "models_started": False, "existing_services_touched": False,
            "real_user_credentials_used": False,
            "source_of_images": "actual owned PTY decoded by the independent VT fixture",
        }


def interrupted(_signal, _frame):
    raise TimeoutError("reference PTY acceptance interrupted")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--decoder", required=True, type=Path)
    parser.add_argument("--fixture-binary", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be a complete frozen source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all prebuilt executables must exist; this script never builds missing inputs")
    before = {name: digest(path) for name, path in binaries.items()}
    args.out.mkdir(parents=True, exist_ok=True)
    if (args.out / "report.json").exists():
        parser.error("use a new output directory to preserve prior acceptance evidence")
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    report = {"status": "failed", "source_revision": args.source_revision}
    with tempfile.TemporaryDirectory(prefix="ssmcp-reference-pty-",
                                     dir=args.out.resolve().parent) as temporary:
        harness = MessageReferences(Path(temporary), args.bin_dir.resolve(),
                                    binaries["decoder"], args.out)
        def redact(value):
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    value = value.replace(secret, "[redacted]")
            return value
        try:
            report.update(harness.run())
        except Exception as error:
            report["error"] = redact(f"{type(error).__name__}: {error}")
            report["traceback"] = redact(traceback.format_exc())
            print(report["traceback"], flush=True)
            if harness.last_frontend and not harness.last_frontend.closed:
                try:
                    harness.save_shot("references-failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
            harness.diagnostics()
        finally:
            try:
                harness.close_frontends()
            finally:
                try:
                    harness.cleanup()
                except Exception as error:
                    report["status"] = "failed"
                    report["cleanup_error"] = redact(f"{type(error).__name__}: {error}")
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            report["status"] = "failed"
            report["error"] = "a prebuilt executable changed during acceptance"
        report.update(
            checks=harness.checks, frames=harness.frames, receipts=harness.receipts,
            frame_receipts=harness.frame_receipts, queue_artifacts=harness.queue_artifacts,
            elapsed_seconds=round(time.monotonic() - started, 3),
            binaries_sha256_before=before, binaries_sha256_after=after,
            inputs_unchanged=before == after,
            source_sha256={name: digest(Path(__file__).with_name(name)) for name in (
                "e2e_message_references.py", "e2e_input_history.py", "e2e_composer.py",
                "e2e_dashboard.py", "e2e_client_ui.py", "e2e_account_gateway.py", "e2e.py")})
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if redact(encoded) != encoded:
            raise AssertionError("the shareable report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "frames": len(report["frames"]),
                      "elapsed_seconds": report["elapsed_seconds"]}))
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
