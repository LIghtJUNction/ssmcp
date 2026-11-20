#!/usr/bin/env python3
"""Run the eighteen release suites using frozen inputs; never build or install."""
from __future__ import annotations

import argparse
import ast
import hashlib
import configparser
import io
import tarfile
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import time
import tomllib

SUITES = (
    "message_references", "force_send", "listener_transports", "zuse_worker",
    "interactions", "private_desktop", "account_compat", "input_history",
    "reload_state", "session_groups",
    "client_ui", "canvas_logs", "composer", "questions", "presentations",
    "context_memory", "file_paste", "relay_composer",
)
GIT_VIEW_SUITES = {"account_compat", "presentations", "context_memory"}
REPORT_ONLY_SUITES = {"account_compat", "composer", "relay_composer"}
OLD_COMMIT = "4e3132d60600dca5053af81cdefa6962c3058ac4"
OLD_BLOB_ID = "8d15b56207461ee817635c6b134380e1c2ffdc4d"
OLD_BLOB_SHA256 = "6e169387073dfb5919f6714658ac28781cfe04c7e5800ada690c0c67fed3d4d0"
OLD_SERVER_SHA256 = "e8f59d66493e68ba3c196a72ae3733b8b7336fbfb1da0cf92960069637f1f7bf"
DEFAULT_OLD_SERVER = "/var/tmp/ssmcp-relay-stage-20261120/old-0.10.13/ssmcpd"
DEFAULT_GIT_DIR = "/home/lightjunction/Documents/GITHUB/ssmcp/.git"
SAFE_PATH = "/usr/local/bin:/usr/bin:/bin"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def canonical_digest(value):
    return hashlib.sha256(json.dumps(
        value, sort_keys=True, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")).hexdigest()


def write_json(path, value):
    # Only the runner writes these receipt names. Suite evidence is left intact.
    path = Path(path)
    temporary = path.with_name(path.name + ".writing")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def clean_environment():
    # No inherited auth, live desktop/session, reload marker, or Python overrides.
    return {
        "PATH": SAFE_PATH, "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TZ": "UTC",
        "TERM": "xterm-256color", "PYTHONDONTWRITEBYTECODE": "1",
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_REPLACE_OBJECTS": "1", "GIT_NO_LAZY_FETCH": "1",
        "HOME": "/tmp/final17-home", "USER": "ssmcp-e2e", "LOGNAME": "ssmcp-e2e",
        "TMPDIR": "/tmp", "XDG_RUNTIME_DIR": "/tmp/final17-runtime",
        "XDG_CONFIG_HOME": "/tmp/final17-config", "XDG_DATA_HOME": "/tmp/final17-data",
        "XDG_CACHE_HOME": "/tmp/final17-cache",
    }


def git_read(args, *command):
    return subprocess.check_output(
        ["/usr/bin/git", "--git-dir=" + str(args.git_dir), *command],
        cwd=args.source_path, env=clean_environment(), timeout=30,
    )


def legacy_blob(args):
    blob = git_read(args, "show", OLD_COMMIT + ":src/code_mode.js")
    require(hashlib.sha256(blob).hexdigest() == OLD_BLOB_SHA256,
            "legacy code_mode.js differs from the pinned read-only fixture object")
    return {"commit": OLD_COMMIT, "blob_id": OLD_BLOB_ID,
            "sha256": hashlib.sha256(blob).hexdigest(), "bytes": len(blob)}


def tree_manifest(root):
    rows = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            require(path.resolve(strict=True).is_relative_to(root),
                    "frozen source symlink leaves SOURCE_PATH: " + relative)
            target = os.readlink(path)
            rows[relative] = {"type": "symlink", "target": target,
                              "sha256": hashlib.sha256(os.fsencode(target)).hexdigest()}
        elif path.is_file():
            info = path.stat()
            rows[relative] = {"type": "file", "bytes": info.st_size,
                              "mode": stat.S_IMODE(info.st_mode), "sha256": digest(path)}
        elif not path.is_dir():
            raise RuntimeError("unsupported frozen source entry: " + relative)
    return rows


def input_snapshot(args):
    files = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd",
             "ssmcp-frontend": args.bin_dir / "ssmcp-frontend",
             "decoder": args.decoder, "decoder_alias": args.bin_dir / "examples/vt_snapshot",
             "fixture_binary": args.fixture_binary, "old_server": args.old_server,
             "candidate_gates": args.gates, "acceptance_runner": Path(__file__).resolve()}
    files.update(getattr(args, "gated_origins", {}))
    binaries = {name: {"path": str(path), "bytes": path.stat().st_size,
                      "mode": stat.S_IMODE(path.stat().st_mode), "sha256": digest(path)}
                for name, path in files.items()}
    sources = tree_manifest(args.source_path)
    record = {"source_revision": args.source_revision, "source_path": str(args.source_path),
              "source_files": sources, "source_tree_sha256": canonical_digest(sources),
              "files": binaries, "legacy_code_mode": legacy_blob(args)}
    record["input_set_sha256"] = canonical_digest(record)
    return record


def input_check(reference, current):
    changed = []
    for section in ("source_files", "files"):
        before, after = reference[section], current[section]
        changed.extend(section + "/" + key for key in sorted(before.keys() | after.keys())
                       if before.get(key) != after.get(key))
    if reference["legacy_code_mode"] != current["legacy_code_mode"]:
        changed.append("legacy_code_mode")
    return {"unchanged": not changed,
            "input_set_sha256": current["input_set_sha256"], "changed": changed}


def cli_flags(path):
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    return {
        argument.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
        and node.func.attr == "add_argument"
        for argument in node.args
        if isinstance(argument, ast.Constant) and isinstance(argument.value, str)
        and argument.value.startswith("--")
    }


def suite_arguments(args, name, evidence, engine_version):
    common = ["--bin-dir", str(args.bin_dir), "--decoder", str(args.decoder),
              "--out", str(evidence)]
    report = evidence / "report.json"
    if name in ("message_references", "interactions", "session_groups"):
        return common + ["--fixture-binary", str(args.fixture_binary),
                         "--source-revision", args.source_revision], report
    if name in ("force_send", "listener_transports", "input_history", "reload_state", "file_paste"):
        return common, report
    if name == "client_ui":
        return ["--bin-dir", str(args.bin_dir), "--out", str(evidence)], evidence / "ui-smoke.json"
    if name == "canvas_logs":
        return ["--bin-dir", str(args.bin_dir), "--out", str(evidence),
                "--report", str(report)], report
    if name in ("questions", "presentations", "context_memory"):
        return common + ["--report", str(report)], report
    if name in ("composer", "relay_composer"):
        arguments = ["--bin-dir", str(args.bin_dir), "--decoder", str(args.decoder),
                     "--report", str(report)]
        if name == "relay_composer":
            arguments += ["--fixture-binary", str(args.fixture_binary)]
        return arguments, report
    if name == "zuse_worker":
        return ["--binary", str(args.bin_dir / "ssmcp"),
                "--expected-engine-version", engine_version,
                "--evidence-dir", str(evidence / "native"),
                "--report", str(report)], report
    if name == "private_desktop":
        return ["--bin-dir", str(args.bin_dir),
                "--evidence-dir", str(evidence / "screenshots"),
                "--report", str(report)], report
    if name == "account_compat":
        return ["--client", str(args.bin_dir / "ssmcp"),
                "--old-server", str(args.old_server),
                "--new-server", str(args.bin_dir / "ssmcpd"),
                "--report", str(report), "--work-dir", "/tmp",
                "--check-default", "--server-version", "both"], report
    raise RuntimeError("unknown acceptance suite: " + name)


def static_source_checks(args):
    require(not (args.source_path / ".git").exists(), "SOURCE_PATH must be a gitless frozen tree")
    require(not any(path.name == ".git" for path in args.source_path.rglob(".git")),
            "SOURCE_PATH contains Git metadata")
    package = tomllib.loads((args.source_path / "Cargo.toml").read_text())
    version = package["package"]["version"]
    require(isinstance(version, str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version),
            "SOURCE_PATH must declare a stable semantic package version")
    expected = getattr(args, "expected_version", None)
    require(expected is None or version == expected,
            f"source package.version {version} differs from expected {expected}")
    args.source_version = version
    lock = tomllib.loads((args.source_path / "Cargo.lock").read_text())
    engines = {item["version"] for item in lock["package"] if item["name"] == "zuse-computer"}
    require(len(engines) == 1, "Cargo.lock must identify one zuse-computer version")
    engine = next(iter(engines))
    checks = {}
    for name in SUITES:
        script = args.source_path / "scripts" / ("e2e_" + name + ".py")
        flags = cli_flags(script)
        argv, _ = suite_arguments(args, name, Path("/unused-evidence"), engine)
        requested = {value for value in argv if value.startswith("--")}
        require(requested <= flags, f"{name} CLI changed: missing {sorted(requested - flags)}")
        checks[name] = {"script": str(script), "sha256": digest(script),
                        "arguments_verified": sorted(requested)}
    old_tree = ast.parse((args.source_path / "scripts/e2e_account_compat.py").read_text())
    old_commits = [
        node.value.value for node in old_tree.body
        if isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant)
        and any(isinstance(target, ast.Name) and target.id == "OLD_COMMIT" for target in node.targets)
    ]
    require(old_commits == [OLD_COMMIT], "account_compat OLD_COMMIT changed; review the legacy fixture")
    return engine, checks


