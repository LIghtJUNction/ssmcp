#!/usr/bin/env python3
"""Exercise file-only @ completion and submission through an owned real PTY."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from e2e_input_history import InputHistory, ENTER


class AtCompletion(InputHistory):
    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                       check=True, timeout=5)
        files = {"notes.txt": "ASCII_FILE_ONLY\n",
                 "目录/含 空格 界.txt": "UNICODE_FILE_ONLY\n中文材料\n",
                 "vanishes.txt": "REMOVE_AFTER_SELECTION\n",
                 "mouse-alpha.txt": "MOUSE_ALPHA\n",
                 "mouse-beta.txt": "MOUSE_BETA\n",
                 "鼠标 01 中文说明文档 带空格和很长的文件名 界.txt": "NARROW_ALPHA\n",
                 "鼠标 02 中文说明文档 带空格和很长的文件名 界.txt": "NARROW_BETA\n"}
        for name, content in files.items():
            path = self.workspace / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        front = self.start_front("at-completion")
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Reference fixture member", "role": "file-only reference regression",
        })
        self.sensitive.append(member["token"])
        self.choose_local_recipient(front, member)

        for typed, path, expected in [
            ("@not", "notes.txt", "@notes.txt "),
            ('@"目录/含', "目录/含 空格 界.txt", '@"目录/含 空格 界.txt" '),
        ]:
            source = self.complete_reference(front, typed, "@" + path, expected,
                                             ENTER, "select-" + ("ascii" if path == "notes.txt" else "unicode"))
            self.check(self.ui("/display/conversation")["target"] == member["agent_id"],
                       "accepting a file-only reference preserves the exact recipient")
            record = self.queue_current(front, source)
            references = self.delivered_references(front, member, record, source)
            self.check(len(references) == 1 and references[0]["path"] == path
                       and references[0]["preview"] == files[path],
                       "the second Enter delivers exactly the selected file-only reference")

        source = self.complete_reference(front, "@van", "@vanishes.txt", "@vanishes.txt ",
                                         ENTER, "select-before-file-disappears")
        before = {record["id"] for record in self.queue()["queue"]}
        vanished = self.workspace / "vanishes.txt"
        vanished.unlink()
        record = self.queue_current(front, source)
        front.until(lambda shot: any("reference notice:" in row.lower()
                                     and "@vanishes.txt:" in row for row in shot["rows"]),
                    "the accepted missing reference displays its warning")
        state = self.ui("/display/conversation")
        self.check(not state["draft"]["text"],
                   "accepting a missing selected reference clears the acknowledged composer")
        self.check(state["target"] == member["agent_id"],
                   "accepting a missing selected reference preserves the exact recipient")
        created = [item for item in self.queue()["queue"] if item["id"] not in before]
        self.check(len(created) == 1 and created[0]["id"] == record["id"]
                   and created[0]["status"] == "queued",
                   "the second Enter queues the selected missing reference exactly once")
        self.check(any("@vanishes.txt:" in warning
                       for warning in record.get("reference_warnings", [])),
                   "the exact queued identity retains the missing-reference warning")
        self.check(not vanished.exists(),
                   "referencing the disappeared file does not recreate it")
        self.save_shot("missing-reference-accepted", front, cells=True)

        # Restore the file only after the original payload was acknowledged.
        # Delivery must use that frozen unavailable reference, not reread it.
        vanished.write_text(files["vanishes.txt"], encoding="utf-8")
        queued = self.record(self.queue(), message_id=record["id"])
        self.check(queued["status"] == "queued" and queued["text"] == record["text"]
                   and queued["reference_warnings"] == record["reference_warnings"],
                   "restoring the file leaves the acknowledged queue payload and warning frozen")
        references = self.delivered_references(front, member, record, source)
        self.check(len(references) == 1 and references[0]["kind"] == "file"
                   and references[0]["path"] == "vanishes.txt"
                   and references[0]["status"] == "unavailable"
                   and references[0].get("warning") in record["reference_warnings"]
                   and "read" not in references[0] and "preview" not in references[0],
                   "delivery preserves the exact source with one unavailable reference and no fabricated contents")
        self.check(not self.messages(self.work(member=member)),
                   "the acknowledged missing-reference request is not delivered again")

        long_path = "鼠标 02 中文说明文档 带空格和很长的文件名 界.txt"
        for typed, visible_prefix, path, source, narrow in [
            ("@mouse-", "@mouse-beta.txt  ", "mouse-beta.txt", "@mouse-beta.txt ", False),
            ('@"鼠标 ', "@鼠标 02 ", long_path, '@"' + long_path + '" ', True),
        ]:
            front.send(typed)
            visible = lambda shot: any(row.lstrip().startswith(visible_prefix) for row in shot["rows"])
            shot = front.until(visible, "the second mouse-selectable file suggestion")
            if narrow:
                front.resize(18, 48)
                shot = front.until(lambda current: current["size"] == [18, 48] and visible(current),
                                   "the reference menu redrawn after narrowing the terminal")
            row = next(index for index, line in enumerate(shot["rows"])
                       if line.lstrip().startswith(visible_prefix))
            before = {record["id"] for record in self.queue()["queue"]}
            front.send(f"\x1b[<0;3;{row + 1}M\x1b[<0;3;{row + 1}m".encode())
            state = self.wait_state(front, lambda current: current["draft"]["text"] == source,
                                    "clicking the second file suggestion did not select that exact file")
            self.check(state["target"] == member["agent_id"]
                       and state["draft"]["cursor"] == len(source.encode("utf-8")),
                       "mouse completion preserves the exact recipient and UTF-8 cursor")
            self.check({record["id"] for record in self.queue()["queue"]} == before,
                       "clicking a file suggestion does not submit the draft")
            self.save_shot("mouse-completed-" + ("narrow" if narrow else "wide"), front, cells=True)
            record = self.queue_current(front, source)
            references = self.delivered_references(front, member, record, source)
            self.check(len(references) == 1 and references[0]["path"] == path
                       and references[0]["preview"] == files[path],
                       "Enter after mouse selection delivers the clicked file rather than the first suggestion")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the isolated reference client restores its owned terminal")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    report = {"status": "failed", "mode": "real CLI and PTY; production account router over loopback HTTP/WebSocket; synthetic GitHub provider",
              "binary_sha256": hashlib.file_digest((args.bin_dir / "ssmcp").open("rb"), "sha256").hexdigest()}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-at-completion-e2e-") as directory:
        harness = AtCompletion(Path(directory), args.bin_dir, args.decoder, args.out)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            message = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[redacted]")
            report["error"] = message
            if harness.last_frontend and not harness.last_frontend.closed:
                harness.save_shot("failure", harness.last_frontend, cells=True)
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]), "error": report.get("error")}, ensure_ascii=False))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
