"""Publication invariants: reject wrong tags, incomplete artifacts and collisions."""

import json
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
import urllib.error

import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ssmcp-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "ssmcp"\nversion = "0.5.0"\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "ssmcp"\nversion = "0.5.0"\n')
        self.git("init", "--quiet", "-b", "main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("tag", "v0.5.0")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True, stderr=subprocess.PIPE).strip()

    def test_validate_tag_and_locked_version(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            result = release.validate_tag("v0.5.0", self.root)
        self.assertEqual(result["version"], "0.5.0")
        self.assertEqual(result["sha"], self.git("rev-parse", "HEAD"))
        self.assertEqual(result["prerelease"], "false")

    def test_reject_mismatched_or_unsafe_tag(self):
        for tag in ("v0.5.1", "0.5.0", "v00.5.0", "v0.5.0\n", "v0.5.0/evil", "v0.5.0-01"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_tag(tag, self.root)

    def test_reject_lockfile_drift(self):
        (self.root / "Cargo.lock").write_text('[[package]]\nname="ssmcp"\nversion="0.4.0"\n')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            release.validate_tag("v0.5.0", self.root)

    def test_reject_dirty_source(self):
        with (self.root / "Cargo.toml").open("a") as stream:
            stream.write("# dirty\n")
        with self.assertRaises(subprocess.CalledProcessError):
            release.validate_tag("v0.5.0", self.root)

    def test_reject_tag_outside_main(self):
        self.git("checkout", "--quiet", "-b", "unmerged")
        (self.root / "change").write_text("unmerged")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "unmerged")
        self.git("tag", "-f", "v0.5.0")
        with self.assertRaises(subprocess.CalledProcessError):
            release.validate_tag("v0.5.0", self.root)

    def test_reject_wrong_checkout(self):
        (self.root / "change").write_text("later")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "later")
        with self.assertRaisesRegex(ValueError, "checkout"):
            release.validate_tag("v0.5.0", self.root)

    def test_archives_do_not_depend_on_source_mtime(self):
        stage = self.root / "stage"
        stage.mkdir()
        source = stage / "ssmcp"
        source.write_bytes(b"test binary\n")
        for extension in (".tar.gz", ".zip"):
            first, second = self.root / ("first" + extension), self.root / ("second" + extension)
            release.write_archive(stage, first, 1700000000)
            os.utime(source, (1700100000, 1700100000))
            release.write_archive(stage, second, 1700000000)
            self.assertEqual(release.digest(first), release.digest(second))

    def fixtures(self):
        output = self.root / "dist"
        output.mkdir()
        for target in release.TARGETS:
            path = output / release.archive_name(target, "0.5.0")
            path.write_bytes(target.encode())
            (output / (path.name + ".sha256")).write_text(f"{release.digest(path)}  {path.name}\n")
        scripts = self.root / "scripts"
        scripts.mkdir(exist_ok=True)
        for name in ("install.sh", "install.ps1"):
            (scripts / name).write_text(name)
            (output / name).write_text(name)
        return output

    def test_checksums_require_every_platform_and_match_manifest(self):
        output = self.fixtures()
        release.checksums(self.root)
        manifest = json.loads((output / "release-manifest.json").read_text())
        self.assertEqual(len(manifest["artifacts"]), 8)
        self.assertEqual(manifest["commit"], self.git("rev-parse", "HEAD"))
        self.assertEqual(len((output / "SHA256SUMS").read_text().splitlines()), 8)
        next(output.glob("*.zip")).unlink()
        with self.assertRaisesRegex(ValueError, "archive set"):
            release.checksums(self.root)

    def test_checksums_reject_tampered_download(self):
        output = self.fixtures()
        next(output.glob("*.zip")).write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            release.checksums(self.root)

    def test_checksums_reject_unexpected_release_files(self):
        output = self.fixtures()
        (output / "unexpected.txt").write_text("not a release asset")
        with self.assertRaisesRegex(ValueError, "unexpected release files"):
            release.checksums(self.root)

    def test_checksums_reject_changed_installers(self):
        output = self.fixtures()
        (output / "install.sh").write_text("not the validated installer")
        with self.assertRaisesRegex(ValueError, "installer is missing or differs"):
            release.checksums(self.root)

    def test_main_prepare_does_not_create_tag_and_skips_existing_versions(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(release.prepare(automatic=True, root=self.root)["publish"], "false")
            self.git("tag", "-d", "v0.5.0")
            prepared = release.prepare(automatic=True, root=self.root)
        self.assertEqual(prepared["publish"], "true")
        self.assertEqual(prepared["create_tag"], "true")
        self.assertEqual(prepared["sha"], self.git("rev-parse", "HEAD"))
        self.assertEqual(self.git("tag", "--list"), "")

    def test_validated_tag_is_pushed_once_and_never_moved(self):
        self.git("tag", "-d", "v0.5.0")
        sha = self.git("rev-parse", "HEAD")
        with tempfile.TemporaryDirectory(prefix="ssmcp-release-origin-") as remote:
            subprocess.run(["git", "init", "--bare", "--quiet", remote], check=True)
            self.git("remote", "add", "origin", remote)
            self.git("push", "--quiet", "origin", "main")
            release.create_tag("v0.5.0", sha, self.root)
            release.create_tag("v0.5.0", sha, self.root)
            published = subprocess.check_output(
                ["git", "--git-dir", remote, "rev-parse", "refs/tags/v0.5.0^{commit}"], text=True).strip()
            self.assertEqual(published, sha)
            with self.assertRaisesRegex(ValueError, "validated checkout"):
                release.create_tag("v0.5.0", "0" * 40, self.root)
            self.assertEqual(self.git("rev-parse", "refs/tags/v0.5.0^{commit}"), sha)

    def test_bump_updates_manifest_and_lock_together_without_dependency_changes(self):
        manifest = '[package]\nname = "ssmcp"\nversion = "0.5.9"\n\n[dependencies.fixture]\nversion = "0.5.9"\n'
        lock = 'version = 4\n\n[[package]]\nname = "ssmcp"\nversion = "0.5.9"\n\n[[package]]\nname = "fixture"\nversion = "0.5.9"\n'
        for level, expected in (("patch", "0.5.10"), ("minor", "0.6.0"), ("major", "1.0.0")):
            with self.subTest(level=level), mock.patch.dict(os.environ, {}, clear=True):
                (self.root / "Cargo.toml").write_text(manifest)
                (self.root / "Cargo.lock").write_text(lock)
                release.bump(level, self.root)
                self.assertEqual(release.version(self.root), expected)
                self.assertIn('[dependencies.fixture]\nversion = "0.5.9"', (self.root / "Cargo.toml").read_text())
                updated = (self.root / "Cargo.lock").read_text()
                self.assertIn('name = "ssmcp"\nversion = "' + expected + '"', updated)
                self.assertIn('name = "fixture"\nversion = "0.5.9"', updated)

    def test_inconsistent_lockfile_prevents_both_bump_writes(self):
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "ssmcp"\nversion = "0.4.0"\n')
        before = {name: (self.root / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        with self.assertRaises(ValueError):
            release.bump("patch", self.root)
        self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})

    def test_registry_only_treats_404_as_unpublished(self):
        for status in (404, 403, 500):
            error = urllib.error.HTTPError("https://crates.io/test", status, "test", {}, None)
            with mock.patch.object(release.urllib.request, "urlopen", side_effect=error):
                if status == 404:
                    self.assertIsNone(release.registry_checksum("0.5.0"))
                else:
                    with self.assertRaises(urllib.error.HTTPError):
                        release.registry_checksum("0.5.0")

    def test_registry_rejects_a_yanked_version(self):
        payload = io.BytesIO(json.dumps({"version": {"yanked": True, "checksum": "ignored"}}).encode())
        with mock.patch.object(release.urllib.request, "urlopen", return_value=payload), \
             self.assertRaisesRegex(ValueError, "yanked"):
            release.registry_checksum("0.5.0")

    def test_uploaded_crate_must_match_the_verified_package(self):
        path = self.root / "target/package/ssmcp-0.5.0.crate"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"crate")
        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=[None, "different"]), \
             mock.patch.object(release.subprocess, "run"), \
             self.assertRaisesRegex(ValueError, "published crate checksum differs"):
            release.publish(self.root)

    def test_published_identical_package_is_idempotent_without_secret(self):
        path = self.root / "target/package/ssmcp-0.5.0.crate"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"crate")
        with mock.patch.dict(os.environ, {}, clear=True), \
             mock.patch.object(release, "registry_checksum", return_value=release.digest(path)), \
             mock.patch.object(release.subprocess, "run") as publish:
            release.publish(self.root)
            publish.assert_not_called()

    def test_registry_collision_and_missing_credentials_prevent_publish(self):
        path = self.root / "target/package/ssmcp-0.5.0.crate"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"crate")
        for remote, message in (("different", "different bytes"), (None, "CARGO_REGISTRY_TOKEN")):
            with mock.patch.dict(os.environ, {}, clear=True), \
                 mock.patch.object(release, "registry_checksum", return_value=remote), \
                 mock.patch.object(release.subprocess, "run") as publish, \
                 self.assertRaisesRegex(ValueError, message):
                release.publish(self.root)
            publish.assert_not_called()


if __name__ == "__main__":
    unittest.main()