def sandbox_command(args, name, suite_dir, temporary, script, argv):
    command = [
        "/usr/bin/bwrap", "--die-with-parent",
        "--unshare-user", "--uid", str(os.getuid()), "--gid", str(os.getgid()),
        "--unshare-pid", "--unshare-ipc", "--unshare-net", "--unshare-uts",
        "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
        "--tmpfs", "/run", "--bind", str(temporary), "/tmp",
    ]
    # Rebind exact frozen inputs after /tmp and /run, including inputs located there.
    for path in (args.source_path, args.bin_dir, args.decoder, args.fixture_binary,
                 args.old_server, args.git_dir):
        command += ["--ro-bind", str(path), str(path)]
    descriptors = []
    if name in GIT_VIEW_SUITES:
        # These unchanged suites need Git metadata in SOURCE_PATH. Their own
        # fixture repositories must still work, so do not set global GIT_DIR.
        # The host source remains gitless; only this namespace gets a read-only
        # metadata view with detached HEAD and the original read-only objects.
        command += ["--tmpfs", str(args.source_path)]
        for entry in sorted(args.source_path.iterdir()):
            command += ["--ro-bind", str(entry), str(entry)]
        metadata = args.source_path / ".git"
        command += ["--dir", str(metadata), "--dir", str(metadata / "refs"),
                    "--symlink", str(args.git_dir / "objects"), str(metadata / "objects")]
        for filename, contents in (
            ("HEAD", args.source_revision + "\n"),
            ("config", "[core]\nrepositoryformatversion = 0\nbare = false\n"),
        ):
            descriptor = os.memfd_create("final17-git-" + filename, os.MFD_CLOEXEC)
            os.write(descriptor, contents.encode("utf-8"))
            os.lseek(descriptor, 0, os.SEEK_SET)
            descriptors.append(descriptor)
            command += ["--file", str(descriptor), str(metadata / filename)]
        command += ["--remount-ro", str(args.source_path)]
    command += ["--bind", str(suite_dir), str(suite_dir),
                "--chdir", str(args.source_path)]
    return command + [sys.executable, "-B", str(script), *argv], descriptors

