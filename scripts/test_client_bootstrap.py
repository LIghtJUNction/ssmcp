#!/usr/bin/env python3
"""Offline bootstrap fixtures: bounded same-origin verification, atomic install and argv."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "src/client_downloads/install.sh").read_text()
ORIGIN = "https://ssmcp.example"


class Bootstrap(unittest.TestCase):
    def run_fixture(self, os_name="Linux", arch="x86_64", downloader="curl",
                    fault="", glibc="glibc 2.35", symlink="", rosetta=False):
        with tempfile.TemporaryDirectory(prefix="ssmcp-bootstrap-test-") as temporary:
            root = Path(temporary)
            programs = root / "bin"
            programs.mkdir()
            downloads = root / "downloads"
            downloads.mkdir()
            home = root / "home"
            home.mkdir()
            for name in ["mktemp", "chmod", "rm", "mkdir", "wc", "tr", "cat", "mv", "sha256sum"]:
                source = shutil.which(name)
                self.assertIsNotNone(source, name)
                (programs / name).symlink_to(source)
            for name, text in {
                "uname": '#!/bin/sh\ncase "$1" in -s) printf "%s\\n" "$SSMCP_TEST_OS";; -m) printf "%s\\n" "$SSMCP_TEST_ARCH";; esac\n',
                "getconf": '#!/bin/sh\nprintf "%s\\n" "$SSMCP_TEST_GLIBC"\n',
                "sysctl": '#!/bin/sh\nprintf "%s\\n" "$SSMCP_TEST_ROSETTA"\n',
                "ssmcp": "#!/bin/sh\nexit 93\n",
            }.items():
                path = programs / name
                path.write_text(text)
                path.chmod(0o700)
            installed_dir = home / "user bin ' spaces"
            actual_dir = home / "actual-bin" if symlink == "directory" else installed_dir
            actual_dir.mkdir()
            if symlink == "directory":
                installed_dir.symlink_to(actual_dir)
            installed = installed_dir / "ssmcp"
            previous = b"previous installed executable\n"
            if symlink == "target":
                victim = home / "victim"
                victim.write_bytes(previous)
                installed.symlink_to(victim)
            else:
                installed.write_bytes(previous)
                installed.chmod(0o700)
            executable = root / "client"
            executable.write_text("""#!/usr/bin/python3
import json, os, pathlib, sys
if sys.argv[1:] == ["--version"]:
    print("ssmcp " + os.environ["SSMCP_TEST_VERSION"])
else:
    pathlib.Path(os.environ["SSMCP_TEST_ROOT"], "join-argv.json").write_text(
        json.dumps({"path":sys.argv[0],"args":sys.argv[1:]}))
""" + "# fixture padding\n" * 1024)
            fetch = programs / downloader
            fetch.write_text("""#!/usr/bin/python3
import hashlib, json, os, pathlib, sys
root = pathlib.Path(os.environ["SSMCP_TEST_ROOT"])
with (root / "download-argv.jsonl").open("a") as output:
    output.write(json.dumps(sys.argv[1:]) + "\\n")
fault = os.environ["SSMCP_TEST_FAULT"]
if fault == "download": sys.exit(22)
flag = "--output" if pathlib.Path(sys.argv[0]).name == "curl" else "-O"
output = pathlib.Path(sys.argv[sys.argv.index(flag)+1])
url = next(value for value in sys.argv if value.startswith("https://"))
binary = (root / "client").read_bytes()
if "/client/release/" in url:
    if fault == "metadata_limit":
        output.write_bytes(b"x" * 20000)
    else:
        wire = "6" if fault == "wire" else "7"
        digest = "0" * 64 if fault == "hash" else hashlib.sha256(binary).hexdigest()
        size = len(binary) + (1 if fault == "size" else 0)
        output.write_text(f"ssmcp-release-v1\\n0.9.0\\n{wire}\\n{size}\\n{digest}\\n")
elif "/client/bin/" in url:
    output.write_bytes(binary[:20] if fault == "truncated" else binary)
else:
    raise SystemExit("unexpected origin or endpoint")
