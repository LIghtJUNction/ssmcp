#!/usr/bin/env python3
"""Measure a real Conversation read group in an isolated foreground PTY.

This driver reads only owned fixture skills through the production account
router. It never submits a user request, calls a model, or injects UI events.
Use an already-built CLI, library router fixture and vt_snapshot decoder.
A single run is a reachability/memory probe, not an A/B performance claim.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import tempfile
import time

from e2e_composer import Composer
from e2e_client_ui import empty_composer_ready
from e2e_dashboard import text


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def identity(pid):
    try:
        values = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    except FileNotFoundError:
        return None
    return {"pid": pid, "start_ticks": int(values[19]), "state": values[0]}


def owned_pids(parent):
    found, pending = set(), [parent]
    while pending:
        pid = pending.pop()
        if pid in found:
            continue
        found.add(pid)
        try:
            tasks = list(Path(f"/proc/{pid}/task").iterdir())
        except FileNotFoundError:
            continue
        for task in tasks:
            try:
                pending.extend(int(value) for value in (task / "children").read_text().split())
            except FileNotFoundError:
                continue
    return found


def memory(pid):
    before = identity(pid)
    if before is None:
        raise AssertionError("owned process disappeared during memory measurement")
    values = {}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]:
        key, _, value = line.partition(":")
        if value.strip().endswith("kB"):
            values[key] = int(value.split()[0])
    status = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        key, _, value = line.partition(":")
        if key == "VmHWM":
            status["rss_high_water_kib"] = int(value.split()[0])
    after = identity(pid)
    if after is None or before["start_ticks"] != after["start_ticks"]:
        raise AssertionError("owned PID changed identity during memory measurement")
    return {**before, **status, "rss_kib": values["Rss"], "pss_kib": values["Pss"],
            "private_kib": values["Private_Clean"] + values["Private_Dirty"],
            "pss_anon_kib": values.get("Pss_Anon"),
            "pss_file_kib": values.get("Pss_File"),
            "swap_pss_kib": values.get("SwapPss", 0)}


def sample(parent):
    rows = [memory(pid) for pid in sorted(owned_pids(parent))]
    own = next(row for row in rows if row["pid"] == parent)
    return {"parent": own, "processes": rows,
            "owned_tree": {key: sum(row[key] for row in rows)
                           for key in ("pss_kib", "private_kib", "swap_pss_kib")}}


def summarize(samples):
    # RSS includes shared mappings, so only PSS/private bytes are additive.
    return {scope: {key: statistics.median(row[scope][key] for row in samples)
                    for key in ("pss_kib", "private_kib", "swap_pss_kib")}
            for scope in ("parent", "owned_tree")}


class ReadGroupMemory(Composer):
    def __init__(self, directory, binary_dir, decoder, fixture, *, count, size):
        super().__init__(directory, binary_dir, decoder)
        self.fixture_binary = fixture
        self.fixture_timeout_seconds = 300
        self.http_timeout = 25
        self.count = count
        self.size = size
        self.result = {"phases": {}, "checks": self.checks}

    def create_skills(self):
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        paths = []
        for index in range(self.count):
            relative = f".agents/skills/memory-fixture-{index:03}/SKILL.md"
            path = self.workspace / relative
            path.parent.mkdir(parents=True)
            heading = (f"---\nname: memory-fixture-{index:03}\n"
                       "description: Inert read-only memory measurement fixture\n---\n")
            content = heading + ("Read-only fixture body.\n" * (self.size // 23 + 1))
            path.write_text(content[:self.size])
            paths.append(relative)
        self.result["workload"] = {"skills": self.count, "document_bytes_each": self.size,
                                   "paths": paths, "terminal": {"rows": 58, "cols": 106}}

    def event_group(self):
        events = self.ui("/display/events")
        self.check(events["event_scope"] == "conversation", "measurement uses real Conversation EventView")
        groups = [item for item in events["read_groups"] if item["count"] == self.count]
        if len(groups) != 1:
            raise AssertionError(f"expected one real {self.count}-item read group; observed "
                                 f"{[(group['count'], group['expanded']) for group in events['read_groups']]}")
        group = groups[0]
        # Expansion and selection are view state, not retained record data.
        retained_keys = ("id", "summary", "lines", "bytes", "truncated", "status", "command", "kind")
        members = [{key: item.get(key) for key in retained_keys}
                   for item in events["events"] if item["id"] in group["members"]]
        return {"group": group, "members": members,
                "member_retained_bytes": sum(item["bytes"] for item in members),
                "member_retained_lines": sum(item["lines"] for item in members),
                "retained_events": events["retained_events"], "retained_bytes": events["retained_bytes"],
                "dropped_events": events["dropped_events"]}

    def phase(self, name, front, *, expanded=None):
        # The frame predicate, not elapsed sleep, establishes render completion.
        if expanded is not None:
            marker = ".agents/skills/memory-fixture-000/SKILL.md" if expanded else f"Read {self.count} items"
            shot = front.until(lambda state: marker in text(state), f"{name} visible frame")
            group = self.event_group()
            self.check(group["group"]["expanded"] == expanded, f"{name} native expansion state matches the real frame")
        else:
            shot, group = front.snapshot(), None
        samples = []
        for _ in range(7):
            front.pump(0.1)
            samples.append(sample(front.process.pid))
        phase = {"group": group, "screen_rows": shot["rows"], "samples": samples,
                 "median": summarize(samples),
                 "sampled_parent_pss_max_kib": max(row["parent"]["pss_kib"] for row in samples)}
        self.result["phases"][name] = phase
        return phase

    def click_group(self, front):
        marker = f"Read {self.count} items"
        shot = front.until(lambda value: marker in text(value), "selectable native read-group heading")
        row = next(index for index, value in enumerate(shot["rows"]) if marker in value)
        # SGR coordinates are one-based; the real mouse handler selects/toggles
        # the EventView hit row. No private display state is mutated.
        front.send(f"\x1b[<0;5;{row + 1}M\x1b[<0;5;{row + 1}m")

    def run(self):
        self.create_skills()
        self.prepare()
        front = self.frontend("event-cache", self.config("event-cache"))
        self.oauth_login(front.credentials)
        front.until(empty_composer_ready, "isolated real foreground composer")
        self.phase("before_scan", front)
        result = self.call("ssmcp.skills.list", {"paths": [".agents/skills"], "limit": self.count})
        self.check(not result.get("spilled"), "bounded skill listing remains an actual inline native result")
        self.phase("initial_folded", front, expanded=False)
        self.click_group(front)
        expanded = self.phase("expanded", front, expanded=True)
        self.click_group(front)
        folded = self.phase("folded", front, expanded=False)
        self.click_group(front)
        reopened = self.phase("reexpanded", front, expanded=True)
        for other in (folded, reopened):
            self.check(other["group"]["group"]["members"] == expanded["group"]["group"]["members"],
                       "collapse/reopen preserves every original read event identity")
            self.check(other["group"]["members"] == expanded["group"]["members"],
                       "collapse/reopen preserves retained read summaries, lengths and metadata")
        visible = lambda phase: [row.strip() for row in phase["screen_rows"] if "memory-fixture-" in row]
        self.check(visible(expanded) == visible(reopened), "reopening renders the same owned file paths")
        self.check(self.ui("/display/conversation")["draft"]["text"] == "", "measurement never invents a user requirement")
        self.check(not self.external_invocations("event-cache"), "measurement never invokes an external coding harness")
        return self.result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--skills", type=int, default=12)
    parser.add_argument("--document-bytes", type=int, default=2048)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("physical memory measurements require Linux /proc/smaps_rollup")
    if not 3 <= args.skills <= 48 or not 256 <= args.document_bytes <= 128 * 1024:
        parser.error("owned workload must use 3..48 skills of 256..131072 bytes")
    args.report.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "measurement": "single-binary-real-PTY-reachability-probe",
              "valid_ab_comparison": False, "memory_goal_met": None,
              "cli_sha256": digest(args.bin_dir / "ssmcp"), "fixture_sha256": digest(args.fixture),
              "decoder_sha256": digest(args.decoder),
              "notes": ["A single probe cannot establish an optimization benefit.",
                        "Phase medians are resident PSS; sampled maxima are not continuous peaks.",
                        "VmHWM is per-process RSS high-water, not PSS and not summed across processes.",
                        "Physical document size is not assumed to equal rendered audit body size."]}
    with tempfile.TemporaryDirectory(prefix="ssmcp-event-cache-") as temporary:
        os.chmod(temporary, 0o700)
        harness = ReadGroupMemory(Path(temporary), args.bin_dir.resolve(), args.decoder.resolve(),
                                  args.fixture.resolve(), count=args.skills, size=args.document_bytes)
        try:
            report["result"] = harness.run()
            report["passed"] = True
        except BaseException as error:
            report["passed"] = False
            report["failure"] = str(error)
            report["result"] = harness.result
            raise
        finally:
            try:
                harness.close_frontends()
                harness.cleanup()
                report["cleanup_confirmed"] = all(process.poll() is not None for process in harness.processes)
            finally:
                args.report.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
