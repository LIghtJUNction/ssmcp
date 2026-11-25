#!/usr/bin/env python3
"""Validate, package and publish a tag using only Python's standard library."""

import argparse
from contextlib import contextmanager
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import zipfile


TARGETS = {
    "x86_64-unknown-linux-gnu": ["ssmcp", "ssmcpd"],
    "aarch64-unknown-linux-gnu": ["ssmcp", "ssmcpd"],
    "x86_64-apple-darwin": ["ssmcp"],
    "aarch64-apple-darwin": ["ssmcp"],
    "x86_64-pc-windows-msvc": ["ssmcp"],
    "aarch64-pc-windows-msvc": ["ssmcp"],
}
ZRUN_GIT = "https://github.com/LIghtJUNction/zrun.git"
TAG = re.compile(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?")
ROOT = Path(__file__).resolve().parents[1]
PROJECT_Z_DIRECT = {"zplugin": "zplugin", "zharness": "zharness", "ztuio": "ztuio",
                    "zbridge": "zbridge", "zi18n": "zi18n", "zhex": "zhex"}
PROJECT_Z_PATCHES = {"zplugin", "zguard", "zcall", "zconv", "zmcpio", "zclap", "z64", "zsys"}
PROJECT_Z_REPOS = {name: name for name in PROJECT_Z_PATCHES | set(PROJECT_Z_DIRECT.values())}
PROJECT_Z_REPOS["zplugin-macros"] = "zplugin"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
SHA = re.compile(r"[0-9a-f]{40}")
CHECKSUM = re.compile(r"[0-9a-f]{64}")
VERIFIED_CRATE = "ssmcp-verified-crate.json"
VERIFIED_SOURCE = "verified-source.json"
REGISTRY_EXPORT_LOCK = "ssmcp-registry-Cargo.lock"
RELEASE_CONTRACT = "git-source-v1"
CRATE_DEFERRALS = {
    "0.10.25": "Independent registry dependencies and the external package require separate verification.",
    "0.10.26": "Independent registry dependencies and the external package require separate verification.",
    "0.10.27": "Independent registry dependencies and the external package require separate verification.",
}


def command(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd or ROOT, text=True).strip()


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def version(root=None):
    root = root or ROOT
    with (root / "Cargo.toml").open("rb") as stream:
        package = tomllib.load(stream)["package"]
    if package["name"] != "ssmcp":
        raise ValueError("this release helper expects the ssmcp package")
    return package["version"]



def publication_packages(root=None):
    """Only this repository's application is published here."""
    root = root or ROOT
    return [("ssmcp", version(root))]


def read_toml(path):
    with path.open("rb") as stream:
        return tomllib.load(stream)


def dependency_tables(manifest):
    for target, table in [(None, manifest), *manifest.get("target", {}).items()]:
        for key, kind in (("dependencies", "normal"), ("build-dependencies", "build"),
                          ("dev-dependencies", "dev")):
            for name, spec in table.get(key, {}).items():
                yield target, kind, name, {"version": spec} if isinstance(spec, str) else spec


def project_z_url(package):
    return f"https://github.com/LIghtJUNction/{PROJECT_Z_REPOS[package]}.git"


def require_project_z_git(name, spec, package, *, upstream=False):
    allowed = {"package", "git", "default-features", "features", "optional"}
    if upstream:
        allowed.add("version")
    if (not isinstance(spec, dict) or spec.get("package", name) != package
            or spec.get("git") != project_z_url(package)
            or set(spec) - allowed
            or any(type(spec.get(key, False)) is not bool for key in ("default-features", "optional"))
            or not isinstance(spec.get("features", []), list)
            or any(not isinstance(feature, str) for feature in spec.get("features", []))):
        raise ValueError(f"{name} must use canonical default-branch Git source {project_z_url(package)}")


def package_version(manifest, workspace):
    value = manifest["package"].get("version")
    if value == {"workspace": True}:
        value = workspace.get("workspace", {}).get("package", {}).get("version")
    return value


def development_dependencies(root=None, *, modules=None):
    """Read version metadata from the checked-out Git packages and match the lock."""
    root = Path(root or ROOT)
    manifest = read_toml(root / "Cargo.toml")
    dependencies = manifest.get("dependencies", {})
    locked = locked_project_z(read_toml(root / "Cargo.lock"))
    result = []
    for target, kind, name, spec in dependency_tables(manifest):
        package = spec.get("package", name)
        if name in PROJECT_Z_DIRECT or package in PROJECT_Z_REPOS:
            if target is not None or kind != "normal" or PROJECT_Z_DIRECT.get(name) != package:
                raise ValueError(f"unexpected Project Z dependency declaration: {name}")
    if manifest.get("replace"):
        raise ValueError("[replace] can bypass the Project Z source policy")
    patches = manifest.get("patch", {}).get("crates-io", {})
    for source, entries in manifest.get("patch", {}).items():
        for name, spec in entries.items():
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if name in PROJECT_Z_REPOS or package in PROJECT_Z_REPOS:
                if source != "crates-io" or name not in PROJECT_Z_PATCHES or package != name:
                    raise ValueError(f"unexpected Project Z patch: {source}/{name}")
                require_project_z_git(name, spec, package)
                if set(spec) - {"git", "package"}:
                    raise ValueError(f"Project Z patch must select only Git source: {name}")
    for name, package_name in PROJECT_Z_DIRECT.items():
        dependency = dependencies.get(name)
        if dependency is None:
            continue
        require_project_z_git(name, dependency, package_name)
        path = root / "crates" / name / "Cargo.toml"
        if not path.is_file():
            raise ValueError(f"{name} source is missing; initialize the pinned submodules recursively")
        local = read_toml(path)
        release_version = package_version(local, local)
        if modules is None:
            modules = {row["path"]: row for row in source_submodules(root)}
        module = modules.get("crates/" + name)
        row = locked.get(package_name)
        if (local["package"]["name"] != package_name or not module or not row
                or row["version"] != release_version
                or project_z_revision(package_name, row.get("source")) != module["commit"]):
            raise ValueError(f"{name} source, gitlink and locked dependency version/commit differ")
        version_key(release_version)
        if name == "ztuio" and local.get("lib", {}).get("name", name) != name:
            raise ValueError("the ztuio package must expose the ztuio library name")
        if name == "zharness":
            plugin = dependencies.get("zplugin", {})
            declared = local.get("dependencies", {}).get("zplugin")
            requirement = declared.get("version") if isinstance(declared, dict) else declared
            if not plugin or "zplugin" not in locked:
                raise ValueError("zharness requires the same locked zplugin package as ssmcp")
            expected = "=" + locked["zplugin"]["version"]
            if isinstance(declared, dict) and "git" in declared:
                require_project_z_git("zplugin", declared, "zplugin", upstream=True)
                if requirement is not None and requirement != expected:
                    raise ValueError("zharness requires the same locked zplugin version as ssmcp")
            else:
                if isinstance(declared, dict) and (
                    declared.get("package", "zplugin") != "zplugin"
                    or set(declared) - {"package", "version", "default-features", "features", "optional"}
                ):
                    raise ValueError("zharness must resolve zplugin through its canonical Git source")
                if "zplugin" not in patches:
                    raise ValueError("registry zplugin in zharness requires the canonical crates.io patch")
                if requirement != expected:
                    raise ValueError("zharness requires the same locked zplugin version as ssmcp")
        result.append((package_name, release_version))
    return result


def source_submodules(root=None):
    """Record only initialized, clean gitlinks at their committed object IDs."""
    root = Path(root or ROOT).resolve()
    records = []

    def visit(folder, prefix=""):
        listing = command("git", "ls-tree", "-r", "-z", "HEAD", cwd=folder)
        for entry in listing.split("\0"):
            if not entry:
                continue
            metadata, path = entry.split("\t", 1)
            mode, kind, commit = metadata.split(" ", 2)
            if mode != "160000":
                continue
            relative = Path(path)
            if kind != "commit" or relative.is_absolute() or ".." in relative.parts:
                raise ValueError("invalid submodule gitlink path")
            checkout = folder / relative
            label = prefix + path
            if (checkout.is_symlink() or not checkout.is_dir()
                    or Path(command("git", "rev-parse", "--show-toplevel", cwd=checkout)).resolve()
                    != checkout.resolve()):
                raise ValueError(f"submodule {label} is not initialized; run git submodule update --init --recursive")
            if command("git", "rev-parse", "HEAD", cwd=checkout) != commit:
                raise ValueError(f"submodule {label} differs from its pinned gitlink")
            dirty = subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=checkout).returncode
            untracked = command("git", "ls-files", "--others", "--exclude-standard", cwd=checkout)
            if dirty or untracked:
                raise ValueError(f"submodule {label} contains uncommitted source")
            records.append({"path": label, "commit": commit,
                            "tree": command("git", "rev-parse", "HEAD^{tree}", cwd=checkout)})
            visit(checkout, label + "/")

    visit(root)
    return sorted(records, key=lambda record: record["path"])


def version_key(value):
    """A SemVer ordering key, restricted to the versions accepted by release tags."""
    match = TAG.fullmatch("v" + value) if isinstance(value, str) else None
    if not match or any(part.isdigit() and len(part) > 1 and part.startswith("0")
                        for part in (match.group(4) or "").split(".")):
        raise ValueError("version must be MAJOR.MINOR.PATCH with an optional SemVer prerelease")
    prerelease = match.group(4)
    suffix = (1,) if prerelease is None else (
        0, tuple((0, int(part)) if part.isdigit() else (1, part) for part in prerelease.split(".")))
    return tuple(int(match.group(index)) for index in (1, 2, 3)) + (suffix,)


def package_identity(package):
    return package["name"], package["version"], package.get("source") or None


def project_z_revision(name, source):
    match = re.fullmatch(re.escape("git+" + project_z_url(name)) + r"#([0-9a-f]{40})", source or "")
    if not match:
        raise ValueError(f"{name} requires a locked canonical Project Z Git source")
    return match[1]