def evidence_inventory(evidence):
    artifacts, snapshots = [], []
    for path in sorted(evidence.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        require(path.resolve().is_relative_to(evidence.resolve()), "evidence escaped its owned directory")
        kind = "artifact"
        if path.suffix == ".json":
            try:
                data, _ = read_json_file(path, evidence)
                if vt_snapshot(data):
                    kind = "actual_terminal_vt_snapshot"
            except (ValueError, UnicodeError, OSError, RuntimeError):
                pass
        elif path.suffix.lower() in (".png", ".jpg", ".jpeg"):
            try:
                from PIL import Image
                with Image.open(path) as picture:
                    require(picture.width > 0 and picture.height > 0, "empty desktop capture")
                    picture.verify()
                kind = "actual_owned_desktop_capture"
            except (ValueError, OSError, RuntimeError):
                pass
        row = {"path": str(path), "relative_path": path.relative_to(evidence).as_posix(),
               "bytes": path.stat().st_size, "sha256": digest(path), "kind": kind}
        artifacts.append(row)
        if kind.startswith("actual_"):
            snapshots.append(row)
    return artifacts, snapshots


def stop_owned_process(process):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=10)


def execute_suite(args, name, engine, baseline):
    suite_dir = args.out / "suites" / name
    suite_dir.mkdir(parents=True, mode=0o700)
    temporary = args.out / "tmp" / name
    temporary.mkdir(parents=True, mode=0o700)
    for directory in ("final17-home", "final17-runtime", "final17-config", "final17-data", "final17-cache"):
        (temporary / directory).mkdir(mode=0o700)
    evidence = suite_dir / "evidence"
    evidence.mkdir(mode=0o700)
    script = args.source_path / "scripts" / ("e2e_" + name + ".py")
    argv, report = suite_arguments(args, name, evidence, engine)
    command, descriptors = sandbox_command(args, name, suite_dir, temporary, script, argv)
    log = suite_dir / "run.log"
    row = {"name": name, "status": "failed", "command": command, "script": str(script),
           "script_sha256": digest(script), "log": str(log), "report": str(report),
           "private_tmp": str(temporary), "ui_evidence_required": name not in REPORT_ONLY_SUITES,
           "ui_evidence_contract": ("upstream script exports report/log only" if name in REPORT_ONLY_SUITES
                                   else "actual PTY VT frames or owned desktop captures"),
           "readonly_source_git_view": name in GIT_VIEW_SUITES}
    started = time.monotonic()
    process = None
    try:
        row["inputs_before"] = input_check(baseline, input_snapshot(args))
        require(row["inputs_before"]["unchanged"], "a frozen input changed before the suite")
        env = clean_environment()
        env["SSMCP_E2E_FIXTURE_BINARY"] = str(args.fixture_binary)
        print(json.dumps({"started": name, "log": str(log)}, ensure_ascii=False), flush=True)
        with log.open("w") as output:
            process = subprocess.Popen(command, cwd=args.source_path, env=env,
                stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                start_new_session=True, pass_fds=tuple(descriptors))
            try:
                row["exit_code"] = process.wait(timeout=args.suite_timeout)
            except subprocess.TimeoutExpired:
                row["timed_out"] = True
            except KeyboardInterrupt:
                row["interrupted"] = True
            finally:
                stop_owned_process(process)
                row["exit_code"] = process.returncode
        require(row["exit_code"] == 0 and not row.get("timed_out") and not row.get("interrupted"),
                "suite process did not complete successfully")
        data, parsed_report = read_json_file(report, evidence)
        row["report_sha256"] = parsed_report["sha256"]
        row["suite_report_status"] = "passed" if name in ("account_compat", "client_ui") else data.get("status", "missing")
        row["checks"] = verify_report(data, name, args, engine, baseline)
        row["cleanup"] = data.get("cleanup")
        row["suite_error"] = data.get("error") or data.get("cleanup_error")
        row["report_source_revision"] = data.get("source_revision", data.get("source_commit"))
        row["report_inputs_unchanged"] = data.get("inputs_unchanged")
        row["engine_version"] = data.get("engine_version")
        if name == "zuse_worker":
            row["desktop_text_report"] = verify_desktop_text(data, evidence, baseline)
        row["status"] = "passed"
    except KeyboardInterrupt:
        row["interrupted"] = True
        row["error"] = "controller interrupted"
    except Exception as error:
        row["error"] = f"{type(error).__name__}: {error}"
    finally:
        try:
            if process is not None:
                stop_owned_process(process)
                row["exit_code"] = process.returncode
            row["owned_child_reaped"] = process is None or process.poll() is not None
        except Exception as error:
            row["status"] = "failed"
            row["owned_cleanup_error"] = f"{type(error).__name__}: {error}"
        for descriptor in descriptors:
            os.close(descriptor)
        row["seconds"] = round(time.monotonic() - started, 3)
        try:
            row["inputs_after"] = input_check(baseline, input_snapshot(args))
            require(row["inputs_after"]["unchanged"], "a frozen input changed during the suite")
            row["artifacts"], row["ui_snapshots"] = evidence_inventory(evidence)
            if "report_sha256" in row:
                included = next((item for item in row["artifacts"] if item["path"] == str(report)), None)
                require(included is not None and included["sha256"] == row["report_sha256"],
                        "parsed suite report differs from its recorded artifact")
            if row["ui_evidence_required"]:
                require(row["ui_snapshots"], "no actual valid UI snapshot was produced")
                expected_kind = ("actual_owned_desktop_capture" if name in ("zuse_worker", "private_desktop")
                                 else "actual_terminal_vt_snapshot")
                require(any(item["kind"] == expected_kind for item in row["ui_snapshots"]),
                        "suite lacks its required real UI capture type")
        except Exception as error:
            row["status"] = "failed"
            row.setdefault("error", f"{type(error).__name__}: {error}")
        if log.is_file():
            row["log_sha256"] = digest(log)
        write_json(suite_dir / "suite.json", row)
    return row


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag, variable, value_type in (
        ("source-revision", "SOURCE_REVISION", str), ("source-path", "SOURCE_PATH", Path),
        ("bin-dir", "BIN_DIR", Path), ("decoder", "DECODER", Path),
        ("fixture-binary", "FIXTURE_BINARY", Path), ("gates", "GATES_RECEIPT", Path),
    ):
        default = os.environ.get(variable)
        parser.add_argument("--" + flag, type=value_type, default=default,
                            required=default is None, help=f"frozen input; also accepts {variable}")
    parser.add_argument("--out", type=Path, default=Path(__file__).resolve().parent / "results",
                        help="new evidence directory; never reuse an earlier run")
    parser.add_argument("--old-server", type=Path, default=Path(DEFAULT_OLD_SERVER))
    parser.add_argument("--git-dir", type=Path, default=Path(DEFAULT_GIT_DIR),
                        help="original repository object store; read only")
    parser.add_argument("--expected-version", default=os.environ.get("EXPECTED_VERSION"))
    parser.add_argument("--suite-timeout", type=int, default=900)
    parser.add_argument("--submodule-git-dir", action="append", default=[], metavar="PATH=GIT_DIR",
                        help="explicit read-only object store for a materialized pinned gitlink; may repeat")
    args = parser.parse_args()
    mappings = {}
    for value in args.submodule_git_dir:
        path, separator, directory = value.partition("=")
        require(separator and path and directory and not Path(path).is_absolute()
                and ".." not in Path(path).parts and Path(path).as_posix() == path,
                "--submodule-git-dir needs a safe relative PATH=GIT_DIR")
        require(path not in mappings, "duplicate submodule object mapping")
        mappings[path] = Path(directory).resolve()
    args.submodule_git_dirs = mappings
    require(re.fullmatch(r"[0-9a-f]{40}", args.source_revision), "SOURCE_REVISION must be a complete SHA")
    require(args.suite_timeout > 0, "--suite-timeout must be positive")
    for name in ("source_path", "bin_dir", "decoder", "fixture_binary", "old_server", "git_dir"):
        setattr(args, name, getattr(args, name).resolve())
    args.gates = args.gates.absolute()
    args.out = args.out.resolve()
    require(not args.out.exists(), "use a new --out directory to preserve existing evidence")
    for path in (args.source_path, args.bin_dir, args.git_dir, args.gates.resolve(),
                 args.decoder, args.fixture_binary, args.old_server, Path(__file__).resolve(),
                 *args.submodule_git_dirs.values()):
        require(not args.out.is_relative_to(path) and not path.is_relative_to(args.out),
                "output must not overlap a frozen input or controller")
    return args


