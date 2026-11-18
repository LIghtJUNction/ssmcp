#!/usr/bin/env python3
"""Exercise Linux file clipboard MIME handling in owned PTYs.

Uses the existing isolated SSH/OAuth/InputHistory fixture and a private PATH
wl-paste executable. Never connects to the desktop clipboard, starts a model,
or changes a shared frontend. Build candidates separately, then run:
  python3 scripts/e2e_file_paste.py --bin-dir target/debug --out /tmp/file-paste

File copies use current wire7 @references and the rooted native image reader.
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
import stat
import subprocess
import tempfile
import time

from e2e_dashboard import sha256, text
from e2e_input_history import InputHistory, png_fixture


PASTE_READER = r"""#!/usr/bin/python3
import base64
import json
import os
from pathlib import Path
import sys
import time

root = Path(os.environ["SSMCP_E2E_FILE_CLIPBOARD"])
selection = json.loads((root / "selection.json").read_text(encoding="utf-8"))
if sys.argv[1:] == ["--list-types"]:
    print("\n".join(selection["payloads"]))
else:
    if len(sys.argv) != 4 or sys.argv[1:3] != ["--no-newline", "--type"]:
        raise SystemExit("unexpected clipboard helper arguments")
    mime = sys.argv[3]
    with (root / "reads.log").open("a", encoding="utf-8") as log:
        log.write(mime + "\n")
    if selection["gate"]:
        (root / "blocked").touch()
        deadline = time.monotonic() + 2.5
        while not (root / "release").exists():
            if time.monotonic() >= deadline:
                raise SystemExit("fixture gate was not released")
            time.sleep(0.005)
    sys.stdout.buffer.write(base64.b64decode(selection["payloads"][mime], validate=True))