def locked_project_z(lock):
    packages = {}
    for row in lock.get("package", []):
        name = row["name"]
        if name in PROJECT_Z_REPOS:
            project_z_revision(name, row.get("source"))
            if name in packages:
                raise ValueError(f"duplicate or split Project Z source identity: {name}")
            packages[name] = row
    for row in lock.get("patch", {}).get("unused", []):
        if row["name"] in PROJECT_Z_REPOS:
            raise ValueError(f"unused Project Z override in Cargo.lock: {row['name']}")
    return packages


def application_version_files(root):
    """Version bytes owned by this application, including its desktop path lock."""
    root = Path(root)
    files = [Path("Cargo.toml"), Path("Cargo.lock")]
    desktop = Path("apps/ssmcp-desktop")
    manifest_path, lock_path = root / desktop / "Cargo.toml", root / desktop / "Cargo.lock"
    if manifest_path.exists():
        manifest = read_toml(manifest_path)
        dependency = manifest.get("dependencies", {}).get("ssmcp")
        if (manifest.get("package", {}).get("name") != "ssmcp-desktop"
                or not isinstance(dependency, dict)
                or not isinstance(dependency.get("path"), str)
                or Path(dependency["path"]).is_absolute()
                or (manifest_path.parent / dependency["path"]).resolve() != root.resolve()
                or set(dependency) & {"version", "git", "registry", "registry-index"}):
            raise ValueError("ssmcp-desktop must use this repository's ssmcp path dependency")
        if not lock_path.is_file():
            raise ValueError("apps/ssmcp-desktop/Cargo.lock must exist before a release bump")
        files.append(desktop / "Cargo.lock")
    elif lock_path.exists():
        raise ValueError("desktop Cargo.lock has no matching application manifest")
    return files


def validate_package(tag, root):
    match = TAG.fullmatch(tag)
    if not match or any(part.isdigit() and len(part) > 1 and part.startswith("0")
                        for part in (match.group(4) or "").split(".")):
        raise ValueError("tag must be vMAJOR.MINOR.PATCH with an optional SemVer prerelease")
    actual = version(root)
    if tag != "v" + actual:
        raise ValueError(f"tag {tag} does not match Cargo.toml version {actual}")
    lock = read_toml(root / "Cargo.lock")
    locked_project_z(lock)
    for name, package_version in publication_packages(root) + development_dependencies(root):
        locked = [p["version"] for p in lock["package"] if p["name"] == name]
        if locked != [package_version]:
            raise ValueError(f"Cargo.lock must contain exactly the manifest's {name} version")
    for relative in application_version_files(root)[2:]:
        rows = [row for row in read_toml(root / relative)["package"] if row["name"] == "ssmcp"]
        if len(rows) != 1 or rows[0]["version"] != actual or rows[0].get("source") is not None:
            raise ValueError(f"{relative} must contain exactly this local ssmcp version")
    return actual


def cargo_git_checkout(manifest, revision):
    checkout = Path(command("git", "rev-parse", "--show-toplevel", cwd=manifest.parent)).resolve()
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    masks = command("git", "ls-files", "-v", cwd=checkout).splitlines()
    untracked = set(command("git", "ls-files", "--others", cwd=checkout).splitlines())
    if (not checkout.is_relative_to(cargo_home / "git/checkouts")
            or not manifest.is_relative_to(checkout)
            or command("git", "rev-parse", "HEAD", cwd=checkout) != revision
            or any(line[0].islower() or line.startswith("S ") for line in masks)
            or untracked - {".cargo-ok"}
            or (checkout / ".cargo-ok").is_symlink()):
        raise ValueError(f"Cargo checkout differs from locked Git source: {manifest}")
    if subprocess.run(["git", "-c", "core.fsmonitor=false", "diff", "--quiet", "HEAD", "--"],
                      cwd=checkout).returncode:
        raise ValueError(f"Cargo checkout contains uncommitted source: {manifest}")
    return checkout, command("git", "rev-parse", "HEAD^{tree}", cwd=checkout)


def metadata_graph(root, release_version):
    metadata = json.loads(command("cargo", "metadata", "--locked", "--format-version", "1", cwd=root))
    resolve = metadata.get("resolve") or {}
    packages = {item["id"]: item for item in metadata["packages"]}
    nodes = {item["id"]: item for item in resolve.get("nodes", [])}
    app = packages.get(resolve.get("root"))
    if (not app or package_identity(app) != ("ssmcp", release_version, None)
            or Path(app["manifest_path"]).resolve() != root / "Cargo.toml"
            or Path(metadata["workspace_root"]).resolve() != root
            or not nodes or nodes.keys() != packages.keys()
            or len(nodes) != len(resolve["nodes"]) or len(packages) != len(metadata["packages"])):
        raise ValueError("full Cargo metadata does not identify the SSMCP dependency graph")
    return app["id"], packages, nodes


def require_shared_zplugin(root_id, packages, nodes):
    harnesses = [key for key, item in packages.items() if item["name"] == "zharness"]
    if not harnesses:
        return
    plugins = {key for key, item in packages.items() if item["name"] == "zplugin"}
    if len(plugins) != 1 or any(
            {dep["pkg"] for dep in nodes[key]["deps"] if packages[dep["pkg"]]["name"] == "zplugin"} != plugins
            for key in [root_id, *harnesses]):
        raise ValueError("ssmcp and zharness must resolve the same single zplugin package identity")


def graph_records(entries, nodes, identities):
    for key, item in entries.items():
        item["dependencies"] = sorted([
            {"name": dep["name"], "package": list(identities[dep["pkg"]]), "kinds": dep["dep_kinds"]}
            for dep in nodes[key]["deps"]
        ], key=lambda row: json.dumps(row, sort_keys=True))
        item["features"] = sorted(nodes[key]["features"])
    return sorted(entries.values(), key=lambda row: (
        row["name"], row["version"], row["source"] or "", row.get("manifest", "")))


def dependency_graph(root=None):
    """Bind full Cargo metadata to the checked-out commits, gitlinks and lock."""
    root = Path(root or ROOT).resolve()
    manifest_path = root / "Cargo.toml"
    checksum = digest(root / "Cargo.lock")
    lock = read_toml(root / "Cargo.lock")
    internal = locked_project_z(lock)
    modules = {row["path"]: row for row in source_submodules(root)}
    direct = dict(development_dependencies(root, modules=modules))
    required = set(direct)
    required.update(PROJECT_Z_PATCHES & read_toml(manifest_path).get("patch", {}).get("crates-io", {}).keys())
    root_id, packages, nodes = metadata_graph(root, version(root))
    identities = {key: package_identity(item) for key, item in packages.items()}
    locked = {package_identity(item) for item in lock["package"]}
    if len(locked) != len(lock["package"]) or not set(identities.values()) <= locked:
        raise ValueError("Cargo metadata package identities differ from Cargo.lock")
    entries = {}
    git_records = {}
    for key, (name, resolved_version, source) in identities.items():
        manifest = Path(packages[key]["manifest_path"]).resolve(strict=True)
        item = {"name": name, "version": resolved_version, "source": source}
        if name in PROJECT_Z_REPOS:
            revision = project_z_revision(name, source)
            if name in git_records or name not in internal or package_identity(internal[name]) != identities[key]:
                raise ValueError(f"duplicate or split Project Z source identity: {name}")
        if source is None:
            if (not manifest.is_relative_to(root)
                    or Path(command("git", "rev-parse", "--show-toplevel", cwd=manifest.parent)).resolve() != root):
                raise ValueError(f"path dependency escapes the SSMCP repository: {name}")
            item["manifest"] = manifest.relative_to(root).as_posix()
            item["manifest_sha256"] = digest(manifest)
        elif source.startswith("git+"):
            revision = source.rpartition("#")[2]
            if not SHA.fullmatch(revision):
                raise ValueError(f"missing locked Git revision for {name}")
            checkout, tree = cargo_git_checkout(manifest, revision)
            data = read_toml(manifest)
            if data["package"]["name"] != name or package_version(data, read_toml(checkout / "Cargo.toml")) != resolved_version:
                raise ValueError(f"Cargo Git checkout manifest identity differs for {name}")
            item.update(commit=revision, tree=tree, manifest=manifest.relative_to(checkout).as_posix(),
                        manifest_sha256=digest(manifest))
            if name in PROJECT_Z_REPOS:
                git_records[name] = item
                module = modules.get("crates/" + PROJECT_Z_REPOS[name])
                if (name in PROJECT_Z_DIRECT.values() and module is None
                        or module and (module["commit"], module["tree"]) != (revision, tree)):
                    raise ValueError(f"Cargo Git checkout differs from the {name} source gitlink")
        entries[key] = item
    if required - git_records.keys() or internal.keys() != git_records.keys():
        raise ValueError(f"Project Z graph differs from declared sources or lock: missing {sorted(required - git_records.keys())}")
    app_dependencies = {identities[dep["pkg"]][:2] for dep in nodes[root_id]["deps"]}
    if not set(direct.items()) <= app_dependencies:
        raise ValueError("Cargo metadata is missing declared direct Project Z dependency edges")
    require_shared_zplugin(root_id, packages, nodes)
    if checksum != digest(root / "Cargo.lock"):
        raise ValueError("Cargo.lock changed during dependency verification")
    return {"cargo_lock_sha256": checksum, "packages": graph_records(entries, nodes, identities)}


def emit(result):
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            for key, value in result.items():
                stream.write(f"{key}={value}\n")
    print(json.dumps(result))
    return result


def event_mode(event_name, ref, event):
    """Only a pushed version tag or an explicit main dispatch can publish."""
    if event_name == "push" and ref.startswith("refs/tags/v"):
        return ref.removeprefix("refs/tags/"), False
    if event_name == "workflow_dispatch" and ref == "refs/heads/main":
        inputs = event.get("inputs") or {}
        tag = inputs.get("tag", "")
        if not isinstance(tag, str):
            raise ValueError("the release tag input must be a string")
        build_only = inputs.get("build_only", False)
        if not isinstance(build_only, (bool, str)) or build_only not in (True, False, "true", "false"):
            raise ValueError("the build_only input must be a boolean")
        if (build_only is True or build_only == "true") and tag:
            raise ValueError("build-only validation cannot select a release tag")
        # False is not authorization to publish main; only an explicit tag is.
        return tag or None, not bool(tag)
    if ref == "refs/heads/release-check":
        if event_name == "push" and not event.get("created", False):
            return None, True
        if event_name == "create" and event.get("ref_type") == "branch" and event.get("ref") == "release-check":
            return None, True
    raise ValueError("this event is not an authorized release or main preflight trigger")


