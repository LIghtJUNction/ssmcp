"""Offline Git/archive checks; no Cargo or real SSMCP binary is executed."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock

import collect_native_validation_kit as collector


def git(root, *arguments):
    return subprocess.check_output([
        "git", "-C", str(root), "-c", "core.hooksPath=" + os.devnull,
        "-c", "user.name=Validation fixture", "-c", "user.email=fixture@example.invalid",
        *arguments,
    ], stderr=subprocess.PIPE).decode().strip()


class CollectorTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="ssmcp-kit-test-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.root = self.base / "repo"
        self.root.mkdir()
        git(self.root, "init", "--quiet")
        (self.root / "scripts").mkdir()
        shutil.copy2(Path(collector.__file__), self.root / collector.PRODUCER_SOURCE)
        (self.root / "src").mkdir()
        (self.root / "src/code_mode.js").write_text("// source runner fixture\n")
        (self.root / "README.md").write_text("Committed source fixture.\n")
        (self.root / ".gitignore").write_text("/target/\n__pycache__/\n")
        (self.root / "scripts/fixture.sh").write_text("#!/bin/sh\nexit 0\n")
        (self.root / "scripts/fixture.sh").chmod(0o755)
        (self.root / "scripts/readme-link").symlink_to("../README.md")
        git(self.root, "add", ".")
        dependency = self.base / "dependency"
        dependency.mkdir()
        git(dependency, "init", "--quiet")
        (dependency / "not-exported.txt").write_text("gitlink content\n")
        git(dependency, "add", ".")
        git(dependency, "commit", "--quiet", "-m", "Dependency fixture")
        self.dependency_commit = git(dependency, "rev-parse", "HEAD")
        (self.root / "crates").mkdir()
        git(self.root, "clone", "--quiet", "--no-hardlinks", str(dependency), "crates/dependency")
        git(self.root, "update-index", "--add", "--cacheinfo",
            "160000," + self.dependency_commit + ",crates/dependency")
        git(self.root, "commit", "--quiet", "-m", "Source fixture")
        self.debug = self.root / "target/debug"
        for name in ["ssmcp", "ssmcpd", "ssmcp-frontend", "examples/vt_snapshot", "deps/lib-tests"]:
            path = self.debug / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"Not an executable: offline artifact fixture " + name.encode())
            path.chmod(0o755)
        self.fixture = self.debug / "deps/lib-tests"
        self.identity_path = self.base / "identity.json"
        self.identity = collector.capture(self.root, self.identity_path)
        self.archive = self.base / (collector.KIT_NAME + ".tar.gz")
        self.listed = "".join((name + "sample" if name.endswith("::") else name) + ": test\n"
                              for name in collector.REQUIRED_SELECTORS)
        self.ignored = "http::relay_composer_fixture::relay_composer_server_fixture: test\n"
        listing = mock.patch.object(collector, "read_test_list", side_effect=
                                    lambda _path, ignored=False: self.ignored if ignored else self.listed)
        self.listing = listing.start()
        self.addCleanup(listing.stop)

    def collect(self):
        return collector.collect(self.root, self.identity_path, self.archive,
                                 str(self.fixture), {"fixture": "offline; no CI gates were run"})

    def test_archive_preserves_source_modes_symlinks_and_regular_binary_copies(self):
        result = self.collect()
        self.assertEqual(result["sha256"], hashlib.sha256(self.archive.read_bytes()).hexdigest())
        self.assertEqual(result["base_commit"], self.identity["base_commit"])
        with tarfile.open(self.archive, "r:gz") as bundle:
            names = set(bundle.getnames())
            read = lambda name: bundle.extractfile(name).read()
            manifest = json.loads(read("manifest.json"))
            self.assertEqual(manifest["status"], "passed_and_frozen")
            self.assertEqual(manifest["path_base"], "archive-root")
            self.assertEqual(manifest["source_digest"], self.identity["source_digest"])
            self.assertEqual(json.loads(read("source-files.json")), self.identity["entries"])
            self.assertEqual(manifest["source_tree"]["excluded_gitlinks"],
                             {"crates/dependency": self.dependency_commit})
            self.assertFalse(any(".git" in Path(name).parts for name in names))
            self.assertFalse(any(name.startswith(("source/target/", "source/crates/dependency/"))
                                 for name in names))
            link = bundle.getmember("source/scripts/readme-link")
            self.assertTrue(link.issym())
            self.assertEqual(link.linkname, "../README.md")
            self.assertEqual(bundle.getmember("source/scripts/fixture.sh").mode, 0o755)
            example = bundle.getmember("bin/examples/vt_snapshot")
            self.assertTrue(example.isreg())
            self.assertEqual(example.mode, 0o755)
            self.assertEqual(read("bin/examples/vt_snapshot"), read("bin/vt_snapshot"))
            for artifact in manifest["artifacts"].values():
                data = read(artifact["path"])
                self.assertEqual(hashlib.sha256(data).hexdigest(), artifact["sha256"])
                self.assertEqual(len(data), artifact["bytes"])
                self.assertEqual(bundle.getmember(artifact["path"]).mode, int(artifact["mode"], 8))
            self.assertEqual(manifest["producer"]["sha256"],
                             hashlib.sha256(read(manifest["producer"]["source"])).hexdigest())
            self.assertEqual(read("test-list.txt").decode(), self.listed)
        self.assertEqual(self.listing.call_count, 2)

    def test_dirty_and_untracked_source_are_rejected(self):
        for path in [self.root / "README.md", self.root / "untracked.txt"]:
            with self.subTest(path=path.name):
                previous = path.read_bytes() if path.exists() else None
                path.write_text("Unvalidated change.\n")
                with self.assertRaisesRegex(ValueError, "worktree must be clean"):
                    self.collect()
                if previous is None:
                    path.unlink()
                else:
                    path.write_bytes(previous)
                self.assertFalse(self.archive.exists())

    def test_clean_new_commit_and_tampered_digest_are_rejected(self):
        (self.root / "README.md").write_text("Different committed source.\n")
        git(self.root, "add", "README.md")
        git(self.root, "commit", "--quiet", "-m", "Changed source")
        with self.assertRaisesRegex(ValueError, "identity recorded before validation"):
            self.collect()
        tampered = {**self.identity, "source_digest": "0" * 64}
        self.identity_path.write_text(json.dumps(tampered))
        with self.assertRaisesRegex(ValueError, "digest does not match"):
            self.collect()
        self.assertFalse(self.archive.exists())

    def test_missing_fixture_never_triggers_a_build_or_listing(self):
        with self.assertRaisesRegex(ValueError, "SSMCP_E2E_FIXTURE_BINARY"):
            collector.collect(self.root, self.identity_path, self.archive, None, {})
        self.listing.assert_not_called()
        self.assertFalse(self.archive.exists())

    def test_corrupted_copy_is_rejected_before_an_archive_is_published(self):
        original_copy = shutil.copy2
        def corrupt(source, destination, **kwargs):
            original_copy(source, destination, **kwargs)
            if destination.name == "ssmcp":
                destination.write_bytes(b"changed bytes")
        with mock.patch.object(collector.shutil, "copy2", side_effect=corrupt):
            with self.assertRaisesRegex(ValueError, "file changed while freezing"):
                self.collect()
        self.assertFalse(self.archive.exists())

    def test_source_change_during_archiving_removes_the_incomplete_archive(self):
        original_add = tarfile.TarFile.add
        def change_source(bundle, name, arcname=None, *args, **kwargs):
            result = original_add(bundle, name, arcname, *args, **kwargs)
            if arcname == "source":
                (self.root / "README.md").write_text("Changed during archiving.\n")
            return result
        with mock.patch.object(collector.tarfile.TarFile, "add", new=change_source):
            with self.assertRaisesRegex(ValueError, "worktree must be clean"):
                self.collect()
        self.assertFalse(self.archive.exists())
        self.assertFalse(self.archive.with_suffix(".gz.sha256").exists())

    def test_missing_selectors_and_ignored_account_tests_are_rejected(self):
        listed = self.listed
        self.listed = "unrelated::test: test\n"
        with self.assertRaisesRegex(ValueError, "lib-tests is missing"):
            self.collect()
        self.listed = listed
        self.ignored += collector.DEFAULT_TEST_SELECTORS[0] + ": test\n"
        with self.assertRaisesRegex(ValueError, "ignored by the default Cargo test gate"):
            self.collect()
        self.assertFalse(self.archive.exists())


class EntryPointTests(unittest.TestCase):
    def test_fixture_enumeration_never_runs_test_bodies(self):
        with mock.patch.object(collector.subprocess, "run") as run:
            run.return_value.stdout = b"fixture::test: test\n"
            collector.read_test_list(Path("fixture"))
            self.assertEqual(run.call_args.args[0], ["fixture", "--list", "--format", "terse"])
            collector.read_test_list(Path("fixture"), ignored=True)
            self.assertEqual(run.call_args.args[0], ["fixture", "--list", "--format", "terse", "--ignored"])
            self.assertEqual(run.call_args.kwargs["timeout"], 30)

    def test_ci_provenance_keeps_workflow_and_checkout_refs_distinct(self):
        environment = {
            "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
            "GITHUB_REPOSITORY": "fixture/ssmcp", "GITHUB_RUN_ID": "12", "GITHUB_RUN_ATTEMPT": "2",
            "GITHUB_JOB": "linux", "GITHUB_WORKFLOW": "CI",
            "GITHUB_WORKFLOW_REF": "fixture/ssmcp/.github/workflows/ci.yml@refs/heads/main",
            "GITHUB_WORKFLOW_SHA": "a" * 40, "GITHUB_SHA": "b" * 40,
            "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_SERVER_URL": "https://github.invalid",
            "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        }
        provenance = collector.ci_provenance(environment)
        self.assertEqual(provenance["github_workflow_sha"], "a" * 40)
        self.assertEqual(provenance["github_sha"], "b" * 40)
        self.assertEqual(provenance["run_url"], "https://github.invalid/fixture/ssmcp/actions/runs/12")
        for name in ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "profile override"):
                collector.ci_provenance({**environment, name: "1"})

    def test_non_ci_or_wrong_platform_cannot_claim_success(self):
        for environment in [{}, {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "RUNNER_ARCH": "ARM64"},
                            {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64"}]:
            with self.subTest(environment=environment), self.assertRaises(ValueError):
                collector.ci_provenance(environment)


if __name__ == "__main__":
    unittest.main()
