#!/usr/bin/env python3
"""Verify temporary bootstrap argv, server-only download and cleanup without network."""
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
    def run_fixture(self, os_name="Linux", arch="x86_64", downloader="curl", fail=False):
        with tempfile.TemporaryDirectory(prefix="ssmcp-bootstrap-test-") as temporary:
            root = Path(temporary)
            programs = root / "bin"
            programs.mkdir()
            downloads = root / "downloads"
            downloads.mkdir()
            for name in ["mktemp", "chmod", "rm"]:
                source = shutil.which(name)
                self.assertIsNotNone(source, name)
                (programs / name).symlink_to(source)
            uname = programs / "uname"
            uname.write_text("#!/bin/sh\ncase \"$1\" in -s) printf '%s\\n' \"$SSMCP_TEST_OS\";; -m) printf '%s\\n' \"$SSMCP_TEST_ARCH\";; esac\n")
            uname.chmod(0o700)
            fetch = programs / downloader
            fetch.write_text("""#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ["SSMCP_TEST_ROOT"])
(root / "download-argv.json").write_text(json.dumps(sys.argv[1:]))
if os.environ["SSMCP_TEST_FAIL"] == "1": sys.exit(22)
flag = "--output" if pathlib.Path(sys.argv[0]).name == "curl" else "-O"
output = pathlib.Path(sys.argv[sys.argv.index(flag)+1])
output.write_text('''#!/usr/bin/python3
import json, os, pathlib, sys
pathlib.Path(os.environ["SSMCP_TEST_ROOT"], "join-argv.json").write_text(json.dumps(sys.argv[1:]))
''')
""")
            fetch.chmod(0o700)
            script = SOURCE.replace("@@ORIGIN@@", "'" + ORIGIN + "'")
            secret = "ci_" + "a" * 64
            args = ["--server", ORIGIN, "--token", secret, "--name", "B '$() spaces"]
            env = dict(os.environ, PATH=str(programs), TMPDIR=str(downloads),
                       SSMCP_TEST_ROOT=str(root), SSMCP_TEST_OS=os_name,
                       SSMCP_TEST_ARCH=arch, SSMCP_TEST_FAIL="1" if fail else "0")
            result = subprocess.run(["/bin/sh", "-c", script, "sh", *args], env=env,
                                    stdin=subprocess.DEVNULL, capture_output=True, timeout=10)
            join = root / "join-argv.json"
            fetched = root / "download-argv.json"
            report = (result, json.loads(join.read_text()) if join.exists() else None,
                      json.loads(fetched.read_text()) if fetched.exists() else None)
            self.assertEqual(list(downloads.iterdir()), [], "temporary client was not removed")
            if result.returncode == 0:
                self.assertEqual(report[1], ["join", *args])
                self.assertNotIn(secret, result.stdout.decode() + result.stderr.decode())
                self.assertNotIn(secret, json.dumps(report[2]))
            return report

    def test_native_target_downloads_from_configured_server_and_preserves_argv(self):
        for os_name, arch, target in [
            ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
        ]:
            with self.subTest(target=target):
                result, _, request = self.run_fixture(os_name, arch)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                urls = [part for part in request if part.startswith("https://")]
                self.assertEqual(urls, [ORIGIN + "/client/bin/" + target])
                self.assertNotIn("--location", request)

    def test_wget_fallback_needs_no_archive_tool_or_runtime_on_device(self):
        result, _, request = self.run_fixture(downloader="wget")
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertIn("--max-redirect=0", request)
        self.assertIn("--tries=1", request)
        self.assertEqual([p for p in request if p.startswith("https://")],
                         [ORIGIN + "/client/bin/x86_64-unknown-linux-gnu"])

    def test_failed_download_never_starts_a_client(self):
        result, join, _ = self.run_fixture(fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(join)
        self.assertIn("client_download_dir", result.stderr.decode())

    def test_unsupported_platform_is_explicit(self):
        result, join, request = self.run_fixture(arch="riscv64")
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(join)
        self.assertIsNone(request)
        self.assertIn("architecture", result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
