#!/usr/bin/env python3
"""Validate, package and publish a tag using only Python's standard library."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
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
TAG = re.compile(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?")
ROOT = Path(__file__).resolve().parents[1]


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


def registry_dependencies(root=None):
    """Validate independent source pins; their repositories own publication."""
    root = root or ROOT
    with (root / "Cargo.toml").open("rb") as stream:
        manifest = tomllib.load(stream)
    dependencies = manifest.get("dependencies", {})
    result = []
    local = {}
    for name, package_name in (("zplugin", "zplugin"), ("zharness", "zharness"),
                               ("ztuio", "ztuio"), ("zuse", "zuse-computer")):
        dependency = dependencies.get(name)
        if dependency is None:
            continue
        expected_path = "crates/" + name
        if (not isinstance(dependency, dict)
                or dependency.get("package", name) != package_name
                or dependency.get("path") != expected_path
                or not isinstance(dependency.get("version"), str)
                or not dependency["version"].startswith("=")):
            raise ValueError(f"ssmcp must pin its local {name} package by path and exact version")
        release_version = dependency["version"][1:]
        version_key(release_version)
        path = root / expected_path / "Cargo.toml"
        if not path.is_file():
            raise ValueError(f"{name} source is missing; initialize the pinned submodules recursively")
        with path.open("rb") as stream:
            local[name] = tomllib.load(stream)
        package = local[name]["package"]
        source_version = package.get("version")
        if source_version == {"workspace": True}:
            source_version = local[name].get("workspace", {}).get("package", {}).get("version")
        if package["name"] != package_name or source_version != release_version:
            raise ValueError(f"{name} source and exact dependency version differ")
        if name == "ztuio" and local[name].get("lib", {}).get("name", name) != name:
            raise ValueError("the ztuio package must expose the ztuio library name")
        result.append((package_name, release_version))
    if "zharness" in local:
        if "zplugin" not in local:
            raise ValueError("the local zharness integration requires the same pinned zplugin source")
        expected = "=" + dict(result)["zplugin"]
        dependency = local["zharness"].get("dependencies", {}).get("zplugin")
        declared = dependency.get("version") if isinstance(dependency, dict) else dependency
        if declared != expected:
            raise ValueError("zharness and ssmcp must use the same exact zplugin version")
        patch = manifest.get("patch", {}).get("crates-io", {}).get("zplugin")
        if (not isinstance(patch, dict) or patch.get("path") != "crates/zplugin"
                or patch.get("package", "zplugin") != "zplugin"
                or patch.get("version") not in (None, expected, expected[1:])):
            raise ValueError("patch.crates-io must unify zplugin at the pinned local source")
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

def validate_package(tag, root):
    match = TAG.fullmatch(tag)
    if not match or any(part.isdigit() and len(part) > 1 and part.startswith("0")
                        for part in (match.group(4) or "").split(".")):
        raise ValueError("tag must be vMAJOR.MINOR.PATCH with an optional SemVer prerelease")
    actual = version(root)
    if tag != "v" + actual:
        raise ValueError(f"tag {tag} does not match Cargo.toml version {actual}")
    with (root / "Cargo.lock").open("rb") as stream:
        locked_packages = tomllib.load(stream)["package"]
    for name, package_version in publication_packages(root) + registry_dependencies(root):
        locked = [p["version"] for p in locked_packages if p["name"] == name]
        if locked != [package_version]:
            raise ValueError(f"Cargo.lock must contain exactly the manifest's {name} version")
    return actual


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
                 "build_only": str(build_only).lower()})


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
    root = root or ROOT
    old = version(root)
    # Validate every local dependency/lock entry before changing either app file.
    validate_package("v" + old, root)
    new = bumped_version(old, level, custom_version)
    manifest_path, lock_path = root / "Cargo.toml", root / "Cargo.lock"
    manifest, manifest_count = re.subn(
        r'(?m)^(version\s*=\s*")' + re.escape(old) + r'("\s*)$',
        lambda match: match[1] + new + match[2], manifest_path.read_text(encoding="utf-8"), count=1)
    lock, lock_count = re.subn(
        r'(\[\[package\]\]\nname = "ssmcp"\nversion = ")' + re.escape(old) + r'("\n)',
        lambda match: match[1] + new + match[2], lock_path.read_text(encoding="utf-8"))
    if manifest_count != 1 or lock_count != 1:
        raise ValueError("expected one matching ssmcp version in Cargo.toml and Cargo.lock")
    result = {"tag": "v" + new, "version": new, "dry_run": str(dry_run).lower()}
    if not dry_run:
        manifest_path.write_text(manifest, encoding="utf-8")
        lock_path.write_text(lock, encoding="utf-8")
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
        (stage / "docs").mkdir()
        shutil.copy2(root / "docs/releases.md", stage / "docs/releases.md")
        if "linux" in target:
            shutil.copy2(root / "src/code_mode.js", stage / "code-mode.js")
            shutil.copy2(root / "docs/deployment.md", stage / "docs/deployment.md")
            shutil.copytree(root / "packaging", stage / "packaging")
            shutil.copy2(root / "scripts/setup_server.py", stage / "setup-server")
        write_archive(stage, archive, epoch)
    checksum = digest(archive)
    (output / (archive.name + ".sha256")).write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(json.dumps({"archive": archive.name, "sha256": checksum, "bytes": archive.stat().st_size}))


def checksums(root=None):
    """Verify GitHub archives; Publisher raw-client release.json is separate."""
    root = root or ROOT
    release_version = version(root)
    output = root / "dist"
    expected = {archive_name(target, release_version) for target in TARGETS}
    actual = {p.name for p in output.iterdir() if p.name.endswith((".zip", ".tar.gz"))}
    if actual != expected:
        raise ValueError(f"release archive set differs: missing={sorted(expected-actual)}, extra={sorted(actual-expected)}")
    installers = {"install.sh", "install.ps1"}
    allowed = expected | installers | {name + ".sha256" for name in expected} | {"SHA256SUMS", "release-manifest.json"}
    unexpected = {p.name for p in output.iterdir()} - allowed
    if unexpected:
        raise ValueError(f"unexpected release files: {sorted(unexpected)}")
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
    (output / "SHA256SUMS").write_text("".join(f"{r['sha256']}  {r['name']}\n" for r in records), encoding="utf-8")
    manifest = {"package": "ssmcp", "version": release_version,
                "commit": command("git", "rev-parse", "HEAD", cwd=root),
                "submodules": source_submodules(root), "artifacts": records}
    (output / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
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
    for package, release_version in registry_dependencies(root):
        if registry_checksum(release_version, package) is None:
            raise ValueError(f"publish {package} {release_version} from its independent repository before releasing ssmcp")


def publish(root=None):
    root = root or ROOT
    check_registry_dependencies(root)
    pending = []
    # Check the entire verified set before the first irreversible upload.
    for package, release_version in publication_packages(root):
        if not check_registry(root, package, release_version):
            path = root / "target/package" / f"{package}-{release_version}.crate"
            pending.append((package, release_version, digest(path)))
    for package, release_version, checksum in pending:
        # Upload only packages verified from this application repository.
        subprocess.run(["cargo", "publish", "--locked", "--no-verify", "--registry", "crates-io", "--package", package],
                       cwd=root, check=True)
        for _ in range(18):
            existing = registry_checksum(release_version, package)
            if existing is not None:
                if existing != checksum:
                    raise ValueError(f"published crate checksum differs from the verified package: {package}")
                print(f"Verified crates.io publication of {package} {release_version}")
                break
            time.sleep(10)
        else:
            raise TimeoutError(f"crates.io has not exposed {package} {release_version} yet; rerun this tag's release")


def release_notes(root=None):
    root = root or ROOT
    release_version = version(root)
    notes = (
        "Download an archive or use the attached installers. The separate `publish_crates` "
        "job requires the independently published dependencies, then publishes ssmcp "
        "and verifies its checksum; after it succeeds, install with "
        f"`cargo install ssmcp --version {release_version} --locked --bin ssmcp`.\n\n"
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
    checksums(root)
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
    assets = {a["name"]: a for a in metadata["assets"]}
    for path in sorted((root / "dist").iterdir()):
        if not path.is_file():
            continue
        if path.name in assets:
            asset = assets[path.name]
            checksum = "sha256:" + digest(path)
            if asset.get("digest") != checksum:
                with tempfile.TemporaryDirectory(prefix="ssmcp-asset-") as temporary:
                    subprocess.run(["gh", "release", "download", tag, "--repo", repository,
                                    "--pattern", path.name, "--dir", temporary], cwd=root, check=True)
                    if digest(Path(temporary) / path.name) != digest(path):
                        raise ValueError(f"existing release asset differs: {path.name}; assets are never overwritten")
            continue
        subprocess.run(["gh", "release", "upload", tag, str(path), "--repo", repository], cwd=root, check=True)
    uploaded = json.loads(command("gh", "api", endpoint, cwd=root))
    uploaded_assets = {a["name"]: a for a in uploaded["assets"]}
    for path in (root / "dist").iterdir():
        asset = uploaded_assets.get(path.name)
        if not asset or asset["size"] != path.stat().st_size:
            raise ValueError(f"release upload is incomplete: {path.name}")
        if asset.get("digest") and asset["digest"] != "sha256:" + digest(path):
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
    commands.add_parser("checksums")
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
    elif args.command == "checksums":
        checksums()
    elif args.command == "check-registry":
        check_registry_dependencies()
        for package_name, release_version in publication_packages():
            check_registry(package=package_name, release_version=release_version)
    elif args.command in {"publish", "publish-crate"}:
        publish()
    else:
        release(args.tag)


if __name__ == "__main__":
    main()
