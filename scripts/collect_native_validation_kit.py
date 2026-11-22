#!/usr/bin/env python3
"""Freeze validated Linux CI outputs without rebuilding or rerunning tests."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile


KIT_NAME = "native-validation-kit-linux-x86_64"
PRODUCER_SOURCE = "scripts/collect_native_validation_kit.py"
DEFAULT_TEST_SELECTORS = (
    "portal::tests::http_login_callback_replay_and_cross_account_mutations",
    "device::tests::username_binding_and_admin_ban_cover_real_http_and_live_websockets",
)
REQUIRED_SELECTORS = (
    "worker::pool::command_logs_tests::native_exec_records_real_start_and_exit_on_its_original_pool",
    "http::temporary_links::",
    "http::worker_reminders::session_tests::real_session_probe_preserves_errors_and_does_not_consume_user_input",
    "http::relay_composer_fixture::relay_composer_server_fixture",
    *DEFAULT_TEST_SELECTORS,
)


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def describe(path):
    info = path.lstat()
    value = {"mode": f"{stat.S_IMODE(info.st_mode):04o}"}
    if stat.S_ISLNK(info.st_mode):
        target = os.readlink(path)
        data = os.fsencode(target)
        return {**value, "kind": "symlink", "target": target,
                "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"expected a regular file or source symlink: {path}")
    return {**value, "kind": "file", "bytes": info.st_size, "sha256": sha256(path)}


def digest(entries):
    encoded = json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def git(root, *arguments):
    return subprocess.check_output(["git", "-C", str(root), *arguments])


def source_identity(root):
    if git(root, "status", "--porcelain=v1", "-z", "--untracked-files=all",
           "--ignore-submodules=none"):
        raise ValueError("source worktree must be clean, including submodules and untracked files")
    entries, files, gitlinks = {}, {}, {}
    for record in git(root, "ls-files", "--stage", "-z").split(b"\0"):
        if not record:
            continue
        metadata, raw_name = record.split(b"\t", 1)
        mode, object_id, stage = metadata.decode().split()
        name = os.fsdecode(raw_name)
        if stage != "0":
            raise ValueError(f"unmerged source: {name}")
        if mode == "160000":
            gitlinks[name] = object_id
            entries[name] = "gitlink:" + object_id
            continue
        value = describe(root / name)
        if (mode == "120000") != (value["kind"] == "symlink"):
            raise ValueError(f"source type differs from Git: {name}")
        files[name] = value
        entries[name] = ("symlink:" + value["target"]
                         if value["kind"] == "symlink" else value["sha256"])
    return {"base_commit": git(root, "rev-parse", "HEAD").decode().strip(),
            "base_tree": git(root, "rev-parse", "HEAD^{tree}").decode().strip(),
            "source_digest": digest(entries), "entries": entries,
            "files": files, "excluded_gitlinks": gitlinks}


def write_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def outside_source(root, path):
    if path.resolve().is_relative_to(root.resolve()):
        raise ValueError("validation evidence must be outside the source worktree")


def capture(root, identity_path):
    outside_source(root, identity_path)
    identity = source_identity(root)
    if source_identity(root) != identity:
        raise ValueError("source changed while recording its initial identity")
    write_json(identity_path, identity)
    return identity


def copy_checked(source, destination, expected):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination, follow_symlinks=False)
    if describe(destination) != expected or describe(source) != expected:
        raise ValueError(f"file changed while freezing: {source}")


def ci_provenance(environment):
    if (environment.get("GITHUB_ACTIONS") != "true"
            or environment.get("RUNNER_OS") != "Linux"
            or environment.get("RUNNER_ARCH") != "X64"):
        raise ValueError("collection requires the successful Linux x86_64 CI job")
    names = ["GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB",
             "GITHUB_WORKFLOW", "GITHUB_WORKFLOW_REF", "GITHUB_WORKFLOW_SHA",
             "GITHUB_SHA", "GITHUB_EVENT_NAME", "GITHUB_SERVER_URL", "RUNNER_OS", "RUNNER_ARCH"]
    if any(not environment.get(name) for name in names):
        raise ValueError("CI provenance is incomplete")
    for name in ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"):
        if environment.get(name) != "0":
            raise ValueError(f"expected the validated CI profile override {name}=0")
    result = {name.lower(): environment[name] for name in names}
    result["run_url"] = (f"{environment['GITHUB_SERVER_URL']}/{environment['GITHUB_REPOSITORY']}"
                         f"/actions/runs/{environment['GITHUB_RUN_ID']}")
    result["validation_guard"] = "success() after all preceding Linux job steps"
    return result


def read_test_list(binary, ignored=False):
    command = [str(binary), "--list", "--format", "terse"]
    if ignored:
        command.append("--ignored")
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            check=True, timeout=30)
    return result.stdout.decode("utf-8")


def test_names(listed):
    return {line.removesuffix(": test") for line in listed.splitlines() if line.endswith(": test")}


def collect(root, identity_path, archive, fixture, provenance):
    outside_source(root, identity_path)
    outside_source(root, archive)
    if archive.exists() or archive.with_suffix(archive.suffix + ".sha256").exists():
        raise ValueError("validation archive or checksum already exists")
    expected = json.loads(identity_path.read_text(encoding="utf-8"))
    if digest(expected["entries"]) != expected["source_digest"]:
        raise ValueError("recorded source digest does not match its index")
    if source_identity(root) != expected:
        raise ValueError("source differs from the identity recorded before validation")
    producer_hash = sha256(Path(__file__).resolve())
    if expected["files"][PRODUCER_SOURCE]["sha256"] != producer_hash:
        raise ValueError("collector does not match the validated source")
    debug = (root / "target/debug").resolve(strict=True)
    if not fixture:
        raise ValueError("SSMCP_E2E_FIXTURE_BINARY must identify the already validated lib-tests")
    fixture = Path(fixture).resolve(strict=True)
    sources = {"ssmcp": debug / "ssmcp", "ssmcpd": debug / "ssmcpd",
               "ssmcp-frontend": debug / "ssmcp-frontend",
               "vt_snapshot": debug / "examples/vt_snapshot", "lib-tests": fixture}
    artifacts = {}
    with tempfile.TemporaryDirectory(prefix=KIT_NAME + "-", dir=archive.parent) as temporary:
        kit = Path(temporary) / KIT_NAME
        kit.mkdir()
        for name, source in sources.items():
            source = source.resolve(strict=True)
            if not source.is_relative_to(debug):
                raise ValueError(f"validated binary is outside target/debug: {name}")
            value = describe(source)
            if value["kind"] != "file" or not int(value["mode"], 8) & stat.S_IXUSR:
                raise ValueError(f"validated binary is not a regular executable: {name}")
            destination = kit / "bin" / name
            copy_checked(source, destination, value)
            artifacts[name] = {**value, "path": f"bin/{name}",
                               "source": str(source.relative_to(root))}
        runner = root / "src/code_mode.js"
        value = expected["files"]["src/code_mode.js"]
        if value["kind"] != "file":
            raise ValueError("the code-mode runner must be a regular source file")
        copy_checked(runner, kit / "libexec/ssmcp/code-mode.js", value)
        artifacts["code-mode.js"] = {**value, "path": "libexec/ssmcp/code-mode.js",
                                     "source": "src/code_mode.js"}
        value = describe(kit / "bin/vt_snapshot")
        copy_checked(kit / "bin/vt_snapshot", kit / "bin/examples/vt_snapshot", value)
        artifacts["examples/vt_snapshot"] = {
            **value, "path": "bin/examples/vt_snapshot", "same_as": "vt_snapshot"}
        listed = read_test_list(kit / "bin/lib-tests")
        names = test_names(listed)
        for selector in REQUIRED_SELECTORS:
            if not any(name.startswith(selector) if selector.endswith("::") else name == selector
                       for name in names):
                raise ValueError(f"validated lib-tests is missing {selector}")
        ignored = read_test_list(kit / "bin/lib-tests", ignored=True)
        if any(name in test_names(ignored) for name in DEFAULT_TEST_SELECTORS):
            raise ValueError("required account tests are ignored by the default Cargo test gate")
        (kit / "test-list.txt").write_text(listed, encoding="utf-8")
        (kit / "ignored-test-list.txt").write_text(ignored, encoding="utf-8")
        frozen_entries = {name: "gitlink:" + commit
                          for name, commit in expected["excluded_gitlinks"].items()}
        for name, value in expected["files"].items():
            destination = kit / "source" / name
            if not destination.parent.resolve().is_relative_to(kit / "source"):
                raise ValueError(f"source path leaves the frozen tree: {name}")
            copy_checked(root / name, destination, value)
            copied = describe(destination)
            frozen_entries[name] = ("symlink:" + copied["target"]
                                    if copied["kind"] == "symlink" else copied["sha256"])
        if frozen_entries != expected["entries"] or digest(frozen_entries) != expected["source_digest"]:
            raise ValueError("frozen source does not match the validated source digest")
        write_json(kit / "source-files.json", frozen_entries)
        write_json(kit / "source-tree.json", {
            "source_digest": expected["source_digest"], "entries": expected["files"],
            "excluded_gitlinks": expected["excluded_gitlinks"]})
        if source_identity(root) != expected:
            raise ValueError("source changed while freezing the validated outputs")
        manifest = {key: expected[key] for key in ("base_commit", "base_tree", "source_digest")}
        manifest.update({
            "status": "passed_and_frozen", "worktree": str(root), "evidence": ".",
            "path_base": "archive-root", "phases": [], "artifacts": artifacts,
            "producer": {"path": str(Path(__file__).resolve()), "sha256": producer_hash,
                         "source": "source/" + PRODUCER_SOURCE,
                         "commit": expected["base_commit"]},
            "ci": provenance,
            "cargo_profile_overrides": {"CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0"},
            "test_list": {**describe(kit / "test-list.txt"), "path": "test-list.txt"},
            "ignored_test_list": {**describe(kit / "ignored-test-list.txt"), "path": "ignored-test-list.txt"},
            "required_selectors": REQUIRED_SELECTORS,
            "default_test_selectors": DEFAULT_TEST_SELECTORS,
            "source_files": {**describe(kit / "source-files.json"), "path": "source-files.json"},
            "source_tree": {
                "path": "source", "source_digest": expected["source_digest"],
                "copied_entries": len(expected["files"]),
                "bytes": sum(value["bytes"] for value in expected["files"].values()),
                "index": "source-tree.json", "index_sha256": sha256(kit / "source-tree.json"),
                "index_bytes": (kit / "source-tree.json").stat().st_size,
                "index_mode": describe(kit / "source-tree.json")["mode"],
                "excluded_gitlinks": expected["excluded_gitlinks"]},
        })
        write_json(kit / "manifest.json", manifest)
        created = False
        try:
            with archive.open("xb") as stream:
                created = True
                with tarfile.open(fileobj=stream, mode="w:gz", compresslevel=3, dereference=False) as bundle:
                    for path in sorted(kit.iterdir()):
                        bundle.add(path, arcname=path.name)
            if source_identity(root) != expected:
                raise ValueError("source changed while archiving the validated outputs")
        except BaseException:
            if created:
                archive.unlink(missing_ok=True)
            raise
    checksum = sha256(archive)
    with archive.with_suffix(archive.suffix + ".sha256").open("x") as stream:
        stream.write(f"{checksum}  {archive.name}\n")
    return {"archive": str(archive), "bytes": archive.stat().st_size, "sha256": checksum,
            "base_commit": expected["base_commit"], "source_digest": expected["source_digest"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("capture", "collect"))
    parser.add_argument("--identity", required=True, type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--fixture-binary", default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.mode == "capture":
        result = capture(root, args.identity)
        print(json.dumps({key: result[key] for key in ("base_commit", "base_tree", "source_digest")}))
    else:
        if args.archive is None:
            parser.error("collect requires --archive")
        print(json.dumps(collect(root, args.identity, args.archive, args.fixture_binary,
                                 ci_provenance(os.environ))))


if __name__ == "__main__":
    main()
