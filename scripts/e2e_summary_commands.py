#!/usr/bin/env python3
"""Check command summaries using two isolated authenticated relays and real PTYs.

Consumes frozen client/server/router/VT binaries. No Cargo, model, installed
account, existing client or service is used. Readable command cards before
summary updates are covered separately by e2e_readable_commands.py.
"""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

from e2e_client_ui import body_text, prompt_text
from e2e_composer import LEFT
from e2e_dashboard import text
from e2e_interactions import digest
from e2e_readable_commands import ReadableCommands
from e2e_tool_summaries import DRAFT, TOOL, ToolSummaries

SCENARIOS = ("exec-only", "mixed")


class SummaryCommands(ToolSummaries):
    def __init__(self, directory, binary_dir, decoder, output, font, scenario):
        super().__init__(directory, binary_dir, decoder, output, font)
        self.scenario = scenario
        self.fixture_timeout_seconds = 180
        self.native_requests = {}
        self.jobs = []
        self.baseline_ids = set()

    # Reuse the current Sessions -> local Next route without starting a model.
    start_front = ReadableCommands.start_front

    @property
    def call_count(self):
        return sum(self.native_requests.values())

    def native_work(self, name, arguments):
        result = self.call(name, arguments)
        self.native_requests[name] = self.native_requests.get(name, 0) + 1
        return result

    def stat(self, phase):
        result = self.native_work("ssmcp.fs.stat", {"path": "input.txt"})
        self.check(result["kind"] == "file", phase + ": the native file lookup succeeds")

    def run_native_command(self, front, tag):
        script = (
            "from pathlib import Path\n"
            "p = Path('.summary-command-runs')\n"
            f"p.write_text((p.read_text() if p.exists() else '') + {tag!r} + '\\n')\n"
            f"print({tag!r})\n"
        )
        started = self.native_work("ssmcp.exec.start", {
            "command": ["python3", "-c", script], "cwd": str(self.workspace),
            "title": "Execute owned " + tag + " command", "timeout_ms": 10000,
        })
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            finished = self.native_work("ssmcp.exec.wait", {
                "job_id": started["job_id"], "wait_ms": 500,
            })
            front.pump(0.02)
            if finished.get("running") is False:
                self.check(finished.get("exit_code") == 0,
                           tag + ": the real native command completes successfully")
                self.jobs.append({
                    "job_id": started["job_id"], "exit_code": finished["exit_code"],
                })
                return
        raise AssertionError("the owned native command exceeded its completion deadline")

    def phase(self, front, tag):
        if self.scenario == "mixed":
            self.stat(tag + " before command")
        self.run_native_command(front, tag)
        if self.scenario == "mixed":
            self.stat(tag + " after command")

    def canonical(self, group_id=None):
        _, listed = self.groups()
        groups = [group for group in listed if group["group_id"] not in self.baseline_ids]
        if len(groups) != 1:
            raise AssertionError("expected one canonical API group: " + json.dumps(groups))
        group = groups[0]
        self.check((group_id is None or group["group_id"] == group_id)
                   and group["call_count"] == self.call_count,
                   "one stable canonical group counts every real stat/start/wait request")
        return group

    def visible_canonical(self, front, group_id, summary, *, expanded=None):
        _, visible = self.wait_group(
            front, group_id,
            lambda group: group["summary"] == summary
            and group["call_count"] == self.call_count
            and (expanded is None or group["expanded"] is expanded),
            "the unique summary window reaches its expected count and fold state")
        members = visible["members"]
        self.check(visible["count"] == len(members) == self.call_count
                   and len(set(members)) == len(members),
                   "the single visible summary window owns every logical row exactly once")
        return visible

    def capture_folded_pair(self, front, group_id, summary):
        renderer = Path(__file__).with_name("render_ui_snapshot.py")
        for columns in (120, 48):
            front.resize(32, columns)
            shot = front.until(
                lambda value: value["size"] == [32, columns]
                and body_text(value).count(summary) == 1
                and re.search(r"\b" + str(self.call_count) + r" calls\b", body_text(value))
                and DRAFT in prompt_text(value),
                self.scenario + ": one summary title, count and draft at "
                + str(columns) + " columns", cells=True)
            self.visible_canonical(front, group_id, summary, expanded=False)
            title = next(line for line in body_text(shot).splitlines() if summary in line)
            self.check("▸" in title,
                       str(columns) + " columns: the physical summary header remains folded")
            self.check(self.ui("/display/conversation/draft") == self.saved_draft,
                       str(columns) + " columns: the unsent draft and interior cursor are unchanged")
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            if any(secret and secret in encoded for secret in [self.secret, *self.sensitive]):
                raise AssertionError("a private fixture credential reached a terminal snapshot")
            stem = "summary-commands-" + self.scenario + "-folded-" + str(columns)
            snapshot = self.output / (stem + ".json")
            snapshot.write_text(encoded, encoding="utf-8")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
            subprocess.run([
                sys.executable, str(renderer), str(snapshot),
                str(self.output / (stem + ".png")), "--font", str(self.font),
            ], check=True, capture_output=True, text=True, timeout=20)
            self.frames.append(stem + ".png")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        (self.workspace / "input.txt").write_text("owned summary command fixture\n")
        front = self.start_front("PTY-summary-commands-" + self.scenario)
        front.resize(32, 120)
        front.send(DRAFT)
        front.send(LEFT * 3)
        expected_draft = {"text": DRAFT, "cursor": len(DRAFT.encode()) - 3}
        self.saved_draft = self.wait_state(
            front, lambda state: state["draft"] == expected_draft,
            "the owned draft has an interior cursor")["draft"].copy()
        discovered = self.response("return await tools.search(%s, %s);" % (
            json.dumps(self.client_id),
            json.dumps({"name_prefix": TOOL, "include_schemas": True})))
        self.check(any(item.get("name") == TOOL
                       for item in discovered["structuredContent"]["result"]["tools"]),
                   "the live client advertises the exact native summary primitive")
        self.baseline_ids = {group["group_id"] for group in self.groups()[1]}

        self.phase(front, "first")
        initial = self.canonical()
        group_id = initial["group_id"]
        summary = ("Local command checks" if self.scenario == "exec-only"
                   else "Mixed file and command checks")
        labeled = self.update(initial, summary)
        visible = self.visible_canonical(front, group_id, summary)
        self.check(visible["expanded"] is True,
                   "the first summary preserves the command group's readable default")
        front.until(lambda shot: body_text(shot).count(summary) == 1
                    and "Ran python3 -c" in body_text(shot),
                    "the same summary ID has one real title and readable native argv")
        self.check(True, "the real PTY shows one summary title and the original command argv")

        # An actual pointer gesture establishes the user's fold preference.
        self.mouse(front, summary, 0)
        self.visible_canonical(front, group_id, summary, expanded=False)
        self.phase(front, "second")
        current = self.canonical(group_id)
        self.check(current["revision"] == labeled["revision"]
                   and current["covered_call_count"] == labeled["covered_call_count"],
                   "new native events keep the same summary revision and prior coverage")
        self.visible_canonical(front, group_id, summary, expanded=False)
        self.visible_canonical(front, group_id, summary, expanded=False)
        self.check(self.ui("/display/conversation/draft") == self.saved_draft,
                   "new calls and repeated native refresh preserve the user's draft and cursor")

        final_summary = ("Local commands complete" if self.scenario == "exec-only"
                         else "Mixed checks complete")
        final = self.update(current, final_summary)
        self.visible_canonical(front, group_id, final_summary, expanded=False)
        self.capture_folded_pair(front, group_id, final_summary)
        self.check((self.workspace / ".summary-command-runs").read_text() == "first\nsecond\n",
                   "summary updates, folding and inspection never re-execute either command")
        return {
            "status": "passed", "scenario": self.scenario,
            "checks": len(self.checks), "passed_checks": self.checks,
            "frames": self.frames, "summary_receipts": self.receipts,
            "native_requests": self.native_requests, "native_jobs": self.jobs,
            "logical_call_count": self.call_count,
            "final_group": {key: final[key] for key in (
                "group_id", "revision", "summary", "call_count", "covered_call_count")},
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("bin-dir", "decoder", "fixture-binary", "out", "font"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be the complete frozen binary source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all frozen executables must exist; this fixture never builds them")
    if not args.font.is_file():
        parser.error("--font must name an existing font file")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use a new empty output directory to preserve earlier evidence")
    sources = [
        "e2e_summary_commands.py", "e2e_tool_summaries.py", "e2e_readable_commands.py",
        "e2e_interactions.py", "e2e_input_history.py", "e2e_composer.py",
        "e2e_dashboard.py", "e2e_client_ui.py", "e2e_account_gateway.py",
        "e2e.py", "render_ui_snapshot.py",
    ]
    before = {name: digest(path) for name, path in binaries.items()}
    source_before = {name: digest(Path(__file__).with_name(name)) for name in sources}
    font_before = digest(args.font)
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    results, secrets = [], []
    for scenario in SCENARIOS:
        print("SCENARIO " + scenario, flush=True)
        with tempfile.TemporaryDirectory(prefix="ssmcp-summary-commands-" + scenario + "-") as temporary:
            harness = SummaryCommands(
                Path(temporary), args.bin_dir.resolve(), binaries["decoder"],
                args.out / scenario, args.font.resolve(), scenario)
            phase_started = time.monotonic()
            try:
                result = harness.run()
            except Exception as error:
                harness.diagnostics()
                detail = f"{type(error).__name__}: {error}"
                for secret in [harness.secret, *harness.sensitive]:
                    if secret:
                        detail = detail.replace(secret, "[redacted]")
                result = {
                    "status": "failed", "scenario": scenario, "error": detail,
                    "checks": len(harness.checks), "passed_checks": harness.checks,
                    "frames": harness.frames, "summary_receipts": harness.receipts,
                    "native_requests": harness.native_requests, "native_jobs": harness.jobs,
                }
            finally:
                try:
                    harness.close_frontends()
                finally:
                    harness.cleanup()
            result["elapsed_seconds"] = round(time.monotonic() - phase_started, 3)
            results.append(result)
            secrets.extend([harness.secret, *harness.sensitive])
    after = {name: digest(path) for name, path in binaries.items()}
    source_after = {name: digest(Path(__file__).with_name(name)) for name in sources}
    font_after = digest(args.font)
    unchanged = before == after and source_before == source_after and font_before == font_after
    report = {
        "status": "passed" if unchanged and all(item["status"] == "passed" for item in results) else "failed",
        "source_revision": args.source_revision,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "checks": sum(item["checks"] for item in results), "scenarios": results,
        "frames": [item["scenario"] + "/" + frame for item in results for frame in item["frames"]],
        "binaries_sha256_before": before, "binaries_sha256_after": after,
        "source_sha256_before": source_before, "source_sha256": source_after,
        "font_path": str(args.font.resolve()), "font_sha256_before": font_before,
        "font_sha256": font_after, "inputs_unchanged": unchanged,
        "transport": "two independent private account relays, real native exec and owned PTYs",
        "models_started": False, "real_user_credentials_used": False,
        "existing_clients_reloaded": False, "existing_services_touched": False,
    }
    if not unchanged:
        report["error"] = "a frozen executable, fixture source or font changed during acceptance"
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if any(secret and secret in encoded for secret in secrets):
        raise AssertionError("the report contains a private fixture credential")
    (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": report["checks"],
                      "frames": len(report["frames"]), "report": str(args.out / "report.json")}))
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
