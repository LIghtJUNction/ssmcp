#!/usr/bin/env python3
"""Check /send file and skill completion through an owned PTY and local listener.

Reuses the authenticated account-router fixture, listener ACK assertions, and
independent VT decoder. All inputs are prebuilt; no Cargo or model is started.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

from e2e_composer import ENTER, HOME
from e2e_force_send import ForceSend
from e2e_input_history import REFERENCE_BEGIN, REFERENCE_END, text
from e2e_message_references import MessageReferences, digest


class SendReferenceCompletion(ForceSend, MessageReferences):
    """Share the listener lifecycle and reference snapshot helpers."""

    def submit_selected(self, front, lease, command, kind, name, ordinary):
        body = command.removeprefix("/send ")
        self.check(self.ui("/display/conversation")["draft"]["text"] == command,
                   "the completed command remains byte-for-byte intact before submission")
        front.send(ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.03)
            status = self.listener("status")
            if status["pending"]:
                delivery_id = status["pending"]["delivery_id"]
                break
        else:
            raise AssertionError("Enter did not reach the local listener\n" + front.failure_detail())
        offered = self.listener("read", **lease, wait_ms=0)["delivery"]
        self.check(offered is not None and offered["delivery_id"] == delivery_id
                   and offered["connection_id"] == self.client_id
                   and offered["listener_id"] == lease["listener_id"]
                   and offered["source_text"] == body and offered["source"] == "client_user",
                   "the listener receives the exact original body and delivery identity")
        raw, separator, encoded = offered["text"].partition(REFERENCE_BEGIN)
        self.check(raw == body and bool(separator) and encoded.endswith(REFERENCE_END),
                   "the wire body preserves every original byte before reference metadata")
        payload = json.loads(encoded[:-len(REFERENCE_END)])
        references = payload["references"]
        missing = name.startswith("missing-")
        reference_kind = "text" if kind == "file" and not missing else kind
        self.check(payload["schema_version"] == 1
                   and payload["source_sha256"] == hashlib.sha256(body.encode()).hexdigest()
                   and len(references) == 1 and references[0]["kind"] == reference_kind,
                   "one reference is bound to the exact submitted UTF-8 source")
        reference = references[0]
        self.check(reference.get("path" if kind == "file" else "name") == name,
                   "the receiver gets the selected reference rather than its typed prefix")
        if missing:
            self.check(reference.get("status") == "unavailable" and reference.get("warning")
                       and "read" not in reference and "preview" not in reference,
                       "a missing reference delivers an explicit warning without fabricated data")
        elif kind == "file":
            self.check(reference.get("preview") == self.file_text and "read" in reference,
                       "the available file reference contains the real fixture text")
        else:
            self.check(reference.get("root") == "workspace"
                       and reference.get("status") != "unavailable"
                       and "read" in reference and "warning" not in reference,
                       "the available skill resolves to the workspace skill and its read action")
        self.listener("ack", **lease, delivery_id=delivery_id, status="accepted")
        receipt = self.receipt(front, delivery_id, "accepted")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "the acknowledged command did not clear")
        self.check(self.listener("read", **lease, wait_ms=0)["delivery"] is None,
                   "the acknowledged command is not replayed to the listener")
        self.ordinary_unchanged(ordinary)
        self.listener("close", **lease)
        self.receipts.append({"delivery_id": delivery_id, "source": body,
                              "references": references, "receipt": receipt})

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        self.file_text = "SEND_REFERENCE_CONTENT\n真实文件内容\n"
        (self.workspace / "send-note.txt").write_text(self.file_text, encoding="utf-8")
        skill = self.workspace / ".agents/skills/send-helper/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text(
            "---\nname: send-helper\ndescription: Owned send completion fixture.\n---\n\n"
            "Use the actual SEND_SKILL_CONTENT fixture.\n", encoding="utf-8")
        front = self.start_front("send-reference-completion")
        front.resize(32, 120)
        front.until(lambda shot: shot["size"] == [32, 120], "the owned completion viewport")
        self.agents(front)
        front.send(HOME + ENTER)
        self.wait_state(front, lambda state: state["target"] is None
                        and state["native_recipient"] is None
                        and state["view"] == "activity" and state["focus"] == "input",
                        "the explicitly selected local Next caller route")
        ordinary = self.submit_source(front, "ORDINARY_SENTINEL remains in its own queue")

        for kind, prefix, name in (
            ("file", "@send-no", "send-note.txt"),
            ("skill", "$send-he", "send-helper"),
        ):
            candidate = ("@" if kind == "file" else "$") + name
            for key_name, key in (("enter", ENTER), ("tab", b"\t")):
                label = kind.upper() + "_" + key_name.upper()
                typed = "/send " + label + " 处理 " + prefix
                completed = "/send " + label + " 处理 " + candidate + " "
                self.check(not self.ui("/display/conversation")["draft"]["text"],
                           "each completion starts with an empty owned draft")
                lease = self.open_listener()
                previous_receipts = self.listener("status")["receipts"]
                self.focus_input(front)
                front.send(typed)
                front.until(lambda shot: any(
                    row.lstrip().startswith("› " + candidate + "  ") for row in shot["rows"]),
                    "the actual " + kind + " candidate inside /send")
                if key_name == "enter":
                    self.capture(front, "send-" + kind + "-picker", candidate)
                self.check(self.listener("read", **lease, wait_ms=0)["delivery"] is None,
                           "typing a reference does not create a listener delivery")
                front.send(key)
                state = self.wait_state(
                    front, lambda current: current["draft"]["text"] == completed,
                    "the first " + key_name + " must select the reference without sending")
                self.check(state["draft"]["cursor"] == len(completed.encode())
                           and state["target"] is None and state["native_recipient"] is None,
                           "completion preserves the UTF-8 cursor and the exact local route")
                snapshot = self.listener("status")
                self.check(not state["force_pending"] and snapshot["pending"] is None
                           and snapshot["receipts"] == previous_receipts
                           and self.listener("read", **lease, wait_ms=0)["delivery"] is None,
                           "the first " + key_name + " only selects the " + kind + " candidate")
                self.ordinary_unchanged(ordinary)
                self.submit_selected(front, lease, completed, kind, name, ordinary)
                if key_name == "enter":
                    self.capture(front, "send-" + kind + "-accepted", "监听程序已接收",
                                 body_marker=label)

        for kind, token, name in (
            ("file", "@missing-send.txt", "missing-send.txt"),
            ("skill", "$missing-send-skill", "missing-send-skill"),
        ):
            command = "/send MISSING_" + kind.upper() + " " + token
            self.focus_input(front)
            front.send(command)
            front.until(lambda shot: ("No matching files" if kind == "file"
                                      else "No matching skills") in text(shot),
                        "the unavailable " + kind + " suggestion")
            lease = self.open_listener()
            self.submit_selected(front, lease, command, kind, name, ordinary)
            front.until(lambda shot: any("引用提示：" in row and token in row
                                        for row in shot["rows"]),
                        "the unavailable reference notice after accepted ACK")
            self.capture(front, "send-missing-" + kind, "监听程序已接收",
                         body_marker="MISSING_" + kind.upper())
        self.check(not (self.workspace / "missing-send.txt").exists(),
                   "sending a missing file does not create it")
        final, restored = self.disconnect_front(front)
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the owned client restores its terminal mode")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--decoder", required=True, type=Path)
    parser.add_argument("--fixture-binary", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--font", required=True, type=Path,
                        help="monospace font used by the existing VT snapshot renderer")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be the complete tested source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all prebuilt executables must exist; no build is started here")
    if not args.font.is_file():
        parser.error("--font must name an existing monospace font")
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use an empty output directory to preserve previous evidence")
    before = {name: digest(path) for name, path in binaries.items()}
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise TimeoutError("send-reference PTY acceptance interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    report = {"status": "failed", "source_revision": args.source_revision,
              "transport": "owned authenticated account router, WebSocket, PTY and local listener",
              "models_started": False, "existing_services_touched": False}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-send-reference-",
                                     dir=args.out.parent) as temporary:
        harness = SendReferenceCompletion(Path(temporary), args.bin_dir.resolve(),
                                          binaries["decoder"], args.out)

        def redact(value):
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    value = value.replace(secret, "[redacted]")
            return value

        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            report["error"] = redact(f"{type(error).__name__}: {error}")
            if harness.last_frontend and not harness.last_frontend.closed:
                try:
                    harness.save_shot("send-reference-failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError):
                    pass
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
        owned = list(harness.processes)
        gateway = getattr(harness, "account_gateway", None)
        if gateway is not None and gateway.process is not None:
            owned.append(gateway.process)
        report["cleanup"] = {"all_exited": all(item.poll() is not None for item in owned)}
        if not report["cleanup"]["all_exited"]:
            report.update(status="failed", cleanup_error="an owned process remained after cleanup")
        images = []
        try:
            for frame in harness.frames:
                source = args.out / frame
                image = source.with_suffix(".png")
                subprocess.run(
                    [sys.executable, str(Path(__file__).with_name("render_ui_snapshot.py")),
                     str(source), str(image), "--font", str(args.font.resolve()), "--size", "16"],
                    check=True, timeout=15, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                images.append(image.name)
        except (OSError, subprocess.SubprocessError) as error:
            report.update(status="failed", render_error=redact(f"{type(error).__name__}: {error}"))
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            report.update(status="failed", error="a prebuilt executable changed during acceptance")
        report.update(checks=harness.checks, frames=harness.frames, images=images,
                      receipts=harness.receipts, elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      inputs_unchanged=before == after,
                      script_sha256=digest(__file__))
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if redact(encoded) != encoded:
            raise AssertionError("the report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "images": len(report["images"]), "error": report.get("error")},
                     ensure_ascii=False))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
