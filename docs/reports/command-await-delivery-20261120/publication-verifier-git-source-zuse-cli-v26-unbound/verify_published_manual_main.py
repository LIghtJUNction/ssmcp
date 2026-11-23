"""Verify the v0.10.26 CLI-lifecycle manual-main Git-source publication with crate deferral.

UNBOUND preparation: runtime S/P/run and original dispatch evidence have
not been bound. Missing or unsupported evidence fails closed. This program
never dispatches a workflow, creates a tag, publishes, or deploys.
Only the reviewed git-source-v1 contract is supported: no legacy, registry or
build-only mode, and no registry claim or registry package download.
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
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


# Source receipt policy is unchanged; the reviewed CLI recipe has three gitlinks.
RELEASE_CONTRACT = "git-source-v1"
RELEASE_VERSION = "0.10.26"
CRATE_PUBLICATION = {
    "status": "deferred",
    "reason": "Independent registry dependencies and the external package require separate verification.",
}
PROJECT_Z_DIRECT = {"zplugin": "zplugin", "zharness": "zharness", "ztuio": "ztuio"}
PROJECT_Z_PATCHES = {"zplugin", "zguard", "zcall", "zconv", "zmcpio", "zclap", "z64", "zsys"}
PROJECT_Z_REPOS = {name: name for name in PROJECT_Z_PATCHES | {"zharness", "ztuio"}}
PROJECT_Z_REPOS["zplugin-macros"] = "zplugin"


def require(condition, message):
    if not condition:
        raise ValueError(message)


# This is the reviewed recipe source, not a runtime product/workflow binding.
# Updating any pin requires a new source review; never learn pins from the run.
RECIPE_SOURCE_COMMIT = "4c6cd21b2ab46c11febbac7c1921e302da49f4b2"
REVIEWED_RECIPE = {
    ".github/workflows/release.yml": (22855, "d7f29c1ac896e247a1c7e00aa3f68e349cbe8fd81a45dd3c317e5c25fc26e677"),
    ".github/workflows/ci.yml": (14994, "e7c753ca1a058b43f798cb1f31bc2b8980a09b3bc72cdb1996ef6eaadb5a7172"),
    ".github/workflows/deploy-server.yml": (4499, "2b8accbeaa94a674a1e6ddea762567cd97b74a1293cc45a53c4e34032a1e4a4c"),
    "scripts/release.py": (65111, "a11208e6f6949d0af6893774046295b71aaa0fa8b65ca8a462f53b94db5a1855"),
    "scripts/install.sh": (6783, "25e5f2f8f93ca093b3851277fe436d3ba3a1451962f70ed957280f97f96c4906"),
    "scripts/install.ps1": (12928, "e2c0607275d6b04315e4ddce85d01280dcc56b2bb2710be0849fbaeee626d051"),
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
            "deferred_version": RELEASE_VERSION, "release_contract": RELEASE_CONTRACT,
            "crate_publication": dict(CRATE_PUBLICATION), "files": verified}


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
    for target, table in [(None, manifest), *manifest.get("target", {}).items()]:
        for key, kind in (("dependencies", "normal"), ("build-dependencies", "build"),
                          ("dev-dependencies", "dev")):
            for name, spec in table.get(key, {}).items():
                yield target, kind, name, {"version": spec} if isinstance(spec, str) else spec


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


def verify_graph(graph, lock, release_version, direct):
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
    require(packages[app].get("manifest") == "Cargo.toml",
            "Git-source graph root manifest differs")
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
        source = identity[2]
        if source is None or source.startswith("git+"):
            relative_manifest(row.get("manifest"))
            require(isinstance(row.get("manifest_sha256"), str)
                    and re.fullmatch(r"[0-9a-f]{64}", row["manifest_sha256"]) is not None,
                    "Git-source graph manifest digest is invalid")
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


def project_z_url(package):
    return f"https://github.com/LIghtJUNction/{PROJECT_Z_REPOS[package]}.git"


def require_project_z_git(name, spec, package):
    """Root declarations select the default Git branch, with no version selector."""
    allowed = {"package", "git", "default-features", "features", "optional"}
    require(isinstance(spec, dict) and spec.get("package", name) == package
            and spec.get("git") == project_z_url(package) and not set(spec) - allowed
            and all(type(spec.get(key, False)) is bool
                    for key in ("default-features", "optional"))
            and isinstance(spec.get("features", []), list)
            and all(isinstance(feature, str) for feature in spec.get("features", [])),
            "Project Z declaration is not a canonical unversioned Git source: " + name)


def manifest_package_version(manifest, workspace):
    value = manifest.get("package", {}).get("version")
    if value == {"workspace": True}:
        value = workspace.get("workspace", {}).get("package", {}).get("version")
    require(isinstance(value, str) and bool(value), "missing Git package version metadata")
    return value


def verify_source_gitlinks(checkout, candidate, modules):
    """Match three recorded gitlinks to S and read their immutable commit trees."""
    require(isinstance(modules, list) and len(modules) == 3,
            "Git-source publication requires three recorded submodules")
    recorded = {}
    for row in modules:
        require(isinstance(row, dict) and set(row) == {"path", "commit", "tree"},
                "invalid source submodule receipt")
        relative_manifest(row["path"])
        require(row["path"] not in recorded
                and all(isinstance(row[key], str)
                        and re.fullmatch(r"[0-9a-f]{40}", row[key]) is not None
                        for key in ("commit", "tree")),
                "duplicate or invalid source submodule identity")
        recorded[row["path"]] = row
    require(modules == sorted(modules, key=lambda row: row["path"]),
            "source submodule order differs from the reviewed receipt")
    pins = {}
    for entry in git_read(checkout, "ls-tree", "-r", "-z", candidate).split(b"\0"):
        if not entry:
            continue
        metadata, raw_path = entry.split(b"\t", 1)
        mode, kind, commit = metadata.split(b" ", 2)
        if mode == b"160000":
            path = raw_path.decode("utf-8")
            relative_manifest(path)
            require(kind == b"commit" and path not in pins, "invalid source gitlink")
            pins[path] = commit.decode("ascii")
    require(len(pins) == 3 and {path: row["commit"] for path, row in recorded.items()} == pins,
            "manifest submodules differ from the candidate's three gitlinks")
    for path, row in recorded.items():
        module = checkout / path
        require(module.is_dir() and not module.is_symlink()
                and module.resolve().is_relative_to(checkout),
                "submodule object checkout is missing or escapes the source checkout: " + path)
        top = git_read(module, "rev-parse", "--show-toplevel").decode().strip()
        require(Path(top).resolve() == module.resolve(),
                "submodule object checkout is not an independent Git root: " + path)
        require(git_read(module, "cat-file", "-t", row["commit"]).strip() == b"commit",
                "recorded submodule commit object is missing: " + path)
        actual_tree = git_read(module, "rev-parse", row["commit"] + "^{tree}").decode().strip()
        require(row["tree"] == actual_tree, "recorded submodule tree differs: " + path)
    return recorded


def verify_source_contract(manifest, candidate, release_version, checkout):
    """Bind schema-2 Git-source receipts to S without a registry or Cargo invocation.

    Cargo metadata features/kinds and Git trees without a recorded gitlink remain
    formal-workflow receipt evidence. All package identities and graph edges are
    checked against S's lock. Local and gitlink-backed manifests are also read
    from immutable Git objects, never from a mutable package checkout.
    """
    expected_fields = {
        "schema_version", "package", "version", "commit", "tree",
        "source_manifest_sha256", "cargo_lock_sha256", "toolchain", "submodules",
        "dependency_graph", "crate_publication", "verified_crate", "artifacts",
    }
    require(release_version == RELEASE_VERSION
            and isinstance(manifest, dict) and set(manifest) == expected_fields
            and type(manifest["schema_version"]) is int and manifest["schema_version"] == 2,
            "only the reviewed v0.10.26 schema-2 Git-source manifest is supported")
    require(manifest["crate_publication"] == CRATE_PUBLICATION
            and manifest["verified_crate"] is None,
            "v0.10.26 must defer crate publication; registry, legacy and build-only receipts are unsupported")
    candidate_tree = git_read(checkout, "rev-parse", candidate + "^{tree}").decode().strip()
    candidate_manifest = git_read(checkout, "show", candidate + ":Cargo.toml")
    candidate_lock = git_read(checkout, "show", candidate + ":Cargo.lock")
    require(re.fullmatch(r"[0-9a-f]{40}", candidate_tree) is not None
            and manifest["package"] == "ssmcp" and manifest["version"] == release_version
            and manifest["commit"] == candidate and manifest["tree"] == candidate_tree
            and manifest["source_manifest_sha256"] == digest(candidate_manifest)
            and manifest["cargo_lock_sha256"] == digest(candidate_lock),
            "Git-source identity differs from S, its tree, Cargo.toml or Cargo.lock")
    toolchain = manifest["toolchain"]
    require(isinstance(toolchain, dict) and set(toolchain) == {"cargo", "rustc"}
            and all(isinstance(value, str) and bool(value) for value in toolchain.values()),
            "missing recorded source-verification toolchain")
    development = tomllib.loads(candidate_manifest.decode("utf-8"))
    require(development.get("package", {}).get("name") == "ssmcp"
            and development["package"].get("version") == release_version,
            "candidate package identity differs")
    lock = tomllib.loads(candidate_lock.decode("utf-8"))
    locked = locked_packages(lock)
    app = ("ssmcp", release_version, None)
    require({identity for identity in locked if identity[0] == "ssmcp"} == {app},
            "Cargo.lock must identify exactly the candidate application")
    modules = verify_source_gitlinks(checkout, candidate, manifest["submodules"])
    internal = {}
    for identity in locked:
        name, _, source = identity
        if name not in PROJECT_Z_REPOS:
            continue
        canonical = "git+" + project_z_url(name) + "#"
        require(isinstance(source, str) and source.startswith(canonical)
                and re.fullmatch(r"[0-9a-f]{40}", source[len(canonical):]) is not None
                and name not in internal,
                "Project Z lock has split or noncanonical sources: " + name)
        internal[name] = identity
    require(not any(row["name"] in PROJECT_Z_REPOS
                    for row in lock.get("patch", {}).get("unused", [])),
            "candidate lock has an unused Project Z override")
    require(not development.get("replace"), "[replace] bypasses the Git source policy")
    for target, kind, name, spec in dependency_tables(development):
        require(isinstance(spec, dict), "invalid candidate dependency declaration: " + name)
        package = spec.get("package", name)
        if name in PROJECT_Z_DIRECT or package in PROJECT_Z_REPOS:
            require(target is None and kind == "normal" and PROJECT_Z_DIRECT.get(name) == package,
                    "unexpected Project Z dependency declaration: " + name)
    patches = development.get("patch", {}).get("crates-io", {})
    for patched_source, entries in development.get("patch", {}).items():
        for name, spec in entries.items():
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if name in PROJECT_Z_REPOS or package in PROJECT_Z_REPOS:
                require(patched_source == "crates-io" and name in PROJECT_Z_PATCHES and package == name,
                        "unexpected Project Z patch: " + name)
                require_project_z_git(name, spec, package)
                require(not set(spec) - {"git", "package"},
                        "Project Z patch must select only its Git source: " + name)

    # Version metadata comes from the pinned Git packages, never a root selector.
    direct = {}
    root_manifests = {}
    direct_records = {}
    for name, package in PROJECT_Z_DIRECT.items():
        spec = development.get("dependencies", {}).get(name)
        if spec is None:
            continue
        require_project_z_git(name, spec, package)
        path = "crates/" + PROJECT_Z_REPOS[package]
        module = modules.get(path)
        identity = internal.get(package)
        require(module is not None and identity is not None
                and identity[2].rpartition("#")[2] == module["commit"],
                "direct Project Z dependency differs from its locked gitlink: " + name)
        raw = git_read(checkout / path, "show", module["commit"] + ":Cargo.toml")
        data = tomllib.loads(raw.decode("utf-8"))
        require(data.get("package", {}).get("name") == package
                and manifest_package_version(data, data) == identity[1],
                "direct Project Z Git package version differs from Cargo.lock: " + name)
        if name == "ztuio":
            require(data.get("lib", {}).get("name", name) == name,
                    "ztuio must expose the ztuio library name")
        root_manifests[path] = (raw, data)
        direct[package] = identity[1]
        direct_records[package] = {
            "version": identity[1], "source": identity[2],
            "gitlink": path, "commit": module["commit"], "tree": module["tree"],
            "manifest_sha256": digest(raw),
        }

    graph = manifest["dependency_graph"]
    require(isinstance(graph, dict) and set(graph) == {"cargo_lock_sha256", "packages"}
            and graph["cargo_lock_sha256"] == digest(candidate_lock),
            "dependency graph is not bound to S's Cargo.lock")
    packages = verify_graph(graph["packages"], lock, release_version, direct)
    actual_internal = {identity[0]: identity for identity in packages
                       if identity[0] in PROJECT_Z_REPOS}
    require(internal == actual_internal, "Project Z graph differs from the candidate lock")
    required = set(direct) | (PROJECT_Z_PATCHES & patches.keys())
    require(required <= internal.keys(), "graph is missing a declared Project Z source")
    checked_manifests = []
    for identity, row in packages.items():
        name, resolved_version, source = identity
        raw = None
        workspace = None
        origin = None
        if source is None:
            raw = git_read(checkout, "show", candidate + ":" + row["manifest"])
            workspace = development
            origin = "candidate"
        elif name in internal:
            path = "crates/" + PROJECT_Z_REPOS[name]
            module = modules.get(path)
            require(module is not None if name in PROJECT_Z_DIRECT.values() else True,
                    "direct Project Z graph package has no gitlink: " + name)
            if module is not None:
                require((row["commit"], row["tree"]) == (module["commit"], module["tree"]),
                        "Project Z graph differs from the declared gitlink: " + name)
                if path not in root_manifests:
                    root_raw = git_read(checkout / path, "show", module["commit"] + ":Cargo.toml")
                    root_manifests[path] = (root_raw, tomllib.loads(root_raw.decode("utf-8")))
                root_raw, workspace = root_manifests[path]
                raw = (root_raw if row["manifest"] == "Cargo.toml"
                       else git_read(checkout / path, "show",
                                     module["commit"] + ":" + row["manifest"]))
                origin = path
        if raw is not None:
            data = tomllib.loads(raw.decode("utf-8"))
            require(digest(raw) == row["manifest_sha256"]
                    and data.get("package", {}).get("name") == name
                    and manifest_package_version(data, workspace) == resolved_version,
                    "graph manifest differs from the immutable Git object: " + name)
            checked_manifests.append({"name": name, "source_object": origin,
                                      "manifest": row["manifest"], "sha256": digest(raw)})
    return {
        "contract": RELEASE_CONTRACT, "receipt_schema_source": RECIPE_SOURCE_COMMIT,
        "source_commit": candidate, "source_tree": candidate_tree,
        "source_manifest_sha256": digest(candidate_manifest),
        "cargo_lock_sha256": digest(candidate_lock),
        "recorded_toolchain": toolchain, "gitlink_count": len(modules),
        "submodules": manifest["submodules"], "direct_project_z": direct_records,
        "dependency_graph_packages": len(packages),
        "dependency_graph_sha256": digest(json.dumps(graph, sort_keys=True,
                                                     separators=(",", ":")).encode()),
        "immutable_manifests_checked": checked_manifests,
        "crate_publication": dict(CRATE_PUBLICATION),
        "registry_verified": False,
        "metadata_scope": (
            "Package identities and dependency edges are checked against S's lock; "
            "candidate and gitlink-backed manifest bytes are checked against Git objects. "
            "Feature/kind selection and Git trees without a recorded gitlink rely on the "
            "reviewed formal workflow receipt and are not recomputed locally."
        ),
    }


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
    version = RELEASE_VERSION
    tag = "v" + version
    checkout = args.checkout
    report_path = args.report_dir / ("verification-" + candidate[:12] + ".json")
    report = {"schema_version": 2, "status": "checking", "tag": tag, "candidate_sha": candidate,
              "workflow_sha": workflow_sha, "release_run_id": run_id, "run_attempt": 1,
              "route": "manual-main", "release_contract": RELEASE_CONTRACT,
              "crate_publication": dict(CRATE_PUBLICATION),
              "registry_verified": False, "provenance_verified": False,
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
        required = {"validate", "finalize", "publish_release"}
        required.update("Build " + target for target in targets)
        required.update("Verify installed server " + target for target in targets[:2])
        required.update("checks / linux (" + image + ")" for image in ["ubuntu-24.04", "ubuntu-24.04-arm"])
        required.update("checks / Client (" + image + ")" for image in
                        ["macos-15", "macos-15-intel", "windows-2025", "windows-11-arm"])
        by_name = {job["name"]: job for job in jobs}
        skipped = {"publish_crates", "deploy_server"}
        assert len(required) == 17 and set(by_name) == required | skipped
        assert all(by_name[name]["conclusion"] == "success" for name in required)
        assert all(by_name[name]["conclusion"] == "skipped" for name in skipped)
        report["jobs"] = [{"id": job["id"], "name": job["name"], "conclusion": job["conclusion"]} for job in jobs]
        report["jobs_summary"] = {"total": len(jobs), "success": len(required),
                                  "skipped": sorted(skipped)}
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

        document_names = ({"release-manifest.json", "SHA256SUMS", "install.sh", "install.ps1"}
                          | {name + ".sha256" for name in archives})
        require(len(document_names) == 10, "public document set differs from E8")
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            publication_documents = dict(pool.map(read_asset, sorted(document_names)))
        manifest = json.loads(publication_documents["release-manifest.json"])
        report["verified_source_contract"] = verify_source_contract(
            manifest, candidate, version, checkout)
        records = manifest["artifacts"]
        require(isinstance(records, list) and len(records) == 8
                and all(isinstance(record, dict) and set(record) == {"name", "sha256", "bytes"}
                        and isinstance(record["name"], str)
                        and isinstance(record["sha256"], str)
                        and re.fullmatch(r"[0-9a-f]{64}", record["sha256"]) is not None
                        and type(record["bytes"]) is int and record["bytes"] > 0
                        for record in records), "invalid release artifact records")
        require([record["name"] for record in records]
                == sorted(archives) + ["install.ps1", "install.sh"],
                "manifest artifact set/order differs from the reviewed eight-record contract")
        for record in records:
            asset = assets[record["name"]]
            assert asset["size"] == record["bytes"]
            assert asset.get("digest") == "sha256:" + record["sha256"]
            if record["name"] in archives:
                # package() uses native Path.write_text; Windows emits CRLF.
                newline = "\r\n" if record["name"].endswith("-pc-windows-msvc.zip") else "\n"
                expected_sidecar = f"{record['sha256']}  {record['name']}{newline}".encode()
                assert publication_documents[record["name"] + ".sha256"] == expected_sidecar
            else:
                candidate_installer = source("scripts/" + record["name"])
                require(publication_documents[record["name"]] == candidate_installer
                        and digest(candidate_installer) == record["sha256"],
                        "public installer bytes differ from the candidate: " + record["name"])
        assert publication_documents["SHA256SUMS"] == "".join(
            f"{r['sha256']}  {r['name']}\n" for r in records).encode("utf-8")
        report["github_release"] = {"id": release["id"], "url": release["html_url"], "published_at": release["published_at"],
            "asset_count": len(assets), "manifest_sha256": digest(publication_documents["release-manifest.json"]),
            "assets": [{"name": a["name"], "size": a["size"], "digest": a["digest"]} for a in assets.values()]}
        report["publication_documents"] = {
            name: {"bytes": len(data), "sha256": digest(data)}
            for name, data in sorted(publication_documents.items())}
        report["manifest"] = manifest

        # H2 deliberately does not fetch or validate a registry crate. The exact
        # deferred policy and null verified_crate were checked in the source contract.
        report["execution_binding"] = verify_execution(
            run, by_name, candidate, version, manifest, assets, publication_documents,
            api_json=api, api_bytes=api_bytes, download=download,
            evidence_dir=args.report_dir / (f"evidence-{run_id}-attempt-1"))
        require(report["execution_binding"].get("verified") is True,
                "step and artifact provenance has not been verified")
        final_run = api(f"repos/{repository}/actions/runs/{run_id}")
        for key in ("id", "event", "head_branch", "head_sha", "path", "workflow_id",
                    "run_attempt", "status", "conclusion"):
            require(final_run.get(key) == run.get(key),
                    "run identity or attempt changed during verification: " + key)
        report["verification_scope"] = (
            "Downloaded all 16 public assets: manifest, SHA256SUMS, six sidecars, two installers, "
            "and six binary archives. Public documents and archives are byte-bound to the "
            "finalized-release-assets Actions ZIP; each binary archive also matches its "
            "platform build artifact. Outer ZIP digests are checked separately from member hashes. "
            "Installer bytes equal the immutable S scripts. The verified-source artifact is "
            "matched to the schema-2 manifest, candidate commit/tree, Cargo.toml, Cargo.lock, "
            "three gitlinks and dependency graph. Direct Project Z declarations use canonical "
            "Git URLs with no version/rev/tag/branch/path selector; locked versions and "
            "gitlink-backed package manifests are independently cross-checked. Graph identities "
            "and edges are checked against S's lock; feature/kind selection and Git trees without "
            "a recorded gitlink remain successful reviewed-workflow receipt evidence. "
            "The original dispatch capture and independent native readback bind P, S and the "
            "single attempt. All 19 jobs are checked: 17 successful jobs have their initial "
            "checkout/log windows verified, while publish_crates and deploy_server are skipped. "
            "Only the reviewed git-source-v1 publication provenance artifacts are required; "
            "auxiliary CI artifacts do not change that exact subset. Crate publication is "
            "deferred: no registry metadata, crate bytes, registry graph or registry availability "
            "was downloaded or verified. Legacy and build-only modes are unsupported. This is "
            "an ordinary reviewed CI chain, not malicious-builder attestation. No binary was "
            "installed or production service changed."
        )
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
                          "release_contract": RELEASE_CONTRACT,
                          "crate_publication": report["crate_publication"]["status"],
                          "registry_verified": False, "error": report.get("error")}))


if __name__ == "__main__":
    main()
