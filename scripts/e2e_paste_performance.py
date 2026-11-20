#!/usr/bin/env python3
"""Measure real paste/input latency with an owned router, PTY and VT decoder.

Run once for each frozen baseline/candidate binary set. --baseline-report adds
descriptive comparisons; no machine-dependent speed threshold replaces the
correctness checks. All payloads are synthetic. Nothing is built or installed.

Timings are observations of the physical PTY. pty_last_write_at means the
terminal accepted the bytes, NOT that the client read them or completed fsync.
Preparing may be superseded before a frame is observed; null means unobserved.
The reference-ready frame follows the normal asynchronous private-file path.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import stat
import tempfile
import time

from e2e_client_ui import prompt_text
from e2e_composer import ENTER, ESC
from e2e_dashboard import text
from e2e_input_history import PASTE_END, PASTE_START
from e2e_interactions import Interactions, digest


MAX_PASTE = 4 * 1024 * 1024
REFERENCE = re.compile(r'@"(\.ssmcp/inputs/paste-[0-9a-f]{32}\.txt)"')
PLAIN_END = "PLAIN_INPUT_DONE"
HELPERS = (
    "e2e_paste_performance.py", "e2e_interactions.py", "e2e_input_history.py",
    "e2e_composer.py", "e2e_dashboard.py", "e2e_client_ui.py",
    "e2e_account_relay.py", "e2e_account_gateway.py", "e2e.py",
)


def synthetic_payload(size):
    """Exact UTF-8 byte size, >= 8 lines, mixed newlines; no real user text."""
    head = "".join(
        f"SYNTHETIC_PASTE_ROW_{index:02d} 中文 e\u0301 only fixture data\r\n"
        for index in range(10)
    ).encode()
    tail = b"\rSYNTHETIC_PASTE_END\n"
    if size < len(head) + len(tail):
        raise ValueError("synthetic multiline payload is too small")
    return head + b"x" * (size - len(head) - len(tail)) + tail


def source_binding(root):
    paths = {root / name for name in ("Cargo.toml", "Cargo.lock", "build.rs")}
    for directory in ("src", "crates"):
        paths.update(path for path in (root / directory).rglob("*") if path.is_file())
    hashes = {str(path.relative_to(root)): digest(path)
              for path in sorted(paths) if path.is_file()}
    aggregate = hashlib.sha256(
        json.dumps(hashes, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return {
        "product_source_sha256": aggregate,
        "product_source_files": len(hashes),
        "fixture_source_sha256": {name: digest(root / "scripts" / name) for name in HELPERS},
    }


class PastePerformance(Interactions):
    def __init__(self, directory, binary_dir, decoder, output, case_timeout):
        super().__init__(directory, binary_dir, decoder, output)
        self.case_timeout = case_timeout
        self.fixture_timeout_seconds = min(1800, int(8 * case_timeout + 120))
        self.samples = []
        self.timing_active = False

    def rpc(self, *args, **kwargs):
        if self.timing_active:
            raise AssertionError("an RPC must not cause redraw during paste timing")
        return super().rpc(*args, **kwargs)

    def ui(self, path):
        if path != "/display/conversation":
            return super().ui(path)
        # Object projections include the whole draft and cannot be paged.
        # Read only the state used here; keep every text page below 8 KiB.
        scalar_ui = super().ui
        state = {key: scalar_ui(f"{path}/{key}") for key in (
            "view", "focus", "target", "native_recipient", "selected_agent",
            "retained_paste_bytes",
        )}
        text_path = path + "/draft/text"
        offset, total, parts = 0, None, []
        while True:
            page = self.observe("ssmcp.ui.get", {
                "path": text_path, "offset": offset, "limit": 1024,
            })
            if (page.get("schema_version") != 1 or page.get("path") != text_path
                    or not isinstance(page.get("value"), str)
                    or type(page.get("total")) is not int
                    or not 0 <= page["total"] <= 16 * 1024):
                raise AssertionError("native draft text page has an invalid envelope")
            if total is None:
                total = page["total"]
            chunk = page["value"]
            end = offset + len(chunk)  # ui.get offsets count Unicode code points.
            if (page["total"] != total or len(chunk) != min(1024, total - offset)
                    or (end < total and (type(page.get("next_offset")) is not int
                                         or page["next_offset"] != end))
                    or (end == total and "next_offset" in page)):
                raise AssertionError("native draft text pages are incomplete or inconsistent")
            parts.append(chunk)
            if end == total:
                break
            offset = end
        state["draft"] = {"text": "".join(parts),
                          "cursor": scalar_ui(path + "/draft/cursor")}
        return state

    def start_front(self, label):
        # Use the current Sessions entry, without a fixed column-count fallback.
        front = self.frontend(label, self.config(label))
        self.oauth_login(front.credentials)
        front.input_bytes = 0
        send = front.send

        def counted(data):
            if self.timing_active:
                raise AssertionError("an extra key must not cause a timed redraw")
            front.input_bytes += len(data.encode() if isinstance(data, str) else data)
            send(data)

        front.send = counted
        front.resize(32, 120)
        front.send(ESC)
        front.pump(0.15)
        self.wait_state(front, lambda value: value["focus"] == "input"
                        and not value["draft"]["text"], "the owned composer is empty")
        front.send("/sessions\r")
        self.wait_state(front, lambda value: value["view"] == "agents"
                        and value["selected_agent"] == "member:next",
                        "Sessions selects the local Next caller")
        front.send(ENTER)
        self.wait_state(front, self.route_ready, "the real Next caller input is active")
        self.check("--preview" not in front.process.args,
                   "the measured client is a real account client, not a preview")
        return front

    @staticmethod
    def route_ready(value):
        return (value["view"] == "activity" and value["focus"] == "input"
                and value["target"] == "next" and value["native_recipient"] is None)

    def queue(self):
        # Read the exact records used by the before/after comparison.
        # Require one complete snapshot; keep UI observation free of work calls.
        path = "/steering/queue"
        projection = self.observe("ssmcp.ui.get", {
            "path": path, "offset": 0, "limit": 4096,
        })
        if (not isinstance(projection, dict)
                or type(projection.get("schema_version")) is not int
                or projection["schema_version"] != 1
                or projection.get("path") != path
                or not isinstance(projection.get("value"), list)
                or type(projection.get("total")) is not int
                or not 0 <= projection["total"] <= 4096
                or len(projection["value"]) != projection["total"]
                or "next_offset" in projection):
            raise AssertionError("native queue projection is invalid or incomplete")
        return {"queue": projection["value"]}

    def empty_draft(self, front):
        state = self.ui("/display/conversation")
        self.check(self.route_ready(state), "each sample retains the ordinary Next caller route")
        if state["draft"]["text"]:
            # One owned-draft clear, outside timing; avoid 16 KiB of backspaces.
            front.send(b"\x03")
            self.wait_state(front, lambda value: self.route_ready(value)
                            and not value["draft"]["text"], "clear only the synthetic draft")
        # An empty RPC draft can precede its repaint. Require the actual empty
        # composer before timing, without accepting a stale reference frame.
        return front.until(
            lambda shot: shot["alternate_screen"] and not shot["hide_cursor"]
            and "".join(prompt_text(shot).split()) == "❯Messagetheagent",
            "the physical composer is empty before the next sample")

    def transfer(self, front, data, sample, predicate=None):
        """No throttling: write the remaining span; select only on backpressure.

        Output is drained through the existing Frontend/VT decoder, never through
        UI RPCs. Snapshot/decode cost is part of these observed upper bounds.
        """
        started = time.monotonic_ns()
        deadline = time.monotonic() + self.case_timeout
        pending = memoryview(data)
        previous_output = len(front.transcript)
        last_shot = None
        sample.update(
            clock="monotonic_ns", started_at=started, wire_bytes=len(data),
            pty_first_write_at=None, pty_last_write_at=None,
            first_visible_at=None, preparing_visible_at=None, ready_at=None,
            reference_ready_at=None, ready_reference=None, write_calls=0, short_writes=0,
            write_attempts=0, min_write_bytes=None, max_write_bytes=0,
            backpressure_waits=0, vt_snapshots=0, accepted_bytes=0,
        )
        with selectors.DefaultSelector() as selector:
            selector.register(front.master, selectors.EVENT_READ)
            selector.register(front.process.stdout, selectors.EVENT_READ)
            while pending or (predicate is not None and sample["ready_at"] is None):
                if time.monotonic() >= deadline:
                    raise AssertionError("timed input did not reach its real ready frame\n"
                                         + front.failure_detail())
                if front.process.poll() is not None:
                    raise AssertionError("the owned client exited during input\n"
                                         + front.failure_detail())
                blocked = False
                if pending:
                    before_write = time.monotonic_ns()
                    sample["write_attempts"] += 1
                    try:
                        count = os.write(front.master, pending)
                    except InterruptedError:
                        continue
                    except BlockingIOError:
                        blocked = True
                        sample["backpressure_waits"] += 1
                    else:
                        if count <= 0:
                            raise AssertionError("a nonblocking PTY write made no progress")
                        sample["write_calls"] += 1
                        sample["short_writes"] += int(count < len(pending))
                        sample["accepted_bytes"] += count
                        sample["min_write_bytes"] = min(sample["min_write_bytes"] or count, count)
                        sample["max_write_bytes"] = max(sample["max_write_bytes"], count)
                        front.input_bytes += count
                        if sample["pty_first_write_at"] is None:
                            sample["pty_first_write_at"] = before_write
                        pending = pending[count:]
                        if not pending:
                            sample["pty_last_write_at"] = time.monotonic_ns()

                events = selectors.EVENT_READ | (selectors.EVENT_WRITE if blocked else 0)
                selector.modify(front.master, events)
                # A writable PTY is never polled with a fixed per-chunk sleep.
                # Once all bytes are sent, waiting is solely for output/readiness.
                waiting = blocked or (not pending and predicate is not None)
                wait = min(1.0, max(0, deadline - time.monotonic())) if waiting else 0
                available = selector.select(wait)
                if any(mask & selectors.EVENT_READ for _, mask in available):
                    front.pump(0)
                if len(front.transcript) != previous_output:
                    previous_output = len(front.transcript)
                    if predicate is not None:
                        last_shot = front.decoder.request(op="snapshot", cells=False)
                        sample["vt_snapshots"] += 1
                        now = time.monotonic_ns()
                        visible, preparing, ready, reference = predicate(last_shot)
                        if preparing and sample["preparing_visible_at"] is None:
                            sample["preparing_visible_at"] = now
                        if visible and sample["first_visible_at"] is None:
                            sample["first_visible_at"] = now
                        if ready and not pending and sample["ready_at"] is None:
                            sample["ready_at"] = now
                            if sample["expected"] == "reference":
                                sample["reference_ready_at"] = now
                                sample["ready_reference"] = reference
                            # No feed, pump, key or RPC may intervene: retain the
                            # exact timed decoder state as a renderable cell grid.
                            grid = front.decoder.request(op="snapshot", cells=True)
                            if any(grid[key] != last_shot[key] for key in
                                   ("rows", "size", "cursor", "hide_cursor", "alternate_screen")):
                                raise AssertionError("ready cell grid differs from the timed frame")
                            rows, columns = grid["size"]
                            if (len(grid.get("cells", [])) != rows
                                    or any(len(row) != columns for row in grid["cells"])):
                                raise AssertionError("ready snapshot is missing its complete cell grid")
                            last_shot = grid
                            sample["ready_frame_grid_confirmed"] = True
        return last_shot

    def send_stream(self, front, data):
        # Also keep inherited helpers correct for short writes, without their
        # old fixed 2 KiB / 10 ms pacing. This path itself is not a measurement.
        self.transfer(front, data, {})

    def capture(self, front, name, shot=None):
        shot = shot or front.decoder.request(op="snapshot", cells=True)
        encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
        self.check(not any(secret and secret in encoded
                           for secret in [self.secret, *self.sensitive]),
                   name + ": the physical VT frame contains no fixture credential")
        stem = "paste-" + name
        (self.output / (stem + ".json")).write_text(encoded, encoding="utf-8")
        (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
        self.frames.append(stem + ".json")

    def sample(self, front, name, payload, *, bracketed=True, expected="reference"):
        initial = self.empty_draft(front)
        baseline_prompt = prompt_text(initial)
        before_queue = self.queue()["queue"]
        inputs = self.workspace / ".ssmcp" / "inputs"
        before_files = set(inputs.glob("paste-*.txt")) if inputs.exists() else set()
        before_paths = {path.relative_to(self.workspace).as_posix() for path in before_files}
        sample = {
            "name": name, "status": "running", "payload_bytes": len(payload),
            "payload_sha256": hashlib.sha256(payload).hexdigest(),
            "bracketed": bracketed, "expected": expected,
            "before_reference_paths": sorted(before_paths),
        }
        self.samples.append(sample)

        def observe(shot):
            prompt = prompt_text(shot)
            compact = "".join(prompt.split())
            screen = text(shot)
            preparing = "Preparing local input" in screen
            matches = list(REFERENCE.finditer(compact))
            reference = (matches[0] if len(matches) == 1
                         and matches[0][1] not in before_paths else None)
            if expected == "reference":
                ready = reference is not None and not preparing and not shot["hide_cursor"]
                visible = preparing or reference is not None
            elif expected == "rejected":
                ready = "Paste exceeds" in screen
                visible = ready
            else:
                ready = PLAIN_END in compact and not shot["hide_cursor"]
                visible = prompt != baseline_prompt and ("PLAIN" in prompt or "Z" in prompt)
            return visible, preparing, ready, reference[0] if reference is not None else None

        data = PASTE_START + payload + PASTE_END if bracketed else payload
        before_input = front.input_bytes
        self.timing_active = True
        try:
            ready_shot = self.transfer(front, data, sample, observe)
        except Exception:
            sample["status"] = "failed"
            raise
        finally:
            self.timing_active = False
        self.check(front.input_bytes - before_input == len(data),
                   name + ": every payload byte was written once, without an extra key")
        sample["pty_write_ms"] = (sample["pty_last_write_at"] - sample["pty_first_write_at"]) / 1e6
        sample["visible_ms"] = (sample["first_visible_at"] - sample["pty_first_write_at"]) / 1e6
        sample["ready_ms"] = (sample["ready_at"] - sample["pty_first_write_at"]) / 1e6
        sample["after_pty_last_write_ready_ms"] = (
            sample["ready_at"] - sample["pty_last_write_at"]) / 1e6
        preparing = sample["preparing_visible_at"]
        sample["after_pty_last_write_preparing_ms"] = (
            (preparing - sample["pty_last_write_at"]) / 1e6 if preparing is not None else None)
        sample["preparing_to_reference_ready_ms"] = (
            (sample["reference_ready_at"] - preparing) / 1e6
            if preparing is not None and sample["reference_ready_at"] is not None else None)
        sample["pty_mib_per_second"] = (
            len(data) / (1024 * 1024) / max(sample["pty_write_ms"] / 1000, 1e-9))

        # These RPCs deliberately happen AFTER the recorded physical ready frame.
        state = self.ui("/display/conversation")
        self.check(self.route_ready(state) and state["retained_paste_bytes"] is None,
                   name + ": completion leaves the ordinary input route usable")
        draft = state["draft"]["text"]
        after_files = set(inputs.glob("paste-*.txt")) if inputs.exists() else set()
        if expected == "reference":
            found = REFERENCE.fullmatch(draft.strip())
            self.check(found is not None, name + ": the complete draft is one private reference")
            self.check(found[0] == sample["ready_reference"],
                       name + ": final draft matches the exact new reference in the timed frame")
            path = self.workspace / found[1]
            self.check(after_files - before_files == {path},
                       name + ": exactly one new private file was created")
            self.check(path.read_bytes() == payload,
                       name + ": stored bytes preserve Unicode, CRLF and the complete tail")
            self.check(stat.S_IMODE(path.stat().st_mode) == 0o600
                       and stat.S_IMODE(inputs.stat().st_mode) == 0o700,
                       name + ": the pasted file and input directory retain private permissions")
            sample["stored_bytes"] = path.stat().st_size
            sample["stored_sha256"] = digest(path)
        elif expected == "rejected":
            self.check(draft == "" and after_files == before_files,
                       name + ": over-limit paste preserves the empty draft and creates no file")
        else:
            self.check(draft.encode() == payload and state["draft"]["cursor"] == len(payload)
                       and after_files == before_files,
                       name + ": raw non-bracketed UTF-8 input stays complete and inline")
        self.check(self.queue()["queue"] == before_queue,
                   name + ": paste does not create or send a user requirement")
        # Preserve exactly the frame that satisfied the timed predicate, not a
        # later frame that a correctness RPC could have helped to produce.
        self.capture(front, name, ready_shot)
        sample["status"] = "passed"
        return sample

    def busy_sample(self, front, payload):
        self.empty_draft(front)
        stop = self.workspace / "busy-paste-stop"
        trace = self.workspace / "busy-paste-trace"
        script = (
            "from pathlib import Path\nimport sys,time\n"
            "stop=Path('busy-paste-stop')\n"
            f"deadline=time.monotonic()+{self.case_timeout + 15!r}\n"
            "with Path('busy-paste-trace').open('w') as trace:\n"
            "    index=0\n"
            "    while not stop.exists() and time.monotonic()<deadline:\n"
            "        trace.write(str(time.monotonic_ns())+'\\n'); trace.flush()\n"
            "        print('b'*128+'BUSY_'+'PASTE_'+'TICK_%06d '%index,flush=True)\n"
            "        index+=1\n"
            "        time.sleep(0.002)\n"
            "if not stop.exists(): raise SystemExit(3)\n"
        )
        job = self.call("ssmcp.exec.start", {
            "command": ["python3", "-c", script], "cwd": str(self.workspace),
            "title": "Synthetic paste background output",
            "timeout_ms": int((self.case_timeout + 20) * 1000),
        })

        def ticks():
            # Ignore an incomplete trailing append, rather than treating a
            # partial monotonic timestamp as producer evidence.
            raw = trace.read_text()
            complete = raw[:raw.rfind("\n") + 1]
            return [int(line) for line in complete.splitlines() if line.isdigit()]

        try:
            self.quiet_until(front, lambda shot: "BUSY_PASTE_TICK_" in text(shot),
                             "real native command output arrives before the busy sample")
            self.check(self.call("ssmcp.exec.status", {"job_id": job["job_id"]})["running"],
                       "the synthetic native output producer is running")
            before_stamps = ticks()
            self.check(bool(before_stamps), "the native producer emitted output before input")
            sample = self.sample(front, "busy-multiline-2738", payload)
            # All status/continuity observation is after the timed ready frame.
            # A paste faster than the producer's tick interval is a valid result.
            self.check(self.call("ssmcp.exec.status", {"job_id": job["job_id"]})["running"],
                       "the same owned output producer remains running after input")
            self.quiet_until(front, lambda _shot: any(
                stamp > sample["ready_at"] for stamp in ticks()),
                "the owned producer emits output after the measured paste interval")
            observed_until = time.monotonic_ns()
            stamps = ticks()
            during = [stamp for stamp in stamps
                      if sample["pty_first_write_at"] <= stamp <= sample["ready_at"]]
            sample["busy_output_ticks_during_input"] = len(during)
            sample["busy_output_during_input"] = "observed" if during else "not_observed"
            sample["busy_running_before"] = True
            sample["busy_running_after"] = True
            sample["busy_last_output_before_at"] = before_stamps[-1]
            sample["busy_first_output_after_at"] = next(
                stamp for stamp in stamps if stamp > sample["ready_at"])
            sample["busy_observed_until_at"] = observed_until
        finally:
            stop.touch()
            self.finish_job(job, front)

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        front = self.start_front("PTY-paste-performance")
        small = synthetic_payload(2738)
        self.check(len(small) == 2738 and len(small.splitlines()) >= 8,
                   "the user-sized sample is synthetic 2738-byte multiline UTF-8 text")
        self.sample(front, "multiline-2738", small)
        self.sample(front, "multiline-64k", synthetic_payload(64 * 1024))
        self.sample(front, "multiline-4m-exact", synthetic_payload(MAX_PASTE))
        self.sample(front, "multiline-4m-plus-one", synthetic_payload(MAX_PASTE + 1),
                    expected="rejected")
        head, tail = "PLAIN_START_中e\u0301_".encode(), PLAIN_END.encode()
        plain = head + b"Z" * (16 * 1024 - len(head) - len(tail)) + tail
        self.sample(front, "ordinary-16k", plain, bracketed=False, expected="inline")
        self.busy_sample(front, small)
        self.check(not self.external_invocations("PTY-paste-performance"),
                   "no installed coding harness or external model was invoked")
        self.empty_draft(front)
        front.disconnect()
        return {
            "status": "passed",
            "transport": "private production account router, real client PTY, independent VT decoder",
            "synthetic_payloads_only": True, "models_started": False,
            "real_user_credentials_used": False, "existing_services_touched": False,
        }


def compare_baseline(report, baseline):
    previous = {sample["name"]: sample for sample in baseline.get("samples", [])}
    comparisons = []
    for sample in report["samples"]:
        old = previous.get(sample["name"])
        if (sample["status"] != "passed" or not old or old.get("status") != "passed"
                or any(old.get(key) != sample.get(key)
                       for key in ("payload_sha256", "payload_bytes", "bracketed", "expected"))):
            continue
        comparisons.append({
            "name": sample["name"],
            "baseline_ready_ms": old["ready_ms"], "candidate_ready_ms": sample["ready_ms"],
            "ready_speedup": old["ready_ms"] / max(sample["ready_ms"], 1e-9),
            "baseline_pty_write_ms": old["pty_write_ms"],
            "candidate_pty_write_ms": sample["pty_write_ms"],
        })
    return comparisons


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("bin-dir", "decoder", "fixture-binary", "out"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--label", default="candidate")
    parser.add_argument("--case-timeout", type=float, default=60)
    parser.add_argument("--baseline-report", type=Path)
    parser.add_argument("--build-receipt", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be a full frozen source SHA")
    if not 5 <= args.case_timeout <= 180:
        parser.error("--case-timeout must be in 5..180 seconds")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all prebuilt executables must exist; the fixture never builds them")
    for path in (args.baseline_report, args.build_receipt):
        if path is not None and not path.is_file():
            parser.error("comparison/build-receipt inputs must be existing files")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use a new empty output directory to preserve previous evidence")
    root = Path(__file__).resolve().parent.parent
    sources_before = source_binding(root)
    binaries_before = {name: digest(path) for name, path in binaries.items()}
    receipt_before = digest(args.build_receipt) if args.build_receipt else None
    baseline = None
    baseline_hash = None
    if args.baseline_report:
        baseline_bytes = args.baseline_report.read_bytes()
        baseline = json.loads(baseline_bytes)
        if not isinstance(baseline, dict):
            parser.error("--baseline-report must contain a JSON report object")
        baseline_hash = hashlib.sha256(baseline_bytes).hexdigest()
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise TimeoutError("paste performance fixture interrupted")

    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-paste-performance-") as directory:
        harness = PastePerformance(Path(directory), args.bin_dir.resolve(),
                                   binaries["decoder"], args.out, args.case_timeout)
        try:
            report = harness.run()
        except Exception as error:
            report = {"status": "failed", "error": f"{type(error).__name__}: {error}"}
        finally:
            try:
                harness.close_frontends()
                harness.cleanup()
            except Exception as error:
                report = {"status": "failed", "error": report.get("error", "")
                          + f"\ncleanup: {type(error).__name__}: {error}"}
        binaries_after = {name: digest(path) for name, path in binaries.items()}
        sources_after = source_binding(root)
        try:
            receipt_after = digest(args.build_receipt) if args.build_receipt else None
        except OSError:
            receipt_after = None
        unchanged = (binaries_before == binaries_after and sources_before == sources_after
                     and receipt_before == receipt_after)
        if not unchanged:
            report.update(status="failed",
                          error="frozen binary, source or build receipt changed during acceptance")
        report.update(
            label=args.label, source_revision=args.source_revision,
            source_revision_provenance="caller-declared; correlate with the immutable build receipt",
            build_receipt_sha256=receipt_before,
            build_receipt_sha256_before=receipt_before, build_receipt_sha256_after=receipt_after,
            elapsed_seconds=round(time.monotonic() - started, 3),
            binaries_sha256_before=binaries_before, binaries_sha256_after=binaries_after,
            source_before=sources_before, source_after=sources_after, inputs_unchanged=unchanged,
            checks=len(harness.checks), passed_checks=harness.checks,
            samples=harness.samples, frames=harness.frames,
            timing_contract={
                "pty_last_write": "last byte accepted by PTY; not client read completion or disk fsync",
                "first_visible": "first decoded paste-specific input/notice frame",
                "preparing_visible": "first observed Preparing local input frame, or null if superseded",
                "ready": "physical complete reference/inline/rejection frame before correctness RPCs",
                "observation": "VT decode cost included; no extra key, resize or UI RPC during timing",
                "performance_gate": "descriptive comparison only; bounded liveness and correctness asserted",
            },
        )
        if baseline is not None:
            report["baseline_report_sha256"] = baseline_hash
            old_helpers = baseline.get("source_before", {}).get("fixture_source_sha256", {})
            current_helpers = sources_before["fixture_source_sha256"]
            if baseline.get("inputs_unchanged") and old_helpers == current_helpers:
                report["comparison"] = compare_baseline(report, baseline)
            else:
                report["comparison"] = []
                report["comparison_note"] = (
                    "no comparison: fixture helper hash mismatch"
                    if old_helpers != current_helpers
                    else "no comparison: baseline inputs changed during acceptance")
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        for secret in [harness.secret, *harness.sensitive]:
            if secret:
                encoded = encoded.replace(secret, "[redacted]")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
        print(json.dumps({"status": report["status"], "checks": len(harness.checks),
                          "report": str(args.out.resolve() / "report.json")}))
        if report["status"] != "passed":
            raise SystemExit("paste acceptance failed; see the redacted report")


if __name__ == "__main__":
    main()