def main():
    os.umask(0o077)
    args = parse_args()
    args.out.mkdir(mode=0o700, parents=True)
    receipt = args.out / "acceptance.json"
    rows = [{"name": name, "status": "not_run"} for name in SUITES]
    record = {"schema": "ssmcp.final17.acceptance.v1", "status": "running",
              "source_revision": args.source_revision, "source_path": str(args.source_path),
              "expected_version": args.expected_version, "bin_dir": str(args.bin_dir),
              "decoder": str(args.decoder), "fixture_binary": str(args.fixture_binary),
              "python": sys.version, "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "candidate_gates": {"path": str(args.gates)},
              "build_scope": {"cargo_allowed": False, "candidate_compilation_allowed": False,
                              "desktop_fixture": "Existing GTK receiver may invoke cc inside its private sandbox."},
              "isolation": {"root_mount": "read-only", "per_suite_tmp": True,
                            "network": "independent namespace; loopback only",
                            "pid_ipc_uts": "independent namespaces", "live_desktop_env": "absent",
                            "remote_models": False, "source_git_overlay_suites": sorted(GIT_VIEW_SUITES)},
              "separate_required_p1": {"status": "not_run_by_this_controller",
                                      "requirement": "A separate passing receipt bound to this exact SSMCP source, binaries, harnesses and final zcalld cohort is required."},
              "checks": rows}
    baseline = None
    interrupted = False
    started = time.monotonic()
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    signal.signal(signal.SIGINT, interrupt)
    try:
        write_json(receipt, record)
        validate_inputs(args)
        source_files = tree_manifest(args.source_path)
        record["gate_binding"] = validate_gates(args, source_files)
        record["candidate_gates"] = record["gate_binding"]["receipt"]
        engine, cli_checks = static_source_checks(args)
        object_id = git_read(args, "rev-parse", OLD_COMMIT + ":src/code_mode.js").decode().strip()
        require(object_id == OLD_BLOB_ID, "original Git object differs from the pinned legacy fixture")
        baseline = input_snapshot(args)
        require(baseline["source_files"] == source_files, "source changed during gate verification")
        verify_gated_baseline(record["gate_binding"], baseline)
        write_json(args.out / "inputs-before.json", baseline)
        record.update(version=args.source_version, expected_engine_version=engine,
                      cli_checks=cli_checks, input_set_sha256_before=baseline["input_set_sha256"],
                      legacy_fixture={**baseline["legacy_code_mode"], "git_dir": str(args.git_dir),
                                      "old_server": baseline["files"]["old_server"]})
        write_json(receipt, record)
        for row in rows:
            name = row["name"]
            row["status"] = "running"
            write_json(receipt, record)
            try:
                result = execute_suite(args, name, engine, baseline)
                row.clear()
                row.update(result)
            except Exception as error:
                row.update(status="failed", error=f"{type(error).__name__}: {error}")
            write_json(receipt, record)
            print(json.dumps({"finished": name, "status": row["status"], "seconds": row.get("seconds"),
                              "checks": row.get("checks", 0)}, ensure_ascii=False), flush=True)
            if row["status"] != "passed":
                interrupted = bool(row.get("interrupted"))
                record["error"] = "acceptance stopped at " + name
                break
    except KeyboardInterrupt:
        interrupted = True
        record["error"] = "runner interrupted"
    except Exception as error:
        record["error"] = f"{type(error).__name__}: {error}"
    finally:
        if baseline is not None:
            try:
                final = input_snapshot(args)
                write_json(args.out / "inputs-after.json", final)
                record["inputs_unchanged"] = input_check(baseline, final)
            except Exception as error:
                record["inputs_unchanged"] = {"unchanged": False,
                                             "error": f"{type(error).__name__}: {error}"}
        for row in rows:
            if row["status"] == "running":
                row.update(status="failed", error="suite did not finish")
            elif row["status"] == "not_run":
                row["reason"] = "acceptance stopped before this suite"
        record["seconds"] = round(time.monotonic() - started, 3)
        record["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        record["status"] = ("passed" if not interrupted and not record.get("error")
                            and record.get("inputs_unchanged", {}).get("unchanged") is True
                            and all(row["status"] == "passed" for row in rows) else "failed")
        write_json(receipt, record)
    print(json.dumps({"status": record["status"], "receipt": str(receipt)}), flush=True)
    return 0 if record["status"] == "passed" else 1