def crate_publication(release_version, *, build_only=False):
    version_key(release_version)
    if build_only:
        return {"status": "not_requested", "reason": "Build-only validation does not publish crates."}
    if release_version in CRATE_DEFERRALS:
        return {"status": "deferred", "reason": CRATE_DEFERRALS[release_version]}
    return {"status": "registry", "reason": None}


def require_registry_mode(root):
    policy = crate_publication(version(root))
    if policy["status"] != "registry":
        raise ValueError(f"crate publication is {policy['status']}: {policy['reason']}")


def validate_tag(tag, root=None, *, build_only=False):
    root = root or ROOT
    if build_only and tag is not None:
        raise ValueError("build-only validation cannot select a release tag")
    actual = validate_package("v" + version(root) if build_only else tag or "", root)
    sha = command("git", "rev-parse", "HEAD", cwd=root)
    if not build_only and command("git", "rev-parse", f"refs/tags/{tag}^{{commit}}", cwd=root) != sha:
        raise ValueError("the checkout must be the requested tag's commit")
    subprocess.run(["git", "merge-base", "--is-ancestor", sha, "refs/remotes/origin/main"],
                   cwd=root, check=True)
    subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root, check=True)
    modules = source_submodules(root)
    return emit({"tag": tag or "", "version": actual, "sha": sha, "submodule_count": len(modules),
                 "prerelease": str("-" in actual).lower(),
                 "publish": str(not build_only).lower(), "create_tag": "false",
                 "build_only": str(build_only).lower(), "release_contract": RELEASE_CONTRACT,
                 "crate_publication": crate_publication(actual, build_only=build_only)["status"],
                 "gui_clients": str(bool(gui_clients(root))).lower()})


def prepare(tag=None, automatic=False, root=None):
    """Legacy prepare --auto now performs a main preflight without publishing."""
    if automatic:
        return validate_tag(tag, root, build_only=True)
    if tag is None:
        raise ValueError("provide an existing release tag or --auto for preflight")
    return validate_tag(tag, root)


def create_tag(tag, sha, root=None):
    root = root or ROOT
    validate_package(tag, root)
    if sha != command("git", "rev-parse", "HEAD", cwd=root):
        raise ValueError("tag must identify the validated checkout")
    command("git", "fetch", "origin", "main", "--tags", cwd=root)
    subprocess.run(["git", "merge-base", "--is-ancestor", sha, "refs/remotes/origin/main"], cwd=root, check=True)
    subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root, check=True)
    source_submodules(root)
    exists = subprocess.run(["git", "show-ref", "--verify", "--quiet", "refs/tags/" + tag], cwd=root).returncode == 0
    if exists:
        if command("git", "rev-parse", f"refs/tags/{tag}^{{commit}}", cwd=root) != sha:
            raise ValueError("release tag already points at another commit")
    else:
        command("git", "config", "user.name", "github-actions[bot]", cwd=root)
        command("git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com", cwd=root)
        command("git", "tag", "-a", tag, "-m", "ssmcp " + tag, cwd=root)
    # Retry a tag whose earlier push failed; an existing remote tag is never forced.
    command("git", "push", "origin", "refs/tags/" + tag, cwd=root)


def bumped_version(old, level, custom_version=None):
    previous = version_key(old)
    if level == "custom":
        if not custom_version:
            raise ValueError("custom bump requires --custom-version without a leading v")
        if version_key(custom_version) <= previous:
            raise ValueError("custom version must be greater than the current version")
        return custom_version
    if custom_version:
        raise ValueError("--custom-version is only valid with the custom level")
    if level not in {"major", "minor", "patch"}:
        raise ValueError("unknown version increment")
    if "-" in old:
        raise ValueError("automatic bump expects a stable package version; use custom for prereleases")
    parts = [int(part) for part in old.split(".")]
    index = {"major": 0, "minor": 1, "patch": 2}[level]
    parts[index] += 1
    parts[index + 1:] = [0] * (2 - index)
    return ".".join(map(str, parts))

def bump(level, root=None, *, custom_version=None, dry_run=False):
    root = Path(root or ROOT)
    old = version(root)
    # Validate and prepare every replacement before changing any version bytes.
    validate_package("v" + old, root)
    new = bumped_version(old, level, custom_version)
    manifest_path = root / "Cargo.toml"
    manifest, manifest_count = re.subn(
        r'(?m)^(version\s*=\s*")' + re.escape(old) + r'("\s*)$',
        lambda match: match[1] + new + match[2], manifest_path.read_text(encoding="utf-8"), count=1)
    if manifest_count != 1:
        raise ValueError("expected one matching ssmcp version in Cargo.toml")
    replacements = {manifest_path: manifest}
    for relative in application_version_files(root)[1:]:
        lock_path = root / relative
        lock, count = re.subn(
            r'(\[\[package\]\]\nname = "ssmcp"\nversion = ")' + re.escape(old) + r'("\n)',
            lambda match: match[1] + new + match[2], lock_path.read_text(encoding="utf-8"))
        if count != 1:
            raise ValueError(f"expected one matching ssmcp version in {relative}")
        replacements[lock_path] = lock
    result = {"tag": "v" + new, "version": new, "dry_run": str(dry_run).lower()}
    if not dry_run:
        for path, text in replacements.items():
            path.write_text(text, encoding="utf-8")
    return emit(result)

def archive_name(target, release_version):
    if target not in TARGETS:
        raise ValueError("unsupported release target: " + target)
    extension = ".zip" if "windows" in target else ".tar.gz"
    return f"ssmcp-{target}{extension}"


def asset_name(target):
    return archive_name(target, version())


def write_archive(stage, output, epoch):
    files = sorted(p for p in stage.rglob("*") if p.is_file())
    if output.suffix == ".zip":
        stamp = time.gmtime(max(epoch, 315532800))[:6]
        with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for path in files:
                info = zipfile.ZipInfo(path.relative_to(stage).as_posix(), stamp)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (path.stat().st_mode & 0xFFFF) << 16
                archive.writestr(info, path.read_bytes())
    else:
        with output.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w") as archive:
                for path in files:
                    info = archive.gettarinfo(path, arcname=path.relative_to(stage).as_posix())
                    info.mtime, info.uid, info.gid = epoch, 0, 0
                    info.uname = info.gname = ""
                    with path.open("rb") as stream:
                        archive.addfile(info, stream)


def locked_zrun(root):
    with (root / "Cargo.lock").open("rb") as stream:
        packages = [p for p in tomllib.load(stream)["package"] if p["name"] == "zrun"]
    if len(packages) != 1:
        raise ValueError("expected exactly one zrun runtime in Cargo.lock")
    package = packages[0]
    source, _, revision = package.get("source", "").partition("#")
    if source.split("?", 1)[0] != "git+" + ZRUN_GIT or not re.fullmatch("[0-9a-f]{40}", revision):
        raise ValueError("Cargo.lock must pin the official zrun Git source to a full commit")
    return package["version"], revision


def bundle_zrun(target, root, stage):
    version, revision = locked_zrun(root)
    source = stage.parent / "zrun-source"
    source.mkdir()
    subprocess.run(["git", "init", "--quiet"], cwd=source, check=True)
    subprocess.run(["git", "fetch", "--quiet", "--depth=1", ZRUN_GIT, revision], cwd=source, check=True)
    subprocess.run(["git", "checkout", "--quiet", "--detach", "FETCH_HEAD"], cwd=source, check=True)
    if command("git", "rev-parse", "HEAD", cwd=source) != revision:
        raise ValueError("bundled zrun source differs from the pinned revision")
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", root / "target" / "zrun-companion")).resolve()
    subprocess.run([
        "cargo", "build", "--locked", "--release", "--package", "zrun-cli",
        "--target", target, "--target-dir", str(target_dir),
    ], cwd=source, check=True)
    binary = target_dir / target / "release" / "zrun"
    if command(str(binary), "--version", cwd=source) != f"zrun {version}":
        raise ValueError("bundled zrun executable differs from the pinned version")
    shutil.copy2(binary, stage / "zrun")
    shutil.copy2(source / "LICENSE", stage / "LICENSE-zrun")


def package(target, root=None):
    root = root or ROOT
    release_version = version(root)
    output = root / "dist"
    output.mkdir(exist_ok=True)
    archive = output / archive_name(target, release_version)
    epoch = int(command("git", "show", "-s", "--format=%ct", "HEAD", cwd=root))
    with tempfile.TemporaryDirectory(prefix="ssmcp-package-") as temporary:
        stage = Path(temporary) / f"ssmcp-{release_version}-{target}"
        stage.mkdir()
        for binary in TARGETS[target]:
            filename = binary + (".exe" if "windows" in target else "")
            source = root / "target" / target / "release" / filename
            reported = command(str(source), "--version", cwd=root)
            if reported != f"{binary} {release_version}":
                raise ValueError(f"unexpected version from {filename}: {reported}")
            command(str(source), "--help", cwd=root)
            shutil.copy2(source, stage / filename)
        for filename in ("README.md", "LICENSE"):
            shutil.copy2(root / filename, stage / filename)
        if "windows" not in target:
            bundle_zrun(target, root, stage)
        (stage / "docs").mkdir()
        shutil.copy2(root / "docs/releases.md", stage / "docs/releases.md")
        if "linux" in target:
            shutil.copy2(root / "src/code_mode.js", stage / "code-mode.js")
            shutil.copy2(root / "docs/deployment.md", stage / "docs/deployment.md")
            shutil.copy2(root / "docs/relay-integration.md", stage / "docs/relay-integration.md")
            shutil.copytree(root / "packaging", stage / "packaging")
            shutil.copy2(root / "scripts/setup_server.py", stage / "setup-server")
        write_archive(stage, archive, epoch)
    checksum = digest(archive)
    (output / (archive.name + ".sha256")).write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(json.dumps({"archive": archive.name, "sha256": checksum, "bytes": archive.stat().st_size}))


