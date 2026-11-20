#!/usr/bin/env python3
"""Check readable command cards using an owned router, native commands and PTY.

Consumes prebuilt product/router-fixture/VT-decoder binaries. All processes,
credentials, Git data and command side effects belong to this disposable fixture.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from e2e_composer import ENTER, ESC, HOME
from e2e_dashboard import text
from e2e_input_history import END
from e2e_interactions import Interactions, digest

ROWS = 40
OUTPUT_ROWS = {f"OUTPUT_ROW_{n}" for n in range(8)}


class ReadableCommands(Interactions):
    def __init__(self, directory, binary_dir, decoder, output, font):
        super().__init__(directory, binary_dir, decoder, output)
        self.font = font
        self.fixture_timeout_seconds = 180
        self.pngs = []

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        executable = shutil.which("true")
        if not executable:
            raise RuntimeError("the fixture requires the installed true utility")
        (Path(environment["PATH"]) / "true").symlink_to(Path(executable).absolute())
        return environment

    def start_front(self, label):
        # Do not use the legacy column parser or Left-to-Agents assumption.
        # The unified Sessions browser still exposes view=agents in UI state.
        front = self.frontend(label, self.config(label))
        self.oauth_login(front.credentials)
        front.input_bytes = 0
        send = front.send

        def counted(data):
            front.input_bytes += len(data.encode() if isinstance(data, str) else data)
            send(data)

        front.send = counted
        front.resize(ROWS, 120)
        front.send(ESC)
        front.pump(0.15)
        self.wait_state(front, lambda state: state["focus"] == "input"
                        and not state["draft"]["text"], "the owned input is ready")
        front.send("/sessions" + "\r")
        self.wait_state(front, lambda state: state["view"] == "agents"
                        and state["focus"] == "content"
                        and state["selected_agent"] == "member:next",
                        "the unified Sessions browser selects this process")
        front.until(lambda shot: "Sessions" in text(shot), "the current Sessions page")
        front.send(ENTER)
        self.wait_state(front, lambda state: state["view"] == "activity"
                        and state["focus"] == "input" and state["target"] is None
                        and state["native_recipient"] is None,
                        "Sessions opens the local Next caller conversation")
        return front

    def events(self):
        return self.ui("/display/events")

    def command_event(self, title):
        found = [event for event in self.events()["events"]
                 if event["command"] and title in event["summary"]]
        if len(found) != 1:
            raise AssertionError("expected one retained command for " + title)
        return found[0]

    def capture(self, front, name):
        shot = front.snapshot(cells=True)
        encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
        self.check(not any(secret and secret in encoded
                           for secret in [self.secret, *self.sensitive]),
                   name + ": the PTY frame contains no private credential or fixture token")
        stem = "readable-commands-" + name
        path = self.output / (stem + ".json")
        path.write_text(encoded, encoding="utf-8")
        (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
        png = self.output / (stem + ".png")
        subprocess.run([sys.executable, str(Path(__file__).with_name("render_ui_snapshot.py")),
                        str(path), str(png), "--font", str(self.font)],
                       check=True, timeout=20, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.frames.append(path.name)
        self.pngs.append(png.name)
        return shot

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       check=True, timeout=5, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        # One predictable git status line even if the client creates local metadata.
        (self.workspace / ".git/info/exclude").write_text("*\n!readable-result.txt\n")
        (self.workspace / "readable-result.txt").write_text("synthetic change\n")
        front = self.start_front("PTY-readable-commands")
        secret = "readable-command-private-fixture"
        self.sensitive.append(secret)
        counter = self.workspace / ".readable-command-runs"
        script = ("# INLINE_SOURCE_HEAD\n"
                  "from pathlib import Path\n"
                  "p=Path('.readable-command-runs')\n"
                  "p.write_text((p.read_text() if p.exists() else '')+'once\\n')\n"
                  "print('\\n'.join(f'OUTPUT_ROW_{n}' for n in range(8)))\n"
                  + "# bounded inline parameter padding\n" * 80
                  + "# INLINE_SOURCE_TAIL API_TOKEN=" + secret + "\n")
        commands = [
            ("Inspect repository state", ["git", "status", "--short"]),
            ("Inspect eight output lines", ["python3", "-c", script]),
            ("Inspect empty output", ["true"]),
        ]
        before = {job["job_id"] for job in self.call("ssmcp.exec.list")["jobs"]}
        jobs = []
        for title, argv in commands:
            job = self.call("ssmcp.exec.start",
                            {"command": argv, "cwd": str(self.workspace),
                             "title": title, "timeout_ms": 15000})
            finished = self.finish_job(job, front)
            self.check(finished["exit_code"] == 0, title + ": the real native command completed")
            jobs.append(finished)
        self.check(counter.read_text() == "once\n", "the Python command executed exactly once")
        self.quiet_until(front, lambda shot: "Ran true" in text(shot)
                         and "Ran python3 -c" in text(shot)
                         and "Ran git status --short" in text(shot),
                         "all completed command argv arrive in the real conversation")

        event_ids = {self.command_event(title)["id"] for title, _ in commands}
        for columns in (120, 48):
            front.resize(ROWS, columns)
            front.pump(0.15)
            front.until(lambda shot: shot["size"] == [ROWS, columns]
                        and "Ran true" in text(shot)
                        and "Ran python3 -c" in text(shot)
                        and "Ran git status --short" in text(shot),
                        "all adjacent command cards at " + str(columns) + " columns")
            shot = self.capture(front, f"folded-{columns}")
            visible = text(shot)
            compact = re.sub(r"\s+", "", visible)
            self.check(visible.count("Ran python3 -c") == 1 and "<script>" in visible,
                       f"{columns} columns: the inline script has one readable command header")
            self.check("INLINE_SOURCE_HEAD" not in compact
                       and "INLINE_SOURCE_TAIL" not in compact
                       and "boundedinlineparameterpadding" not in compact
                       and "frompathlibimportPath" not in compact,
                       f"{columns} columns: folded command details hide the inline source body")
            state = self.events()
            self.check(not any(event_ids.intersection(group["members"])
                               for group in state["tool_groups"]),
                       f"{columns} columns: commands are not members of a Tools group")
            self.check("Tools ·" not in visible,
                       f"{columns} columns: adjacent commands remain individually visible")
            self.check("?? readable-result.txt" in visible,
                       f"{columns} columns: the real git status output is visible")
            shown = set(re.findall(r"\bOUTPUT_ROW_[0-7]\b", visible))
            self.check(shown == {"OUTPUT_ROW_5", "OUTPUT_ROW_6", "OUTPUT_ROW_7"},
                       f"{columns} columns: the long command previews exactly three output rows")
            self.check("+5 lines · Show details · Enter" in visible
                       and visible.count("Show details") >= 3 and "(no output)" in visible,
                       f"{columns} columns: long, short and empty commands expose details")
            for title, _ in commands:
                event = self.command_event(title)
                self.check(event["status"] == "exit=0" and not event["expanded"]
                           and event["panel"]["visible_rows"] <= 3,
                           f"{columns} columns: {title} keeps its native status and preview bound")

            row = next(index for index, value in enumerate(shot["rows"])
                       if "Ran python3 -c" in value)
            col = shot["rows"][row].index("Ran python3 -c") + 1
            front.send(f"\x1b[<0;{col};{row + 1}M\x1b[<0;{col};{row + 1}m".encode())
            front.pump(0.15)
            event = self.command_event(commands[1][0])
            if not event["expanded"]:
                self.check(event["selected"], "a real click selects the exact Python command")
                front.send(ENTER)
            front.until(lambda value: "Command" in text(value) and "Enter fold" in text(value),
                        "the selected command expands in its conversation", cells=True)
            self.check(self.command_event(commands[1][0])["expanded"],
                       f"{columns} columns: click or selected Enter expands the exact command")
            front.send(HOME)
            front.pump(0.15)
            head = self.capture(front, f"expanded-head-{columns}")
            compact_head = re.sub(r"\s+", "", text(head))
            self.check("INLINE_SOURCE_HEAD" in compact_head
                       and "Inspecteightoutputlines" in compact_head,
                       f"{columns} columns: Home exposes the inline source and descriptive title")
            front.send(END)
            front.pump(0.15)
            tail = self.capture(front, f"expanded-output-{columns}")
            seen = set(re.findall(r"\bOUTPUT_ROW_[0-7]\b", text(tail)))
            self.check(seen == OUTPUT_ROWS,
                       f"{columns} columns: End exposes every real output line")
            header_row = next(index for index, value in enumerate(tail["rows"])
                              if "Ran python3 -c" in value)
            source_tail = tail
            for _ in range(6):
                compact_tail = re.sub(r"\s+", "", text(source_tail))
                if "INLINE_SOURCE_TAIL" in compact_tail and "[redacted]" in compact_tail:
                    break
                # Scroll the selected command's own panel in small increments.
                # PageUp follows the outer window height and can skip this tail.
                front.send(f"\x1b[<64;9;{header_row + 2}M".encode())
                front.pump(0.15)
                source_tail = front.snapshot(cells=True)
            source_tail = self.capture(front, f"expanded-source-tail-{columns}")
            compact_tail = re.sub(r"\s+", "", text(source_tail))
            self.check("INLINE_SOURCE_TAIL" in compact_tail and "[redacted]" in compact_tail,
                       f"{columns} columns: bounded panel scrolling reveals the redacted source tail")
            self.check(self.command_event(commands[1][0])["panel"]["visible_rows"] > 3,
                       f"{columns} columns: expanding increases the command's own viewport")
            front.send(ENTER)
            front.pump(0.15)
            self.check(not self.command_event(commands[1][0])["expanded"],
                       f"{columns} columns: Enter folds the same command again")

        after_jobs = self.call("ssmcp.exec.list")["jobs"]
        self.check({job["job_id"] for job in after_jobs}
                   == before | {job["job_id"] for job in jobs},
                   "viewing, clicking, resizing and scrolling create no extra native jobs")
        for job in jobs:
            after = self.call("ssmcp.exec.status", {"job_id": job["job_id"]})
            self.check(all(after[key] == job[key] for key in
                           ("job_id", "pid", "command", "cwd", "state", "running",
                            "exit_code", "exit_signal", "cancelled", "timed_out")),
                       "command inspection preserves the exact completed native job")
        self.check(counter.read_text() == "once\n",
                   "expanding or scrolling never re-executes the Python side effect")
        self.check(not self.ui("/display/conversation")["draft"]["text"]
                   and not self.queue()["queue"],
                   "command inspection leaves the composer and outgoing queue empty")
        return {"status": "passed", "checks": len(self.checks), "passed_checks": self.checks,
                "frames": self.frames, "pngs": self.pngs, "quiet_checks": self.quiet_checks,
                "widths": [120, 48], "native_commands": 3,
                "transport": "production account router, native exec, owned PTY, independent VT",
                "models_started": False, "real_user_credentials_used": False,
                "existing_clients_reloaded": False, "existing_services_touched": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("bin-dir", "decoder", "fixture-binary", "out", "font"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must name a complete frozen source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all prebuilt executables must exist; this fixture never builds them")
    if not args.font.is_file():
        parser.error("--font must name an existing font file")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use a new empty output directory to preserve previous evidence")
    before = {name: digest(path) for name, path in binaries.items()}
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-readable-commands-") as temporary:
        harness = ReadableCommands(Path(temporary), args.bin_dir.resolve(), binaries["decoder"],
                                   args.out, args.font.resolve())
        try:
            report = harness.run()
        except Exception as error:
            harness.diagnostics()
            detail = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    detail = detail.replace(secret, "[redacted]")
            report = {"status": "failed", "error": detail, "checks": len(harness.checks),
                      "passed_checks": harness.checks, "frames": harness.frames,
                      "pngs": harness.pngs}
        finally:
            harness.close_frontends()
            harness.cleanup()
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            raise AssertionError("a frozen executable changed during the PTY acceptance")
        sources = ["e2e_readable_commands.py", "e2e_interactions.py", "e2e_input_history.py",
                   "e2e_composer.py", "e2e_dashboard.py", "e2e_client_ui.py",
                   "e2e_account_gateway.py", "e2e.py", "render_ui_snapshot.py"]
        report.update(source_revision=args.source_revision,
                      elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      source_sha256={name: digest(Path(__file__).with_name(name))
                                     for name in sources}, inputs_unchanged=True)
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if any(secret and secret in encoded for secret in [harness.secret, *harness.sensitive]):
            raise AssertionError("the shareable report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
        print(json.dumps({"status": report["status"], "checks": report["checks"],
                          "report": str(args.out / "report.json"), "pngs": report["pngs"]}))
        if report["status"] != "passed":
            raise SystemExit(report["error"])


if __name__ == "__main__":
    main()