def read_json_file(path, root=None):
    path = Path(path)
    if root is not None:
        root = Path(root).resolve(strict=True)
        require(path.absolute().is_relative_to(root), "report is outside its owned evidence directory")
        current = path.absolute()
        while current != root:
            require(not current.is_symlink(), "report path contains a symlink")
            current = current.parent
        require(not root.is_symlink(), "evidence root became a symlink")
    require(not path.is_symlink(), "JSON receipt must not be a symlink")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "JSON receipt must be a regular file")
        raw = stream.read(16 * 1024 * 1024 + 1)
    require(len(raw) <= 16 * 1024 * 1024, "JSON receipt exceeds the evidence limit")
    data = json.loads(raw)
    require(isinstance(data, dict), "JSON receipt must contain an object")
    return data, {"path": str(path), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}

def verify_source_revision(args, actual):
    """Compare the complete materialized source with immutable Git archives."""
    expected, archives = {}, []
    def inspect(git_dir, revision, prefix="", depth=0):
        require(depth <= 8, "submodule archive depth exceeds the source contract")
        def read(*command):
            return subprocess.check_output(
                ["/usr/bin/git", "--git-dir=" + str(git_dir), *command],
                cwd=args.source_path, env=clean_environment(), timeout=60)
        raw = read("archive", "--format=tar", revision)
        archives.append({"path": prefix or ".", "revision": revision,
                         "git_dir": str(Path(git_dir).resolve()),
                         "sha256": hashlib.sha256(raw).hexdigest()})
        modules_text = None
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
            for item in archive:
                relative = Path(item.name)
                require(not relative.is_absolute() and ".." not in relative.parts,
                        "unsafe path in the immutable Git archive")
                if item.isdir():
                    continue
                name = prefix + relative.as_posix()
                require(name not in expected, "duplicate source archive entry")
                if item.issym():
                    body = os.fsencode(item.linkname)
                    expected[name] = {"type": "symlink", "target": item.linkname,
                                      "sha256": hashlib.sha256(body).hexdigest()}
                else:
                    require(item.isfile(), "unsupported Git archive entry: " + name)
                    body = archive.extractfile(item).read()
                    expected[name] = {"type": "file", "bytes": len(body),
                                      "sha256": hashlib.sha256(body).hexdigest(),
                                      "executable": bool(item.mode & 0o111)}
                    if item.name == ".gitmodules":
                        modules_text = body.decode("utf-8")
        links = []
        for entry in read("ls-tree", "-rz", "--full-tree", revision).split(b"\0"):
            if not entry:
                continue
            metadata, path = entry.split(b"\t", 1)
            mode, kind, commit = metadata.split()
            if mode == b"160000":
                links.append((os.fsdecode(path), commit.decode("ascii")))
        modules = {}
        if modules_text is not None:
            config = configparser.ConfigParser(interpolation=None)
            config.read_string(modules_text)
            for section in config.sections():
                match = re.fullmatch(r'submodule "(.+)"', section)
                if match and config.has_option(section, "path"):
                    modules[config[section]["path"]] = match.group(1)
        for path, commit in links:
            nested = prefix + path + "/"
            # A plain git archive leaves gitlinks empty. Materialized gitlinks
            # must also match their exact pinned commit, never a moving checkout.
            if not any(name.startswith(nested) for name in actual):
                continue
            name = modules.get(path)
            require(isinstance(name, str) and not Path(name).is_absolute()
                    and ".." not in Path(name).parts,
                    "materialized submodule lacks a safe pinned Git mapping: " + nested)
            module_git = getattr(args, "submodule_git_dirs", {}).get(
                nested.rstrip("/"), Path(git_dir) / "modules" / name)
            require(module_git.is_dir(), "cannot verify materialized submodule objects: " + nested)
            inspect(module_git, commit, nested, depth + 1)
    inspect(args.git_dir, args.source_revision)
    require(set(actual) == set(expected),
            "frozen source paths differ from the exact Git source: missing="
            + repr(sorted(set(expected) - set(actual))[:8])
            + ", extra=" + repr(sorted(set(actual) - set(expected))[:8]))
    for name, item in expected.items():
        observed = actual[name]
        for key in ("type", "sha256", "target", "bytes"):
            if key in item:
                require(observed.get(key) == item[key], "frozen source differs from Git: " + name)
        if item["type"] == "file":
            require(bool(observed["mode"] & 0o111) == item["executable"],
                    "frozen source executable mode differs from Git: " + name)
    return {"source_revision": args.source_revision, "archives": archives,
            "source_files": len(actual), "source_tree_sha256": canonical_digest(actual)}

