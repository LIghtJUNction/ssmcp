"""Verify a manual-main publication with separate workflow and product identities.

UNBOUND preparation: final S, workflow P, run and original dispatch evidence have
not been selected. Missing or unsupported evidence fails closed. This program
never dispatches a workflow, creates a tag, publishes, or deploys.
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tomllib
import urllib.request

from dispatch_receipt_adapter import parse_dispatch_capture
from manual_main_provenance import verify_execution


def api(endpoint):
    result = subprocess.run(["gh", "api", "--hostname", "github.com", endpoint],
                            capture_output=True, text=True, timeout=60, check=True)
    return json.loads(result.stdout)


def download(url):
    request = urllib.request.Request(url, headers={"User-Agent": "ssmcp-release-verification"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


def digest(data):
    return hashlib.sha256(data).hexdigest()


# Receipt schema from scripts/release.py at 5d087d1f7363fe5741f6080168037623362280e7.
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
PROJECT_Z_DIRECT = {"zplugin": "zplugin", "zharness": "zharness",
                    "ztuio": "ztuio", "zuse": "zuse-computer"}
PROJECT_Z_REPOS = {name: name for name in
                   ("zplugin", "zguard", "zcall", "zconv", "zmcpio", "zclap", "z64",
                    "zsys", "zharness", "ztuio")}
PROJECT_Z_REPOS.update({"zuse-computer": "zuse", "zplugin-macros": "zplugin"})


def require(condition, message):
    if not condition:
        raise ValueError(message)


# Q is the reviewed recipe source, not a selected release candidate or main SHA.
# Updating any pin requires a new source review; never learn pins from the run.
RECIPE_SOURCE_COMMIT = "647bce2ec9cf74beab43adea831c2e0712d28ea8"
REVIEWED_RECIPE = {
    ".github/workflows/release.yml": (18881, "6dd28b97b77386edb6fa8d6538ed9954f0ed00b1b97e1baa2651f339415c2295"),
    ".github/workflows/ci.yml": (14941, "d6d920faddfa8840a1ec4b37d54fddb6d169335139fa1bf241d04d716a753873"),
    ".github/workflows/deploy-server.yml": (4499, "2b8accbeaa94a674a1e6ddea762567cd97b74a1293cc45a53c4e34032a1e4a4c"),
    "scripts/release.py": (54070, "43e995d675689f3c881f4452172796f4e2f7b195dbceef3dbb9b1ed1d9f95be4"),
    "scripts/install.sh": (6783, "25e5f2f8f93ca093b3851277fe436d3ba3a1451962f70ed957280f97f96c4906"),
    "scripts/install.ps1": (29653, "24844a03fb6eaef23979748b47f6fa1ecc6e956b3272c0d3c41dd7f36ded1277"),
}
DISPATCH_API_VERSION = "2026-03-10"


def api_bytes(endpoint):
    """Return the authenticated GitHub response body without text re-encoding."""
    result = subprocess.run(["gh", "api", "--hostname", "github.com", endpoint],
                            capture_output=True, timeout=180, check=True)
    return result.stdout


def git_read(checkout, *arguments):
    return subprocess.check_output(
        ["git", "--no-replace-objects", *arguments], cwd=checkout,
        stderr=subprocess.PIPE, timeout=60)


def verify_reviewed_recipe(checkout, candidate, workflow_sha):
    """Require S <= P and the same reviewed publication recipe at both commits."""
    for label, commit in (("candidate", candidate), ("workflow", workflow_sha)):
        require(re.fullmatch(r"[0-9a-f]{40}", commit) is not None,
                label + " must be an exact commit SHA")
        require(git_read(checkout, "cat-file", "-t", commit).strip() == b"commit",
                label + " must be a commit object")
    # Replacement objects must not manufacture either ancestry or recipe bytes.
    ancestry = subprocess.run(
        ["git", "--no-replace-objects", "merge-base", "--is-ancestor", candidate, workflow_sha],
        cwd=checkout, capture_output=True, timeout=60)
    require(ancestry.returncode == 0,
            "candidate is not a verified ancestor of the exact workflow commit")
    verified = {}
    for path, (expected_bytes, expected_sha256) in REVIEWED_RECIPE.items():
        source = git_read(checkout, "show", candidate + ":" + path)
        workflow = git_read(checkout, "show", workflow_sha + ":" + path)
        require(source == workflow and len(source) == expected_bytes
                and digest(source) == expected_sha256,
                "P/S publication recipe differs from the reviewed bytes: " + path)
        verified[path] = {"bytes": len(source), "sha256": digest(source)}
    return {"recipe_source_commit": RECIPE_SOURCE_COMMIT,
            "candidate_sha": candidate, "workflow_sha": workflow_sha,
            "candidate_is_ancestor_of_workflow": True,
            "deferred_version": "0.10.25", "files": verified}


def read_original_evidence(path, expected_sha256, label):
    """Bind original bytes; a hash by itself does not establish their origin."""
    require(re.fullmatch(r"[0-9a-f]{64}", expected_sha256) is not None,
            label + " requires an exact lowercase SHA-256")
    try:
        data = path.read_bytes()
    except OSError:
        raise ValueError(label + " original bytes are unavailable") from None
    require(data and digest(data) == expected_sha256,
            label + " bytes differ from the supplied original receipt hash")
    # The original private receipt path and its full content are not public report fields.
    return data, {"bytes": len(data), "sha256": digest(data)}


def explicit_false(value, label):
    require(value is False or type(value) is str and value == "false",
            label + " must be explicitly false; missing or other falsey values are invalid")
    return False


def verify_dispatch_contract(receipt, run, repository, run_id, tag, workflow_sha):
    """Check facts from the bound original capture adapter, never direct CLI JSON."""
    request, response = receipt["request"], receipt["response"]
    workflow_id = run["workflow_id"]
    require(type(workflow_id) is int and workflow_id > 0, "invalid workflow identity")
    base = f"https://api.github.com/repos/{repository}"
    endpoints = {base + "/actions/workflows/release.yml/dispatches",
                 base + f"/actions/workflows/{workflow_id}/dispatches"}
    require(request["method"] == "POST" and request["url"] in endpoints
            and request["api_version"] == DISPATCH_API_VERSION,
            "dispatch request endpoint, method or API version differs")
    body = request["body"]
    require(isinstance(body, dict) and set(body) == {"ref", "inputs"}
            and body.get("ref") == "main",
            "original dispatch must explicitly select main")
    inputs = body.get("inputs")
    require(isinstance(inputs, dict) and set(inputs) == {"tag", "deploy_server", "build_only"}
            and inputs.get("tag") == tag,
            "original dispatch tag differs")
    require("deploy_server" in inputs and "build_only" in inputs,
            "original dispatch must contain both explicit false inputs")
    explicit_false(inputs["deploy_server"], "deploy_server")
    explicit_false(inputs["build_only"], "build_only")
    result = response["body"]
    require(type(response["status"]) is int and response["status"] == 200
            and isinstance(result, dict), "dispatch did not return the required HTTP 200 receipt")
    require(type(result.get("workflow_run_id")) is int
            and result["workflow_run_id"] == run_id
            and result.get("run_url") == base + f"/actions/runs/{run_id}"
            and result.get("html_url") == f"https://github.com/{repository}/actions/runs/{run_id}",
            "dispatch response does not identify this exact workflow run")
    require(run["id"] == run_id and run["event"] == "workflow_dispatch"
            and run["head_branch"] == "main" and run["head_sha"] == workflow_sha
            and type(run.get("run_attempt")) is int and run["run_attempt"] == 1,
            "dispatch response is not bound to the original manual-main run")
    return {"api_version": DISPATCH_API_VERSION, "run_id": run_id, "run_attempt": 1,
            "workflow_sha": workflow_sha, "requested_ref": "main", "tag": tag,
            "raw_deploy_server": inputs["deploy_server"],
            "raw_build_only": inputs["build_only"],
            "deploy_server": False, "build_only": False}


def verify_referenced_workflows(run, repository, workflow_sha):
    """Check actual resolved references; a referenced deployment did not execute."""
    references = run.get("referenced_workflows")
    require(isinstance(references, list), "run is missing referenced_workflows evidence")
    expected = {f"{repository}/.github/workflows/{name}@{workflow_sha}"
                for name in ("ci.yml", "deploy-server.yml")}
    seen = set()
    for reference in references:
        require(isinstance(reference, dict) and reference.get("path") in expected
                and reference["path"] not in seen and reference.get("sha") == workflow_sha
                and reference.get("ref") == "refs/heads/main",
                "resolved reusable workflow does not match the reviewed main recipe")
        seen.add(reference["path"])
    require(seen == expected, "resolved reusable workflow set differs from the reviewed recipe")
    return [{"path": row["path"], "sha": row["sha"], "ref": row["ref"]}
            for row in references]


def package_identity(row):
    require(isinstance(row, dict) and isinstance(row.get("name"), str)
            and bool(row["name"]) and isinstance(row.get("version"), str)
            and bool(row["version"]), "invalid package identity")
    source = row.get("source")
    require(source is None or isinstance(source, str) and bool(source),
            "invalid package source")
    return row["name"], row["version"], source


def dependency_tables(manifest):
    for table in [manifest, *manifest.get("target", {}).values()]:
        for key in ("dependencies", "build-dependencies", "dev-dependencies"):
            for name, spec in table.get(key, {}).items():
                yield name, {"version": spec} if isinstance(spec, str) else spec


def locked_packages(lock):
    rows = lock.get("package")
    require(isinstance(rows, list) and bool(rows), "missing lock packages")
    result = {}
    for row in rows:
        identity = package_identity(row)
        require(identity not in result, "duplicate locked package: " + row["name"])
        result[identity] = row
    return result


def locked_edges(packages, row):
    result = set()
    references = row.get("dependencies", [])
    require(isinstance(references, list), "invalid locked dependencies")
    for reference in references:
        require(isinstance(reference, str), "invalid locked dependency reference")
        match = re.fullmatch(r"([^\s()]+)(?: ([^\s()]+))?(?: \(([^()]+)\))?", reference)
        require(match is not None, "invalid locked dependency reference: " + reference)
        name, version, source = match.groups()
        matches = {identity for identity in packages if identity[0] == name
                   and (version is None or identity[1] == version)
                   and (source is None or identity[2] == source)}
        require(len(matches) == 1, "ambiguous or absent locked dependency: " + reference)
        result.update(matches)
    return result


def relative_manifest(value):
    require(isinstance(value, str) and bool(value), "missing graph manifest")
    path = PurePosixPath(value)
    require(value == path.as_posix() and not path.is_absolute()
            and ".." not in path.parts and "\\" not in value and value != ".",
            "invalid graph manifest path")


def verify_graph(graph, lock, release_version, direct, *, registry):
    """Check a receipt graph against its own lock; inactive lock rows may be absent."""
    require(isinstance(graph, list) and bool(graph), "missing receipt graph")
    locked = locked_packages(lock)
    packages = {}
    for row in graph:
        identity = package_identity(row)
        require("source" in row and identity not in packages and identity in locked,
                "receipt graph has a duplicate or unlocked identity")
        packages[identity] = row
    app = ("ssmcp", release_version, None)
    require(app in packages, "receipt graph is missing the application")
    if not registry:
        require(packages[app].get("manifest") == "Cargo.toml",
                "development graph root manifest differs")
    require(all(identity == app or identity[2] == CRATES_IO for identity in packages)
            if registry else True, "registry graph contains a non-registry dependency")
    for identity, row in packages.items():
        features = row.get("features")
        require(isinstance(features, list) and all(isinstance(x, str) for x in features)
                and features == sorted(set(features)), "invalid graph features")
        edges = row.get("dependencies")
        require(isinstance(edges, list), "missing graph dependency edges")
        allowed = locked_edges(locked, locked[identity])
        for edge in edges:
            require(isinstance(edge, dict) and isinstance(edge.get("name"), str)
                    and bool(edge["name"]), "invalid graph dependency edge")
            target = edge.get("package")
            require(isinstance(target, list) and len(target) == 3,
                    "invalid graph dependency identity")
            dependency = package_identity(dict(zip(("name", "version", "source"), target)))
            require(dependency in packages and dependency in allowed,
                    "receipt graph has a dangling or unlocked dependency edge")
            kinds = edge.get("kinds")
            require(isinstance(kinds, list) and bool(kinds), "missing dependency kinds")
            for kind in kinds:
                require(isinstance(kind, dict) and set(kind) == {"kind", "target"}
                        and kind["kind"] in (None, "dev", "build")
                        and (kind["target"] is None or isinstance(kind["target"], str)),
                        "invalid dependency kind")
        if not registry:
            source = identity[2]
            if source is None or source.startswith("git+"):
                relative_manifest(row.get("manifest"))
            if source and source.startswith("git+"):
                revision = source.rpartition("#")[2]
                require(re.fullmatch(r"[0-9a-f]{40}", revision) is not None
                        and row.get("commit") == revision
                        and isinstance(row.get("tree"), str)
                        and re.fullmatch(r"[0-9a-f]{40}", row["tree"]) is not None,
                        "Git graph revision or tree is invalid")
    root_edges = {tuple(edge["package"]) for edge in packages[app]["dependencies"]}
    for name, version in direct.items():
        matches = {identity for identity in packages if identity[:2] == (name, version)}
        require(len(matches) == 1 and matches <= root_edges,
                "graph is missing a direct Project Z edge: " + name)
    harnesses = [identity for identity in packages if identity[0] == "zharness"]
    if harnesses:
        plugins = {identity for identity in packages if identity[0] == "zplugin"}
        require(len(plugins) == 1, "graph must resolve a single zplugin")
        for identity in [app, *harnesses]:
            selected = {tuple(edge["package"]) for edge in packages[identity]["dependencies"]
                        if edge["package"][0] == "zplugin"}
            require(selected == plugins, "ssmcp and zharness must share the same zplugin")
    return packages


def read_crate_members(crate, release_version):
    """Read regular members without extraction; reject ambiguous archive paths."""
    prefix = "ssmcp-" + release_version
    members = {}
    with tarfile.open(fileobj=io.BytesIO(crate), mode="r:gz") as archive:
        for entry in archive:
            if entry.name == prefix and entry.isdir():
                continue
            path = PurePosixPath(entry.name)
            require("\\" not in entry.name and entry.name == path.as_posix()
                    and len(path.parts) >= 2 and path.parts[0] == prefix
                    and ".." not in path.parts and (entry.isfile() or entry.isdir()),
                    "unsafe crate member: " + entry.name)
            if entry.isdir():
                continue
            name = path.relative_to(prefix).as_posix()
            require(name not in members, "duplicate crate member: " + name)
            stream = archive.extractfile(entry)
            require(stream is not None, "unreadable crate member: " + name)
            with stream:
                members[name] = stream.read()
    require({"Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json",
             "src/lib.rs"} <= members.keys(), "crate is missing source or package identity")
    require(not {".cargo/config", ".cargo/config.toml"} & members.keys(),
            "crate must not carry Cargo source configuration")
    return members


def verify_crate_contract(manifest, crate, candidate, release_version, candidate_tree,
                          candidate_manifest, candidate_lock):
    """Bind official crate bytes and both receipt graphs to immutable candidate inputs.

    This function performs no I/O or Cargo execution. Graph features, dependency
    kinds and transitive Git trees remain the exact formal workflow's metadata
    receipt; identities, lock edges, direct edges and declared gitlinks are checked here.
    """
    require(re.fullmatch(r"[0-9a-f]{40}", candidate) is not None
            and re.fullmatch(r"[0-9a-f]{40}", candidate_tree) is not None,
            "invalid candidate commit or tree")
    require(manifest.get("package") == "ssmcp" and manifest.get("version") == release_version
            and manifest.get("commit") == candidate, "release manifest identity differs")
    verified = manifest.get("verified_crate")
    require(isinstance(verified, dict), "missing verified_crate receipt")
    require(verified.get("name") == f"ssmcp-{release_version}.crate"
            and type(verified.get("bytes")) is int and verified["bytes"] == len(crate)
            and verified.get("sha256") == digest(crate), "official crate differs from verified bytes")
    source = verified.get("source")
    require(isinstance(source, dict), "missing verified source")
    require(source.get("commit") == candidate and source.get("tree") == candidate_tree
            and source.get("manifest_sha256") == digest(candidate_manifest),
            "verified source differs from candidate commit, tree or manifest")
    require(all(isinstance(source.get(key), str) and bool(source[key])
                for key in ("cargo", "rustc")), "missing recorded verification toolchain")
    development = tomllib.loads(candidate_manifest.decode())
    require(development["package"]["name"] == "ssmcp"
            and development["package"]["version"] == release_version,
            "candidate package identity differs")
    dev_lock = tomllib.loads(candidate_lock.decode())
    graph = manifest.get("dependency_graph")
    require(isinstance(graph, dict) and source.get("dependency_graph") == graph,
            "release and verified source dependency graphs differ")
    require(graph.get("cargo_lock_sha256") == digest(candidate_lock),
            "development graph is not bound to candidate Cargo.lock")
    direct = {}
    for name, package in PROJECT_Z_DIRECT.items():
        spec = development.get("dependencies", {}).get(name)
        if spec is None:
            continue
        require(isinstance(spec, dict) and spec.get("package", name) == package
                and spec.get("git") == f"https://github.com/LIghtJUNction/{PROJECT_Z_REPOS[package]}.git"
                and not set(spec) - {"package", "git", "version", "default-features", "features", "optional"}
                and isinstance(spec.get("version"), str) and spec["version"].startswith("="),
                "invalid candidate Project Z declaration: " + name)
        direct[package] = spec["version"][1:]
    dev_packages = verify_graph(graph.get("packages"), dev_lock, release_version,
                                direct, registry=False)
    modules = manifest.get("submodules")
    require(isinstance(modules, list), "missing submodule receipts")
    modules_by_path = {row["path"]: row for row in modules}
    require(len(modules_by_path) == len(modules), "duplicate submodule receipt")
    internal = {}
    for identity in locked_packages(dev_lock):
        name, _, src = identity
        if name not in PROJECT_Z_REPOS:
            continue
        canonical = f"git+https://github.com/LIghtJUNction/{PROJECT_Z_REPOS[name]}.git#"
        require(isinstance(src, str) and src.startswith(canonical)
                and re.fullmatch(r"[0-9a-f]{40}", src[len(canonical):]) is not None
                and name not in internal, "Project Z lock has split or noncanonical sources")
        internal[name] = identity
    require(not any(row["name"] in PROJECT_Z_REPOS
                    for row in dev_lock.get("patch", {}).get("unused", [])),
            "development lock has an unused Project Z override")
    actual_internal = {identity[0]: identity for identity in dev_packages
                       if identity[0] in PROJECT_Z_REPOS}
    require(internal == actual_internal, "Project Z receipt graph differs from development lock")
    patches = development.get("patch", {}).get("crates-io", {})
    required = set(direct) | (set(PROJECT_Z_REPOS) & patches.keys())
    require(required <= internal.keys(), "development graph is missing a declared Project Z source")
    for name, identity in internal.items():
        row = dev_packages[identity]
        module = modules_by_path.get("crates/" + PROJECT_Z_REPOS[name])
        require(module is not None if name in direct else True,
                "direct Project Z source has no gitlink")
        if module is not None:
            require((row["commit"], row["tree"]) == (module["commit"], module["tree"]),
                    "Project Z graph differs from declared gitlink")

    members = read_crate_members(crate, release_version)
    require(members["Cargo.toml.orig"] == candidate_manifest,
            "crate Cargo.toml.orig differs from candidate manifest")
    vcs = json.loads(members[".cargo_vcs_info.json"])
    require(vcs.get("git", {}).get("sha1") == candidate
            and vcs["git"].get("dirty", False) is False and vcs.get("path_in_vcs") == "",
            "crate VCS identity differs from candidate")
    normalized_manifest = tomllib.loads(members["Cargo.toml"].decode())
    require(normalized_manifest.get("package", {}).get("name") == "ssmcp"
            and normalized_manifest["package"].get("version") == release_version
            and not {"patch", "replace", "workspace"} & normalized_manifest.keys(),
            "invalid normalized manifest")
    for name, spec in dependency_tables(normalized_manifest):
        require(isinstance(spec, dict) and isinstance(spec.get("version"), str)
                and not {"git", "path", "rev", "tag", "branch", "registry",
                         "registry-index", "workspace"} & spec.keys(),
                "normalized dependency is not a crates.io version: " + name)
    for name, package in PROJECT_Z_DIRECT.items():
        if package not in direct:
            continue
        original = development["dependencies"][name]
        normalized = normalized_manifest.get("dependencies", {}).get(name, {})
        require(isinstance(normalized, dict) and normalized.get("package", name) == package
                and normalized.get("version") == original["version"]
                and normalized.get("default-features", True) == original.get("default-features", True)
                and sorted(normalized.get("features", [])) == sorted(original.get("features", [])),
                "normalized Project Z package, version or features differ: " + name)
    registry_lock = tomllib.loads(members["Cargo.lock"].decode())
    registry_packages = locked_packages(registry_lock)
    app = ("ssmcp", release_version, None)
    require(app in registry_packages and "patch" not in registry_lock,
            "invalid normalized registry lock")
    for identity, row in registry_packages.items():
        require(identity == app or identity[2] == CRATES_IO
                and isinstance(row.get("checksum"), str)
                and re.fullmatch(r"[0-9a-f]{64}", row["checksum"]) is not None,
                "normalized lock contains a non-registry dependency or invalid checksum")
    require(all((name, version, CRATES_IO) in registry_packages
                for name, version in direct.items()), "normalized lock lacks direct Project Z versions")
    normalized = {"manifest_sha256": digest(members["Cargo.toml"]),
                  "lock_sha256": digest(members["Cargo.lock"]),
                  "packages": registry_lock["package"], "direct_project_z": direct}
    require(verified.get("normalized") == normalized,
            "normalized manifest or lock differs from verified receipt")
    registry_graph = verified.get("registry_graph")
    registry_nodes = verify_graph(registry_graph, registry_lock, release_version,
                                  direct, registry=True)
    binding = {"receipt_schema_source": "5d087d1f7363fe5741f6080168037623362280e7",
               "source_commit": candidate, "source_tree": candidate_tree,
               "source_manifest_sha256": digest(candidate_manifest),
               "development_lock_sha256": digest(candidate_lock),
               "normalized_manifest_sha256": normalized["manifest_sha256"],
               "normalized_lock_sha256": normalized["lock_sha256"],
               "verified_crate_sha256": verified["sha256"], "verified_crate_bytes": len(crate),
               "development_graph_packages": len(dev_packages),
               "registry_graph_packages": len(registry_nodes),
               "registry_graph_sha256": digest(json.dumps(registry_graph, sort_keys=True,
                                                          separators=(",", ":")).encode())}
    return members, binding


def parse_arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", help="Exact 40-character lowercase candidate commit SHA")
    parser.add_argument("run_id", type=int, help="Exact original manual-main release workflow run ID")
    parser.add_argument("--workflow-sha", required=True,
                        help="Exact main commit P that supplied this run's workflow definitions")
    parser.add_argument("--checkout", type=Path, required=True,
                        help="Existing Git working-tree root containing the candidate and submodule objects")
    parser.add_argument("--report-dir", type=Path, required=True,
                        help="Report directory; only its final component may be created")
    parser.add_argument("--dispatch-receipt", type=Path, required=True,
                        help="Original gh api --include dispatch capture bytes")
    parser.add_argument("--dispatch-receipt-sha256", required=True,
                        help="Independently recorded SHA-256 of the original dispatch capture")
    parser.add_argument("--native-execution-receipt", type=Path, required=True,
                        help="Original native execution receipt independently read by the reviewer")
    parser.add_argument("--native-execution-receipt-sha256", required=True,
                        help="Independently recorded SHA-256 of that original native receipt")
    parser.add_argument("--native-execution-job-id", required=True,
                        help="Expected native job ID from independent execution readback")
    parser.add_argument("--native-capture-path", required=True,
                        help="Exact remote capture path from independent execution readback, not the capture")
    args = parser.parse_args(argv)
    if re.fullmatch(r"[0-9a-f]{40}", args.candidate) is None:
        parser.error("candidate must be an exact 40-character lowercase commit SHA")
    if re.fullmatch(r"[0-9a-f]{40}", args.workflow_sha) is None:
        parser.error("--workflow-sha must be an exact 40-character lowercase commit SHA")
    if args.run_id <= 0:
        parser.error("run_id must be a positive integer")
    for name in ("dispatch_receipt_sha256", "native_execution_receipt_sha256"):
        if re.fullmatch(r"[0-9a-f]{64}", getattr(args, name)) is None:
            parser.error("--" + name.replace("_", "-") + " must be an exact lowercase SHA-256")
    if (not args.native_execution_job_id or len(args.native_execution_job_id) > 256
            or re.search(r"[\s\x00-\x1f\x7f]", args.native_execution_job_id)):
        parser.error("--native-execution-job-id must be an explicit single identifier")
    if (not PurePosixPath(args.native_capture_path).is_absolute()
            or ".." in PurePosixPath(args.native_capture_path).parts
            or re.search(r"[\x00-\x1f\x7f]", args.native_capture_path)):
        parser.error("--native-capture-path must be an explicit absolute remote path")
    try:
        checkout = args.checkout.expanduser().resolve(strict=True)
        if not checkout.is_dir():
            parser.error("--checkout must be an existing Git working-tree root")
        top = subprocess.check_output(
            ["git", "--no-replace-objects", "rev-parse", "--show-toplevel"], cwd=checkout, text=True,
            stderr=subprocess.PIPE, timeout=15).strip()
        if Path(top).resolve(strict=True) != checkout:
            parser.error("--checkout must name the Git working-tree root, not a subdirectory")
        for name, commit in (("candidate", args.candidate), ("workflow", args.workflow_sha)):
            if git_read(checkout, "cat-file", "-t", commit).strip() != b"commit":
                parser.error(name + " must identify a commit object in --checkout")
        for name in ("dispatch_receipt", "native_execution_receipt"):
            path = getattr(args, name).expanduser().resolve(strict=True)
            if not path.is_file():
                parser.error("--" + name.replace("_", "-") + " must be an existing file")
            setattr(args, name, path)
        report_dir = args.report_dir.expanduser().resolve()
        report_dir.mkdir(exist_ok=True)
        if not report_dir.is_dir():
            parser.error("--report-dir must be a directory")
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        parser.error("invalid local verification paths: " + str(error))
    args.checkout = checkout
    args.report_dir = report_dir
    return args


def main(argv=None):
    require(__debug__, "Python assertions must remain enabled; do not use -O or PYTHONOPTIMIZE")
    args = parse_arguments(argv)
    candidate, run_id = args.candidate, args.run_id
    workflow_sha = args.workflow_sha
    repository = "LIghtJUNction/ssmcp"
    version = "0.10.25"
    tag = "v" + version
    checkout = args.checkout
    report_path = args.report_dir / ("verification-" + candidate[:12] + ".json")
    report = {"schema_version": 2, "status": "checking", "tag": tag, "candidate_sha": candidate,
              "workflow_sha": workflow_sha, "release_run_id": run_id, "run_attempt": 1,
              "route": "manual-main", "provenance_verified": False,
              "started_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}


    def source(path):
        return git_read(checkout, "show", candidate + ":" + path)

    try:
        report["reviewed_recipe"] = verify_reviewed_recipe(checkout, candidate, workflow_sha)
        dispatch_bytes, report["dispatch_receipt"] = read_original_evidence(
            args.dispatch_receipt, args.dispatch_receipt_sha256, "dispatch receipt")
        native_bytes, report["native_execution_receipt"] = read_original_evidence(
            args.native_execution_receipt, args.native_execution_receipt_sha256,
            "native execution receipt")
        # Both hashes and the expected job/path come from independent original
        # execution readback. Self-authored JSON does not establish authenticity.
        dispatch = parse_dispatch_capture(
            dispatch_bytes, expected_capture_sha256=args.dispatch_receipt_sha256,
            native_execution_receipt_bytes=native_bytes,
            expected_native_execution_receipt_sha256=args.native_execution_receipt_sha256,
            expected_native_execution_binding={"native_job_id": args.native_execution_job_id,
                                               "capture_path": args.native_capture_path})
        report["native_execution_binding"] = {"native_job_id": args.native_execution_job_id,
                                               "capture_path_verified_but_redacted": True}
        run = api(f"repos/{repository}/actions/runs/{run_id}")
        assert run["head_sha"] == workflow_sha and run["path"] == ".github/workflows/release.yml"
        assert run["event"] == "workflow_dispatch" and run["head_branch"] == "main"
        assert run["id"] == run_id and type(run["run_attempt"]) is int and run["run_attempt"] == 1
        assert run["status"] == "completed" and run["conclusion"] == "success"
        assert run["repository"]["full_name"] == repository
        assert run["url"] == f"https://api.github.com/repos/{repository}/actions/runs/{run_id}"
        assert run["html_url"] == f"https://github.com/{repository}/actions/runs/{run_id}"
        attempt = api(f"repos/{repository}/actions/runs/{run_id}/attempts/1")
        for key in ("id", "event", "head_branch", "head_sha", "path", "workflow_id",
                    "run_attempt", "status", "conclusion"):
            require(attempt.get(key) == run.get(key), "run/attempt identity differs: " + key)
        require(attempt.get("repository", {}).get("full_name") == repository,
                "attempt repository identity differs")
        references = verify_referenced_workflows(run, repository, workflow_sha)
        attempt_references = verify_referenced_workflows(attempt, repository, workflow_sha)
        require(sorted(references, key=lambda r: r["path"])
                == sorted(attempt_references, key=lambda r: r["path"]),
                "run/attempt reusable workflow references differ")
        workflow = api(f"repos/{repository}/actions/workflows/{run['workflow_id']}")
        require(workflow.get("id") == run["workflow_id"]
                and workflow.get("path") == ".github/workflows/release.yml",
                "release workflow ID/path readback differs")
        report["workflow_origin"] = {
            "basis": "independent run/attempt API head_sha and reviewed expected P",
            "workflow_sha": workflow_sha, "workflow_id": run["workflow_id"],
            "direct_github_workflow_sha_context_observed": False,
            "referenced_workflows": references,
            "references_are_not_execution_evidence": True}
        report["dispatch_binding"] = verify_dispatch_contract(
            dispatch, run, repository, run_id, tag, workflow_sha)
        page = api(f"repos/{repository}/actions/runs/{run_id}/attempts/1/jobs?per_page=100")
        jobs = page["jobs"]
        assert page["total_count"] == len(jobs) == 19
        assert all(job["run_id"] == run_id and type(job["run_attempt"]) is int
                   and job["run_attempt"] == 1 and job["head_sha"] == workflow_sha
                   and job["status"] == "completed" for job in jobs)
        targets = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-apple-darwin",
                   "aarch64-apple-darwin", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
        required = {"validate", "finalize", "publish_release", "publish_crates"}
        required.update("Build " + target for target in targets)
        required.update("Verify installed server " + target for target in targets[:2])
        required.update("checks / linux (" + image + ")" for image in ["ubuntu-24.04", "ubuntu-24.04-arm"])
        required.update("checks / Client (" + image + ")" for image in
                        ["macos-15", "macos-15-intel", "windows-2025", "windows-11-arm"])
        by_name = {job["name"]: job for job in jobs}
        assert set(by_name) == required | {"deploy_server"}
        assert all(by_name[name]["conclusion"] == "success" for name in required)
        assert by_name["deploy_server"]["conclusion"] == "skipped"
        report["jobs"] = [{"id": job["id"], "name": job["name"], "conclusion": job["conclusion"]} for job in jobs]
        reference = api(f"repos/{repository}/git/ref/tags/{tag}")
        target = reference["object"]
        if target["type"] == "tag":
            target = api(f"repos/{repository}/git/tags/{target['sha']}")["object"]
        assert target["type"] == "commit" and target["sha"] == candidate

        release = api(f"repos/{repository}/releases/tags/{tag}")
        assert release["tag_name"] == tag and not release["draft"] and not release["prerelease"]
        assets = {asset["name"]: asset for asset in release["assets"]}
        archives = {"ssmcp-" + target + (".zip" if "windows" in target else ".tar.gz") for target in targets}
        expected = archives | {name + ".sha256" for name in archives} | {"install.sh", "install.ps1", "SHA256SUMS", "release-manifest.json"}
        assert set(assets) == expected and len(release["assets"]) == 16
        assert all(asset["state"] == "uploaded" for asset in assets.values())

        def read_asset(name):
            data = download(assets[name]["browser_download_url"])
            assert len(data) == assets[name]["size"], name + " size differs"
            assert assets[name].get("digest") == "sha256:" + digest(data), name + " digest differs"
            return name, data

        small_names = {"release-manifest.json", "SHA256SUMS"} | {name + ".sha256" for name in archives}
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            small = dict(pool.map(read_asset, sorted(small_names)))
        manifest = json.loads(small["release-manifest.json"])
        assert manifest["package"] == "ssmcp" and manifest["version"] == version and manifest["commit"] == candidate
        records = manifest["artifacts"]
        assert len(records) == 8 and {record["name"] for record in records} == archives | {"install.sh", "install.ps1"}
        for record in records:
            asset = assets[record["name"]]
            assert asset["size"] == record["bytes"]
            assert asset.get("digest") == "sha256:" + record["sha256"]
            if record["name"] in archives:
                # package() uses native Path.write_text; Windows emits CRLF.
                newline = "\r\n" if record["name"].endswith("-pc-windows-msvc.zip") else "\n"
                expected_sidecar = f"{record['sha256']}  {record['name']}{newline}".encode()
                assert small[record["name"] + ".sha256"] == expected_sidecar
            else:
                assert digest(source("scripts/" + record["name"])) == record["sha256"]
        assert small["SHA256SUMS"].decode() == "".join(f"{r['sha256']}  {r['name']}\n" for r in records)
        pins = {}
        tree = git_read(checkout, "ls-tree", "-r", candidate).decode()
        for line in tree.splitlines():
            mode, kind, commit_and_path = line.split(" ", 2)
            if mode == "160000":
                commit, path = commit_and_path.split("\t", 1)
                assert kind == "commit"
                pins[path] = commit
        assert len(pins) == 4
        assert {item["path"]: item["commit"] for item in manifest["submodules"]} == pins
        for item in manifest["submodules"]:
            path = checkout / item["path"]
            actual_tree = git_read(path, "rev-parse", item["commit"] + "^{tree}").decode().strip()
            assert item["tree"] == actual_tree
        report["github_release"] = {"id": release["id"], "url": release["html_url"], "published_at": release["published_at"],
            "asset_count": len(assets), "manifest_sha256": digest(small["release-manifest.json"]),
            "assets": [{"name": a["name"], "size": a["size"], "digest": a["digest"]} for a in assets.values()]}
        report["manifest"] = manifest

        registry_url = f"https://crates.io/api/v1/crates/ssmcp/{version}"
        registry = json.loads(download(registry_url))["version"]
        assert registry["num"] == version and not registry["yanked"]
        crate_url = f"https://static.crates.io/crates/ssmcp/ssmcp-{version}.crate"
        crate = download(crate_url)
        assert digest(crate) == registry["checksum"]
        candidate_tree = git_read(checkout, "rev-parse", candidate + "^{tree}").decode().strip()
        _, crate_binding = verify_crate_contract(
            manifest, crate, candidate, version, candidate_tree, source("Cargo.toml"), source("Cargo.lock"))
        report["verified_crate_contract"] = crate_binding
        prefix = "ssmcp-" + version + "/"
        with tarfile.open(fileobj=io.BytesIO(crate), mode="r:gz") as archive:
            def member(name):
                stream = archive.extractfile(prefix + name)
                assert stream is not None
                return stream.read()
            vcs = json.loads(member(".cargo_vcs_info.json"))
            assert vcs["git"]["sha1"] == candidate and not vcs["git"].get("dirty", False)
            package = tomllib.loads(member("Cargo.toml").decode())["package"]
            assert package["name"] == "ssmcp" and package["version"] == version
            source_archive = git_read(checkout, "archive", "--format=tar", candidate, "src")
            expected_sources = {}
            with tarfile.open(fileobj=io.BytesIO(source_archive), mode="r:") as source_tar:
                for entry in source_tar.getmembers():
                    if entry.isfile():
                        assert entry.name.startswith("src/")
                        expected_sources[entry.name] = source_tar.extractfile(entry).read()
            assert expected_sources, "candidate contains no product source"
            actual_sources = {entry.name[len(prefix):] for entry in archive.getmembers()
                              if entry.isfile() and entry.name.startswith(prefix + "src/")}
            assert actual_sources == set(expected_sources), "published product source file set differs"
            source_hashes = {}
            for path, expected_bytes in sorted(expected_sources.items()):
                assert member(path) == expected_bytes, "published source differs: " + path
                source_hashes[path] = digest(expected_bytes)
        report["crate"] = {"registry_url": registry_url, "archive_url": crate_url, "version": version,
            "bytes": len(crate), "sha256": digest(crate), "yanked": registry["yanked"], "vcs": vcs,
            "matched_source_files_sha256": source_hashes}
        report["execution_binding"] = verify_execution(
            run, by_name, candidate, version, manifest, assets, crate,
            api_json=api, api_bytes=api_bytes, download=download,
            evidence_dir=args.report_dir / (f"evidence-{run_id}-attempt-1"))
        require(report["execution_binding"].get("verified") is True,
                "step and artifact provenance has not been verified")
        final_run = api(f"repos/{repository}/actions/runs/{run_id}")
        for key in ("id", "event", "head_branch", "head_sha", "path", "workflow_id",
                    "run_attempt", "status", "conclusion"):
            require(final_run.get(key) == run.get(key),
                    "run identity or attempt changed during verification: " + key)
        report["verification_scope"] = "Downloaded manifest, SHA256SUMS, six sidecars, six public binary archives, the official crate, 18 exact-attempt job logs and seven Actions artifact ZIPs. Each binary archive is byte-identical across the CI artifact and public asset; outer ZIP digests are independently checked. Installer bytes remain matched using GitHub asset digests and exact candidate source. Every regular src/** file in the official crate is byte-identical to the candidate, with an exact file-set match. The crate and verified receipt also match the finalize artifact; candidate identity, Git development lock/graph, Cargo.toml.orig, and the distinct normalized registry manifest/lock/graph are cross-checked. Cargo metadata feature selection and transitive Git trees remain bound to the successful reviewed formal workflow, not re-executed locally. Original dispatch capture, independent native execution readback, run/attempt, step windows and artifact IDs are required. This is an ordinary reviewed CI chain, not malicious-builder attestation. No binary was installed or production service changed."
        report["provenance_verified"] = True
        require(report["provenance_verified"] is True, "publication provenance remains unverified")
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = type(error).__name__ + ": " + str(error)
        raise
    finally:
        report["finished_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report_path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps({"status": report["status"], "path": str(report_path), "candidate_sha": candidate,
                          "release_url": report.get("github_release", {}).get("url"),
                          "crate_sha256": report.get("crate", {}).get("sha256"), "error": report.get("error")}))


if __name__ == "__main__":
    main()