GUI_CLIENTS = {
    "linux": {
        "directory": "apps/ssmcp-desktop", "entry": "Cargo.toml",
        "asset": "ssmcp-desktop-x86_64-unknown-linux-gnu-release.tar.gz",
        "target": "x86_64-unknown-linux-gnu", "profile": "release",
        "scope": "Native Release build and Rust tests; mapped X11/OpenGL window under Xvfb/Mesa llvmpipe. No live session interaction.",
    },
    "windows": {
        "directory": "apps/ssmcp-desktop", "entry": "Cargo.toml",
        "asset": "ssmcp-desktop-x86_64-pc-windows-msvc-debug.zip",
        "target": "x86_64-pc-windows-msvc", "profile": "debug",
        "scope": "Native MSVC build and Rust tests; window interaction and live gateway are separate checks.",
    },
    "android": {
        "directory": "apps/ssmcp-android", "entry": "app/build.gradle.kts",
        "asset": "ssmcp-android-debug.apk", "target": "android-api-35-x86_64",
        "profile": "debug",
        "scope": "One custom Instrumentation invocation on API 35; protocol/controller checks use an injected gateway.",
    },
    "ios": {
        "directory": "apps/ssmcp-ios", "entry": "SSMCP.xcodeproj/project.pbxproj",
        "asset": None, "target": "ios-simulator", "profile": "debug",
        "scope": "Unsigned simulator build and XCTest; no device IPA, App Store signing, or live gateway check.",
    },
}


def gui_clients(root):
    clients = {}
    for platform, spec in GUI_CLIENTS.items():
        directory = Path(root) / spec["directory"]
        if directory.exists():
            if not directory.is_dir() or not (directory / spec["entry"]).is_file():
                raise ValueError(f"incomplete {platform} application source")
            clients[platform] = spec
    return clients


def gui_receipt_name(platform):
    return f"ssmcp-gui-{platform}.json"


def gui_source(root, platform):
    spec = GUI_CLIENTS[platform]
    return {
        "commit": command("git", "rev-parse", "HEAD", cwd=root),
        "tree": command("git", "rev-parse", "HEAD^{tree}", cwd=root),
        "application_tree": command("git", "rev-parse", "HEAD:" + spec["directory"], cwd=root),
        "release_version": version(root),
    }


def validate_android_instrumentation(text):
    # This application has one custom Instrumentation, not a JUnit test runner.
    # adb may exit zero when the application reports RESULT_CANCELED.
    success = "SSMCP protocol and controller checks passed:"
    failures = ("SSMCP check failed:", "INSTRUMENTATION_FAILED", "INSTRUMENTATION_ABORTED",
                "FAILURES!!!", "Process crashed")
    codes = re.findall(r"(?m)^INSTRUMENTATION_CODE:\s*(-?\d+)\s*$", text)
    if (text.count(success) != 1 or any(marker in text for marker in failures)
            or (codes and codes != ["-1"])):
        raise ValueError("Android Instrumentation did not report one successful controller/protocol run")


def validate_gui_test_log(platform, text):
    if platform == "android":
        validate_android_instrumentation(text)
    elif platform in {"linux", "windows"}:
        passed = re.findall(r"test result: ok\. (\d+) passed;", text)
        if not passed or max(map(int, passed)) == 0 or "test result: FAILED." in text:
            raise ValueError(f"{platform} Cargo log does not contain a successful nonempty native test run")
    else:
        summaries = re.findall(r"Executed (\d+) tests?, with (\d+) failures", text)
        if (not summaries or max(int(count) for count, _ in summaries) == 0
                or any(int(failed) for _, failed in summaries)):
            raise ValueError("iOS XCTest log does not contain a successful nonempty test run")


def validate_linux_gui_window(window, source_commit):
    expected = {"source_commit": source_commit, "target": GUI_CLIENTS["linux"]["target"],
                "profile": "release", "backend": "x11-opengl", "renderer": "llvmpipe",
                "title": "ssmcp-desktop", "mapped": True, "exit_after_ms": 10000,
                "exit_code": 0}
    if (not isinstance(window, dict) or any(window.get(key) != value for key, value in expected.items())
            or window.get("mapped") is not True
            or any(type(window.get(key)) is not int for key in
                   ("exit_after_ms", "exit_code", "elapsed_ms", "width", "height", "binary_bytes"))
            or not 10000 <= window["elapsed_ms"] < 30000
            or window["width"] <= 0 or window["height"] <= 0 or window["binary_bytes"] < 20
            or not re.fullmatch(r"[0-9a-f]{64}", str(window.get("binary_sha256", "")))):
        raise ValueError("Linux GUI requires a successful bounded native window receipt for this Release source")


def validate_linux_gui_executable(stream):
    header = stream.read(20)
    if (len(header) < 20 or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != 62):
        raise ValueError("the Linux GUI build must provide an x86_64 ELF executable")


def validate_windows_x64_executable(stream):
    header = stream.read(64)
    if len(header) == 64 and header[:2] == b"MZ":
        offset = int.from_bytes(header[60:64], "little")
        if 64 <= offset <= 1024 * 1024:
            stream.seek(offset)
            if stream.read(6) == b"PE\0\0\x64\x86":
                return
    raise ValueError("the Windows desktop bundle requires x86_64 PE executables")


def check_companion_receipt(cli, platform, source):
    spec = GUI_CLIENTS[platform]
    expected = {"file": "ssmcp.exe" if platform == "windows" else "ssmcp",
                "target": spec["target"], "profile": spec["profile"], "source_commit": source["commit"]}
    if (not isinstance(cli, dict) or any(cli.get(key) != value for key, value in expected.items())
            or type(cli.get("bytes")) is not int or cli["bytes"] < 20
            or not re.fullmatch(r"[0-9a-f]{64}", str(cli.get("sha256", "")))):
        raise ValueError("companion CLI receipt does not match the desktop source, target or profile")


def verify_companion_stream(stream, platform, cli):
    if platform == "windows":
        validate_windows_x64_executable(stream)
    else:
        validate_linux_gui_executable(stream)
    stream.seek(0)
    if hashlib.file_digest(stream, "sha256").hexdigest() != cli["sha256"]:
        raise ValueError("companion CLI bytes differ from the independently tested executable")


def verify_windows_gui_archive(archive, source, cli):
    with zipfile.ZipFile(archive) as package:
        expected = {"ssmcp-desktop.exe", "ssmcp.exe", "README.md", "LICENSE", "build-source.json"}
        if (len(package.infolist()) != len(expected) or set(package.namelist()) != expected
                or any(info.is_dir() or stat.S_ISLNK(info.external_attr >> 16) for info in package.infolist())):
            raise ValueError("Windows GUI archive must contain the two sibling executables and source proof")
        with package.open("ssmcp-desktop.exe") as stream:
            validate_windows_x64_executable(stream)
        if package.getinfo("ssmcp.exe").file_size != cli["bytes"]:
            raise ValueError("Windows companion CLI size differs from the checked executable")
        with package.open("ssmcp.exe") as stream:
            verify_companion_stream(stream, "windows", cli)
        if json.loads(package.read("build-source.json")) != {**source, "companion_cli": cli}:
            raise ValueError("Windows GUI archived source proof differs from the release checkout")


def verify_linux_gui_archive(archive, source, window, cli):
    """Bind the published executable and source proof to the actual window probe."""
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        expected = {"ssmcp-desktop", "ssmcp", "README.md", "LICENSE", "build-source.json"}
        if (len(members) != len(expected) or {member.name for member in members} != expected
                or not all(member.isfile() for member in members)):
            raise ValueError("Linux GUI archive must contain only the two executables, instructions, license and source proof")
        binary = package.getmember("ssmcp-desktop")
        if binary.size != window["binary_bytes"] or binary.mode != 0o755:
            raise ValueError("Linux GUI archive differs from the executable tested in the native window")
        with package.extractfile(binary) as stream:
            validate_linux_gui_executable(stream)
        with package.extractfile(binary) as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != window["binary_sha256"]:
                raise ValueError("Linux GUI archive differs from the executable tested in the native window")
        companion = package.getmember("ssmcp")
        if companion.size != cli["bytes"] or companion.mode != 0o755:
            raise ValueError("Linux companion CLI has the wrong size or executable mode")
        with package.extractfile(companion) as stream:
            verify_companion_stream(stream, "linux", cli)
        with package.extractfile("build-source.json") as stream:
            if json.load(stream) != {**source, "companion_cli": cli}:
                raise ValueError("Linux GUI archived source proof differs from the release checkout")


def gui_exit_receipt_succeeded(platform, checks, source, spec):
    """Require every platform-specific native gate and an exact source receipt."""
    exits = ["build_exit", "test_exit"]
    if platform == "windows":
        exits += ["cli_test_exit", "cli_build_exit", "native_plugin_exit"]
    expected = {"source_commit": source["commit"], "target": spec["target"],
                "profile": spec["profile"], **dict.fromkeys(exits, 0)}
    return checks == expected and all(type(checks[key]) is int for key in exits)