def validate_gates(args, source_files):
    gates, receipt = read_json_file(args.gates)
    require(gates.get("status") == "passed", "candidate gates must have status passed")
    require(gates.get("source_revision") == args.source_revision,
            "candidate gates refer to a different source SHA")
    checks = gates.get("checks")
    require(isinstance(checks, list) and checks and all(
        isinstance(check, dict) and type(check.get("exit_code")) is int
        and check["exit_code"] == 0 for check in checks),
        "candidate gates must contain nonempty successful checks")
    binaries = gates.get("binaries")
    require(isinstance(binaries, dict), "candidate gates lack a binaries object")
    selected = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd",
                "ssmcp-frontend": args.bin_dir / "ssmcp-frontend", "vt_snapshot": args.decoder,
                "library_fixture": args.fixture_binary}
    artifacts, origins = {}, {}
    for name, path in selected.items():
        entry = ({"path": gates.get("library_binary"), "sha256": gates.get("library_sha256")}
                 if name == "library_fixture" else binaries.get(name))
        require(isinstance(entry, dict), "candidate gates lack " + name)
        original, sha = entry.get("path"), entry.get("sha256")
        require(isinstance(original, str) and Path(original).is_absolute(),
                name + " needs an absolute gated path")
        require(isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{64}", sha),
                name + " needs a gated SHA256")
        origin = Path(original).resolve(strict=True)
        require(origin.is_file() and os.access(origin, os.X_OK),
                "gated executable is unavailable: " + name)
        require(digest(origin) == digest(path) == sha,
                "selected or original executable differs from passed gates: " + name)
        origins["gate_origin_" + name] = origin
        artifacts[name] = {"gate_path": original, "resolved_gate_path": str(origin),
                           "input_path": str(path), "sha256": sha}
    alias = args.bin_dir / "examples" / "vt_snapshot"
    require(alias.resolve(strict=True) == args.decoder and digest(alias) == artifacts["vt_snapshot"]["sha256"],
            "decoder alias differs from the gated decoder")
    built_source = verify_built_source(gates, source_files)
    origins["gate_source_manifest"] = Path(built_source["path"]).resolve(strict=True)
    source = verify_source_revision(args, source_files)
    args.gated_origins = origins
    return {"receipt": receipt, "source_revision": args.source_revision,
            "check_count": len(checks), "artifacts": artifacts, "source": source,
            "tested_source_manifest": built_source}