"""


class FilePaste(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.clipboard = directory / "fake-clipboard"
        self.clipboard.mkdir(mode=0o700)
        self.select({"text/plain;charset=utf-8": b""})

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        # Frontend expects PATH to be one private directory; never append host PATH.
        helper = Path(environment["PATH"]) / "wl-paste"
        helper.write_text(PASTE_READER, encoding="utf-8")
        helper.chmod(0o700)
        environment.update(
            WAYLAND_DISPLAY="ssmcp-owned-file-paste-fixture",
            SSMCP_E2E_FILE_CLIPBOARD=str(self.clipboard),
        )
        environment.pop("DISPLAY", None)
        return environment

    def select(self, payloads, *, gate=False):
        for name in ("blocked", "release"):
            (self.clipboard / name).unlink(missing_ok=True)
        selection = {
            "payloads": {mime: base64.b64encode(data).decode("ascii")
                         for mime, data in payloads.items()},
            "gate": gate,
        }
        temporary = self.clipboard / "selection.tmp"
        temporary.write_text(json.dumps(selection), encoding="utf-8")
        temporary.replace(self.clipboard / "selection.json")

    def reads(self):
        path = self.clipboard / "reads.log"
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def wait_file(self, front, name):
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            front.pump(0.01)
            if (self.clipboard / name).exists():
                return
            if front.process.poll() is not None:
                break
        raise AssertionError("private clipboard helper did not reach " + name)

    def inputs(self):
        root = self.workspace / ".ssmcp" / "inputs"
        if not root.exists():
            return {}
        result = {}
        for path in sorted(root.rglob("*")):
            mode = path.lstat().st_mode
            content = sha256(path) if stat.S_ISREG(mode) else None
            result[str(path.relative_to(root))] = (mode, content)
        return result

    def wait_inputs(self, front, before):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.02)
            if self.inputs() == before:
                return
        raise AssertionError("the rejected import left private files or changed an earlier copy")

    def clear_draft(self, front):
        state = self.ui("/display/conversation")
        draft = state["draft"]["text"]
        self.check(state["draft"]["cursor"] == len(draft.encode("utf-8")),
                   "the fixture clears only a draft whose cursor is at its UTF-8 end")
        front.send(b"\x7f" * len(draft))
        self.wait_state(front, lambda current: not current["draft"]["text"],
                        "the owned draft did not clear")

    def unchanged_queue(self, target):
        state = self.ui("/display/conversation")
        self.check(state["target"] == target and not self.queue()["queue"],
                   "local file input preserves the selected recipient and never auto-queues")
        return state

    def references(self, draft):
        escaped = re.findall(r'@"((?:\\.|[^"\\])*)"', draft)
        self.check(draft == "\n" + "\n".join('@"' + item + '"' for item in escaped) + "\n",
                   "every imported file is one quoted reference between exact newlines")
        return [re.sub(r'\\([\\"])', r'\1', item) for item in escaped]

    def verify_copies(self, draft, originals):
        paths = self.references(draft)
        self.check(len(paths) == len(originals) and len(set(paths)) == len(paths),
                   "a successful MIME import inserts every selected file exactly once")
        expected = {path.name: path.read_bytes() for path in originals}
        self.check(len(expected) == len(originals), "fixture source names are distinct")
        actual = {}
        for relative in paths:
            path = Path(relative)
            self.check(not path.is_absolute() and ".." not in path.parts
                       and len(path.parts) == 4 and path.parts[:2] == (".ssmcp", "inputs")
                       and path.parts[2].startswith("files-"),
                       "imported references identify only a private workspace file batch")
            copied = self.workspace / path
            self.check(stat.S_ISREG(copied.lstat().st_mode)
                       and stat.S_IMODE(copied.stat().st_mode) == 0o600
                       and stat.S_IMODE(copied.parent.stat().st_mode) == 0o700,
                       "imported files and their batch directory have private Unix permissions")
            actual[copied.name] = copied.read_bytes()
        self.check(actual == expected, "all imported names and complete original bytes are preserved")
        self.check(all(path.read_bytes() == expected[path.name] for path in originals),
                   "copying clipboard files leaves every source present and unchanged")
        return paths

    def read_native_image(self, relative, expected):
        response = self.response(
            "const r = await call(%s, 'ssmcp.fs.read_image', %s); "
            "image(r); return r.structuredContent;" % (
                json.dumps(self.client_id), json.dumps({"path": relative}),
            )
        )
        images = [item for item in response["content"] if item.get("type") == "image"]
        self.check(not self.messages(response) and len(images) == 1
                   and images[0]["mimeType"] == "image/png"
                   and base64.b64decode(images[0]["data"], validate=True) == expected,
                   "the imported PNG is actually returned as complete native MCP image content")
        metadata = response["structuredContent"]["result"]
        self.check(metadata["path"] == relative and metadata["size"] == len(expected)
                   and metadata["sha256"] == hashlib.sha256(expected).hexdigest(),
                   "native image metadata and image bytes identify the same private file")

    def paste_success(self, front, mime, payload, originals, *, command=False, thumbnail=False):
        before_reads = len(self.reads())
        payloads = {mime: payload}
        if thumbnail:
            payloads["image/png"] = png_fixture()
        self.select(payloads)
        front.send("/paste\r" if command else b"\x16")
        state = self.wait_state(front, lambda current: current["draft"]["text"].startswith('\n@"'),
                                "the actual file MIME did not insert private references")
        paths = self.verify_copies(state["draft"]["text"], originals)
        self.check(self.reads()[before_reads:] == [mime],
                   "the native clipboard adapter reads file MIME once, before offered thumbnails")
        self.check(state["draft"]["cursor"] == len(state["draft"]["text"].encode("utf-8")),
                   "file insertion leaves the cursor at the complete UTF-8 reference block end")
        self.unchanged_queue(self.target)
        return paths

    def rejected_batch(self, front, valid, invalid, marker, label):
        self.clear_draft(front)
        draft = "保留原稿 " + label + " 界"
        front.send(draft)
        before_state = self.wait_state(front, lambda current: current["draft"]["text"] == draft,
                                       "the rejection fixture draft was not visible")
        before_files, before_reads = self.inputs(), len(self.reads())
        self.select({"text/uri-list": (valid.as_uri() + "\r\n" + invalid.as_uri() + "\r\n").encode()})
        front.send(b"\x16")
        front.until(lambda shot: "Local input failed:" in text(shot) and marker in text(shot),
                    label + " produces a clear native import error")
        self.check(self.reads()[before_reads:] == ["text/uri-list"],
                   label + " exercised the actual file-list clipboard adapter")
        after = self.unchanged_queue(self.target)
        self.check(after["draft"] == before_state["draft"],
                   label + " preserves the complete Unicode draft and cursor without partial insertion")
        self.wait_inputs(front, before_files)
        self.check(self.inputs() == before_files,
                   label + " leaves no partial batch and preserves existing private copies")
        self.save_shot(label, front)

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(
            ["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        sources = self.directory / "copied files"
        sources.mkdir(mode=0o700)
        note, png = sources / 'notes "中文" draft.txt', sources / "image 中文.png"
        note.write_bytes("完整中文材料\nsecond line\n".encode("utf-8"))
        png.write_bytes(png_fixture())
        source_bytes = {path: path.read_bytes() for path in (note, png)}
        front = self.start_front("file-paste")
        self.target = self.ui("/display/conversation")["target"]
        self.check(not self.queue()["queue"], "the owned frontend begins with no queued requests")

        paths = self.paste_success(
            front, "text/uri-list",
            ("# file manager selection\r\n" + note.as_uri() + "\r\n" + png.as_uri() + "\r\n").encode(),
            [note, png], thumbnail=True,
        )
        native_text = self.call("ssmcp.fs.read_text", {"path": paths[0], "max_bytes": 1024})
        self.check(native_text["text"].encode("utf-8") == source_bytes[note]
                   and native_text["size"] == native_text["bytes"] == len(source_bytes[note])
                   and native_text["offset"] == 0 and native_text["eof"]
                   and native_text["next_offset"] == len(source_bytes[note])
                   and native_text["sha256"] == hashlib.sha256(source_bytes[note]).hexdigest(),
                   "the native rooted text tool reads every original UTF-8 byte of an imported file")
        self.read_native_image(paths[1], source_bytes[png])
        self.save_shot("uri-list", front, cells=True)

        self.clear_draft(front)
        self.paste_success(front, "x-special/gnome-copied-files",
                           ("cut\n" + note.as_uri() + "\n").encode(), [note], command=True)
        self.check(note.exists() and note.read_bytes() == source_bytes[note],
                   "GNOME cut clipboard intent is copied without moving or deleting its source")
        self.save_shot("gnome-cut", front)

        self.clear_draft(front)
        before_files, before_reads = self.inputs(), len(self.reads())
        literal = str(note)
        self.select({"text/plain;charset=utf-8": literal.encode("utf-8")})
        front.send(b"\x16")
        literal_state = self.wait_state(front, lambda current: current["draft"]["text"] == literal,
                                        "a plain-text absolute path did not remain literal")
        self.check(literal_state["draft"]["cursor"] == len(literal.encode("utf-8"))
                   and self.inputs() == before_files
                   and self.reads()[before_reads:] == ["text/plain;charset=utf-8"],
                   "text/plain path-like contents remain exact text and create no private file copies")
        self.unchanged_queue(self.target)

        missing, fifo, bad = sources / "missing-file.txt", sources / "named-pipe", sources / "bad.png"
        os.mkfifo(fifo, mode=0o600)
        bad.write_bytes(png_fixture()[:24])
        self.rejected_batch(front, note, missing, "missing-file", "missing")
        self.rejected_batch(front, note, fifo, "regular file", "fifo")
        self.rejected_batch(front, note, bad, "image", "bad-png")
        self.check(stat.S_ISFIFO(fifo.lstat().st_mode) and not missing.exists()
                   and bad.read_bytes() == png_fixture()[:24],
                   "the named pipe and invalid source fixtures remain unchanged")

        self.clear_draft(front)
        initial, addition = "正在编辑 界", "，继续补充"
        front.send(initial)
        self.wait_state(front, lambda current: current["draft"]["text"] == initial,
                        "the async edit fixture draft did not arrive")
        before_files, before_reads = self.inputs(), len(self.reads())
        self.select({"text/uri-list": (note.as_uri() + "\n" + png.as_uri() + "\n").encode()}, gate=True)
        front.send(b"\x16")
        self.wait_file(front, "blocked")
        try:
            front.send(addition)
            edited = self.wait_state(front, lambda current: current["draft"]["text"] == initial + addition,
                                     "typing during clipboard preparation did not remain editable")
        finally:
            (self.clipboard / "release").touch()
        front.until(lambda shot: "Draft or recipient changed" in text(shot),
                    "a successful async import rejects its stale draft stamp")
        after = self.unchanged_queue(self.target)
        self.check(after["draft"] == edited["draft"]
                   and after["draft"]["cursor"] == len((initial + addition).encode("utf-8"))
                   and self.reads()[before_reads:] == ["text/uri-list"],
                   "async completion preserves later Unicode edits instead of inserting stale references")
        self.wait_inputs(front, before_files)
        self.check(self.inputs() == before_files,
                   "discarding an async file result rolls back every new private copy")
        self.check(all(path.exists() and path.read_bytes() == data for path, data in source_bytes.items()),
                   "all source files remain byte-for-byte unchanged after every clipboard scenario")
        self.save_shot("stale-draft", front, cells=True)


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("file-paste E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("this fixture exercises the Linux wl-paste MIME adapter")
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd", "vt_snapshot": decoder}
    for executable in binaries.values():
        if not executable.is_file():
            parser.error(f"missing {executable}; build the candidates and vt_snapshot first")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH public-key + OAuth + owned PTY + independent vt100 decoder",
        "clipboard": "private PATH fake wl-paste; no real desktop clipboard or shared frontend",
        "scope": "file MIME priority, UTF-8/PNG @references, GNOME cut copy, native image read, literal text, atomic invalid batches and async draft rollback; no models",
        "binaries": {name: sha256(path) for name, path in binaries.items()},
        "sources": {name: sha256(Path(__file__).with_name(name)) for name in (
            "e2e_file_paste.py", "e2e_input_history.py", "e2e.py", "e2e_dashboard.py", "e2e_composer.py",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-file-paste-e2e-") as directory:
        harness = FilePaste(Path(directory), args.bin_dir, decoder, args.out)
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
                    harness.save_shot("failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
            (args.out / "report.json").write_text(encoded, encoding="utf-8")
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "duration_seconds": report["duration_seconds"]}), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
