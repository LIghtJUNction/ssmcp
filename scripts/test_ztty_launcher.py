#!/usr/bin/env python3
"""Exercise the optional launcher with private mock executables; opens no windows."""

import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


LAUNCHER = Path(__file__).with_name("ssmcp-ztty.sh")
MOCK = r'''
import base64, json, os, sys
kind = "version" if sys.argv[1:] == ["--version"] else os.environ["MOCK_KIND"]
with open(os.environ["MOCK_LOG"], "a", encoding="utf-8") as output:
    output.write(json.dumps({
        "kind": kind,
        "executable": os.path.abspath(sys.argv[0]),
        "argv": [base64.b64encode(os.fsencode(arg)).decode() for arg in sys.argv[1:]],
        "cwd": base64.b64encode(os.getcwdb()).decode(),
        "socket": os.environ.get("ZTTY_SOCKET"),
    }) + "\n")
if kind == "version":
    print(os.environ.get("MOCK_VERSION", "ztty 0.2.1"))
    sys.exit(int(os.environ.get("MOCK_VERSION_EXIT", "0")))
if kind == "terminal":
    command = sys.argv[sys.argv.index("-e") + 1:]
    os.environ["MOCK_KIND"] = "client"
    os.execv(command[0], command)
sys.exit(int(os.environ.get("MOCK_CLIENT_EXIT", "0")))
'''


@unittest.skipUnless(os.name == "posix", "the optional ztty launcher uses POSIX sh")
class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ssmcp ztty fixture ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / "home with spaces"
        self.bin = self.root / "path with spaces"
        self.cwd = self.root / "project 中文"
        self.cwd.mkdir()
        self.log = self.root / "calls.jsonl"
        self.ztty = self.make_binary(self.home / ".local/bin/ztty")
        self.client = self.make_binary(self.bin / "ssmcp")
        self.env = os.environ.copy()
        for name in ("SSMCP_ZTTY", "SSMCP_LAUNCH_CLIENT"):
            self.env.pop(name, None)
        self.env.update({
            "HOME": str(self.home),
            "PATH": str(self.bin) + os.pathsep + os.defpath,
            "ZTTY_SOCKET": "@old-user-broker-must-not-be-used",
            "MOCK_LOG": str(self.log),
            "MOCK_KIND": "terminal",
            "MOCK_VERSION": "ztty 0.2.1",
            "MOCK_VERSION_EXIT": "0",
            "MOCK_CLIENT_EXIT": "0",
        })

    def make_binary(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + MOCK, encoding="utf-8")
        path.chmod(0o755)
        return path

    def invoke(self, arguments=(), **environment):
        return subprocess.run(
            [b"/bin/sh", os.fsencode(LAUNCHER), *map(os.fsencode, arguments)],
            cwd=self.cwd,
            env={**self.env, **environment},
            capture_output=True,
            timeout=10,
        )

    def records(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_default_paths_preserve_cwd_and_every_argument_byte(self):
        arguments = [b"", b"--flag", b"white space", "中文", b"line\nbreak",
                     b"literal $(touch NEVER) ; & *", b'a"quote', b"trailing\\", b"invalid-\xff"]
        result = self.invoke(arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        probe, terminal, client = self.records()
        self.assertEqual([probe["kind"], terminal["kind"], client["kind"]], ["version", "terminal", "client"])
        expected = [os.fsencode(arg) for arg in arguments]
        self.assertEqual([base64.b64decode(arg) for arg in client["argv"]], expected)
        self.assertEqual([base64.b64decode(arg) for arg in terminal["argv"]],
                         [b"--name", b"ssmcp", b"--geometry", b"120x30", b"-e", os.fsencode(self.client), *expected])
        for record in (probe, terminal, client):
            self.assertEqual(base64.b64decode(record["cwd"]), os.fsencode(self.cwd))
            self.assertEqual(record["socket"], terminal["socket"])
            self.assertNotEqual(record["socket"], self.env["ZTTY_SOCKET"])
        self.assertFalse((self.cwd / "NEVER").exists())

    def test_explicit_paths_with_spaces_override_defaults(self):
        ztty = self.make_binary(self.root / "chosen terminal/ztty new")
        client = self.make_binary(self.root / "chosen client/ssmcp new")
        self.ztty.unlink()
        self.client.unlink()
        result = self.invoke(["user@host"], SSMCP_ZTTY=str(ztty), SSMCP_LAUNCH_CLIENT=str(client))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.records()[1]["executable"], str(ztty))
        self.assertEqual(self.records()[2]["executable"], str(client))

    def test_each_launch_replaces_an_inherited_socket(self):
        for _ in range(6):
            result = self.invoke()
            self.assertEqual(result.returncode, 0, result.stderr)
        sockets = [record["socket"] for record in self.records() if record["kind"] == "terminal"]
        self.assertEqual(len(set(sockets)), 6)
        for socket in sockets:
            self.assertRegex(socket, rf"^@ssmcp-ztty-{os.getuid()}-[0-9]+-[0-9]{{14}}$")

    def test_old_malformed_and_failed_versions_do_not_launch(self):
        for version in ("ztty 0.1.2", "ztty 0.2.0", "ztty 0.2.1-rc.1", "0.2.1", "ztty 0.2.1\nextra"):
            with self.subTest(version=version):
                result = self.invoke(MOCK_VERSION=version)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"0.2.1 or newer", result.stderr)
        result = self.invoke(MOCK_VERSION_EXIT="23")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"cannot read the ztty version", result.stderr)
        self.assertTrue(all(record["kind"] == "version" for record in self.records()))

    def test_newer_versions_compare_numerically(self):
        for version in ("ztty 0.2.2", "ztty 0.10.0", "ztty 1.0.0"):
            with self.subTest(version=version):
                result = self.invoke(MOCK_VERSION=version)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_invalid_explicit_paths_never_fall_back(self):
        not_executable = self.root / "not executable"
        not_executable.write_text("unusable")
        for name in ("SSMCP_ZTTY", "SSMCP_LAUNCH_CLIENT"):
            for value in ("", str(self.root / "missing"), str(not_executable), str(self.root)):
                with self.subTest(name=name, value=value):
                    result = self.invoke(**{name: value})
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b"executable not found", result.stderr)
        self.assertEqual(self.records(), [])

    def test_missing_path_client_is_clear(self):
        self.client.unlink()
        result = self.invoke(PATH=str(self.bin))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"ssmcp was not found on PATH", result.stderr)
        self.assertEqual(self.records(), [])

    def test_exit_status_is_propagated(self):
        result = self.invoke(MOCK_CLIENT_EXIT="37")
        self.assertEqual(result.returncode, 37, result.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, help="Optional JSON report path")
    args = parser.parse_args()
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(LauncherTests))
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps({"ok": result.wasSuccessful(), "tests": result.testsRun,
                                       "failures": len(result.failures), "errors": len(result.errors),
                                       "skipped": len(result.skipped),
                                       "scope": "POSIX sh with mock ztty/client executables; no desktop windows"}, indent=2) + "\n")
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
