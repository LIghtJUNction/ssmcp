#!/usr/bin/env python3
"""Install a native release archive with mocked downloads; never change user PATH."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

import release


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=release.TARGETS)
    args = parser.parse_args()
    name = release.asset_name(args.target)
    source = release.ROOT / "dist" / name
    windows = "windows" in args.target
    expected_version = "ssmcp " + release.version()

    with tempfile.TemporaryDirectory(prefix="ssmcp installer ") as temporary:
        root = Path(temporary)
        fixture = root / "downloads"
        fixture.mkdir()
        archive = fixture / name
        shutil.copyfile(source, archive)
        sums = fixture / "SHA256SUMS"
        sums.write_text(release.digest(archive) + "  " + name + "\n", encoding="ascii")
        install_dir = root / "user bin"
        installed = install_dir / ("ssmcp.exe" if windows else "ssmcp")
        env = os.environ.copy()
        env.update({
            "SSMCP_TEST_DOWNLOADS": str(fixture),
            "SSMCP_TEST_ROOT": str(release.ROOT),
            "SSMCP_TEST_INSTALL_DIR": str(install_dir),
            "SSMCP_TEST_VERSION": "v" + release.version(),
        })
        if windows:
            wrapper = root / "install-test.ps1"
            wrapper.write_text(
                """$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [int]$TimeoutSec, [string]$Uri, [string]$OutFile)
    if ($env:SSMCP_TEST_FAIL_DOWNLOAD) { throw 'Simulated download failure' }
    $filename = [IO.Path]::GetFileName(([Uri]$Uri).AbsolutePath)
    Copy-Item -LiteralPath (Join-Path $env:SSMCP_TEST_DOWNLOADS $filename) -Destination $OutFile
}
& (Join-Path $env:SSMCP_TEST_ROOT 'scripts/install.ps1') -Version $env:SSMCP_TEST_VERSION -InstallDir $env:SSMCP_TEST_INSTALL_DIR -NoModifyPath
""",
                encoding="utf-8",
            )
            command = ["pwsh", "-NoProfile", "-File", str(wrapper)]
        else:
            fake_bin = root / "bin"
            fake_bin.mkdir()
            curl = fake_bin / "curl"
            curl.write_text(
                "#!" + sys.executable + "\n"
                + """import os, pathlib, shutil, sys, urllib.parse
if os.environ.get('SSMCP_TEST_FAIL_DOWNLOAD'):
    sys.exit(22)
url = next(arg for arg in sys.argv[1:] if arg.startswith('https://'))
parsed = urllib.parse.urlparse(url)
assert parsed.netloc == 'github.com'
name = pathlib.PurePosixPath(parsed.path).name
target = sys.argv[sys.argv.index('--output') + 1]
shutil.copyfile(pathlib.Path(os.environ['SSMCP_TEST_DOWNLOADS']) / name, target)
""",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            env["PATH"] = str(fake_bin) + os.pathsep + env["PATH"]
            command = ["sh", str(release.ROOT / "scripts/install.sh"), "--dir", str(install_dir)]
            env["SSMCP_VERSION"] = env["SSMCP_TEST_VERSION"]

        def run_install(success, error=None):
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=60)
            combined = result.stdout + result.stderr
            if (result.returncode == 0) != success:
                raise AssertionError("unexpected installer outcome:\n" + combined)
            if error and error.lower() not in combined.lower():
                raise AssertionError("missing installer diagnostic: " + error + "\n" + combined)

        run_install(True)
        actual = subprocess.check_output([str(installed), "--version"], text=True).strip()
        if actual != expected_version:
            raise AssertionError(actual)
        installed.write_bytes(b"previous installed version")
        run_install(True)
        actual = subprocess.check_output([str(installed), "--version"], text=True).strip()
        if actual != expected_version:
            raise AssertionError(actual)
        before = release.digest(installed)

        with archive.open("ab") as handle:
            handle.write(b"damaged download")
        run_install(False, "checksum mismatch")
        if release.digest(installed) != before:
            raise AssertionError("corrupt download changed the installed executable")

        shutil.copyfile(source, archive)
        env["SSMCP_TEST_VERSION"] = "v9999.0.0"
        env["SSMCP_VERSION"] = "v9999.0.0"
        run_install(False, "version differs")
        if release.digest(installed) != before:
            raise AssertionError("version mismatch changed the installed executable")

        env["SSMCP_TEST_FAIL_DOWNLOAD"] = "1"
        run_install(False)
        if release.digest(installed) != before:
            raise AssertionError("failed download changed the installed executable")
        print("Installer passed: fresh install, update, corrupt archive, wrong version and download failure")


if __name__ == "__main__":
    main()
