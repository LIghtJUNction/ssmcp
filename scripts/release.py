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
    """Dependency-first package order; keep the Rust import name separate."""
    root = root or ROOT
    result = [("ssmcp", version(root))]
    member = root / "crates/ztui/Cargo.toml"
    if not member.is_file():
        return result
    with member.open("rb") as stream:
        core = tomllib.load(stream)
    if core["package"]["name"] != "ztui-core" or core.get("lib", {}).get("name") != "ztui":
        raise ValueError("the Project Z package must be ztui-core with library name ztui")
    with (root / "Cargo.toml").open("rb") as stream:
        dependency = tomllib.load(stream).get("dependencies", {}).get("ztui", {})
    core_version = core["package"]["version"]
    if (not isinstance(dependency, dict) or dependency.get("package") != "ztui-core"
            or dependency.get("path") != "crates/ztui"
            or dependency.get("version") != "=" + core_version):
        raise ValueError("ssmcp must pin its local ztui-core package by exact version")
    return [("ztui-core", core_version), *result]


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
    for name, package_version in publication_packages(root):
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
    return emit({"tag": tag or "", "version": actual, "sha": sha,
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


def bumped_version(old, level):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", old):
        raise ValueError("automatic bump expects a stable package version")
    parts = [int(part) for part in old.split(".")]
    index = {"major": 0, "minor": 1, "patch": 2}[level]
    parts[index] += 1
    parts[index + 1:] = [0] * (2 - index)
    return ".".join(map(str, parts))


def bump(level, root=None):
    root = root or ROOT
    packages = publication_packages(root)
    old = version(root)
    new = bumped_version(old, level)
    manifest_path, lock_path = root / "Cargo.toml", root / "Cargo.lock"
    manifest, manifest_count = re.subn(
        r'(?m)^(version\s*=\s*")' + re.escape(old) + r'("\s*)$',
        lambda match: match[1] + new + match[2], manifest_path.read_text(encoding="utf-8"), count=1)
    lock, lock_count = re.subn(
        r'(\[\[package\]\]\nname = "ssmcp"\nversion = ")' + re.escape(old) + r'("\n)',
        lambda match: match[1] + new + match[2], lock_path.read_text(encoding="utf-8"))
    if manifest_count != 1 or lock_count != 1:
        raise ValueError("expected one matching ssmcp version in Cargo.toml and Cargo.lock")
    writes = {}
    result = {"tag": "v" + new, "version": new}
    if packages[0][0] == "ztui-core":
        core_old = packages[0][1]
        core_new = bumped_version(core_old, "patch")
        core_path = root / "crates/ztui/Cargo.toml"
        core, core_count = re.subn(
            r'(?m)^(version\s*=\s*")' + re.escape(core_old) + r'("\s*)$',
            lambda match: match[1] + core_new + match[2], core_path.read_text(encoding="utf-8"), count=1)
        manifest, dependency_count = re.subn(
            r'(?m)^(ztui\s*=\s*\{[^\n]*?\bversion\s*=\s*")=' + re.escape(core_old) + r'(")',
            lambda match: match[1] + "=" + core_new + match[2], manifest)
        lock, core_lock_count = re.subn(
            r'(\[\[package\]\]\nname = "ztui-core"\nversion = ")' + re.escape(core_old) + r'("\n)',
            lambda match: match[1] + core_new + match[2], lock)
        if (core_count, dependency_count, core_lock_count) != (1, 1, 1):
            raise ValueError("expected one matching ztui-core version and exact dependency before bumping")
        writes[core_path] = core
        result["ztui_core_version"] = core_new
    writes[manifest_path], writes[lock_path] = manifest, lock
    for path, text in writes.items():
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
                "commit": command("git", "rev-parse", "HEAD", cwd=root), "artifacts": records}
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
        raise ValueError(f"run cargo package --workspace --locked before publishing {package}")
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


def publish(root=None):
    root = root or ROOT
    pending = []
    # Check the entire verified set before the first irreversible upload.
    for package, release_version in publication_packages(root):
        if not check_registry(root, package, release_version):
            path = root / "target/package" / f"{package}-{release_version}.crate"
            pending.append((package, release_version, digest(path)))
    for package, release_version, checksum in pending:
        # finalize jointly verified unpublished workspace dependencies.
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
        "job publishes ztui-core before ssmcp and verifies both checksums; after it succeeds, install with "
        f"`cargo install ssmcp --version {release_version} --locked --bin ssmcp`.\n\n"
        "Linux / macOS: `curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh`\n\n"
        "Windows PowerShell: `irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex`\n\n"
        "Linux archives include `ssmcp` and `ssmcpd`. macOS and Windows archives include the "
        "`ssmcp` client; the gateway daemon runs on Linux. Both x86_64 and ARM64 are available "
        "for each platform.\n\n"
        "Verify downloads against `SHA256SUMS`. `release-manifest.json` records the source "
        "commit and SHA-256 digest of every archive.\n"
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
        prerelease = "-" in version(root)
        subprocess.run(["gh", "release", "edit", tag, "--repo", repository, "--draft=false",
                        f"--prerelease={str(prerelease).lower()}",
                        f"--latest={str(not prerelease).lower()}"], cwd=root, check=True)
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
    commands.add_parser("bump").add_argument("level", choices=["patch", "minor", "major"])
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
        bump(args.level)
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
        for package_name, release_version in publication_packages():
            check_registry(package=package_name, release_version=release_version)
    elif args.command in {"publish", "publish-crate"}:
        publish()
    else:
        release(args.tag)


if __name__ == "__main__":
    main()
