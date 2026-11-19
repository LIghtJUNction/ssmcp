#!/usr/bin/env python3
"""Measure paired real-CLI guardian memory and verify the self-exec lifecycle.

Both binaries must use the same build profile. The script never builds Rust,
uses only owned account-router/PKCE fixtures, and does not expose installed
model providers. Reported PSS is sampled from /proc, including the parent and
the complete owned process tree. Command argv and environment are not reported.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

from e2e import wait_for
from e2e_unified import Unified, fixture_json, process_identity, still_present


HOLD = r"""
import json,os,time
from pathlib import Path
Path('memory-holder.json').write_text(json.dumps({'command_pid':os.getpid()}))
while not Path('memory-holder.release').exists(): time.sleep(.02)
"""

PRELOAD = r"""
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

__attribute__((constructor)) static void fault(void) {
    const char *mode = getenv("SSMCP_GUARDIAN_FIXTURE_FAULT");
    if (!mode) return;
    if (!strcmp(mode, "fallback")) {
        struct sock_filter instructions[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execveat, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog program = {4, instructions};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
            || prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program)) _exit(94);
        return;
    }
    char arguments[256];
    int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
    if (fd < 0) _exit(94);
    ssize_t count = read(fd, arguments, sizeof arguments - 1);
    close(fd);
    if (count <= 0) _exit(94);
    arguments[count] = 0;
    size_t first = strnlen(arguments, count) + 1;
    if (first >= (size_t)count || strcmp(arguments + first, "__process_guardian")) return;
    if (!strcmp(mode, "exit")) _exit(86);
    if (!strcmp(mode, "hang")) for (;;) pause();
}
"""


def private_entry(pid):
    """Inspect only the fixed private-mode marker; never return process argv."""
    try:
        arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
        return len(arguments) > 1 and arguments[1] == b"__process_guardian"
    except FileNotFoundError:
        return False


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
    identity = process_identity(pid)
    if not identity:
        raise AssertionError("an owned process disappeared during a stable memory sample")
    values = {}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]:
        key, _, value = line.partition(":")
        if value.strip().endswith("kB"):
            values[key] = int(value.split()[0])
    return {**identity, "rss_kib": values["Rss"], "pss_kib": values["Pss"],
            "private_kib": values["Private_Clean"] + values["Private_Dirty"],
            "pss_anon_kib": values.get("Pss_Anon"),
            "pss_file_kib": values.get("Pss_File"),
            "swap_pss_kib": values.get("SwapPss", 0)}


def sample(parent, guardian):
    rows = [memory(pid) for pid in sorted(owned_pids(parent))]
    by_pid = {row["pid"]: row for row in rows}
    totals = {key: sum(row[key] for row in rows)
              for key in ("pss_kib", "private_kib", "swap_pss_kib")}
    parent_guardian = {key: by_pid[parent][key] + by_pid[guardian][key] for key in totals}
    return {"parent": by_pid[parent], "guardian": by_pid[guardian],
            "parent_and_guardian": parent_guardian, "owned_tree": totals,
            "processes": rows}


class Guardian(Unified):
    def __init__(self, directory, binary_dir, evidence_dir, fixture, extra_env=None):
        super().__init__(directory, binary_dir, evidence_dir)
        self.fixture_binary = fixture
        self.fixture_timeout_seconds = 600
        self.http_timeout = 25
        self.extra_env = extra_env or {}

    def client_environment(self, *args, **kwargs):
        value = super().client_environment(*args, **kwargs)
        value.update(self.extra_env)
        return value

    def warm_output(self, count, size):
        for _ in range(count):
            job = self.call("ssmcp.exec.start", {
                "title": "Produce bounded memory fixture output",
                "command": [self.python, "-c", f"import sys;sys.stdout.write(('x'*1023+'\\n')*{size // 1024})"],
            })
            final = self.finish_job(job)
            self.check(final["exit_code"] == 0, "bounded output fixture completes normally")

    def hold(self):
        job = self.call("ssmcp.exec.start", {
            "title": "Hold an owned memory measurement command",
            "command": [self.python, "-c", HOLD],
        })
        observed = wait_for(lambda: fixture_json(self.workspace / "memory-holder.json"),
                            timeout=5, description="memory command marker")
        identities = self.record_job_processes(job, observed["command_pid"])
        return job, identities

    def measure(self, scenario, expect_reexec):
        self.bootstrap()
        count, size = (12, 32768) if scenario == "small_output" else (0, 0)
        self.warm_output(count, size)
        job, identities = self.hold()
        self.check(private_entry(job["pid"]) == expect_reexec,
                   "the observed real CLI guardian uses the expected bootstrap path")
        self.warm_output(count, size)
        samples = []
        for _ in range(5):
            samples.append(sample(self.client.pid, job["pid"]))
            time.sleep(.1)
        result = {"scenario": scenario,
                  "load": {"completed_commands_before_guardian": count,
                           "completed_commands_after_guardian": count,
                           "stdout_bytes_per_command": size,
                           "live_commands": 1, "model_calls": 0},
                  "guardian_reexec": expect_reexec, "samples": samples}
        result["median"] = {
            group: {field: statistics.median(item[group][field] for item in samples)
                    for field in ("pss_kib", "private_kib", "swap_pss_kib")}
            for group in ("parent", "guardian", "parent_and_guardian", "owned_tree")}
        self.call("ssmcp.exec.cancel", {"job_id": job["job_id"]})
        self.stopped(identities, "memory measurement cleanup")
        return result

    def lifecycle(self):
        self.bootstrap()
        rejected = self.rejected("ssmcp.exec.start", {
            "command": [str(self.workspace / "does-not-exist")],
        })
        self.check(rejected["rejected"], "the actual command exec-error pipe still rejects missing executables")
        timer = self.call("ssmcp.exec.start", {
            "command": [self.python, "-c", "import time;time.sleep(11);raise SystemExit(23)"],
        })
        self.check(private_entry(timer["pid"]), "lifecycle tests exercise the real self-exec guardian")
        self.native_parent_exit()
        self.cancellation()
        signalled = self.call("ssmcp.exec.start", {
            "command": [self.python, "-c", "import os,signal;os.kill(os.getpid(),signal.SIGALRM)"],
        })
        final = self.finish_job(signalled)
        self.check(final["exit_signal"] == signal.SIGALRM and final["exit_code"] is None,
                   "a native signal exit is reproduced by the same guardian PID")

        final = self.finish_job(timer, timeout=15)
        self.check(final["exit_code"] == 23 and final["exit_signal"] is None,
                   "successful bootstrap cancels its alarm before a longer command runs")
        sibling, _, ready = self.titled_client("guardian-sibling", "Isolated sibling", self.workspace)
        sibling_id = ready["client_id"]
        job, identities = self.tree("gateway-disconnect")
        self.account_gateway.disconnect_client(self.client_id)
        self.stopped(identities, "owned WebSocket disconnect")
        replacement = self.reader.read(timeout=20)
        self.validate_connection_record(replacement)
        self.client_id = replacement["client_id"]
        self.check(self.call("terminal.status", client_id=sibling_id)["running"]
                   and sibling.poll() is None,
                   "one disconnected client leaves its sibling and account grant usable")
        job, identities = self.tree("client-parent-death")
        self.client.kill()
        self.client.wait(timeout=5)
        self.stopped(identities, "abrupt client parent death")
        self.check(self.call("terminal.status", client_id=sibling_id)["running"],
                   "parent-death cleanup remains confined to its owned command tree")

    def pinned_executable(self, baseline):
        self.bootstrap()
        executable = self.binary_dir / "ssmcp"
        pinned = executable.stat()
        replacement = executable.with_name("replacement")
        shutil.copy2(baseline / "ssmcp", replacement)
        os.replace(replacement, executable)
        pinned_job, pinned_ids = self.tree("pinned-inode")
        actual = Path(f"/proc/{pinned_job['pid']}/exe").stat()
        self.check(private_entry(pinned_job["pid"])
                   and (actual.st_dev, actual.st_ino) == (pinned.st_dev, pinned.st_ino),
                   "an atomic executable replacement cannot replace the running guardian implementation")
        self.call("ssmcp.exec.cancel", {"job_id": pinned_job["job_id"]})
        self.stopped(pinned_ids, "pinned executable cleanup")
        # Subsequent new clients need the candidate again; the running client
        # retains the original inode until it exits.
        shutil.copy2(self.candidate_source, replacement)
        os.replace(replacement, executable)

    def bootstrap_fault(self, mode):
        self.bootstrap()
        marker = self.workspace / "unexpected-command-started"
        arguments = {"command": [self.python, "-c",
                                  "from pathlib import Path;Path('unexpected-command-started').write_text('ran')"]}
        started = time.monotonic()
        result = self.code("""
            try { const result=await call(%s, 'ssmcp.exec.start', %s);
              return {rejected:result.isError===true, value:result.structuredContent}; }
            catch(error) { return {rejected:true}; }
        """ % (json.dumps(self.client_id), json.dumps(arguments)))
        elapsed = time.monotonic() - started
        value = result.get("value") or {}
        final = self.finish_job(value) if value.get("job_id") else None
        if mode == "fallback":
            self.check(marker.is_file() and final and final["exit_code"] == 0,
                       "execveat syscall rejection releases the command through the original guardian")
        else:
            self.check(not marker.exists(), "a failed guardian bootstrap never starts the actual command")
            self.check(result.get("rejected") or (final and not final["running"]
                       and (final["exit_code"] != 0 or final["exit_signal"] is not None)),
                       "bootstrap failure reaches an explicit failed native operation or process outcome")
            self.check(elapsed < 18 and (mode != "hang" or elapsed >= 8),
                       "the native start request returns within the bounded loader startup interval")
        self.check(self.call("terminal.status")["running"],
                   "the fixture client remains usable after the bootstrap result")
        return {"fault": mode, "start_request_seconds": round(elapsed, 3),
                "actual_command_ran": marker.exists(), "native_rejected": result.get("rejected"),
                "exit_code": final.get("exit_code") if final else None,
                "exit_signal": final.get("exit_signal") if final else None}


def copied_binary(source, directory):
    directory.mkdir(mode=0o700)
    target = directory / "ssmcp"
    shutil.copy2(source / "ssmcp", target)
    return directory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-bin-dir", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--profile", choices=("debug", "release"), required=True)
    parser.add_argument("--baseline-revision", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=480)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not sys.platform.startswith("linux") or not 1 <= args.repeats <= 5:
        parser.error("this bounded /proc/ELF acceptance fixture requires Linux and 1..5 repeats")
    args.report = args.report.resolve()
    args.report.parent.mkdir(parents=True, exist_ok=True)
    evidence = args.report.with_name(args.report.stem + "-evidence")
    evidence.mkdir(parents=True, exist_ok=True, mode=0o700)
    report = {"schema":"ssmcp.guardian-memory.v1", "status":"running",
              "build_profile":args.profile, "baseline_revision":args.baseline_revision,
              "candidate_revision":args.revision,
              "baseline_sha256":hashlib.sha256((args.baseline_bin_dir / "ssmcp").read_bytes()).hexdigest(),
              "candidate_sha256":hashlib.sha256((args.bin_dir / "ssmcp").read_bytes()).hexdigest(),
              "model_calls":0, "measurements":[], "faults":[], "checks":[]}
    start = time.monotonic()
    failure = None
    def expired(_signum, _frame):
        raise TimeoutError("guardian acceptance deadline expired")
    previous = signal.signal(signal.SIGALRM, expired)
    signal.alarm(args.timeout)
    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp-guardian-memory-") as temporary:
            base = Path(temporary)
            baseline = copied_binary(args.baseline_bin_dir.resolve(), base / "baseline-bin")
            candidate = copied_binary(args.bin_dir.resolve(), base / "candidate-bin")
            def run(label, binary, action, extra_env=None):
                directory = base / label
                directory.mkdir(mode=0o700)
                h = Guardian(directory, binary, evidence / label, args.fixture_binary.resolve(), extra_env)
                h.candidate_source = args.bin_dir.resolve() / "ssmcp"
                try:
                    return action(h)
                except Exception as error:
                    raise AssertionError(h.scrub(f"{label}: {type(error).__name__}: {error}")) from None
                finally:
                    report["checks"].extend({"case":label,"description":item} for item in h.checks)
                    cleanup_errors = []
                    for action in (h.cleanup, h.remove_test_artifacts):
                        try:
                            action()
                        except Exception as error:
                            cleanup_errors.append(h.scrub(str(error)))
                    remaining = [identity for identity in h.tree_identities if still_present(identity)]
                    if remaining:
                        cleanup_errors.append("observed fixture processes remained after client shutdown")
                        for identity in remaining:
                            try:
                                fd = os.pidfd_open(identity["pid"])
                                try:
                                    if still_present(identity):
                                        signal.pidfd_send_signal(fd, signal.SIGKILL)
                                finally:
                                    os.close(fd)
                            except ProcessLookupError:
                                pass
                    report.setdefault("cleanup", []).append({"case":label,"confirmed":not cleanup_errors,
                                                             "remaining":remaining,"errors":cleanup_errors})
                    if cleanup_errors:
                        raise AssertionError("; ".join(cleanup_errors))
            for scenario in ("default", "small_output"):
                for repeat in range(args.repeats):
                    variants = [("baseline",baseline,False),("candidate",candidate,True)]
                    for label, binary, expected in variants[::1 if repeat % 2 == 0 else -1]:
                        value = run(f"{scenario}-{repeat}-{label}", binary,
                                    lambda h: h.measure(scenario, expected))
                        report["measurements"].append({"variant":label,"repeat":repeat,**value})
            run("lifecycle", candidate, lambda h: h.lifecycle())
            run("pinned-executable", candidate, lambda h: h.pinned_executable(baseline))
            source, library = base / "fault.c", base / "fault.so"
            source.write_text(PRELOAD)
            compiler = shutil.which("cc")
            if not compiler:
                raise AssertionError("a native C compiler is required for the isolated loader failure fixture")
            subprocess.run([compiler,"-shared","-fPIC","-O2","-o",str(library),str(source)],
                           check=True, capture_output=True, timeout=30)
            for mode in ("fallback", "exit", "hang"):
                value = run(f"bootstrap-{mode}", candidate, lambda h: h.bootstrap_fault(mode),
                            {"LD_PRELOAD":str(library),"SSMCP_GUARDIAN_FIXTURE_FAULT":mode})
                report["faults"].append(value)
            report["summary"] = {}
            for scenario in ("default", "small_output"):
                medians = {}
                for variant in ("baseline", "candidate"):
                    selected = [item for item in report["measurements"]
                                if item["scenario"] == scenario and item["variant"] == variant]
                    medians[variant] = {group:{field:statistics.median(item["median"][group][field] for item in selected)
                                              for field in ("pss_kib","private_kib","swap_pss_kib")}
                                        for group in ("parent","guardian","parent_and_guardian","owned_tree")}
                report["summary"][scenario] = medians
            report["status"] = "passed"
    except Exception as error:
        failure = f"{type(error).__name__}: {error}"
        report["status"] = "failed"
        report["failure"] = failure
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, previous)
        report["duration_seconds"] = round(time.monotonic() - start, 3)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        args.report.chmod(0o600)
    print(json.dumps({"status":report["status"],"checks":len(report["checks"]),
                      "report":str(args.report),"failure":failure}))
    return 0 if failure is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