def check_count(report, name):
    checks = report.get("checks")
    if name == "interactions":
        require(type(checks) is int and checks > 0,
                "interactions checks must be a positive integer")
        passed = report.get("passed_checks")
        require(isinstance(passed, list), "interactions lacks its passed_checks list")
        require(check_count({"checks": passed}, "_list") == checks,
                "interactions check total disagrees with passed_checks")
        count = checks
    else:
        require(isinstance(checks, list) and checks, "report checks must be a nonempty list")
        for check in checks:
            if isinstance(check, dict):
                require(check.get("passed", True) is True, "report contains a failed check")
                check = next((check[key] for key in ("check", "description", "name") if key in check), None)
            require(isinstance(check, str) and check.strip(), "report contains an empty or invalid check")
        count = len(checks)
    for key in ("count", "check_count"):
        if key in report:
            require(type(report[key]) is int and report[key] == count,
                    "report check total disagrees with " + key)
    return count

def verify_report(data, name, args, engine, baseline):
    key, expected_verdict = (("passed", True) if name == "account_compat" else
                            ("ok", True) if name == "client_ui" else ("status", "passed"))
    require(type(data.get(key)) is type(expected_verdict) and data[key] == expected_verdict,
            "suite report did not declare success")
    count = check_count(data, name)
    hashes = {key: value["sha256"] for key, value in baseline["files"].items()}
    hashes.update(vt_snapshot=hashes["decoder"], fixture=hashes["fixture_binary"],
                  library_fixture=hashes["fixture_binary"],
                  router_test_fixture=hashes["fixture_binary"], client=hashes["ssmcp"],
                  server=hashes["ssmcpd"], new_server=hashes["ssmcpd"])
    expected = {"source_revision": args.source_revision, "source_commit": args.source_revision,
                "engine_version": engine, "expected_engine_version": engine,
                "script_sha256": baseline["source_files"]["scripts/e2e_" + name + ".py"]["sha256"],
                "binary_sha256": hashes["ssmcp"], "client_sha256": hashes["ssmcp"],
                "old_server_sha256": hashes["old_server"], "new_server_sha256": hashes["ssmcpd"]}
    for key, value in expected.items():
        if key in data or (name == "zuse_worker" and key == "engine_version"):
            require(data.get(key) == value, "suite report differs for " + key)
    source_fields = ("sources", "source_sha256")
    for field in (*source_fields, "binaries", "binaries_sha256_before", "binaries_sha256_after"):
        if field in data:
            claimed = data[field]
            require(isinstance(claimed, dict) and claimed, "suite lacks reported hashes for " + field)
            for key, value in claimed.items():
                if field in source_fields:
                    source_key = key
                    if source_key not in baseline["source_files"] and isinstance(key, str) and Path(key).name == key:
                        source_key = "scripts/" + key
                    expected_hash = baseline["source_files"].get(source_key, {}).get("sha256")
                else:
                    expected_hash = hashes.get(key)
                require(expected_hash is not None and value == expected_hash,
                        "suite report contains a mismatched hash in " + field + "/" + str(key))
    require(data.get("inputs_unchanged", True) is True, "suite reported changed inputs")
    require(not data.get("cleanup_error"), "suite reported an owned cleanup error")
    require(data.get("owned_process_cleanup", "passed") == "passed",
            "suite reported unfinished owned cleanup")
    if "cleanup" in data:
        cleanup = data["cleanup"]
        require(isinstance(cleanup, dict) and cleanup.get("all_exited") is True,
                "suite reported unfinished owned processes")
        if "owned_processes" in cleanup:
            processes = cleanup["owned_processes"]
            require(isinstance(processes, list) and all(isinstance(item, dict)
                    and type(item.get("exit_code")) is int for item in processes),
                    "suite reported an owned process without an exit code")
    models = data.get("models_started", False)
    require(models is False or (type(models) is int and models == 0), "suite reported a model invocation")
    return count

def verify_desktop_text(data, evidence, baseline):
    nested = evidence / "native" / "whole-text.json"
    require(data.get("whole_text_report") == str(nested),
            "desktop report must reference its owned GTK text report")
    report, receipt = read_json_file(nested, evidence)
    require(report.get("status") == "passed" and report.get("owned_process_cleanup") == "passed"
            and not report.get("cleanup_error"), "GTK text fixture or cleanup failed")
    require(isinstance(report.get("cases"), list) and report["cases"], "GTK fixture reported no cases")
    for field, name in (("script_sha256", "e2e_x11_text.py"),
                        ("private_xvfb_sha256", "private_xvfb.py"),
                        ("state_probe_script_sha256", "e2e_zuse_worker.py")):
        expected = baseline["source_files"].get("scripts/" + name, {}).get("sha256")
        require(expected is not None and report.get(field) == expected, "GTK fixture differs for " + field)
    require(report.get("binary_sha256") == baseline["files"]["ssmcp"]["sha256"],
            "GTK fixture differs for binary_sha256")
    return {**receipt, "cases": len(report["cases"])}