def package_gui(platform, evidence, binary=None, root=None):
    """Package successful GUI checks from this exact checkout; never publish."""
    root, evidence = Path(root or ROOT), Path(evidence)
    clients = gui_clients(root)
    if platform not in clients:
        raise ValueError(f"the requested source does not contain the {platform} client")
    spec = clients[platform]
    source = gui_source(root, platform)
    if os.environ.get("SOURCE_COMMIT") != source["commit"]:
        raise ValueError("GUI source does not match the requested immutable commit")
    subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root, check=True)
    result = json.loads((evidence / "result.json").read_text(encoding="utf-8-sig"))
    if not gui_exit_receipt_succeeded(platform, result, source, spec):
        raise ValueError("GUI build and test exit receipts must all succeed for this source and target")
    for name in ("build.log", "tests.log"):
        path = evidence / name
        if path.is_symlink() or not path.is_file() or not path.stat().st_size:
            raise ValueError(f"missing GUI evidence: {name}")
    validate_gui_test_log(platform, (evidence / "tests.log").read_text(encoding="utf-8-sig"))
    provenance = {key: os.environ.get(name, "") for key, name in (
        ("repository", "GITHUB_REPOSITORY"), ("run_id", "GITHUB_RUN_ID"),
        ("run_attempt", "GITHUB_RUN_ATTEMPT"), ("workflow_commit", "GITHUB_WORKFLOW_SHA"),
    )}
    if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", provenance["repository"])
            or not all(re.fullmatch(r"[1-9][0-9]*", provenance[key]) for key in ("run_id", "run_attempt"))
            or not re.fullmatch(r"[0-9a-f]{40}", provenance["workflow_commit"])):
        raise ValueError("GUI packages require a concrete Actions run receipt")
    provenance["evidence_artifact"] = f"{platform}-{source['commit']}-{provenance['run_attempt']}"
    receipt = {"schema_version": 1, "platform": platform, "target": spec["target"], "profile": spec["profile"],
               "scope": spec["scope"], "source": source, "checks": result, "actions": provenance,
               "evidence": {path.name: digest(path) for path in sorted(evidence.iterdir())
                            if path.is_file() and not path.is_symlink()
                            and (path.suffix == ".log" or path.name == "result.json")},
               "asset": None}
    if platform == "linux":
        for name in ("window.json", "window.log", "xwininfo.log", "renderer.log"):
            path = evidence / name
            if path.is_symlink() or not path.is_file() or (name != "window.log" and not path.stat().st_size):
                raise ValueError(f"missing Linux native window evidence: {name}")
            receipt["evidence"][name] = digest(path)
        receipt["window"] = json.loads((evidence / "window.json").read_text(encoding="utf-8"))
        validate_linux_gui_window(receipt["window"], source["commit"])
    output = root / "dist"
    output.mkdir(exist_ok=True)
    if spec["asset"]:
        if binary is None or Path(binary).is_symlink() or not Path(binary).is_file():
            raise ValueError("the successful build must provide a regular GUI executable")
        binary = Path(binary)
        archive = output / spec["asset"]
        if platform in {"linux", "windows"}:
            with binary.open("rb") as stream:
                if platform == "linux":
                    validate_linux_gui_executable(stream)
                else:
                    validate_windows_x64_executable(stream)
            if platform == "linux" and (digest(binary) != receipt["window"]["binary_sha256"]
                                         or binary.stat().st_size != receipt["window"]["binary_bytes"]):
                raise ValueError("Linux GUI executable changed after its native window check")
            proof = evidence / "cli-executable.json"
            if proof.is_symlink() or not proof.is_file():
                raise ValueError("missing companion CLI build receipt")
            cli = json.loads(proof.read_text(encoding="utf-8-sig"))
            check_companion_receipt(cli, platform, source)
            companion = binary.parent / cli["file"]
            if companion.is_symlink() or not companion.is_file() or companion.stat().st_size != cli["bytes"]:
                raise ValueError("required sibling companion CLI is missing or changed")
            with companion.open("rb") as stream:
                verify_companion_stream(stream, platform, cli)
            receipt["companion_cli"] = cli
            receipt["evidence"][proof.name] = digest(proof)
            epoch = int(command("git", "show", "-s", "--format=%ct", "HEAD", cwd=root))
            with tempfile.TemporaryDirectory(prefix="ssmcp-desktop-package-") as temporary:
                stage = Path(temporary)
                executable = stage / ("ssmcp-desktop.exe" if platform == "windows" else "ssmcp-desktop")
                shutil.copyfile(binary, executable)
                shutil.copyfile(companion, stage / cli["file"])
                if platform == "linux":
                    executable.chmod(0o755)
                    (stage / cli["file"]).chmod(0o755)
                shutil.copyfile(root / "LICENSE", stage / "LICENSE")
                shutil.copyfile(root / spec["directory"] / "README.md", stage / "README.md")
                (stage / "build-source.json").write_text(json.dumps({**source, "companion_cli": cli}, indent=2) + "\n", encoding="utf-8")
                write_archive(stage, archive, epoch)
            if platform == "linux":
                verify_linux_gui_archive(archive, source, receipt["window"], cli)
            else:
                verify_windows_gui_archive(archive, source, cli)
        else:
            with zipfile.ZipFile(binary) as apk:
                if not {"AndroidManifest.xml", "classes.dex"} <= set(apk.namelist()):
                    raise ValueError("the Android debug package is not an executable APK")
            shutil.copyfile(binary, archive)
        receipt["asset"] = {"name": archive.name, "sha256": digest(archive), "bytes": archive.stat().st_size}
        (output / (archive.name + ".sha256")).write_text(
            f"{receipt['asset']['sha256']}  {archive.name}\n", encoding="utf-8")
    elif binary is not None:
        raise ValueError("iOS simulator validation does not publish a device application")
    path = output / gui_receipt_name(platform)
    path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"receipt": path.name, "source": source, "asset": receipt["asset"]}))
    return receipt


def verified_gui_receipts(root, source):
    output = root / "dist"
    receipts = []
    for platform, spec in gui_clients(root).items():
        path = output / gui_receipt_name(platform)
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"missing GUI build and test receipt: {path.name}")
        receipt = json.loads(path.read_text(encoding="utf-8"))
        expected_source = gui_source(root, platform)
        if (expected_source["commit"] != source["commit"] or expected_source["tree"] != source["tree"]
                or receipt.get("schema_version") != 1 or receipt.get("source") != expected_source
                or any(receipt.get(key) != value for key, value in
                       (("platform", platform), ("target", spec["target"]),
                        ("profile", spec["profile"]), ("scope", spec["scope"])))):
            raise ValueError(f"GUI source or platform proof differs: {platform}")
        checks = receipt.get("checks", {})
        if not gui_exit_receipt_succeeded(platform, checks, source, spec):
            raise ValueError(f"GUI build or tests did not succeed: {platform}")
        actions = receipt.get("actions", {})
        if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", str(actions.get("repository", "")))
                or not all(re.fullmatch(r"[1-9][0-9]*", str(actions.get(key, "")))
                           for key in ("run_id", "run_attempt"))
                or not re.fullmatch(r"[0-9a-f]{40}", str(actions.get("workflow_commit", "")))
                or actions.get("evidence_artifact") != f"{platform}-{source['commit']}-{actions.get('run_attempt')}"):
            raise ValueError(f"GUI Actions provenance is missing or invalid: {platform}")
        evidence = receipt.get("evidence", {})
        if (not isinstance(evidence, dict) or not {"build.log", "tests.log", "result.json"} <= evidence.keys()
                or not all(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value)
                           for value in evidence.values())):
            raise ValueError(f"GUI test evidence hashes are missing or invalid: {platform}")
        artifact = output / spec["asset"] if spec["asset"] else None
        expected_asset = ({"name": artifact.name, "sha256": digest(artifact), "bytes": artifact.stat().st_size}
                          if artifact is not None else None)
        if receipt.get("asset") != expected_asset:
            raise ValueError(f"GUI artifact does not match its successful build: {platform}")
        if platform in {"linux", "windows"}:
            cli = receipt.get("companion_cli")
            check_companion_receipt(cli, platform, source)
            if "cli-executable.json" not in evidence:
                raise ValueError("missing companion CLI evidence hash")
        if platform == "windows":
            verify_windows_gui_archive(artifact, expected_source, cli)
        if platform == "linux":
            validate_linux_gui_window(receipt.get("window"), source["commit"])
            if not {"window.json", "window.log", "xwininfo.log", "renderer.log"} <= evidence.keys():
                raise ValueError("Linux native window evidence hashes are missing")
            verify_linux_gui_archive(artifact, expected_source, receipt["window"], cli)
        receipts.append(receipt)
    return receipts


def checksums(root=None, *, build_only=False, recheck=False):
    """Verify GitHub archives; Publisher raw-client release.json is separate."""
    root = root or ROOT
    release_version = version(root)
    output = root / "dist"
    clients = gui_clients(root)
    gui_assets = {spec["asset"] for spec in clients.values() if spec["asset"]}
    gui_proofs = {gui_receipt_name(platform) for platform in clients}
    expected = {archive_name(target, release_version) for target in TARGETS} | gui_assets
    actual = {p.name for p in output.iterdir() if p.name.endswith((".zip", ".tar.gz", ".apk"))}
    if actual != expected:
        raise ValueError(f"release archive set differs: missing={sorted(expected-actual)}, extra={sorted(actual-expected)}")
    installers = {"install.sh", "install.ps1"}
    inputs = expected | installers | gui_proofs | {name + ".sha256" for name in expected}
    allowed = inputs | {"SHA256SUMS", "release-manifest.json"}
    unexpected = {p.name for p in output.iterdir()} - allowed
    if unexpected:
        raise ValueError(f"unexpected release files: {sorted(unexpected)}")
    for name in sorted(allowed & {p.name for p in output.iterdir()}):
        if (output / name).is_symlink() or not (output / name).is_file():
            raise ValueError(f"release attachment must be a regular file: {name}")
    records = []
    for name in sorted(expected):
        path = output / name
        checksum = digest(path)
        if (output / (name + ".sha256")).read_text(encoding="utf-8") != f"{checksum}  {name}\n":
            raise ValueError(f"checksum mismatch for {name}")
        records.append({"name": name, "sha256": checksum, "bytes": path.stat().st_size})
    for name in sorted(installers):
        path = output / name
        if not path.is_file() or digest(path) != digest(root / "scripts" / name):
            raise ValueError(f"installer is missing or differs from the validated source: {name}")
        records.append({"name": name, "sha256": digest(path), "bytes": path.stat().st_size})
    source = verified_source_receipt(root)
    gui_receipts = verified_gui_receipts(root, source)
    for name in sorted(gui_proofs):
        path = output / name
        records.append({"name": name, "sha256": digest(path), "bytes": path.stat().st_size})
    policy = crate_publication(release_version, build_only=build_only)
    verified = verified_crate_receipt(root) if policy["status"] == "registry" else None
    if verified is not None and verified["source"] != source:
        raise ValueError("verified crate and verified binary source differ")
    manifest = {"schema_version": 2, "package": "ssmcp", "version": release_version,
                "commit": source["commit"], "tree": source["tree"],
                "source_manifest_sha256": source["manifest_sha256"],
                "cargo_lock_sha256": source["cargo_lock_sha256"],
                "toolchain": {"cargo": source["cargo"], "rustc": source["rustc"]},
                "submodules": source["submodules"], "dependency_graph": source["dependency_graph"],
                "crate_publication": policy, "verified_crate": verified, "artifacts": records}
    if gui_receipts:
        manifest["gui_clients"] = gui_receipts
    documents = {
        "SHA256SUMS": "".join(f"{r['sha256']}  {r['name']}\n" for r in records),
        "release-manifest.json": json.dumps(manifest, indent=2) + "\n",
    }
    for name, text in documents.items():
        path = output / name
        if recheck:
            if not path.is_file() or path.read_bytes() != text.encode():
                raise ValueError(f"finalized release proof differs: {name}")
        else:
            path.write_bytes(text.encode())
    print(f"Verified {len(expected)} target archives and {len(installers)} installers")


