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
    parser.add_argument(
        "--windows-smoke-binary", type=Path, metavar="EXE",
        help="install this locally built Windows client once and verify the success instructions",
    )
    args = parser.parse_args()
    name = release.asset_name(args.target)
    source = release.ROOT / "dist" / name
    windows = "windows" in args.target
    if args.windows_smoke_binary and not windows:
        parser.error("--windows-smoke-binary requires a Windows target")
    expected_version = "ssmcp " + release.version()

    with tempfile.TemporaryDirectory(prefix="ssmcp installer ") as temporary:
        root = Path(temporary)
        fixture = root / "downloads"
        fixture.mkdir()
        archive = fixture / name
        if args.windows_smoke_binary:
            stage = root / "stage"
            stage.mkdir()
            shutil.copyfile(args.windows_smoke_binary, stage / "ssmcp.exe")
            release.write_archive(stage, archive, 0)
        else:
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
            "SSMCP_TEST_DOWNLOAD_LOG": str(root / "download-routes.log"),
        })
        if windows:
            if not args.windows_smoke_binary:
                subprocess.run(
                    ["pwsh", "-NoProfile", "-File", str(release.ROOT / "scripts/test_installer_helpers.ps1")],
                    check=True,
                    timeout=120,
                )
            wrapper = root / "install-test.ps1"
            wrapper.write_text(
                """$ErrorActionPreference = 'Stop'
function Invoke-BoundedDownload {
    param([string]$Url, [string]$OutFile, [long]$MaxBytes)
    if ($MaxBytes -notin @(262144, 134217728)) { throw 'Missing independent byte bound.' }
    $parsed = [Uri]$Url
    if ($parsed.Host -notin @('github.com', 'ghfast.top', 'gh-proxy.com')) { throw 'Unexpected download source' }
    $filename = [IO.Path]::GetFileName($parsed.AbsolutePath)
    if ($filename -eq 'SHA256SUMS' -and $parsed.Host -ne 'github.com') { throw 'Untrusted checksum source' }
    [IO.File]::AppendAllText($env:SSMCP_TEST_DOWNLOAD_LOG, $Url + [Environment]::NewLine)
    if ($env:SSMCP_TEST_FAIL_DOWNLOAD) { throw 'fixture download failure' }
    $mode = $env:SSMCP_TEST_DOWNLOAD_MODE
    $primary = $parsed.Host -eq 'github.com'
    $asset = $filename -ne 'SHA256SUMS'
    if ($asset -and (($mode -eq 'primary_stall' -and $primary) -or
        ($mode -eq 'two_stalls' -and $parsed.Host -ne 'gh-proxy.com') -or
        $mode -eq 'all_stall' -or ($mode -eq 'mirror_tamper' -and $primary))) {
        [IO.File]::WriteAllText($OutFile, 'partial download')
        throw 'fixture body/idle deadline exceeded'
    }
    if ($asset -and (($mode -eq 'primary_tamper' -and $primary) -or $mode -eq 'mirror_tamper')) {
        [IO.File]::WriteAllText($OutFile, 'tampered archive')
    } else {
        Copy-Item -LiteralPath (Join-Path $env:SSMCP_TEST_DOWNLOADS $filename) -Destination $OutFile -Force
    }
}
# Replace only the network boundary in a parsed copy of the real installer.
# The real byte/deadline helper is exercised separately by test_installer_helpers.ps1.
$path = Join-Path $env:SSMCP_TEST_ROOT 'scripts/install.ps1'
$source = [IO.File]::ReadAllText($path)
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'Installer has PowerShell syntax errors.' }
$function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Invoke-BoundedDownload' }, $false)
if (-not $function) { throw 'Installer network boundary is missing.' }
$source = $source.Remove($function.Extent.StartOffset, $function.Extent.EndOffset - $function.Extent.StartOffset)
& ([ScriptBlock]::Create($source)) -Version $env:SSMCP_TEST_VERSION -InstallDir $env:SSMCP_TEST_INSTALL_DIR -NoModifyPath
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
assert sys.argv[1] == '--disable'
for flag in ('--connect-timeout', '--speed-time', '--speed-limit', '--max-time', '--max-filesize', '--proto', '--proto-redir'):
    assert flag in sys.argv, flag
url = next(arg for arg in sys.argv[1:] if arg.startswith('https://'))
parsed = urllib.parse.urlparse(url)
assert parsed.netloc in ('github.com', 'ghfast.top', 'gh-proxy.com')
name = pathlib.PurePosixPath(parsed.path).name
assert name != 'SHA256SUMS' or parsed.netloc == 'github.com'
with open(os.environ['SSMCP_TEST_DOWNLOAD_LOG'], 'a') as log:
    log.write(url + chr(10))
if os.environ.get('SSMCP_TEST_FAIL_DOWNLOAD'):
    sys.exit(22)
target = pathlib.Path(sys.argv[sys.argv.index('--output') + 1])
mode = os.environ.get('SSMCP_TEST_DOWNLOAD_MODE', '')
primary = parsed.netloc == 'github.com'
asset = name != 'SHA256SUMS'
if asset and ((mode == 'primary_stall' and primary) or
              (mode == 'two_stalls' and parsed.netloc != 'gh-proxy.com') or
              mode == 'all_stall' or (mode == 'mirror_tamper' and primary)):
    target.write_text('partial download')
    sys.exit(28)
if asset and ((mode == 'primary_tamper' and primary) or mode == 'mirror_tamper'):
    target.write_text('tampered archive')
else:
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
            if success and windows:
                expected = "ssmcp USERNAME@ssmcp.lmm.best"
                if expected not in [line.strip() for line in result.stdout.splitlines()]:
                    raise AssertionError("missing Windows account connection command:\n" + combined)

        run_install(True)
        actual = subprocess.check_output([str(installed), "--version"], text=True).strip()
        if actual != expected_version:
            raise AssertionError(actual)
        if args.windows_smoke_binary:
            if release.digest(installed) != release.digest(args.windows_smoke_binary):
                raise AssertionError("installed executable differs from this build")
            print("Windows installer passed: installed this build and printed the account connection command")
            return
        companion = install_dir / "zrun"
        if not windows:
            actual = subprocess.check_output([str(companion), "--version"], text=True).strip()
            version, _ = release.locked_zrun(release.ROOT)
            if actual != "zrun " + version:
                raise AssertionError(actual)
            companion.write_bytes(b"existing user zrun")
        installed.write_bytes(b"previous installed version")
        run_install(True)
        actual = subprocess.check_output([str(installed), "--version"], text=True).strip()
        if actual != expected_version:
            raise AssertionError(actual)
        if not windows and companion.read_bytes() != b"existing user zrun":
            raise AssertionError("the update replaced an existing zrun")
        if windows and (install_dir / "zrun.exe").exists():
            raise AssertionError("Windows installer installed zrun")
        before = release.digest(installed)

        for mode, expected_host in (
            ("primary_stall", "ghfast.top"),
            ("primary_tamper", "ghfast.top"),
            ("two_stalls", "gh-proxy.com"),
        ):
            env["SSMCP_TEST_DOWNLOAD_MODE"] = mode
            Path(env["SSMCP_TEST_DOWNLOAD_LOG"]).write_text("")
            run_install(True)
            routes = Path(env["SSMCP_TEST_DOWNLOAD_LOG"]).read_text(encoding="utf-8-sig").splitlines()
            if not routes[-1].startswith("https://" + expected_host + "/"):
                raise AssertionError("fallback did not reach expected source: " + repr(routes))
            if any("SHA256SUMS" in route and not route.startswith("https://github.com/") for route in routes):
                raise AssertionError("checksum trust root came from a mirror")
        for mode in ("all_stall", "mirror_tamper"):
            env["SSMCP_TEST_DOWNLOAD_MODE"] = mode
            run_install(False)
            if release.digest(installed) != before:
                raise AssertionError("failed fallback changed the installed executable")
        env.pop("SSMCP_TEST_DOWNLOAD_MODE", None)

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
        print("Installer passed: install/update, bounded downloads, two mirror fallbacks, stalled routes, tampering, version mismatch and preserved existing binary")


if __name__ == "__main__":
    main()