""")
            fetch.chmod(0o700)
            script = SOURCE.replace("@@ORIGIN@@", "'" + ORIGIN + "'").replace("@@WIRE@@", "7")
            secret = "ci_" + "a" * 64
            args = ["--allow", "alice@" + ORIGIN.removeprefix("https://"), "--name", "B '$() spaces"]
            env = dict(os.environ, PATH=str(programs), HOME=str(home),
                       XDG_CONFIG_HOME=str(home / "config"), XDG_STATE_HOME=str(home / "state"),
                       XDG_DATA_HOME=str(home / "data"), XDG_CACHE_HOME=str(home / "cache"),
                       XDG_RUNTIME_DIR=str(home / "runtime"), TMPDIR=str(downloads),
                       SSMCP_INSTALL_DIR=str(installed_dir), SSMCP_TEST_ROOT=str(root),
                       SSMCP_TEST_OS=os_name, SSMCP_TEST_ARCH=arch, SSMCP_TEST_FAULT=fault,
                       SSMCP_TEST_GLIBC=glibc, SSMCP_TEST_ROSETTA="1" if rosetta else "0",
                       SSMCP_TEST_VERSION="0.6.3" if fault == "version" else "0.9.0")
            result = subprocess.run(["/bin/sh", "-c", script, "sh", *args], env=env,
                                    stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
            join_path = root / "join-argv.json"
            request_path = root / "download-argv.jsonl"
            joined = json.loads(join_path.read_text()) if join_path.exists() else None
            requests = [json.loads(line) for line in request_path.read_text().splitlines()] if request_path.exists() else []
            self.assertEqual(list(downloads.iterdir()), [], "temporary downloads were not removed")
            self.assertEqual(list(actual_dir.glob(".ssmcp.new.*")), [], "staged executable was not removed")
            self.assertNotIn(secret, result.stdout.decode() + result.stderr.decode())
            self.assertNotIn(secret, json.dumps(requests))
            if result.returncode == 0:
                self.assertEqual(joined["args"], ["--server", ORIGIN, *args])
                self.assertEqual(joined["path"], str(installed), "connection must use the absolute installed path")
                self.assertEqual(installed.read_bytes(), executable.read_bytes())
                self.assertEqual(installed.stat().st_mode & 0o777, 0o700)
            else:
                self.assertIsNone(joined, "a rejected release must not start a controlled connection")
                self.assertEqual(installed.read_bytes(), previous, "failure changed the installed client")
            return result, joined, requests

    def test_native_targets_install_from_same_gateway_and_preserve_argv(self):
        for os_name, arch, target in [
            ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
        ]:
            with self.subTest(target=target):
                result, _, requests = self.run_fixture(os_name, arch)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                urls = [part for request in requests for part in request if part.startswith("https://")]
                self.assertEqual(urls, [ORIGIN + "/client/release/" + target, ORIGIN + "/client/bin/" + target])
                for request in requests:
                    self.assertIn("--max-redirs", request)
                    self.assertEqual(request[request.index("--max-redirs") + 1], "0")
                    self.assertIn("--max-filesize", request)

    def test_rosetta_selects_native_apple_silicon(self):
        result, _, requests = self.run_fixture("Darwin", "x86_64", rosetta=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertIn(ORIGIN + "/client/bin/aarch64-apple-darwin", requests[-1])

    def test_wget_fallback_keeps_the_same_verification(self):
        result, _, requests = self.run_fixture(downloader="wget")
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(len(requests), 2)
        for request in requests:
            self.assertIn("--max-redirect=0", request)
            self.assertIn("--tries=1", request)

    def test_failures_preserve_installation_and_do_not_join(self):
        for fault, error in [
            ("download", "client_download_dir"), ("hash", "SHA-256"),
            ("version", "version mismatch"), ("wire", "incompatible"),
            ("size", "incomplete"), ("truncated", "incomplete"),
            ("metadata_limit", "client_download_dir"),
        ]:
            with self.subTest(fault=fault):
                result, _, requests = self.run_fixture(fault=fault)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr.decode())
                if fault in ("wire", "metadata_limit", "download"):
                    self.assertEqual(len(requests), 1, "invalid metadata must prevent the binary request")

    def test_installation_symlinks_are_rejected_without_changing_their_target(self):
        for symlink in ["directory", "target"]:
            with self.subTest(symlink=symlink):
                result, _, _ = self.run_fixture(symlink=symlink)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("symlink", result.stderr.decode())

    def test_old_glibc_is_rejected_before_download(self):
        result, _, requests = self.run_fixture(glibc="glibc 2.31")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("2.35", result.stderr.decode())
        self.assertEqual(requests, [])

    def test_unsupported_platform_is_explicit(self):
        result, _, requests = self.run_fixture(arch="riscv64")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(requests, [])
        self.assertIn("architecture", result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
