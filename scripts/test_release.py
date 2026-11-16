"""Publication invariants: reject wrong tags, incomplete artifacts and collisions."""

import json
import io
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import unittest
from unittest import mock
import urllib.error
import zipfile

import release


class EventTests(unittest.TestCase):
    def test_only_version_tag_push_or_explicit_main_dispatch_can_publish(self):
        self.assertEqual(release.event_mode("push", "refs/tags/v0.5.0", {}), ("v0.5.0", False))
        self.assertEqual(release.event_mode("workflow_dispatch", "refs/heads/main", {"inputs": {"tag": "v0.5.0"}}), ("v0.5.0", False))

    def test_preflight_events_never_select_a_release_tag(self):
        for event_name, ref, event in (
            ("workflow_dispatch", "refs/heads/main", {"inputs": {"tag": ""}}),
            ("workflow_dispatch", "refs/heads/main", {}),
            ("workflow_dispatch", "refs/heads/main", {"inputs": {"build_only": False}}),
            ("workflow_dispatch", "refs/heads/main", {"inputs": {"build_only": "false"}}),
            ("workflow_dispatch", "refs/heads/main", {"inputs": {"build_only": True}}),
            ("workflow_dispatch", "refs/heads/main", {"inputs": {"build_only": "true"}}),
            ("create", "refs/heads/release-check", {"ref": "release-check", "ref_type": "branch"}),
            ("push", "refs/heads/release-check", {"created": False}),
        ):
            with self.subTest(event_name=event_name):
                self.assertEqual(release.event_mode(event_name, ref, event), (None, True))

    def test_unrelated_events_and_duplicate_branch_creation_push_are_rejected(self):
        for event_name, ref, event in (
            ("push", "refs/heads/main", {}),
            ("push", "refs/heads/feature", {}),
            ("push", "refs/heads/release-check", {"created": True}),
            ("create", "refs/heads/feature", {"ref": "feature", "ref_type": "branch"}),
            ("create", "refs/tags/v0.5.0", {"ref": "v0.5.0", "ref_type": "tag"}),
            ("create", "refs/heads/release-check", {"ref": "release-check", "ref_type": "tag"}),
            ("workflow_dispatch", "refs/heads/release-check", {"inputs": {"tag": "v0.5.0"}}),
        ):
            with self.subTest(event_name=event_name, ref=ref), self.assertRaises(ValueError):
                release.event_mode(event_name, ref, event)


    def test_dispatch_rejects_ambiguous_or_invalid_inputs(self):
        for inputs in ({"tag": "v0.5.0", "build_only": True},
                       {"tag": "v0.5.0", "build_only": "true"},
                       {"build_only": "yes"}, {"build_only": 1}, {"tag": 5}):
            with self.subTest(inputs=inputs), self.assertRaises(ValueError):
                release.event_mode("workflow_dispatch", "refs/heads/main", {"inputs": inputs})


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        # Local fixture repositories must not inherit developer signing, hooks,
        # templates, or GIT_DIR/GIT_CONFIG_* overrides from the invoking shell.
        self.git_env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.git_env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                             "GIT_TERMINAL_PROMPT": "0"})
        environment = mock.patch.dict(os.environ, self.git_env, clear=True)
        environment.start()
        self.addCleanup(environment.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix="ssmcp-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "ssmcp"\nversion = "0.5.0"\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "ssmcp"\nversion = "0.5.0"\n')
        self.git("init", "--quiet", "--template=", "-b", "main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("config", "commit.gpgSign", "false")
        self.git("config", "tag.gpgSign", "false")
        self.git("config", "core.hooksPath", os.devnull)
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("tag", "v0.5.0")

    def git(self, *args):
        try:
            return subprocess.check_output(["git", *args], cwd=self.root, env=self.git_env,
                                           text=True, stderr=subprocess.PIPE).strip()
        except subprocess.CalledProcessError as error:
            self.fail(f"fixture git {args!r} failed with exit {error.returncode} in {self.root}\n"
                      f"stdout: {error.output}\nstderr: {error.stderr}")

    def test_validate_tag_and_locked_version(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            result = release.validate_tag("v0.5.0", self.root)
        self.assertEqual(result["version"], "0.5.0")
        self.assertEqual(result["sha"], self.git("rev-parse", "HEAD"))
        self.assertEqual(result["prerelease"], "false")
        self.assertEqual(result["publish"], "true")

    def test_build_only_needs_no_tag_and_never_enables_publication(self):
        self.git("tag", "-d", "v0.5.0")
        output = self.root / "action-output"
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True):
            result = release.validate_tag(None, self.root, build_only=True)
        self.assertEqual(result["tag"], "")
        self.assertEqual(result["publish"], "false")
        self.assertIn("publish=false\n", output.read_text())
        self.assertNotIn("publish=true", output.read_text())

    def test_build_only_rejects_tag_and_still_checks_lockfile(self):
        with self.assertRaisesRegex(ValueError, "cannot select"):
            release.validate_tag("v0.5.0", self.root, build_only=True)
        (self.root / "Cargo.lock").write_text('[[package]]\nname="ssmcp"\nversion="0.4.0"\n')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            release.validate_tag(None, self.root, build_only=True)

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
        with self.assertRaises(subprocess.CalledProcessError):
            release.validate_tag(None, self.root, build_only=True)
        with self.assertRaises(subprocess.CalledProcessError):
            release.prepare(automatic=True, root=self.root)

    def test_reject_tag_outside_main(self):
        self.git("checkout", "--quiet", "-b", "unmerged")
        (self.root / "change").write_text("unmerged")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "unmerged")
        self.git("tag", "-f", "v0.5.0")
        with self.assertRaises(subprocess.CalledProcessError):
            release.validate_tag("v0.5.0", self.root)
        with self.assertRaises(subprocess.CalledProcessError):
            release.validate_tag(None, self.root, build_only=True)
        with self.assertRaises(subprocess.CalledProcessError):
            release.prepare(automatic=True, root=self.root)

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

    def test_installer_asset_names_and_flat_archive_layout_stay_compatible(self):
        stage = self.root / "stage"
        stage.mkdir()
        (stage / "ssmcp").write_bytes(b"fixture")
        tar_path, zip_path = self.root / "archive.tar.gz", self.root / "archive.zip"
        release.write_archive(stage, tar_path, 1700000000)
        release.write_archive(stage, zip_path, 1700000000)
        with tarfile.open(tar_path) as archive:
            self.assertEqual(archive.getnames(), ["ssmcp"])
        with zipfile.ZipFile(zip_path) as archive:
            self.assertEqual(archive.namelist(), ["ssmcp"])
        self.assertEqual(release.archive_name("x86_64-unknown-linux-gnu", "0.5.0"),
                         "ssmcp-x86_64-unknown-linux-gnu.tar.gz")
        self.assertEqual(release.archive_name("aarch64-pc-windows-msvc", "0.5.1"),
                         "ssmcp-aarch64-pc-windows-msvc.zip")
        with self.assertRaises(ValueError):
            release.archive_name("unsupported", "0.5.0")

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

    def test_legacy_main_prepare_is_always_preflight_even_without_a_tag(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(release.prepare(automatic=True, root=self.root)["publish"], "false")
            self.git("tag", "-d", "v0.5.0")
            prepared = release.prepare(automatic=True, root=self.root)
        self.assertEqual(prepared["publish"], "false")
        self.assertEqual(prepared["build_only"], "true")
        self.assertEqual(prepared["create_tag"], "false")
        self.assertEqual(prepared["tag"], "")
        self.assertEqual(prepared["sha"], self.git("rev-parse", "HEAD"))
        self.assertEqual(self.git("tag", "--list"), "")

    def test_release_notes_select_matching_version_and_preserve_installation(self):
        installation = release.release_notes(self.root)
        self.assertIn("cargo install ssmcp --version 0.5.0 --locked --bin ssmcp", installation)
        self.assertIn("download/install.sh", installation)
        self.assertIn("download/install.ps1", installation)
        self.assertIn("SHA256SUMS", installation)
        docs = self.root / "docs"
        docs.mkdir()
        (docs / "release-notes-0.4.1.md").write_text("Previous release\n", encoding="utf-8")
        self.assertEqual(release.release_notes(self.root), installation)
        content = "# 0.5.0 升级说明\n\n移除旧参数，保留安装方式。\n"
        (docs / "release-notes-0.5.0.md").write_text(content, encoding="utf-8")
        self.assertEqual(release.release_notes(self.root), installation + "\n---\n\n" + content)

    def test_release_lookup_finds_a_draft_after_tag_404_on_a_later_page(self):
        draft = {"id": 17, "tag_name": "v0.5.0", "draft": True, "assets": []}
        missing = subprocess.CompletedProcess([], 1, "", "gh: Not Found (HTTP 404)")
        pages = [[{"id": 2, "tag_name": "v0.4.1"}], [draft]]
        with mock.patch.object(release.subprocess, "run", return_value=missing), \
             mock.patch.object(release, "command", return_value=json.dumps(pages)):
            self.assertEqual(release.find_release("example/ssmcp", "v0.5.0", self.root), draft)

    def test_release_lookup_does_not_treat_other_http_errors_as_missing(self):
        for status in (403, 500):
            failed = subprocess.CompletedProcess([], 1, "", f"gh: request for v404.0.0 failed (HTTP {status})")
            with self.subTest(status=status), \
                 mock.patch.object(release.subprocess, "run", return_value=failed), \
                 mock.patch.object(release, "command") as listing, \
                 self.assertRaises(RuntimeError):
                release.find_release("example/ssmcp", "v404.0.0", self.root)
            listing.assert_not_called()

    def test_release_lookup_rejects_ambiguous_drafts(self):
        missing = subprocess.CompletedProcess([], 1, "", "gh: Not Found (HTTP 404)")
        drafts = [[{"id": 1, "tag_name": "v0.5.0"}, {"id": 2, "tag_name": "v0.5.0"}]]
        with mock.patch.object(release.subprocess, "run", return_value=missing), \
             mock.patch.object(release, "command", return_value=json.dumps(drafts)), \
             self.assertRaisesRegex(ValueError, "multiple releases"):
            release.find_release("example/ssmcp", "v0.5.0", self.root)

    def test_release_creates_or_resumes_draft_and_retries_without_duplicate_uploads(self):
        output = self.fixtures()
        real_run = subprocess.run
        endpoint = "repos/example/ssmcp/releases"

        def asset(path):
            return {"name": path.name, "size": path.stat().st_size,
                    "digest": "sha256:" + release.digest(path)}

        for resume in (False, True):
            with self.subTest(resume=resume):
                initial_assets = [asset(next(output.glob("*.zip")))] if resume else []
                state = {"release": {"id": 17, "tag_name": "v0.5.0", "draft": True,
                                     "assets": initial_assets} if resume else None,
                         "creates": 0, "uploads": [], "published": 0, "id_reads": 0}

                def fake_run(args, **kwargs):
                    if args[0] != "gh":
                        return real_run(args, **kwargs)
                    metadata = state["release"]
                    if args == ["gh", "api", endpoint + "/tags/v0.5.0"]:
                        if metadata is None or metadata["draft"]:
                            return subprocess.CompletedProcess(args, 1, "", "gh: Not Found (HTTP 404)")
                        return subprocess.CompletedProcess(args, 0, json.dumps(metadata), "")
                    if args == ("gh", "api", "--paginate", "--slurp", endpoint + "?per_page=100"):
                        return subprocess.CompletedProcess(args, 0, json.dumps([[metadata] if metadata else []]), "")
                    if args == ("gh", "api", endpoint + "/17"):
                        state["id_reads"] += 1
                        return subprocess.CompletedProcess(args, 0, json.dumps(metadata), "")
                    if list(args[:3]) == ["gh", "release", "create"]:
                        self.assertIsNone(metadata, "retry must reuse the existing draft")
                        state["creates"] += 1
                        state["release"] = {"id": 17, "tag_name": "v0.5.0", "draft": True, "assets": []}
                    elif list(args[:3]) == ["gh", "release", "upload"]:
                        path = Path(args[4])
                        self.assertNotIn(path.name, [item["name"] for item in metadata["assets"]])
                        metadata["assets"].append(asset(path))
                        state["uploads"].append(path.name)
                    elif list(args[:3]) == ["gh", "release", "edit"]:
                        self.assertEqual({item["name"] for item in metadata["assets"]},
                                         {path.name for path in output.iterdir()})
                        metadata["draft"] = False
                        state["published"] += 1
                    elif list(args[:3]) == ["gh", "release", "view"]:
                        return subprocess.CompletedProcess(args, 0, "https://example.invalid/v0.5.0\n", "")
                    else:
                        self.fail(f"unexpected GitHub request: {args!r}")
                    return subprocess.CompletedProcess(args, 0, "", "")

                with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "example/ssmcp"}), \
                     mock.patch.object(release.subprocess, "run", side_effect=fake_run):
                    release.release("v0.5.0", self.root)
                    uploaded_once = list(state["uploads"])
                    release.release("v0.5.0", self.root)
                self.assertEqual(state["creates"], 0 if resume else 1)
                self.assertEqual(state["uploads"], uploaded_once)
                self.assertEqual(state["published"], 1)
                self.assertEqual(state["id_reads"], 2)

    def test_validated_tag_push_is_idempotent_and_never_moves_a_tag(self):
        self.git("tag", "-d", "v0.5.0")
        sha = self.git("rev-parse", "HEAD")
        with tempfile.TemporaryDirectory(prefix="ssmcp-release-origin-") as remote:
            self.git("init", "--bare", "--quiet", "--template=", remote)
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

    def test_tag_retry_pushes_an_existing_local_tag_missing_from_origin(self):
        sha = self.git("rev-parse", "HEAD")
        with tempfile.TemporaryDirectory(prefix="ssmcp-release-origin-") as remote:
            self.git("init", "--bare", "--quiet", "--template=", remote)
            self.git("remote", "add", "origin", remote)
            self.git("push", "--quiet", "origin", "main")
            release.create_tag("v0.5.0", sha, self.root)
            published = subprocess.check_output(
                ["git", "--git-dir", remote, "rev-parse", "refs/tags/v0.5.0^{commit}"], text=True).strip()
            self.assertEqual(published, sha)

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
