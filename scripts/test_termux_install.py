"""Offline installation, version checks and updates of the native Termux CLI."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import zipfile

SCRIPT = Path(__file__).with_name("install-ssmcp-termux.sh")
TAG = "v0.10.54"


def executable(version):
    return f'#!/bin/sh\nif [ "$1" = --version ]; then echo "ssmcp {version}"; else printf "%s\\n" "$@"; fi\n'.encode()


DISPATCH = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
p = pathlib.Path
state = p(os.environ["FIXTURE"])
tool, args = p(sys.argv[0]).name, sys.argv[1:]
with (state / "calls").open("a") as log:
    log.write(json.dumps([tool, *args]) + "\n")
if tool == "uname":
    print(os.environ["ARCH"])
elif tool == "curl":
    url = next(a for a in args if a.startswith("https://"))
    if url.endswith("/releases/latest"):
        print("https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.54", end="")
    else:
        shutil.copyfile(state / "release" / url.split("/")[-1], args[args.index("-o") + 1])
elif tool == "git":
    if args == ["init", "-q", "source"]:
        p("source").mkdir()
elif tool == "cargo":
    assert args == ["build", "--locked", "--release", "--bin", "ssmcp"]
    if os.environ.get("BUILD_FAIL"):
        sys.exit(44)
    out = p(os.environ["CARGO_TARGET_DIR"]) / "release/ssmcp"
    out.parent.mkdir(parents=True)
    out.write_bytes((state / "client").read_bytes())
elif tool != "pkg":
    sys.exit(2)
'''


class TermuxInstall(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.release = self.base / "release"
        self.release.mkdir()
        tools = self.base / "tools"
        tools.mkdir()
        dispatcher = tools / "dispatch"
        dispatcher.write_text(DISPATCH)
        dispatcher.chmod(0o755)
        for name in ("pkg", "uname", "git", "curl", "cargo"):
            (tools / name).symlink_to(dispatcher.name)
        self.client = executable(TAG[1:])
        (self.base / "client").write_bytes(self.client)
        for name, entry in [("ssmcp-magisk-arm64.zip", "bin/ssmcp"),
                            ("ssmcp-android-debug.apk", "lib/x86_64/libssmcp.so")]:
            with zipfile.ZipFile(self.release / name, "w") as archive:
                archive.writestr(entry, self.client)
                archive.writestr("unused", "unrelated APK/module contents")
        self.checksums()
        self.dest = self.base / "prefix/bin/ssmcp"
        self.env = dict(os.environ, PREFIX=str(self.base / "prefix"), FIXTURE=str(self.base),
                        ARCH="aarch64", PATH=f"{tools}:/usr/bin:/bin")
        self.calls = self.base / "calls"

    def checksums(self):
        (self.release / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(f.read_bytes()).hexdigest()}  {f.name}\n"
            for f in sorted(self.release.glob("ssmcp-*"))))

    def install(self, *args, success=True):
        result = subprocess.run(["/bin/sh", SCRIPT, *args], env=self.env,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def test_native_android_assets_on_both_architectures(self):
        for arch, asset in [("aarch64", "ssmcp-magisk-arm64.zip"),
                            ("x86_64", "ssmcp-android-debug.apk")]:
            with self.subTest(arch=arch):
                self.env["ARCH"] = arch
                self.install()
                self.assertEqual(self.dest.read_bytes(), self.client)
                args = ["alice@relay.test", "--webui"]
                self.assertEqual(subprocess.check_output([self.dest, *args], text=True).splitlines(), args)
                calls = [json.loads(row) for row in self.calls.read_text().splitlines()]
                self.assertTrue(any(arg.endswith("/" + asset) for call in calls for arg in call))
                self.assertFalse(any(call[0] in ("git", "pkg", "cargo") for call in calls))
                self.calls.unlink()

    def test_invalid_tag_and_unsupported_architecture(self):
        for tag in ("v1.2.3; echo fail", "v1.2.3\nextra"):
            self.install("bin", tag, success=False)
        self.assertFalse(self.calls.exists())
        self.env["ARCH"] = "armv7l"
        self.install("bin", TAG, success=False)
        self.assertFalse(self.dest.exists())

    def test_checksum_error_preserves_previous_client(self):
        self.install()
        (self.release / "ssmcp-magisk-arm64.zip").write_bytes(b"corrupt")
        self.install("bin", TAG, success=False)
        self.assertEqual(self.dest.read_bytes(), self.client)

    def test_missing_or_duplicate_digest_fails_before_downloading_asset(self):
        for checksums in ["", (self.release / "SHA256SUMS").read_text() * 2]:
            (self.release / "SHA256SUMS").write_text(checksums)
            self.install("bin", TAG, success=False)
            self.assertNotIn("zip", self.calls.read_text())
            self.calls.unlink()
            self.assertFalse(self.dest.exists())

    def test_version_mismatch_preserves_existing_binary(self):
        self.install()
        with zipfile.ZipFile(self.release / "ssmcp-magisk-arm64.zip", "w") as archive:
            archive.writestr("bin/ssmcp", executable("0.0.1"))
        self.checksums()
        self.install("bin", TAG, success=False)
        self.assertEqual(self.dest.read_bytes(), self.client)

    def test_update_checks_before_replacing_and_never_downgrades(self):
        self.install()
        self.dest.write_bytes(executable("0.10.2"))
        self.assertIn("Update available", self.install("update", "--check").stdout)
        self.assertEqual(self.dest.read_bytes(), executable("0.10.2"))
        self.install("update")
        self.assertEqual(self.dest.read_bytes(), self.client)
        self.dest.write_bytes(executable("0.11.0"))
        self.install("update")
        self.assertEqual(self.dest.read_bytes(), executable("0.11.0"))

    def test_native_source_pins_tag_and_preserves_previous_binary_on_failure(self):
        self.install("source", TAG)
        calls = [json.loads(row) for row in self.calls.read_text().splitlines()]
        self.assertIn(["git", "fetch", "--depth", "1", "origin", "refs/tags/" + TAG], calls)
        self.assertIn(["git", "submodule", "update", "--init", "--recursive", "--depth", "1"], calls)
        self.assertEqual(self.dest.read_bytes(), self.client)
        previous = executable("0.10.2")
        self.dest.write_bytes(previous)
        self.env["BUILD_FAIL"] = "1"
        self.install("source", TAG, success=False)
        self.assertEqual(self.dest.read_bytes(), previous)


if __name__ == "__main__":
    unittest.main()
