#!/usr/bin/env python3
"""Run native P1 only against two passed, immutable build cohorts."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import stat
import subprocess
import tarfile
import time


def require(value, message):
    if not value:
        raise ValueError(message)


def regular(path):
    path = Path(path)
    require(stat.S_ISREG(path.lstat().st_mode), f"not a regular file: {path}")
    return path


def sha(path):
    with regular(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_json(path):
    return json.loads(regular(path).read_text(encoding="utf-8"))


def write_json(path, data):
    path = Path(path)
    temporary = path.with_name(path.name + ".new")
    with temporary.open("x", encoding="utf-8") as stream:
        json.dump(data, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    temporary.replace(path)


def successful_rows(rows, name):
    require(isinstance(rows, list) and bool(rows), f"{name} checks are empty")
    require(all(isinstance(row, dict) and type(row.get("exit_code")) is int
                and row["exit_code"] == 0 for row in rows), f"{name} checks did not all pass")


def artifact(entry, name):
    require(isinstance(entry, dict), f"missing artifact {name}")
    path, digest = entry.get("path"), entry.get("sha256")
    require(isinstance(path, str) and Path(path).is_absolute(), f"{name} needs an absolute path")
    require(isinstance(digest, str) and re.fullmatch(r"[0-9a-f]{64}", digest), f"{name} hash is invalid")
    require(sha(path) == digest, f"{name} differs from its passed build receipt")
    return {"path": path, "sha256": digest, "bytes": Path(path).stat().st_size}


def passed_inputs(args):
    ssmcp = read_json(args.gates)
    native = read_json(args.zcalld_gates)
    require(ssmcp.get("status") == "passed", "SSMCP build is not passed")
    require(ssmcp.get("source_revision") == args.source_revision, "SSMCP source SHA mismatch")
    successful_rows(ssmcp.get("checks"), "SSMCP")
    require(native.get("status") == "passed", "zcalld build is not passed")
    require(native.get("revision") == args.zcalld_revision, "zcalld source SHA mismatch")
    successful_rows(native.get("stages"), "zcalld")
    require(native.get("source_unchanged") is True and native.get("binaries_unchanged") is True,
            "zcalld build input integrity was not verified")
    require(sha(args.zcalld_source_manifest) == native.get("source_manifest_sha256"),
            "zcalld source manifest does not match its build receipt")
    items = {}
    for name in ("ssmcp", "ssmcpd", "ssmcp-frontend", "vt_snapshot"):
        items[name] = artifact(ssmcp.get("binaries", {}).get(name), name)
    items["ssmcp-tests"] = artifact({"path": ssmcp.get("library_binary"),
                                    "sha256": ssmcp.get("library_sha256")}, "ssmcp-tests")
    for name, entry in native.get("binaries", {}).items():
        require(name not in items, "artifact names collide")
        items[name] = artifact(entry, name)
    require("zcalld" in items, "zcalld CLI artifact is missing")
    bin_dir = Path(items["ssmcp"]["path"]).parent
    for name in ("ssmcp", "ssmcpd", "ssmcp-frontend"):
        require(Path(items[name]["path"]) == bin_dir / name,
                "SSMCP product artifacts must share their declared binary directory")
    return ssmcp, native, items, bin_dir


def git(repo, *arguments):
    result = git_bytes(repo, *arguments)
    return result.decode("utf-8").strip()


def git_bytes(repo, *arguments, stdin=None):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_OPTIONAL_LOCKS="0", GIT_NO_LAZY_FETCH="1", GIT_ALLOW_PROTOCOL="",
               GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1",
               GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null")
    result = subprocess.run(["/usr/bin/git", "-c", "core.fsmonitor=false", "-C", str(repo),
                             *arguments], check=True, stdin=stdin, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    return result.stdout


def relative_source_path(name):
    require(isinstance(name, str) and bool(name), "source path is empty")
    path = PurePosixPath(name)
    require(not path.is_absolute() and str(path) == name and ".." not in path.parts
            and not any(part.lower() == ".git" for part in path.parts),
            f"unsafe source path: {name}")
    return name


def gitlinks(repo, revision):
    require(git(repo, "cat-file", "-t", revision) == "commit", "source object is not a commit")
    require(git(repo, "rev-parse", "--verify", revision + "^{commit}") == revision,
            "source commit did not resolve exactly")
    links = {}
    for entry in git_bytes(repo, "ls-tree", "-r", "-z", "--full-tree", revision).split(b"\0"):
        if not entry:
            continue
        header, raw_name = entry.split(b"\t", 1)
        mode, kind, oid = header.split()
        if mode == b"160000":
            name = relative_source_path(os.fsdecode(raw_name))
            require(kind == b"commit" and re.fullmatch(rb"[0-9a-f]{40}", oid), "invalid gitlink")
            require(name not in links, "duplicate gitlink")
            links[name] = oid.decode("ascii")
    return links


def normalized_build_manifest(manifest):
    require(isinstance(manifest, dict) and bool(manifest), "build source manifest is empty")
    normalized = {}
    for name, item in manifest.items():
        relative_source_path(name)
        require(isinstance(item, dict) and type(item.get("mode")) is int,
                f"invalid build source record: {name}")
        kind = item.get("type")
        if kind == "directory":
            continue
        require(kind == "file" and type(item.get("bytes")) is int and item["bytes"] >= 0
                and isinstance(item.get("sha256"), str)
                and re.fullmatch(r"[0-9a-f]{64}", item["sha256"]),
                f"unsupported or invalid build source record: {name}")
        normalized[name] = {"type": "file", "bytes": item["bytes"], "sha256": item["sha256"],
                            "executable": bool(item["mode"] & 0o111)}
    require(normalized, "build source manifest has no files")
    return normalized


def actual_build_manifest(source):
    result = {}
    for path in sorted(source.rglob("*")):
        name = relative_source_path(path.relative_to(source).as_posix())
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode),
                f"unrecorded special source entry: {name}")
        if stat.S_ISREG(info.st_mode):
            result[name] = {"type": "file", "bytes": info.st_size, "sha256": sha(path),
                            "executable": bool(info.st_mode & 0o111)}
    return result


def freeze_archive(entry, revision, archive_path, destination, repo, original_inputs):
    require(isinstance(entry, dict) and entry.get("commit") == revision,
            "build archive identifies another commit")
    origin = artifact({"path": entry.get("archive"), "sha256": entry.get("sha256")},
                      "build source archive")
    original_inputs[origin["path"]] = origin["sha256"]
    with regular(origin["path"]).open("rb") as source, archive_path.open("xb") as target:
        shutil.copyfileobj(source, target)
    require(sha(archive_path) == origin["sha256"], "source archive changed while being frozen")
    with archive_path.open("rb") as stream:
        embedded = git_bytes(repo, "get-tar-commit-id", stdin=stream).strip()
    require(embedded == revision.encode("ascii"), "build archive embeds another commit")
    archive_path.chmod(0o444)
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path) as stream:
        for member in stream.getmembers():
            relative_source_path(member.name.rstrip("/"))
            require(member.isfile() or member.isdir(), "build archive contains a special entry")
        stream.extractall(destination, filter="data")
    return {"path": str(archive_path), "sha256": origin["sha256"], "commit": revision,
            "origin": origin["path"]}


def source_manifest(source):
    require(source.is_dir() and not source.is_symlink(), "source root changed type")
    files, links = {}, {}
    for path in sorted(source.rglob("*")):
        name = path.relative_to(source).as_posix()
        if path.is_symlink():
            target = os.readlink(path)
            require(not os.path.isabs(target) and path.resolve().is_relative_to(source),
                    f"source symlink escapes archive: {name}")
            links[name] = target
        elif path.is_file():
            files[name] = sha(path)
        else:
            require(path.is_dir(), f"unsupported source entry: {name}")
    require(files, "source archive is empty")
    return {"files": files, "links": links}


def freeze_source(args, root, gate, original_inputs):
    source = root / "source"
    archives = gate.get("source_archives")
    require(isinstance(archives, dict) and isinstance(archives.get("gitlinks"), dict),
            "build receipt has no source archives")
    links = gitlinks(args.repository, args.source_revision)
    require(set(links) == {"crates/zharness", "crates/zplugin", "crates/ztuio", "crates/zuse"}
            and set(links) == set(archives["gitlinks"]), "build gitlink inventory differs")
    frozen_archives = {"main": freeze_archive(archives.get("main"), args.source_revision,
                        root / "source.tar", source, args.repository, original_inputs)}
    for name, revision in links.items():
        entry = archives["gitlinks"][name]
        require(isinstance(entry, dict) and isinstance(entry.get("repository"), str)
                and Path(entry["repository"]).is_absolute(), "gitlink repository is missing")
        require(not gitlinks(entry["repository"], revision), "nested gitlinks are unsupported")
        frozen_archives[name] = freeze_archive(entry, revision,
            root / ("gitlink-" + name.replace("/", "_") + ".tar"), source / name,
            entry["repository"], original_inputs)
    manifest_entry = gate.get("source_manifest")
    verified_manifest = artifact(manifest_entry, "SSMCP build source manifest")
    original_inputs[verified_manifest["path"]] = verified_manifest["sha256"]
    raw_manifest = regular(verified_manifest["path"]).read_bytes()
    require(hashlib.sha256(raw_manifest).hexdigest() == verified_manifest["sha256"],
            "SSMCP build manifest changed while reading")
    recorded = json.loads(raw_manifest)
    require(type(manifest_entry.get("entries")) is int and len(recorded) == manifest_entry["entries"],
            "SSMCP build manifest entry count differs")
    expected = normalized_build_manifest(recorded)
    require(actual_build_manifest(source) == expected,
            "test source is not the exact tree recorded by the passed build")
    manifest = source_manifest(source)
    src_tree = git(args.repository, "rev-parse", args.source_revision + ":src")
    manifest.update(product_revision=args.source_revision, test_source_revision=args.source_revision,
                    src_trees={"product": src_tree, "test": src_tree}, gitlinks=links,
                    source_path=str(source), build_entries=expected,
                    build_source_manifest=verified_manifest, source_archives=frozen_archives)
    for path in sorted(source.rglob("*"), reverse=True):
        if not path.is_symlink():
            path.chmod(path.stat().st_mode & 0o555)
    source.chmod(0o555)
    return source, manifest


def stop_owned(process):
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=8)


def freeze_artifacts(root, artifacts, original_inputs):
    directory = root / "artifacts"
    directory.mkdir(mode=0o700)
    frozen = {}
    for name, entry in artifacts.items():
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name), "unsafe artifact name")
        origin = regular(entry["path"])
        original_inputs[str(origin)] = entry["sha256"]
        destination = directory / name
        with origin.open("rb") as source, destination.open("xb") as target:
            shutil.copyfileobj(source, target)
        require(sha(destination) == entry["sha256"], f"artifact changed while freezing: {name}")
        destination.chmod(0o555)
        frozen[name] = {"path": str(destination), "sha256": entry["sha256"],
                        "bytes": destination.stat().st_size, "origin": str(origin)}
    directory.chmod(0o555)
    return frozen, directory


def validate_report(evidence, binding, minimum_checks, started_ns):
    path = regular(evidence / "report.json")
    require(path.stat().st_mtime_ns >= started_ns - 1_000_000_000, "report is not fresh")
    report_bytes = path.read_bytes()
    report = json.loads(report_bytes)
    require(report.get("status") == "passed", "P1 report did not pass")
    require(report.get("source_revision") == binding["product_revision"], "P1 SSMCP SHA mismatch")
    require(report.get("zcalld_revision") == binding["zcalld_revision"], "P1 zcalld SHA mismatch")
    checks = report.get("checks")
    require(isinstance(checks, list) and len(checks) >= minimum_checks
            and all(isinstance(item, str) and bool(item.strip()) for item in checks),
            "P1 checks are empty, malformed, or incomplete")
    require(report.get("inputs_unchanged") is True, "P1 producer reports changed inputs")
    mapping = {"ssmcp": "ssmcp", "ssmcpd": "ssmcpd", "decoder": "vt_snapshot",
               "fixture": "ssmcp-tests", "zcalld": "zcalld"}
    wanted = {key: binding["hashes_before"][name] for key, name in mapping.items()}
    require(report.get("binaries_sha256_before") == wanted
            and report.get("binaries_sha256_after") == wanted, "P1 used another binary cohort")
    require(report.get("harness_sha256_before") == binding["harness_sha256"]
            and report.get("harness_sha256_after") == binding["harness_sha256"],
            "P1 used another harness")
    require(report.get("script_sha256") == binding["script_sha256"], "P1 script hash mismatch")
    requests = report.get("provider_requests")
    require(isinstance(requests, list) and len(requests) == 4
            and all(isinstance(item, dict) and item.get("unexpected") is False
                    and item.get("caller_credential_forwarded") is False for item in requests),
            "provider observed unplanned requests or caller credentials")
    planned = [
        '  P1_INPUT_中文\n    printf "literal $(do-not-run)"\nsecond line  ',
        "P1_PROVIDER_FAILURE",
        "P1_INPUT_BEFORE_ARCHIVE",
        "P1_INPUT_BEFORE_DISCONNECT",
    ]
    for request, content in zip(requests, planned, strict=True):
        body = request.get("body")
        require(request.get("path") == "/v1/chat/completions"
                and isinstance(body, dict) and set(body) == {"model", "stream", "messages"}
                and body.get("model") == "fixture-upstream-model" and body.get("stream") is False
                and body.get("messages") == [{"role": "user", "content": content}],
                "provider request differs from the four planned exact inputs")
    expected_states = {"unix-complete-busy": "completed", "tcp-generation-failed": "failed",
                       "real-reply-idempotency": "completed"}
    receipts = report.get("receipts")
    require(isinstance(receipts, list) and len(receipts) == len(expected_states), "P1 receipts are incomplete")
    require({item.get("scenario") for item in receipts if isinstance(item, dict)} == set(expected_states),
            "P1 receipt scenarios differ")
    for item in receipts:
        result = item.get("result", {})
        require(result.get("execution_state") == expected_states[item["scenario"]]
                and result.get("history_saved") is True and result.get("shown") is True,
                "P1 execution/history receipt does not match the scenario")
    frames = report.get("frames")
    require(isinstance(frames, list) and len(frames) == 2, "P1 frames are missing")
    hashes = {}
    for cols in (120, 48):
        name = f"input-history-zcalld-complete-{cols}.json"
        require(sum(isinstance(frame, dict) and frame.get("name") == "complete"
                    and type(frame.get("cols")) is int and type(frame.get("rows")) is int
                    and frame.get("cols") == cols and frame.get("rows") == 30
                    and frame.get("path") == name for frame in frames) == 1, "P1 frame contract changed")
        frame = regular(evidence / name)
        require(frame.stat().st_mtime_ns >= started_ns - 1_000_000_000, "P1 frame is not fresh")
        frame_bytes = frame.read_bytes()
        captured = json.loads(frame_bytes)
        require(isinstance(captured, dict) and captured.get("ok") is True
                and captured.get("size") == [30, cols]
                and all(type(value) is int for value in captured["size"]),
                "P1 frame has no successful exact-size VT capture")
        rows, cells = captured.get("rows"), captured.get("cells")
        require(isinstance(rows, list) and len(rows) == 30
                and all(isinstance(row, str) for row in rows),
                "P1 frame text rows are malformed")
        require(isinstance(cells, list) and len(cells) == 30
                and all(isinstance(row, list) and len(row) == cols
                        and all(isinstance(cell, dict) and isinstance(cell.get("text"), str)
                                for cell in row) for row in cells),
                "P1 frame lacks a complete actual terminal cell grid")
        require("P1_NATIVE_RESULT" in "\n".join(rows)
                and "P1_NATIVE_RESULT" in "\n".join("".join(cell["text"] or " " for cell in row
                                                          if not cell.get("continuation", False))
                                                    for row in cells),
                "saved P1 frame does not display the native result")
        hashes[name] = hashlib.sha256(frame_bytes).hexdigest()
    return report, hashes, hashlib.sha256(report_bytes).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--zcalld-revision", required=True)
    parser.add_argument("--gates", required=True, type=Path)
    parser.add_argument("--zcalld-gates", required=True, type=Path)
    parser.add_argument("--zcalld-source-manifest", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--minimum-checks", type=int, default=94)
    args = parser.parse_args()
    for revision in (args.source_revision, args.zcalld_revision):
        require(re.fullmatch(r"[0-9a-f]{40}", revision), "full commit SHAs are required")
    require(1 <= args.timeout <= 900 and args.minimum_checks >= 94, "invalid timeout/check contract")
    os.umask(0o077)
    original_inputs = {str(path): sha(path) for path in
                       (args.gates, args.zcalld_gates, args.zcalld_source_manifest)}
    ssmcp, native, artifacts, bin_dir = passed_inputs(args)
    root = args.out.resolve()
    require(not any(root.is_relative_to(Path(mask)) for mask in ("/tmp", "/proc", "/dev", "/sys")),
            "output root would be hidden by the sandbox's private mounts")
    root.mkdir(mode=0o700, exist_ok=False)
    evidence, inputs = root / "evidence", root / "build-inputs"
    evidence.mkdir(mode=0o700)
    inputs.mkdir(mode=0o700)
    source, tests = freeze_source(args, root, ssmcp, original_inputs)
    artifacts, bin_dir = freeze_artifacts(root, artifacts, original_inputs)
    test_manifest = root / "test-source-manifest.json"
    write_json(test_manifest, tests)
    manifests = {}
    for name, origin in {"ssmcp-build": args.gates, "zcalld-build": args.zcalld_gates,
                         "ssmcp-source": Path(tests["build_source_manifest"]["path"]),
                         "zcalld-source": args.zcalld_source_manifest, "test-source": test_manifest}.items():
        destination = inputs / (name + ".json")
        shutil.copyfile(regular(origin), destination)
        destination.chmod(0o444)
        manifests[name] = {"path": str(destination), "sha256": sha(destination)}
        if name != "test-source":
            require(manifests[name]["sha256"] == original_inputs[str(origin)],
                    f"original build input changed while copying: {name}")
    require(read_json(inputs / "ssmcp-build.json") == ssmcp, "SSMCP gate changed during preparation")
    require(read_json(inputs / "zcalld-build.json") == native, "zcalld gate changed during preparation")
    require(sha(inputs / "zcalld-source.json") == native["source_manifest_sha256"],
            "zcalld source manifest changed during preparation")
    require(read_json(inputs / "test-source.json") == tests, "test source manifest copy differs")
    inputs.chmod(0o555)
    script = source / "scripts/e2e_zcalld_listener.py"
    harness = {path.name: sha(path) for path in sorted((source / "scripts").glob("e2e*.py"))}
    before = {name: sha(item["path"]) for name, item in artifacts.items()}
    require(all(before[name] == item["sha256"] for name, item in artifacts.items()), "inputs changed during preparation")
    command = ["/usr/bin/bwrap", "--unshare-net", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
               "--new-session", "--die-with-parent", "--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev",
               "--tmpfs", "/tmp", "--bind", str(evidence), str(evidence), "--clearenv",
               "--setenv", "PATH", "/usr/bin:/bin", "--setenv", "HOME", "/tmp",
               "--setenv", "LANG", "C.UTF-8", "--setenv", "LC_ALL", "C.UTF-8",
               "--setenv", "TERM", "xterm-256color", "--setenv", "PYTHONDONTWRITEBYTECODE", "1",
               "--chdir", str(source), "/usr/bin/python3", "-B", str(script),
               "--bin-dir", str(bin_dir), "--decoder", artifacts["vt_snapshot"]["path"],
               "--fixture-binary", artifacts["ssmcp-tests"]["path"], "--zcalld", artifacts["zcalld"]["path"],
               "--source-revision", args.source_revision, "--zcalld-revision", args.zcalld_revision,
               "--out", str(evidence)]
    binding = {"scope": "final immutable SSMCP and zcalld native P1 cohort", "status": "running",
               "product_revision": args.source_revision, "test_source_revision": args.source_revision,
               "zcalld_revision": args.zcalld_revision, "same_product_src_tree": tests["src_trees"]["product"],
               "ssmcp_gate_status": ssmcp["status"], "zcalld_gate_status": native["status"],
               "build_manifests": manifests, "artifacts": artifacts, "hashes_before": before,
               "source_archives": tests["source_archives"], "original_inputs": original_inputs,
               "source_matches_build_manifest": True,
               "integrity_scope": "Run uses owned read-only copies; original inputs are checked before and after.",
               "harness_sha256": harness, "script_sha256": sha(script), "command": command,
               "source_path": str(source), "evidence_path": str(evidence), "timed_out": False,
               "wrapper_sha256": sha(__file__)}
    write_json(root / "source-binding.json", binding)
    process = None
    started = time.monotonic()
    started_ns = time.time_ns()
    errors = []
    try:
        with (evidence / "outer.log").open("xb") as log:
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                binding["exit_code"] = process.wait(timeout=args.timeout)
            except subprocess.TimeoutExpired:
                binding["timed_out"] = True
                stop_owned(process)
                binding["exit_code"] = process.returncode
        require(not binding["timed_out"] and binding["exit_code"] == 0, "P1 process failed or timed out")
        result, frames, report_hash = validate_report(evidence, binding, args.minimum_checks, started_ns)
        binding.update(e2e_status=result["status"], e2e_checks=len(result["checks"]),
                       e2e_report_sha256=report_hash, frame_sha256=frames)
    except BaseException as error:
        errors.append(f"{type(error).__name__}: {error}")
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        try:
            stop_owned(process)
        except BaseException as error:
            errors.append(f"cleanup {type(error).__name__}: {error}")
        binding["seconds"] = round(time.monotonic() - started, 3)
        try:
            binding["hashes_after"] = {name: sha(item["path"]) for name, item in artifacts.items()}
            binding["artifacts_unchanged"] = binding["hashes_after"] == before
            after = source_manifest(source)
            binding["test_source_unchanged"] = (after == {"files": tests["files"], "links": tests["links"]}
                and actual_build_manifest(source) == tests["build_entries"])
            binding["build_manifests_unchanged"] = all(sha(item["path"]) == item["sha256"] for item in manifests.values())
            binding["source_archives_unchanged"] = all(sha(item["path"]) == item["sha256"]
                                                       for item in tests["source_archives"].values())
            binding["original_inputs_unchanged"] = all(sha(path) == digest
                                                        for path, digest in original_inputs.items())
            require(binding["artifacts_unchanged"] and binding["test_source_unchanged"]
                    and binding["build_manifests_unchanged"] and binding["source_archives_unchanged"]
                    and binding["original_inputs_unchanged"], "immutable inputs changed")
        except BaseException as error:
            errors.append(f"{type(error).__name__}: {error}")
        binding["errors"] = errors
        binding["status"] = "failed" if errors else "passed"
        require(not os.path.lexists(evidence / "source-binding.json"), "producer unexpectedly wrote wrapper receipt")
        write_json(evidence / "source-binding.json", binding)
        write_json(root / "source-binding.json", binding)
    print(json.dumps({key: binding.get(key) for key in
                      ("status", "exit_code", "timed_out", "seconds", "e2e_checks", "errors")}, ensure_ascii=False))
    print(str(evidence / "source-binding.json"))
    return 1 if errors else 0


if __name__ == "__main__":
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"interrupted by signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    raise SystemExit(main())