def registry_checksum(release_version, package="ssmcp"):
    url = f"https://crates.io/api/v1/crates/{package}/{release_version}"
    request = urllib.request.Request(url, headers={"User-Agent": "ssmcp-release (github.com/LIghtJUNction/ssmcp)"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            data = json.load(response)["version"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    if data["yanked"]:
        raise ValueError(f"the requested {package} version is yanked; choose a new version")
    return data["checksum"]


def check_registry(root=None, package="ssmcp", release_version=None):
    root = root or ROOT
    release_version = release_version or dict(publication_packages(root))[package]
    path = root / "target/package" / f"{package}-{release_version}.crate"
    if not path.is_file():
        raise ValueError(f"run cargo package --locked --package {package} before publishing")
    checksum = digest(path)
    existing = registry_checksum(release_version, package)
    if existing is not None:
        if existing != checksum:
            raise ValueError(f"crates.io already contains different bytes for {package} {release_version}")
        print(f"crates.io already contains the identical {package} {release_version} package")
        return True
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise ValueError("configure repository secret CARGO_REGISTRY_TOKEN (legacy CARGO_REGISTY_TOKEN is also accepted)")
    return False


def check_registry_dependencies(root=None):
    for package, release_version in development_dependencies(root):
        if registry_checksum(release_version, package) is None:
            raise ValueError(f"publish {package} {release_version} from its independent repository before releasing ssmcp")


def crate_archive(root, target=None):
    return (target or (root / "target")) / "package" / f"ssmcp-{version(root)}.crate"


def release_source(root):
    root = Path(root).resolve()
    def identity():
        subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root, check=True)
        if command("git", "ls-files", "--others", "--exclude-standard", cwd=root):
            raise ValueError("release verification requires a clean source checkout")
        commit = command("git", "rev-parse", "HEAD", cwd=root)
        return {"commit": commit, "tree": command("git", "rev-parse", f"{commit}^{{tree}}", cwd=root),
                "manifest_sha256": digest(root / "Cargo.toml"),
                "cargo_lock_sha256": digest(root / "Cargo.lock"),
                "submodules": source_submodules(root)}

    source = identity()
    graph = dependency_graph(root)
    if identity() != source or graph["cargo_lock_sha256"] != source["cargo_lock_sha256"]:
        raise ValueError("source commit, lock or gitlinks changed during dependency verification")
    return {**source, "dependency_graph": graph,
            "cargo": command("cargo", "--version", cwd=root),
            "rustc": command("rustc", "--version", cwd=root)}


def source_receipt_path(root):
    return root / "target/release-proof" / VERIFIED_SOURCE


def verify_source(root=None):
    root = Path(root or ROOT).resolve()
    source = release_source(root)
    path = source_receipt_path(root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"contract": RELEASE_CONTRACT, "source": source}, indent=2) + "\n",
                    encoding="utf-8")
    return source


def verified_source_receipt(root):
    recorded = json.loads(source_receipt_path(root).read_text(encoding="utf-8"))
    actual = release_source(root)
    if recorded != {"contract": RELEASE_CONTRACT, "source": actual}:
        raise ValueError("verified source receipt differs from the fixed checkout")
    return actual


def inline_dependency(text, section, name, edit):
    header = re.search(r"(?m)^\[" + re.escape(section) + r"\][ \t]*(?:#.*)?$", text)
    if header is None:
        raise ValueError(f"missing [{section}] for registry export")
    body = text[header.end():]
    next_header = re.search(r"(?m)^\[", body)
    end = next_header.start() if next_header else len(body)
    changed, count = re.subn(r"(?m)^[ \t]*" + re.escape(name)
                             + r"[ \t]*=[ \t]*\{[^\r\n]*\}[^\r\n]*$",
                             lambda match: edit(match[0]), body[:end])
    if count != 1:
        raise ValueError(f"expected one inline {name} declaration in [{section}]")
    return text[:header.end()] + changed + body[end:]


def registry_export_manifest(root):
    """Derive exact registry selectors from verified Git package identities."""
    raw = (root / "Cargo.toml").read_bytes()
    text = raw.decode()
    expected = tomllib.loads(text)
    versions = dict(development_dependencies(root))
    for name, package in PROJECT_Z_DIRECT.items():
        spec = expected.get("dependencies", {}).get(name)
        if spec is None:
            continue
        url = spec.pop("git")
        spec["version"] = "=" + versions[package]

        def replace_source(line):
            line, count = re.subn(r"(?<![\w-])git[ \t]*=[ \t]*(['\"])" + re.escape(url) + r"\1",
                                  'version = ' + json.dumps(spec["version"]), line)
            if count != 1:
                raise ValueError(f"cannot identify the Git selector for {name}")
            return line

        text = inline_dependency(text, "dependencies", name, replace_source)
    unix = "cfg(unix)"
    zrun = expected.get("target", {}).get(unix, {}).get("dependencies", {}).get("zrun")
    if isinstance(zrun, dict) and "git" in zrun:
        require_project_z_zrun(zrun)
        choices = [
            row for row in read_toml(root / "Cargo.lock")["package"]
            if row["name"] == "zrun"
        ]
        if (len(choices) != 1
                or not re.fullmatch(re.escape("git+" + ZRUN_GIT + "#") + SHA.pattern,
                                    choices[0].get("source", ""))):
            raise ValueError("registry export requires one locked Git revision for zrun")
        pinned = "=" + choices[0]["version"]
        version_key(choices[0]["version"])
        del zrun["git"]
        zrun["version"] = pinned
        text = inline_dependency(text, "target.'cfg(unix)'.dependencies", "zrun", lambda line:
                                 re.sub(r'git\s*=\s*"[^"]+"', 'version = ' + json.dumps(pinned), line, count=1))
    # Registry consumers cannot use the development-only internal Git overrides.
    overrides = expected.get("patch", {}).get("crates-io", {})
    for name in sorted(overrides.keys() & PROJECT_Z_PATCHES):
        del overrides[name]
        text = inline_dependency(text, "patch.crates-io", name, lambda _: "")
    if tomllib.loads(text) != expected:
        raise ValueError("registry export changed fields beyond the internal Git source selectors")
    return text.encode()


def require_project_z_zrun(spec):
    if spec.get("git") != ZRUN_GIT or set(spec) - {"git", "features", "default-features"}:
        raise ValueError("zrun registry export requires the canonical Git dependency")


def registry_lock(data, release_version, direct):
    lock = tomllib.loads(data.decode())
    locked = lock["package"]
    seen = set()
    app = ("ssmcp", release_version, None)
    for entry in locked:
        identity = package_identity(entry)
        if (identity in seen or identity != app and
                (identity[2] != CRATES_IO or not CHECKSUM.fullmatch(entry.get("checksum", "")))):
            raise ValueError("normalized Cargo.lock contains an unexpected source or duplicate package")
        seen.add(identity)
    if (app not in seen or "patch" in lock
            or any((name, source_version, CRATES_IO) not in seen for name, source_version in direct.items())):
        raise ValueError("normalized Cargo.lock does not match the registry publication contract")
    return locked


def require_registry_export(export, commit, manifest, lock):
    if (command("git", "rev-parse", "HEAD", cwd=export) != commit
            or (export / "Cargo.toml").read_bytes() != manifest
            or (export / "Cargo.lock").read_bytes() != lock):
        raise ValueError("publication export commit, manifest or registry lock changed")
    subprocess.run(["git", "-c", "core.fsmonitor=false", "diff", "--quiet", "HEAD", "--", ".",
                    ":(exclude)Cargo.toml", ":(exclude)Cargo.lock"], cwd=export, check=True)
    if command("git", "ls-files", "--others", cwd=export):
        raise ValueError("publication export contains uncommitted files outside its manifest and lock")


@contextmanager
def registry_export(root, commit, manifest, frozen_lock=None):
    """Use an owned clone so original worktree, index and lock stay unchanged."""
    with tempfile.TemporaryDirectory(prefix="ssmcp-registry-") as temporary:
        export = Path(temporary) / "source"
        command("git", "clone", "--shared", "--no-tags", "--no-checkout", "--", str(root), str(export), cwd=root)
        command("git", "checkout", "--detach", commit, cwd=export)
        (export / "Cargo.toml").write_bytes(manifest)
        if frozen_lock is None:
            subprocess.run(["cargo", "generate-lockfile"], cwd=export, check=True)
        else:
            (export / "Cargo.lock").write_bytes(frozen_lock)
        lock = (export / "Cargo.lock").read_bytes()
        if frozen_lock is not None and lock != frozen_lock:
            raise ValueError("publication export must reuse the verified registry lock")
        require_registry_export(export, commit, manifest, lock)
        yield export, Path(temporary) / "target", lock
        require_registry_export(export, commit, manifest, lock)


