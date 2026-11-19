#!/usr/bin/env python3
"""Exercise a release server's real code_mode sandbox from an isolated installation."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile


def install_archive(archive_path, binary, runner):
    """Copy only the two regular files required by the server runtime."""
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        for name, destination, mode in (("ssmcpd", binary, 0o755),
                                         ("code-mode.js", runner, 0o644)):
            matches = [member for member in members if member.name == name]
            if len(matches) != 1 or not matches[0].isfile():
                raise ValueError(f"release archive requires one regular {name} file")
            with archive.extractfile(matches[0]) as source, destination.open("wb") as output:
                shutil.copyfileobj(source, output)
            destination.chmod(mode)


def verify(binary_source=None, runner_source=None, archive_path=None):
    checks = []
    with tempfile.TemporaryDirectory(prefix="ssmcp server runtime ") as temporary:
        root = Path(temporary)
        prefix = root / "installed prefix"
        binary = prefix / "bin/ssmcpd"
        runner = prefix / "libexec/ssmcp/code-mode.js"
        binary.parent.mkdir(parents=True)
        runner.parent.mkdir(parents=True)
        if archive_path is not None:
            install_archive(archive_path, binary, runner)
        else:
            shutil.copy2(binary_source, binary)
            shutil.copy2(runner_source, runner)

        environment = os.environ.copy()
        environment.pop("SSMCP_CODE_MODE_RUNNER", None)
        # The diagnostic must work before configuration or credentials are available.
        environment["SSMCP_CONFIG"] = str(root / "configuration-does-not-exist.toml")

        def check(name, override=None, error=None):
            current = environment.copy()
            if override is not None:
                current["SSMCP_CODE_MODE_RUNNER"] = str(override)
            result = subprocess.run([str(binary), "check-runtime"], cwd=root, env=current,
                                    capture_output=True, text=True, timeout=30)
            if error is None:
                if result.returncode != 0:
                    raise AssertionError(f"{name}: {result.stderr.strip()}")
                if json.loads(result.stdout) != {"code_mode": "ok"}:
                    raise AssertionError(f"{name}: unexpected runtime result")
            elif result.returncode == 0 or error not in result.stderr or result.stdout.strip():
                raise AssertionError(f"{name}: expected failure containing {error!r}; "
                                     f"exit={result.returncode}, stderr={result.stderr.strip()!r}")
            checks.append(name)

        check("installed_release_with_space_in_prefix")
        override = root / "explicit runner.js"
        runner.rename(override)
        check("missing_installed_runner_is_rejected", error="code_mode runner is missing")
        check("absolute_override_without_installed_runner", override=override)
        shutil.copy2(override, runner)
        for name, invalid in (("relative_override", override.name),
                              ("missing_override", root / "missing-runner.js"),
                              ("directory_override", root)):
            check(name + "_is_rejected_with_installed_runner", override=invalid,
                  error="SSMCP_CODE_MODE_RUNNER must name an existing absolute runner file")
        check("installed_runner_remains_usable")
    return {"status": "passed", "checks": checks}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--archive", type=Path, help="native Linux release tar.gz to verify")
    source.add_argument("--binary", type=Path, help="release ssmcpd executable to verify")
    parser.add_argument("--runner", type=Path, help="code_mode.js for --binary")
    args = parser.parse_args()
    if (args.binary is None) != (args.runner is None):
        parser.error("--binary and --runner must be supplied together")
    if sys.platform != "linux":
        parser.error("the server runtime requires Linux")
    result = verify(args.binary, args.runner, args.archive)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