def vt_snapshot(data):
    if not isinstance(data, dict) or data.get("ok") is not True:
        return False
    size, rows, cursor = (data.get(key) for key in ("size", "rows", "cursor"))
    return (isinstance(size, list) and len(size) == 2
            and all(type(n) is int and n > 0 for n in size)
            and isinstance(rows, list) and len(rows) == size[0]
            and all(isinstance(row, str) for row in rows)
            and isinstance(cursor, list) and len(cursor) == 2
            and all(type(n) is int and n >= 0 for n in cursor)
            and type(data.get("alternate_screen")) is bool
            and type(data.get("hide_cursor")) is bool)

def validate_inputs(args):
    for name in ("source_path", "bin_dir", "decoder", "fixture_binary", "old_server", "git_dir"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    require(not args.gates.is_symlink(), "--gates must be a regular receipt, not a symlink")
    args.gates = args.gates.resolve(strict=True)
    require(args.source_path.is_dir() and args.bin_dir.is_dir() and args.git_dir.is_dir(),
            "source, binary directory and original Git directory must exist")
    for executable in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", args.bin_dir / "ssmcp-frontend",
                       args.decoder, args.fixture_binary, args.old_server):
        require(executable.is_file() and os.access(executable, os.X_OK),
                "missing prebuilt executable: " + str(executable))
    alias = args.bin_dir / "examples" / "vt_snapshot"
    require(alias.is_file() and alias.resolve() == args.decoder,
            "BIN_DIR/examples/vt_snapshot must resolve to the frozen DECODER")
    require(digest(args.old_server) == OLD_SERVER_SHA256, "old server differs from the pinned fixture SHA256")
    require(Path("/usr/bin/bwrap").is_file(), "bubblewrap is required")

def normalized_source_files(entries):
    require(isinstance(entries, dict) and entries, "source manifest must contain entries")
    result = {}
    for name, entry in entries.items():
        require(isinstance(name, str) and name and not Path(name).is_absolute()
                and ".." not in Path(name).parts and Path(name).as_posix() == name,
                "source manifest contains an unsafe relative path")
        require(isinstance(entry, dict), "source manifest entry is not an object")
        kind = entry.get("type")
        if kind == "directory":
            continue
        if kind == "file":
            require(type(entry.get("bytes")) is int and entry["bytes"] >= 0
                    and type(entry.get("mode")) is int
                    and isinstance(entry.get("sha256"), str)
                    and re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]),
                    "source manifest file entry is incomplete: " + name)
            result[name] = {"type": "file", "bytes": entry["bytes"], "sha256": entry["sha256"],
                            "executable": bool(entry["mode"] & 0o111)}
        elif kind == "symlink":
            target = entry.get("target")
            require(isinstance(target, str), "source manifest symlink lacks its target")
            sha = hashlib.sha256(os.fsencode(target)).hexdigest()
            require(entry.get("sha256", sha) == sha, "source manifest symlink hash is inconsistent")
            result[name] = {"type": "symlink", "target": target, "sha256": sha}
        else:
            raise RuntimeError("source manifest contains unsupported entry: " + name)
    return result


def verify_built_source(gates, source_files):
    descriptor = gates.get("source_manifest")
    require(isinstance(descriptor, dict), "passed gate lacks its tested source manifest")
    path, expected = descriptor.get("path"), descriptor.get("sha256")
    require(isinstance(path, str) and Path(path).is_absolute(),
            "gate source manifest needs an absolute path")
    require(isinstance(expected, str) and re.fullmatch(r"[0-9a-f]{64}", expected),
            "gate source manifest needs its recorded SHA256")
    manifest, receipt = read_json_file(Path(path))
    require(receipt["sha256"] == expected, "tested source manifest bytes differ from passed gate")
    require(type(descriptor.get("entries")) is int and descriptor["entries"] == len(manifest),
            "tested source manifest entry count differs from passed gate")
    built, actual = normalized_source_files(manifest), normalized_source_files(source_files)
    require(set(built) == set(actual),
            "acceptance source paths differ from tested manifest: missing="
            + repr(sorted(set(built) - set(actual))[:8])
            + ", extra=" + repr(sorted(set(actual) - set(built))[:8]))
    for name in built:
        require(actual[name] == built[name], "acceptance source differs from tested manifest: " + name)
    return {**receipt, "entries": len(manifest), "source_files": len(built),
            "normalized_source_sha256": canonical_digest(built)}




def verify_gated_baseline(binding, baseline):
    files = baseline["files"]
    expected = {
        "candidate_gates": binding["receipt"]["sha256"],
        "gate_source_manifest": binding["tested_source_manifest"]["sha256"],
        "decoder_alias": binding["artifacts"]["vt_snapshot"]["sha256"],
    }
    selected = {"ssmcp": "ssmcp", "ssmcpd": "ssmcpd",
                "ssmcp-frontend": "ssmcp-frontend", "vt_snapshot": "decoder",
                "library_fixture": "fixture_binary"}
    for name, entry in binding["artifacts"].items():
        expected[selected[name]] = entry["sha256"]
        expected["gate_origin_" + name] = entry["sha256"]
    for name, sha in expected.items():
        require(files.get(name, {}).get("sha256") == sha,
                "gated input changed during gate verification: " + name)


if __name__ == "__main__":
    raise SystemExit(main())