def normalized_crate(root, data, commit, export_lock=None):
    """Inspect the package Cargo actually normalized and built."""
    prefix = f"ssmcp-{version(root)}"
    members = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for entry in archive:
            if entry.name == prefix and entry.isdir():
                continue
            path = PurePosixPath(entry.name)
            if ("\\" in entry.name or entry.name != path.as_posix() or len(path.parts) < 2 or path.parts[0] != prefix
                    or ".." in path.parts or (not entry.isfile() and not entry.isdir())):
                raise ValueError(f"unsafe normalized package member: {entry.name}")
            if entry.isdir():
                continue
            name = path.relative_to(prefix).as_posix()
            if name in members:
                raise ValueError(f"duplicate normalized package member: {entry.name}")
            with archive.extractfile(entry) as stream:
                members[name] = stream.read()
    required = {"Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json", "src/lib.rs"}
    if not required <= members.keys():
        raise ValueError("missing normalized package manifest, lock or source identity")
    derived = registry_export_manifest(root)
    dirty = (derived != (root / "Cargo.toml").read_bytes()
             or export_lock is not None and export_lock != (root / "Cargo.lock").read_bytes())
    vcs = json.loads(members[".cargo_vcs_info.json"])
    if (vcs.get("git", {}).get("sha1") != commit or vcs["git"].get("dirty", False) is not dirty
            or vcs.get("path_in_vcs") != "" or members["Cargo.toml.orig"] != derived):
        raise ValueError("normalized package source identity differs from the verified checkout")
    for name, raw in members.items():
        if name in {"Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"}:
            continue
        if name in {".cargo/config", ".cargo/config.toml"}:
            raise ValueError("normalized package must not carry Cargo source configuration")
        path = (root / name).resolve()
        if not path.is_relative_to(root.resolve()) or not path.is_file() or path.read_bytes() != raw:
            raise ValueError(f"normalized package member differs from the verified checkout: {name}")
    manifest = tomllib.loads(members["Cargo.toml"].decode())
    package = manifest["package"]
    if ((package["name"], package["version"]) != ("ssmcp", version(root))
            or any(key in manifest for key in ("patch", "replace", "workspace"))):
        raise ValueError("invalid normalized package manifest")
    exported = tomllib.loads(derived.decode()).get("dependencies", {})
    for _, _, name, spec in dependency_tables(manifest):
        if (not isinstance(spec.get("version"), str)
                or set(spec) & {"git", "path", "rev", "tag", "branch", "registry", "registry-index", "workspace"}):
            raise ValueError(f"normalized dependency is not a crates.io version: {name}")
    for name, package_name in PROJECT_Z_DIRECT.items():
        if name not in exported:
            continue
        source = exported[name]
        normalized = manifest.get("dependencies", {}).get(name, {})
        if (not isinstance(normalized, dict) or normalized.get("package", name) != package_name
                or normalized.get("version") != source["version"]
                or normalized.get("default-features", True) != source.get("default-features", True)
                or normalized.get("optional", False) != source.get("optional", False)
                or sorted(normalized.get("features", [])) != sorted(source.get("features", []))):
            raise ValueError(f"normalized {name} changed its package, version or features")
    direct = dict(development_dependencies(root))
    locked = registry_lock(members["Cargo.lock"], version(root), direct)
    proof = {"manifest_sha256": hashlib.sha256(members["Cargo.toml"]).hexdigest(),
             "lock_sha256": hashlib.sha256(members["Cargo.lock"]).hexdigest(),
             "packages": locked, "direct_project_z": direct}
    return proof, members


def package_crate(export, target):
    args = ["cargo", "package", "--locked", "--allow-dirty", "--package", "ssmcp", "--registry", "crates-io",
            "--target-dir", str(target)]
    subprocess.run(args, cwd=export, check=True)
    return crate_archive(export, target)


def check_registry_graph(graph, normalized, release_version):
    packages = {package_identity(item): item for item in graph}
    root = ("ssmcp", release_version, None)
    if (root not in packages or len(packages) != len(graph)
            or any(identity[2] != CRATES_IO for identity in packages if identity != root)
            or not packages.keys() <= {package_identity(item) for item in normalized["packages"]}):
        raise ValueError("normalized package metadata and registry lock identities differ")
    nodes = {identity: {"deps": [{"pkg": tuple(dep["package"])} for dep in item["dependencies"]]}
             for identity, item in packages.items()}
    if any(dep["pkg"] not in packages for node in nodes.values() for dep in node["deps"]):
        raise ValueError("normalized registry graph contains a dangling dependency")
    required = {(name, resolved, CRATES_IO) for name, resolved in normalized["direct_project_z"].items()}
    if not required <= {dep["pkg"] for dep in nodes[root]["deps"]}:
        raise ValueError("normalized registry graph is missing direct Project Z dependency edges")
    require_shared_zplugin(root, packages, nodes)


def normalized_registry_graph(normalized, members, release_version):
    """Resolve only the files and lock actually shipped by Cargo."""
    with tempfile.TemporaryDirectory(prefix="ssmcp-normalized-") as temporary:
        root = Path(temporary).resolve()
        for name, data in members.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        root_id, packages, nodes = metadata_graph(root, release_version)
        identities = {key: package_identity(item) for key, item in packages.items()}
        if identities[root_id] != ("ssmcp", release_version, None):
            raise ValueError("normalized registry graph does not identify the verified application")
        if digest(root / "Cargo.lock") != normalized["lock_sha256"]:
            raise ValueError("normalized Cargo.lock changed during metadata validation")
    entries = {key: {"name": name, "version": resolved, "source": source}
               for key, (name, resolved, source) in identities.items()}
    graph = graph_records(entries, nodes, identities)
    check_registry_graph(graph, normalized, release_version)
    return graph


def check_registry_project_z(normalized):
    for item in normalized["packages"]:
        if item["name"] in PROJECT_Z_REPOS:
            if registry_checksum(item["version"], item["name"]) != item["checksum"]:
                raise ValueError(f"normalized registry dependency is unpublished or changed: {item['name']} {item['version']}")


def export_receipt(manifest, lock):
    return {"manifest_sha256": hashlib.sha256(manifest).hexdigest(),
            "lock_sha256": hashlib.sha256(lock).hexdigest()}


def verify_crate(root=None):
    root = Path(root or ROOT).resolve()
    require_registry_mode(root)
    check_registry_dependencies(root)
    source = release_source(root)
    manifest = registry_export_manifest(root)
    direct = dict(development_dependencies(root))
    with registry_export(root, source["commit"], manifest) as (export, target, lock):
        registry_lock(lock, version(root), direct)
        packed = package_crate(export, target)
        data = packed.read_bytes()
    if release_source(root) != source:
        raise ValueError("source graph changed during cargo package verification")
    normalized, members = normalized_crate(root, data, source["commit"], lock)
    graph = normalized_registry_graph(normalized, members, version(root))
    check_registry_project_z(normalized)
    archive = crate_archive(root)
    receipt = {"source": source, "name": archive.name, "sha256": hashlib.sha256(data).hexdigest(),
               "bytes": len(data), "export": export_receipt(manifest, lock),
               "normalized": normalized, "registry_graph": graph}
    archive.parent.mkdir(parents=True, exist_ok=True)
    archive.write_bytes(data)
    (archive.parent / REGISTRY_EXPORT_LOCK).write_bytes(lock)
    (archive.parent / VERIFIED_CRATE).write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")


def verified_crate_receipt(root):
    root = Path(root).resolve()
    require_registry_mode(root)
    archive = crate_archive(root)
    receipt = json.loads((archive.parent / VERIFIED_CRATE).read_text(encoding="utf-8"))
    source = release_source(root)
    manifest = registry_export_manifest(root)
    lock = (archive.parent / REGISTRY_EXPORT_LOCK).read_bytes()
    registry_lock(lock, version(root), dict(development_dependencies(root)))
    data = archive.read_bytes()
    normalized, members = normalized_crate(root, data, source["commit"], lock)
    actual = {"source": source, "name": archive.name, "sha256": hashlib.sha256(data).hexdigest(),
              "bytes": len(data), "export": export_receipt(manifest, lock), "normalized": normalized}
    if any(receipt.get(key) != value for key, value in actual.items()):
        raise ValueError("verified crate or source graph changed before publication")
    check_registry_graph(receipt["registry_graph"], normalized, version(root))
    if receipt["registry_graph"] != normalized_registry_graph(normalized, members, version(root)):
        raise ValueError("verified registry graph differs from the actual normalized package graph")
    return receipt


def publish(root=None):
    root = Path(root or ROOT).resolve()
    require_registry_mode(root)
    check_registry_dependencies(root)
    verified = verified_crate_receipt(root)
    source = verified["source"]
    checksum = verified["sha256"]
    archive = crate_archive(root)
    package, release_version = publication_packages(root)[0]
    if check_registry(root, package, release_version):
        return
    check_registry_project_z(verified["normalized"])
    manifest = registry_export_manifest(root)
    lock = (archive.parent / REGISTRY_EXPORT_LOCK).read_bytes()
    if export_receipt(manifest, lock) != verified["export"]:
        raise ValueError("publication export manifest or frozen lock differs from the verified crate")
    with registry_export(root, source["commit"], manifest, lock) as (export, target, _):
        args = ["cargo", "publish", "--locked", "--no-verify", "--registry", "crates-io",
                "--package", package, "--target-dir", str(target), "--allow-dirty"]
        subprocess.run([*args, "--dry-run"], cwd=export, check=True)
        # Cargo 1.100 stages the archive used by publish in package/tmp-crate.
        fresh = target / "package" / "tmp-crate" / archive.name
        if (not fresh.is_file() or digest(fresh) != checksum or digest(archive) != checksum
                or release_source(root) != source):
            raise ValueError("fresh package bytes or source graph differ from the verified crate")
        require_registry_export(export, source["commit"], manifest, lock)
        subprocess.run(args, cwd=export, check=True)
        if (digest(fresh) != checksum or digest(archive) != checksum
                or release_source(root) != source):
            raise ValueError("generated crate bytes or source graph changed during Cargo publication")
    for _ in range(18):
        existing = registry_checksum(release_version, package)
        if existing is not None:
            if existing != checksum:
                raise ValueError(f"published crate checksum differs from the verified package: {package}")
            print(f"Verified crates.io publication of {package} {release_version}")
            return
        time.sleep(10)
    raise TimeoutError(f"crates.io has not exposed {package} {release_version} yet; rerun this tag's release")


def release_notes(root=None):
    root = root or ROOT
    release_version = version(root)
    policy = crate_publication(release_version)
    introduction = "Download an archive or use the attached installers. "
    if policy["status"] == "deferred":
        introduction += f"crates.io publication for {release_version} is deferred. {policy['reason']}\n\n"
    else:
        introduction += (
            "The separate `publish_crates` job verifies its upload. After it succeeds, install with "
            f"`cargo install ssmcp --version {release_version} --locked --bin ssmcp`.\n\n"
        )
    notes = introduction + (
        "Linux / macOS: `curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh`\n\n"
        "Windows PowerShell: `irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex`\n\n"
        "Linux archives include `ssmcp` and `ssmcpd`. macOS and Windows archives include the "
        "`ssmcp` client; the gateway daemon runs on Linux. Both x86_64 and ARM64 are available "
        "for each platform.\n\n"
        "Verify downloads against `SHA256SUMS`. `release-manifest.json` records the source "
        "commit, pinned submodule commits/trees and SHA-256 digest of every archive.\n"
    )
    document = root / "docs" / f"release-notes-{release_version}.md"
    if document.is_file():
        notes += "\n---\n\n" + document.read_text(encoding="utf-8").rstrip() + "\n"
    return notes


def find_release(repository, tag, root=None):
    """The tag endpoint excludes drafts; search all release pages before creating one."""
    root = root or ROOT
    endpoint = f"repos/{repository}/releases"
    found = subprocess.run(["gh", "api", f"{endpoint}/tags/{tag}"],
                           text=True, capture_output=True, cwd=root)
    if found.returncode == 0:
        return json.loads(found.stdout)
    if not re.search(r"\bHTTP 404\b", found.stderr):
        raise RuntimeError(found.stderr.strip())
    pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                               endpoint + "?per_page=100", cwd=root))
    matches = [item for page in pages for item in page if item["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError(f"multiple releases use tag {tag}; refusing to choose a draft")
    return matches[0] if matches else None


def should_mark_latest(repository, release_version, root=None):
    """Only a stable version newer than every published stable release is Latest."""
    candidate = version_key(release_version)
    if "-" in release_version:
        return False
    # The Latest pointer can already refer to an older release. Compare all
    # published stable versions, and fail closed if their metadata is unreadable.
    pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                               f"repos/{repository}/releases?per_page=100", cwd=root or ROOT))
    if not isinstance(pages, list) or not pages or any(not isinstance(page, list) for page in pages):
        raise ValueError("invalid releases listing; draft retained without choosing Latest")
    published = []
    for page in pages:
        for item in page:
            if not isinstance(item, dict) or any(type(item.get(flag)) is not bool
                                                  for flag in ("draft", "prerelease")):
                raise ValueError("invalid release metadata; draft retained without choosing Latest")
            if item["draft"] or item["prerelease"]:
                continue
            tag = item.get("tag_name")
            if not isinstance(tag, str) or not tag.startswith("v") or "-" in tag:
                raise ValueError("invalid stable release tag; draft retained without choosing Latest")
            published.append(version_key(tag[1:]))
    return all(candidate > existing for existing in published)


def release(tag, root=None):
    root = root or ROOT
    validate_tag(tag, root)
    checksums(root, recheck=True)
    repository = os.environ["GITHUB_REPOSITORY"]
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
        raise ValueError("invalid repository name")
    metadata = find_release(repository, tag, root)
    if metadata is None:
        with tempfile.TemporaryDirectory(prefix="ssmcp-notes-") as temporary:
            notes = Path(temporary) / "notes.md"
            notes.write_text(release_notes(root), encoding="utf-8")
            subprocess.run(["gh", "release", "create", tag, "--repo", repository, "--verify-tag",
                            "--draft", "--title", tag, "--notes-file", str(notes)], cwd=root, check=True)
        metadata = find_release(repository, tag, root)
        # Creation can precede listing visibility; retry reads, never creation.
        for delay in (1, 2, 4, 8):
            if metadata is not None:
                break
            time.sleep(delay)
            metadata = find_release(repository, tag, root)
        if metadata is None:
            raise RuntimeError(f"created draft for {tag} is not visible in the authenticated releases list; "
                               "draft retained: retry this tag's release job to resume")
    # Keep the resolved identity throughout upload and verification; a draft is
    # intentionally not available through /releases/tags/<tag> yet.
    endpoint = f"repos/{repository}/releases/{metadata['id']}"
    metadata = json.loads(command("gh", "api", endpoint, cwd=root))
    if metadata["tag_name"] != tag:
        raise ValueError("resolved release ID does not match the requested tag")
    def downloaded_digest(path):
        with tempfile.TemporaryDirectory(prefix="ssmcp-asset-") as temporary:
            subprocess.run(["gh", "release", "download", tag, "--repo", repository,
                            "--pattern", path.name, "--dir", temporary], cwd=root, check=True)
            return digest(Path(temporary) / path.name)

    paths = sorted(p for p in (root / "dist").iterdir() if p.is_file())
    expected = {p.name for p in paths}
    assets = {a["name"]: a for a in metadata["assets"]}
    if len(assets) != len(metadata["assets"]) or assets.keys() - expected:
        raise ValueError("existing release contains duplicate or unexpected assets")
    for path in paths:
        if path.name in assets:
            if (assets[path.name].get("digest") != "sha256:" + digest(path)
                    and downloaded_digest(path) != digest(path)):
                raise ValueError(f"existing release asset differs: {path.name}; assets are never overwritten")
            continue
        subprocess.run(["gh", "release", "upload", tag, str(path), "--repo", repository], cwd=root, check=True)
    uploaded = json.loads(command("gh", "api", endpoint, cwd=root))
    uploaded_assets = {a["name"]: a for a in uploaded["assets"]}
    if (len(uploaded_assets) != len(uploaded["assets"])
            or uploaded_assets.keys() != expected):
        raise ValueError("release upload is incomplete or includes unexpected assets")
    for path in paths:
        asset = uploaded_assets.get(path.name)
        if not asset or asset["size"] != path.stat().st_size:
            raise ValueError(f"release upload is incomplete: {path.name}")
        actual = asset.get("digest") or "sha256:" + downloaded_digest(path)
        if actual != "sha256:" + digest(path):
            raise ValueError(f"release upload checksum mismatch: {path.name}")
    if metadata["draft"]:
        release_version = version(root)
        prerelease = "-" in release_version
        latest = should_mark_latest(repository, release_version, root)
        subprocess.run(["gh", "release", "edit", tag, "--repo", repository, "--draft=false",
                        f"--prerelease={str(prerelease).lower()}",
                        f"--latest={str(latest).lower()}"], cwd=root, check=True)
    print(command("gh", "release", "view", tag, "--repo", repository, "--json", "url", "--jq", ".url", cwd=root))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("version")
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("tag", nargs="?")
    prepare_parser.add_argument("--auto", action="store_true",
                                help="legacy main preflight alias; never creates a tag or publishes")
    tag_parser = commands.add_parser("tag")
    tag_parser.add_argument("tag")
    tag_parser.add_argument("sha")
    bump_parser = commands.add_parser("bump")
    bump_parser.add_argument("level", choices=["patch", "minor", "major", "custom"])
    bump_parser.add_argument("--custom-version")
    bump_parser.add_argument("--dry-run", action="store_true")
    validate_parser = commands.add_parser("validate")
    validation_mode = validate_parser.add_mutually_exclusive_group(required=True)
    validation_mode.add_argument("--tag")
    validation_mode.add_argument("--build-only", action="store_true")
    validation_mode.add_argument("--event", action="store_true")
    commands.add_parser("package").add_argument("--target", required=True, choices=TARGETS)
    commands.add_parser("pack").add_argument("target", choices=TARGETS)
    gui = commands.add_parser("package-gui")
    gui.add_argument("--platform", required=True, choices=GUI_CLIENTS)
    gui.add_argument("--evidence", required=True)
    gui.add_argument("--binary")
    sums = commands.add_parser("checksums")
    sums.add_argument("--build-only", action="store_true")
    sums.add_argument("--recheck", action="store_true")
    commands.add_parser("dependency-graph")
    commands.add_parser("verify-source")
    commands.add_parser("verify-crate")
    commands.add_parser("check-registry")
    commands.add_parser("publish")
    commands.add_parser("publish-crate")
    commands.add_parser("release").add_argument("--tag", required=True)
    args = parser.parse_args()
    if args.command == "version":
        print(version())
    elif args.command == "prepare":
        prepare(args.tag, args.auto)
    elif args.command == "tag":
        create_tag(args.tag, args.sha)
    elif args.command == "bump":
        bump(args.level, custom_version=args.custom_version, dry_run=args.dry_run)
    elif args.command == "validate":
        if args.event:
            with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as stream:
                event = json.load(stream)
            tag, build_only = event_mode(os.environ["GITHUB_EVENT_NAME"], os.environ["GITHUB_REF"], event)
            validate_tag(tag, build_only=build_only)
        else:
            validate_tag(args.tag, build_only=args.build_only)
    elif args.command in {"package", "pack"}:
        package(args.target)
    elif args.command == "package-gui":
        package_gui(args.platform, args.evidence, args.binary)
    elif args.command == "checksums":
        checksums(build_only=args.build_only, recheck=args.recheck)
    elif args.command == "verify-source":
        verify_source()
    elif args.command == "dependency-graph":
        print(json.dumps(dependency_graph(), indent=2))
    elif args.command == "verify-crate":
        verify_crate()
    elif args.command == "check-registry":
        require_registry_mode(ROOT)
        check_registry_dependencies()
        for package_name, release_version in publication_packages():
            check_registry(package=package_name, release_version=release_version)
    elif args.command in {"publish", "publish-crate"}:
        publish()
    else:
        release(args.tag)


if __name__ == "__main__":
    main()
