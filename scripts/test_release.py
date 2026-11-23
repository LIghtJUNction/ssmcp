"""Publication invariants: reject wrong tags, incomplete artifacts and collisions."""

import base64
import contextlib
import json
import io
import os
from contextlib import contextmanager
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import tomllib
import tarfile
import unittest
from unittest import mock
import urllib.error
import zipfile

import bump_release
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


    def test_only_publication_is_serialized_across_release_tags(self):
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/release.yml").read_text()
        before, publication = workflow.split("  publish_release:\n", 1)
        publication, after = publication.split("\n  deploy_server:", 1)
        self.assertIn("    concurrency:\n      group: github-release-publication\n"
                      "      cancel-in-progress: false\n", publication)
        self.assertNotIn("    concurrency:\n", before + after)


class DeploymentPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Execute the actual current workflow policy. The release checkout may
        # be an older immutable tag whose helper knows only publish/build_only.
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
        lines = workflow.read_text(encoding="utf-8").splitlines(keepends=True)
        starts = [i for i, line in enumerate(lines) if line.strip() == "# BEGIN release deployment policy"]
        ends = [i for i, line in enumerate(lines) if line.strip() == "# END release deployment policy"]
        if len(starts) != 1 or len(ends) != 1 or starts[0] >= ends[0]:
            raise AssertionError("the workflow must contain one executable deployment policy")
        cls.policy = textwrap.dedent("".join(lines[starts[0] + 1:ends[0]]))

    def run_policy(self, event, *, name="workflow_dispatch", publish="true", existing="", version="0.10.13"):
        with tempfile.TemporaryDirectory(prefix="ssmcp-deployment-policy-") as temporary:
            root = Path(temporary)
            event_path, output = root / "event.json", root / "output"
            event_path.write_text(json.dumps(event), encoding="utf-8")
            output.write_text(existing, encoding="utf-8")
            environment = {"GITHUB_EVENT_NAME": name, "GITHUB_EVENT_PATH": str(event_path),
                           "GITHUB_OUTPUT": str(output), "RELEASE_PUBLISH": publish,
                           "RELEASE_VERSION": version}
            result = subprocess.run([sys.executable, "-I", "-c", self.policy],
                                    env=environment, capture_output=True, text=True, timeout=10)
            return result, output.read_text(encoding="utf-8")

    def assert_policy(self, event, expected, **kwargs):
        result, output = self.run_policy(event, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, f"deploy_server={expected}\n")
        self.assertEqual(json.loads(result.stdout)["deploy_server"], expected)

    def test_legacy_tag_without_deployment_output_keeps_the_default(self):
        # Only the old helper's validated publish field is available to the
        # current workflow; no deploy_server output is supplied by that tag.
        for event in ({}, {"inputs": {}}, {"inputs": {"tag": "v0.10.13"}}):
            with self.subTest(event=event):
                self.assert_policy(event, "true")

    def test_dispatch_accepts_exact_boolean_choices_for_old_or_new_tags(self):
        for requested, expected in ((True, "true"), ("true", "true"),
                                    (False, "false"), ("false", "false")):
            with self.subTest(requested=requested):
                self.assert_policy({"inputs": {"tag": "v0.10.13", "deploy_server": requested}}, expected)

    def test_tag_push_preserves_automatic_deployment(self):
        self.assert_policy({}, "true", name="push")
        self.assert_policy({"inputs": {"deploy_server": False}}, "true", name="push")

    def test_deferred_version_cannot_be_deployed_by_tag_or_dispatch(self):
        events = [("push", {})]
        events.extend(("workflow_dispatch", {"inputs": inputs}) for inputs in (
            {}, {"deploy_server": True}, {"deploy_server": "true"},
            {"deploy_server": False}, {"deploy_server": "false"}))
        for version in ("0.10.15", "0.10.16", "0.10.17", "0.10.18", "0.10.19", "0.10.20", "0.10.21", "0.10.22", "0.10.23", "0.10.24", "0.10.25", "0.10.26"):
            for name, event in events:
                with self.subTest(version=version, name=name, event=event):
                    result, output = self.run_policy(event, name=name, version=version)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output, "deploy_server=false\n")
                    report = json.loads(result.stdout)
                    self.assertEqual(report["publish"], "true")
                    self.assertEqual(report["version"], version)
                    self.assertIn("active relay sessions", report["deployment_deferred"])

    def test_other_versions_keep_existing_deployment_selection(self):
        for version in ("0.10.27", "0.10.24-rc.1"):
            with self.subTest(version=version):
                self.assert_policy({}, "true", name="push", version=version)
                self.assert_policy({"inputs": {"deploy_server": False}}, "false", version=version)

    def test_missing_or_malformed_version_cannot_bypass_deferral(self):
        for version in ("", "v0.10.15", "0.10", "0.10.15\n", "00.10.15",
                        "0.10.15+build", "0.10.15-rc.01", "0.10.15-rc..1"):
            with self.subTest(version=version):
                result, output = self.run_policy({}, version=version)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("validated version output", result.stderr)
                self.assertEqual(output, "")

    def test_build_only_never_deploys_even_when_requested(self):
        for name, event in (("workflow_dispatch", {}),
                            ("workflow_dispatch", {"inputs": {"deploy_server": True}}),
                            ("workflow_dispatch", {"inputs": {"deploy_server": "true"}}),
                            ("workflow_dispatch", {"inputs": {"deploy_server": False}}),
                            ("push", {}), ("create", {})):
            with self.subTest(name=name, event=event):
                self.assert_policy(event, "false", name=name, publish="false")

    def test_invalid_deployment_inputs_fail_before_writing_an_output(self):
        for requested in (None, 0, 1, "", "yes", "TRUE", "FALSE", "false\n", [], {}):
            for publish in ("true", "false"):
                with self.subTest(requested=requested, publish=publish):
                    result, output = self.run_policy({"inputs": {"deploy_server": requested}}, publish=publish)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("must be a boolean", result.stderr)
                    self.assertEqual(output, "")

    def test_malformed_input_containers_fail_before_writing_an_output(self):
        for inputs in (None, 0, 1, "false", [], ["deploy_server"]):
            with self.subTest(inputs=inputs):
                result, output = self.run_policy({"inputs": inputs})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must be an object", result.stderr)
                self.assertEqual(output, "")

    def test_missing_or_invalid_publication_output_never_enables_deployment(self):
        for publish in ("", "True", "1", "true\n", "unknown"):
            with self.subTest(publish=publish):
                result, output = self.run_policy({}, publish=publish)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("validated publish output", result.stderr)
                self.assertEqual(output, "")

    def test_policy_appends_one_result_without_replacing_existing_outputs(self):
        result, output = self.run_policy({"inputs": {"deploy_server": False}}, existing="retained=value\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "retained=value\ndeploy_server=false\n")


class PublicationContractTests(unittest.TestCase):
    def route(self, *, version="0.10.25", publish="true", build_only="false",
              contract="git-source-v1", crate="deferred", event="workflow_dispatch",
              ref="refs/heads/main", selected=None):
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
        source = workflow.read_text().split("# BEGIN release publication contract", 1)[1]
        policy = textwrap.dedent(source.split("# END release publication contract", 1)[0])
        tag = "" if build_only == "true" else "v" + version
        with tempfile.TemporaryDirectory(prefix="ssmcp-publication-policy-") as directory:
            event_path, output = Path(directory) / "event.json", Path(directory) / "outputs"
            event_path.write_text(json.dumps({"inputs": {"tag": selected if selected is not None else tag}}))
            env = {"RELEASE_VERSION": version, "RELEASE_TAG": tag, "RELEASE_PUBLISH": publish,
                   "RELEASE_BUILD_ONLY": build_only, "RELEASE_CONTRACT": contract,
                   "CRATE_PUBLICATION": crate, "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref,
                   "GITHUB_EVENT_PATH": str(event_path), "GITHUB_OUTPUT": str(output)}
            process = subprocess.run([sys.executable, "-I", "-c", policy], env=env,
                                     capture_output=True, text=True, timeout=10)
            return process, output.read_text() if output.exists() else ""

    def test_deferred_binary_releases_and_build_only_select_distinct_crate_modes(self):
        for flags, status in (({}, "deferred"),
                              ({"version": "0.10.26"}, "deferred"),
                              ({"version": "0.10.27"}, "deferred"),
                              ({"publish": "false", "build_only": "true", "crate": "not_requested"},
                               "not_requested"),
                              ({"version": "0.10.26", "publish": "false", "build_only": "true",
                                "crate": "not_requested"}, "not_requested"),
                              ({"version": "0.10.27", "publish": "false", "build_only": "true",
                                "crate": "not_requested"}, "not_requested"),
                              ({"version": "0.10.28", "crate": "registry"}, "registry")):
            with self.subTest(flags=flags):
                policy = release.crate_publication(
                    flags.get("version", "0.10.25"), build_only=flags.get("build_only") == "true")
                self.assertEqual(policy["status"], status)
                result, output = self.route(**flags)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, f"release_contract=git-source-v1\ncrate_publication={status}\n")

    def test_legacy_ref_retry_requires_a_matching_explicit_old_tag(self):
        old = {"version": "0.10.24", "contract": "", "crate": ""}
        result, output = self.route(**old)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "release_contract=legacy-tag-v1\ncrate_publication=registry\n")
        for flags in ({"event": "push"}, {"ref": "refs/heads/other"}, {"selected": ""},
                      {"version": "0.10.25"}, {"crate": "registry"},
                      {"publish": "false", "build_only": "true"}):
            with self.subTest(flags=flags):
                result, output = self.route(**(old | flags))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(output, "")

    def test_missing_or_mismatched_new_policy_cannot_fall_back_to_legacy(self):
        for flags in ({"crate": "registry"}, {"crate": ""}, {"contract": ""},
                      {"version": "0.10.26", "crate": "registry"},
                      {"version": "0.10.27", "crate": "registry"}, {"version": "0.10.28"},
                      {"contract": "unsupported"}, {"version": "0.10.25-01"}):
            with self.subTest(flags=flags):
                result, output = self.route(**flags)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(output, "")


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

    def stub_verified_crate(self, archive=None):
        source = {
            "commit": self.git("rev-parse", "HEAD"),
            "tree": self.git("rev-parse", "HEAD^{tree}"),
            "manifest_sha256": release.digest(self.root / "Cargo.toml"),
            "cargo_lock_sha256": release.digest(self.root / "Cargo.lock"),
            "submodules": release.source_submodules(self.root),
            "cargo": "fixture cargo",
            "rustc": "fixture rustc",
            "dependency_graph": {"cargo_lock_sha256": release.digest(self.root / "Cargo.lock"), "packages": []},
        }
        receipt = {"source": source, "normalized": {"packages": []}}
        if archive is not None:
            receipt["sha256"] = release.digest(archive)
            # Upload-boundary tests use an already verified fixture receipt.
            manifest = release.registry_export_manifest(self.root)
            lock = b"synthetic export lock"
            (archive.parent / release.REGISTRY_EXPORT_LOCK).write_bytes(lock)
            receipt["export"] = release.export_receipt(manifest, lock)

            @contextmanager
            def export(root, commit, actual_manifest, frozen_lock):
                self.assertEqual(root, self.root)
                self.assertEqual((commit, actual_manifest, frozen_lock), (source["commit"], manifest, lock))
                with tempfile.TemporaryDirectory() as temporary:
                    yield self.root, Path(temporary), lock

            for name, value in (("registry_export", export), ("require_registry_export", lambda *_: None)):
                patcher = mock.patch.object(release, name, value)
                patcher.start()
                self.addCleanup(patcher.stop)
        for name, value in (("verified_crate_receipt", receipt), ("release_source", source)):
            patcher = mock.patch.object(release, name, return_value=value)
            patcher.start()
            self.addCleanup(patcher.stop)
        release.verify_source(self.root)

    def same_dry_run_bytes(self, args, **kwargs):
        if "--dry-run" in args:
            archive = release.crate_archive(self.root)
            temporary = release.crate_archive(self.root, Path(args[args.index("--target-dir") + 1]))
            temporary.parent.mkdir(parents=True)
            temporary.write_bytes(archive.read_bytes())

    def gui_fixtures(self, *, package=True):
        self.desktop_version_fixture()
        for spec in release.GUI_CLIENTS.values():
            directory = self.root / spec["directory"]
            (directory / spec["entry"]).parent.mkdir(parents=True, exist_ok=True)
            if not (directory / spec["entry"]).exists():
                (directory / spec["entry"]).write_text("fixture application source\n")
            (directory / "README.md").write_text("fixture client instructions\n")
        (self.root / "LICENSE").write_text("fixture license\n")
        self.git("add", "apps", "LICENSE")
        self.git("commit", "--quiet", "-m", "GUI source fixtures")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        sha = self.git("rev-parse", "HEAD")
        environment = mock.patch.dict(os.environ, {
            "SOURCE_COMMIT": sha, "GITHUB_REPOSITORY": "fixture/ssmcp",
            "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_WORKFLOW_SHA": sha,
        })
        environment.start()
        self.addCleanup(environment.stop)
        fixtures = {}
        for platform, spec in release.GUI_CLIENTS.items():
            directory = self.root / "target/gui-fixtures" / platform
            directory.mkdir(parents=True)
            (directory / "build.log").write_text("fixture successful compilation\n")
            (directory / "tests.log").write_text(
                "INSTRUMENTATION_RESULT: stream=SSMCP protocol and controller checks passed: fixture\n"
                if platform == "android" else "test result: ok. 1 passed; 0 failed;\n"
                if platform == "windows" else "Executed 1 test, with 0 failures (0 unexpected)\n")
            (directory / "result.json").write_text(json.dumps({
                "source_commit": sha, "target": spec["target"], "profile": spec["profile"],
                "build_exit": 0, "test_exit": 0,
            }))
            binary = None
            # Structural fixture bytes only; these are not native acceptance tests.
            if platform == "windows":
                binary = directory / "ssmcp-desktop.exe"
                binary.write_bytes(b"MZfixture executable")
            elif platform == "android":
                binary = directory / "app-debug.apk"
                with zipfile.ZipFile(binary, "w") as archive:
                    archive.writestr("AndroidManifest.xml", "fixture manifest")
                    archive.writestr("classes.dex", "fixture bytecode")
            fixtures[platform] = (directory, binary)
            if package:
                release.package_gui(platform, directory, binary, self.root)
        return fixtures

    def test_gui_assets_and_simulator_receipt_join_the_same_release_manifest(self):
        output = self.fixtures()
        self.gui_fixtures()
        self.stub_verified_crate()
        release.checksums(self.root)
        release.checksums(self.root, recheck=True)
        manifest = json.loads((output / "release-manifest.json").read_text())
        self.assertEqual(len(manifest["artifacts"]), 13)
        self.assertEqual({row["platform"] for row in manifest["gui_clients"]}, {"windows", "android", "ios"})
        self.assertTrue(all(row["source"]["commit"] == manifest["commit"] for row in manifest["gui_clients"]))
        self.assertIsNone(next(row for row in manifest["gui_clients"] if row["platform"] == "ios")["asset"])
        with zipfile.ZipFile(output / release.GUI_CLIENTS["windows"]["asset"]) as archive:
            self.assertEqual(set(archive.namelist()), {"ssmcp-desktop.exe", "README.md", "LICENSE", "build-source.json"})
            self.assertEqual(archive.read("ssmcp-desktop.exe"), b"MZfixture executable")
        result = release.validate_tag(None, self.root, build_only=True)
        self.assertEqual(result["gui_clients"], "true")

    def test_apps_present_cannot_finalize_without_every_gui_asset_and_test_receipt(self):
        output = self.fixtures()
        self.gui_fixtures()
        self.stub_verified_crate()
        for platform, spec in release.GUI_CLIENTS.items():
            required = [release.gui_receipt_name(platform)]
            if spec["asset"]:
                required.extend([spec["asset"], spec["asset"] + ".sha256"])
            for name in required:
                with self.subTest(name=name):
                    path, data = output / name, (output / name).read_bytes()
                    path.unlink()
                    with self.assertRaises((ValueError, FileNotFoundError)):
                        release.checksums(self.root)
                    path.write_bytes(data)
        self.assertFalse((output / "release-manifest.json").exists())

    def test_gui_receipts_reject_other_sources_failed_tests_and_changed_artifacts(self):
        output = self.fixtures()
        self.gui_fixtures()
        self.stub_verified_crate()
        path = output / release.gui_receipt_name("android")
        original = path.read_bytes()
        for mutate in (
            lambda row: row["source"].update(commit="0" * 40),
            lambda row: row["source"].update(application_tree="0" * 40),
            lambda row: row.update(target="android-api-33-x86_64"),
            lambda row: row["checks"].update(test_exit=1),
            lambda row: row["checks"].update(test_exit=False),
            lambda row: row["evidence"].pop("tests.log"),
            lambda row: row["actions"].update(run_id=""),
            lambda row: row["asset"].update(sha256="0" * 64),
        ):
            row = json.loads(original)
            mutate(row)
            path.write_text(json.dumps(row))
            with self.assertRaises(ValueError):
                release.checksums(self.root)
            path.write_bytes(original)
        artifact = output / release.GUI_CLIENTS["android"]["asset"]
        artifact.write_bytes(artifact.read_bytes() + b"changed after tests")
        (output / (artifact.name + ".sha256")).write_text(f"{release.digest(artifact)}  {artifact.name}\n")
        with self.assertRaisesRegex(ValueError, "successful build"):
            release.checksums(self.root)

    def test_gui_packaging_requires_actual_success_and_requested_source(self):
        fixtures = self.gui_fixtures(package=False)
        evidence, binary = fixtures["windows"]
        result = evidence / "result.json"
        original = result.read_bytes()
        for exit_code in (1, False, "0"):
            receipt = json.loads(original)
            receipt["build_exit"] = exit_code
            result.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(ValueError, "exit receipts"):
                release.package_gui("windows", evidence, binary, self.root)
        result.write_bytes(original)
        with mock.patch.dict(os.environ, {"SOURCE_COMMIT": "0" * 40}):
            with self.assertRaisesRegex(ValueError, "immutable commit"):
                release.package_gui("windows", evidence, binary, self.root)
        with mock.patch.dict(os.environ, {"GITHUB_RUN_ID": ""}):
            with self.assertRaisesRegex(ValueError, "Actions run"):
                release.package_gui("windows", evidence, binary, self.root)
        self.assertFalse((self.root / "dist").exists())

    def test_android_custom_runner_requires_explicit_success_even_with_adb_exit_zero(self):
        passed = "INSTRUMENTATION_RESULT: stream=SSMCP protocol and controller checks passed: fixture\n"
        for text in (passed, passed + "INSTRUMENTATION_CODE: -1\n"):
            release.validate_android_instrumentation(text)
        for text in ("", "INSTRUMENTATION_CODE: -1\n", passed + "INSTRUMENTATION_CODE: 0\n",
                     "SSMCP protocol checks passed: outdated runner\n", passed + passed,
                     passed + "SSMCP check failed: AssertionError: fixture\n",
                     passed + "INSTRUMENTATION_FAILED: fixture\n"):
            with self.subTest(text=text), self.assertRaisesRegex(ValueError, "Instrumentation"):
                release.validate_android_instrumentation(text)

    def test_gui_native_test_logs_cannot_claim_a_zero_test_run(self):
        for platform, text in (
            ("windows", ""), ("windows", "test result: ok. 0 passed; 0 failed;\n"),
            ("ios", ""), ("ios", "Executed 0 tests, with 0 failures\n"),
            ("ios", "Executed 3 tests, with 1 failures\n"),
        ):
            with self.subTest(platform=platform, text=text), self.assertRaises(ValueError):
                release.validate_gui_test_log(platform, text)

    def test_incomplete_gui_source_cannot_silently_skip_the_release_gate(self):
        (self.root / "apps/ssmcp-android").mkdir(parents=True)
        with self.assertRaisesRegex(ValueError, "incomplete android"):
            release.validate_tag(None, self.root, build_only=True)

    def test_checksums_require_every_platform_and_match_manifest(self):
        output = self.fixtures()
        self.stub_verified_crate()
        release.checksums(self.root)
        manifest = json.loads((output / "release-manifest.json").read_text())
        self.assertEqual(len(manifest["artifacts"]), 8)
        self.assertEqual(manifest["commit"], self.git("rev-parse", "HEAD"))
        self.assertEqual(manifest["dependency_graph"]["cargo_lock_sha256"], release.digest(self.root / "Cargo.lock"))
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
        self.stub_verified_crate()
        release.checksums(self.root)
        real_run = subprocess.run
        endpoint = "repos/example/ssmcp/releases"

        def asset(path):
            return {"name": path.name, "size": path.stat().st_size,
                    "digest": "sha256:" + release.digest(path)}

        for resume in (False, True):
            with self.subTest(resume=resume):
                initial_assets = [asset(next(output.glob("*.zip")))] if resume else []
                state = {"release": {"id": 17, "tag_name": "v0.5.0", "draft": True,
                                     "prerelease": False, "assets": initial_assets} if resume else None,
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
                        state["release"] = {"id": 17, "tag_name": "v0.5.0", "draft": True,
                                            "prerelease": False, "assets": []}
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
                # Refresh the selected ID before upload and verify it afterward,
                # including the retry after publication.
                self.assertEqual(state["id_reads"], 4)

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

    def desktop_version_fixture(self):
        desktop = self.root / "apps/ssmcp-desktop"
        desktop.mkdir(parents=True)
        (desktop / "Cargo.toml").write_text(
            '[package]\nname = "ssmcp-desktop"\nversion = "0.1.0"\n'
            '[workspace]\n[dependencies]\nssmcp = { path = "../.." }\n'
        )
        (desktop / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "ssmcp"\nversion = "0.5.0"\n'
            '\n[[package]]\nname = "ssmcp-desktop"\nversion = "0.1.0"\ndependencies = ["ssmcp"]\n'
            '\n[[package]]\nname = "fixture"\nversion = "0.5.0"\n'
        )
        return desktop

    def test_bump_updates_desktop_path_lock_without_changing_app_or_other_packages(self):
        desktop = self.desktop_version_fixture()
        paths = [self.root / path for path in release.application_version_files(self.root)]
        before = {path: path.read_bytes() for path in paths}
        manifest = (desktop / "Cargo.toml").read_bytes()
        preview = release.bump("minor", self.root, dry_run=True)
        self.assertEqual(preview["version"], "0.6.0")
        self.assertEqual(before, {path: path.read_bytes() for path in paths})
        release.bump("minor", self.root)
        self.assertEqual(release.version(self.root), "0.6.0")
        self.assertEqual((desktop / "Cargo.toml").read_bytes(), manifest)
        self.assertEqual(
            {row["name"]: row["version"] for row in release.read_toml(desktop / "Cargo.lock")["package"]},
            {"ssmcp": "0.6.0", "ssmcp-desktop": "0.1.0", "fixture": "0.5.0"},
        )
        self.assertEqual(release.validate_package("v0.6.0", self.root), "0.6.0")

    def test_invalid_desktop_lock_prevents_every_version_write(self):
        desktop = self.desktop_version_fixture()
        path = desktop / "Cargo.lock"
        valid = path.read_text()
        variants = [
            None,
            valid.replace('name = "ssmcp"\nversion = "0.5.0"', 'name = "ssmcp"\nversion = "0.4.0"'),
            valid + '\n[[package]]\nname = "ssmcp"\nversion = "0.5.0"\n',
            valid.replace('name = "ssmcp"\nversion = "0.5.0"',
                          'name = "ssmcp"\nversion = "0.5.0"\nsource = "registry+https://example.invalid"'),
        ]
        for content in variants:
            with self.subTest(content=content):
                if content is None:
                    path.unlink()
                else:
                    path.write_text(content)
                files = [self.root / "Cargo.toml", self.root / "Cargo.lock", desktop / "Cargo.toml", path]
                before = {item: item.read_bytes() if item.exists() else None for item in files}
                with self.assertRaises(ValueError):
                    release.bump("minor", self.root)
                self.assertEqual(before, {
                    item: item.read_bytes() if item.exists() else None for item in files
                })

    def test_bump_workflow_commits_desktop_lock_and_reuses_the_same_commit(self):
        desktop = self.desktop_version_fixture()
        self.git("add", "apps/ssmcp-desktop")
        self.git("commit", "--quiet", "-m", "independent desktop fixture")
        baseline = self.git("rev-parse", "HEAD")
        with self.bump_origin() as remote:
            first = bump_release.prepare("minor", baseline, self.root)
            self.assertEqual(
                self.git("diff", "--name-only", baseline, first["sha"]).splitlines(),
                ["Cargo.lock", "Cargo.toml", "apps/ssmcp-desktop/Cargo.lock"],
            )
            self.assertEqual(self.git("status", "--porcelain"), "")
            self.assertIn('name = "ssmcp"\nversion = "0.6.0"', (desktop / "Cargo.lock").read_text())
            self.git("checkout", "--quiet", "--detach", baseline)
            repeated = bump_release.prepare("minor", baseline, self.root)
            self.assertEqual(repeated["sha"], first["sha"])
            self.assertEqual(repeated["status"], "reused")
            self.assertEqual(self.remote_commit(remote, "refs/tags/v0.6.0^{commit}"), first["sha"])

    @contextmanager
    def bump_origin(self):
        with tempfile.TemporaryDirectory(prefix="ssmcp-bump-origin-") as directory:
            remote = Path(directory)
            self.git("init", "--bare", "--quiet", "--template=", str(remote))
            self.git("remote", "add", "origin", str(remote))
            self.git("push", "--quiet", "origin", "main")
            yield remote

    def remote_commit(self, remote, ref="refs/heads/main"):
        return subprocess.check_output(
            ["git", "--git-dir", str(remote), "rev-parse", ref],
            text=True, env=self.git_env
        ).strip()

    def test_bump_workflow_none_current_preserves_v25_without_an_empty_commit(self):
        for name in ("Cargo.toml", "Cargo.lock"):
            path = self.root / name
            path.write_text(path.read_text().replace("0.5.0", "0.10.25"))
        self.git("commit", "--quiet", "-am", "current v25 fixture")
        baseline = self.git("rev-parse", "HEAD")
        before = {name: (self.root / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        with self.bump_origin() as remote:
            for level in ("none", "current"):
                result = bump_release.prepare(level, baseline, self.root)
                self.assertEqual(result["sha"], baseline)
                self.assertEqual(result["tag"], "v0.10.25")
                self.assertFalse(result["version_commit"])
                self.assertTrue(result["tag_prepared"])
                self.assertEqual(self.remote_commit(remote), baseline)
                self.assertEqual(self.remote_commit(remote, "refs/tags/v0.10.25^{commit}"), baseline)
                self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})
                bump_release.verify_dispatch_source(self.root, baseline, baseline, "v0.10.25")
        self.assertEqual(release.crate_publication("0.10.25")["status"], "deferred")

    def test_bump_workflow_rerun_reuses_original_commit_even_after_main_advances(self):
        baseline = self.git("rev-parse", "HEAD")
        with self.bump_origin() as remote:
            first = bump_release.prepare("patch", baseline, self.root)
            self.assertEqual(first["version"], "0.5.1")
            self.assertEqual(first["status"], "prepared")
            self.assertIn("[skip ci]", self.git("show", "-s", "--format=%B", first["sha"]))
            self.assertEqual(self.git("diff", "--name-only", baseline, first["sha"]).splitlines(),
                             ["Cargo.lock", "Cargo.toml"])
            (self.root / "new-work.txt").write_text("keep later work\n")
            self.git("add", "new-work.txt")
            self.git("commit", "--quiet", "-m", "concurrent work after the bump")
            advanced = self.git("rev-parse", "HEAD")
            self.git("push", "--quiet", "origin", "HEAD:main")
            self.git("checkout", "--quiet", "--detach", baseline)
            with mock.patch.dict(os.environ, {"GIT_AUTHOR_DATE": "2001-01-01T00:00:00Z",
                                               "GIT_COMMITTER_DATE": "2001-01-01T00:00:00Z"}):
                repeated = bump_release.prepare("patch", baseline, self.root)
            self.assertEqual(repeated["sha"], first["sha"])
            self.assertEqual(repeated["version"], "0.5.1")
            self.assertEqual(repeated["status"], "reused")
            self.assertEqual(self.remote_commit(remote), advanced)
            self.assertEqual(self.remote_commit(remote, "refs/tags/v0.5.1^{commit}"), first["sha"])

    def test_bump_workflow_stale_baseline_never_overwrites_new_main(self):
        baseline = self.git("rev-parse", "HEAD")
        with self.bump_origin() as remote:
            (self.root / "new-work.txt").write_text("keep another writer's change\n")
            self.git("add", "new-work.txt")
            self.git("commit", "--quiet", "-m", "main advanced before bump")
            advanced = self.git("rev-parse", "HEAD")
            self.git("push", "--quiet", "origin", "HEAD:main")
            self.git("checkout", "--quiet", "--detach", baseline)
            with self.assertRaisesRegex(ValueError, "main advanced"):
                bump_release.prepare("patch", baseline, self.root)
            self.assertEqual(self.remote_commit(remote), advanced)
            absent = subprocess.run(
                ["git", "--git-dir", str(remote), "show-ref", "--verify", "--quiet", "refs/tags/v0.5.1"],
                env=self.git_env, check=False
            )
            self.assertEqual(absent.returncode, 1)

    def test_bump_workflow_tag_collision_rejects_before_main_push(self):
        baseline = self.git("rev-parse", "HEAD")
        with self.bump_origin() as remote:
            self.git("tag", "v0.5.1", baseline)
            self.git("push", "--quiet", "origin", "refs/tags/v0.5.1")
            with self.assertRaisesRegex(ValueError, "another commit"):
                bump_release.prepare("patch", baseline, self.root)
            self.assertEqual(self.remote_commit(remote), baseline)
            self.assertEqual(self.remote_commit(remote, "refs/tags/v0.5.1^{commit}"), baseline)

    def test_bump_workflow_dry_run_never_writes_source_refs_or_tags(self):
        baseline = self.git("rev-parse", "HEAD")
        before = {name: (self.root / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        refs = self.git("show-ref")
        # There is deliberately no remote: a dry run must not even fetch it.
        for level in ("none", "patch", "minor", "major", "custom"):
            with self.subTest(level=level):
                result = bump_release.prepare(
                    level, baseline, self.root, custom_version="0.6.1" if level == "custom" else None,
                    dry_run=True
                )
                self.assertEqual(result["status"], "preview")
                self.assertIsNone(result["sha"])
                self.assertFalse(result["tag_prepared"])
                self.assertEqual(self.git("rev-parse", "HEAD"), baseline)
                self.assertEqual(self.git("show-ref"), refs)
                self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})
        with self.assertRaisesRegex(ValueError, "only valid"):
            bump_release.prepare("none", baseline, self.root, custom_version="0.6.1")
        with self.assertRaisesRegex(ValueError, "original workflow"):
            bump_release.prepare("none", "a" * 40, self.root)

    def test_bump_workflow_can_prepare_without_tagging_or_publishing(self):
        baseline = self.git("rev-parse", "HEAD")
        with self.bump_origin() as remote:
            result = bump_release.prepare("minor", baseline, self.root, publish=False)
            self.assertEqual(result["version"], "0.6.0")
            self.assertFalse(result["tag_prepared"])
            self.assertFalse(result["publish_requested"])
            self.assertEqual(self.remote_commit(remote), result["sha"])
            self.assertIsNone(bump_release.tag_commit(self.root, "v0.6.0"))

    def test_inconsistent_lockfile_prevents_both_bump_writes(self):
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "ssmcp"\nversion = "0.4.0"\n')
        before = {name: (self.root / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        with self.assertRaises(ValueError):
            release.bump("patch", self.root)
        self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})


    def stub_gitlinks(self, *names):
        patcher = mock.patch.object(release, "source_submodules", return_value=[
            {"path": "crates/" + name, "commit": "1" * 40, "tree": "2" * 40} for name in names
        ])
        patcher.start()
        self.addCleanup(patcher.stop)

    def add_ztuio_dependency(self, *, stub=True):
        if stub:
            self.stub_gitlinks("ztuio")
        member = self.root / "crates/ztuio/Cargo.toml"
        member.parent.mkdir(parents=True)
        member.write_text('[package]\nname = "ztuio"\nversion = "0.1.0"\n[lib]\nname = "ztuio"\n')
        with (self.root / "Cargo.toml").open("a") as stream:
            stream.write('\n[dependencies]\nztuio = { git = "https://github.com/LIghtJUNction/ztuio.git" }\n')
        with (self.root / "Cargo.lock").open("a") as stream:
            stream.write('\n[[package]]\nname = "ztuio"\nversion = "0.1.0"\n'
                         f'source = "git+{release.project_z_url("ztuio")}#{"1"*40}"\n')
        output = self.root / "target/package"
        output.mkdir(parents=True, exist_ok=True)
        (output / "ssmcp-0.5.0.crate").write_bytes(b"application crate")
        return member

    def test_ztuio_is_an_independently_published_exact_source_pin(self):
        member = self.add_ztuio_dependency()
        self.assertEqual(release.publication_packages(self.root), [("ssmcp", "0.5.0")])
        self.assertEqual(release.development_dependencies(self.root), [("ztuio", "0.1.0")])
        self.assertEqual(release.validate_package("v0.5.0", self.root), "0.5.0")
        manifest = self.root / "Cargo.toml"
        original = manifest.read_text()
        for changed in (original.replace('ztuio = {', 'ztuio = { version = "0.1.0",'),
                        original.replace('git = "https://github.com/LIghtJUNction/ztuio.git"', 'path = "other/ztuio"')):
            with self.subTest(manifest=changed):
                manifest.write_text(changed)
                with self.assertRaises(ValueError):
                    release.development_dependencies(self.root)
        manifest.write_text(original)
        member.write_text(member.read_text().replace('name = "ztuio"', 'name = "different"', 1))
        with self.assertRaisesRegex(ValueError, "source, gitlink and locked dependency version/commit differ"):
            release.development_dependencies(self.root)

    def test_ztuio_library_name_must_match_the_rust_import(self):
        member = self.add_ztuio_dependency()
        member.write_text(member.read_text().replace('[lib]\nname = "ztuio"', '[lib]\nname = "different"'))
        with self.assertRaisesRegex(ValueError, "library name"):
            release.development_dependencies(self.root)

    def test_ztuio_lock_mismatch_blocks_tag_validation(self):
        self.add_ztuio_dependency()
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('version = "0.1.0"', 'version = "0.0.9"'))
        with self.assertRaisesRegex(ValueError, "ztuio"):
            release.validate_package("v0.5.0", self.root)

    def test_application_bump_preserves_ztuio_version_and_source(self):
        member = self.add_ztuio_dependency()
        core_before = member.read_bytes()
        result = release.bump("minor", self.root)
        self.assertEqual(result["version"], "0.6.0")
        self.assertEqual(member.read_bytes(), core_before)
        self.assertEqual(release.development_dependencies(self.root), [("ztuio", "0.1.0")])
        self.assertNotIn("version", release.read_toml(self.root / "Cargo.toml")["dependencies"]["ztuio"])
        self.assertEqual(release.validate_package("v0.6.0", self.root), "0.6.0")

    def test_project_z_git_requires_canonical_url_and_default_branch(self):
        self.add_ztuio_dependency()
        manifest = self.root / "Cargo.toml"
        original = manifest.read_text()
        canonical = 'git = "https://github.com/LIghtJUNction/ztuio.git"'
        for replacement in (
            canonical.replace(".git", ""),
            canonical.replace("LIghtJUNction", "other"),
            canonical + ', rev = "' + "1" * 40 + '"',
            canonical + ', tag = "v0.1.0"',
            canonical + ', branch = "main"',
            canonical + ', version = "=0.1.0"',
            canonical + ', version = "0.1.0"',
            canonical + ', default-features = "true"',
            'path = "crates/ztuio"',
            'registry = "other"',
            'default-features = true',
        ):
            with self.subTest(replacement=replacement):
                manifest.write_text(original.replace(canonical, replacement))
                with self.assertRaisesRegex(ValueError, "canonical default-branch Git source"):
                    release.development_dependencies(self.root)

    def test_lock_rejects_registry_fallback_and_split_git_sources(self):
        self.add_ztuio_dependency()
        lock = self.root / "Cargo.lock"
        original = lock.read_text()
        canonical = f'git+{release.project_z_url("ztuio")}#{"1"*40}'
        for text in (
            original.replace(canonical, release.CRATES_IO),
            original.replace(canonical, canonical.replace(".git#", ".git?branch=main#")),
            original + f'\n[[package]]\nname = "ztuio"\nversion = "0.2.0"\nsource = "{canonical}"\n',
            original + f'\n[[patch.unused]]\nname = "zguard"\nversion = "0.1.0"\n'
                       f'source = "git+{release.project_z_url("zguard")}#{"2"*40}"\n',
        ):
            with self.subTest(lock=text):
                lock.write_text(text)
                with self.assertRaisesRegex(ValueError, "locked canonical|duplicate or split|unused Project Z override"):
                    release.validate_package("v0.5.0", self.root)

    def test_full_metadata_must_match_lock_and_actual_gitlink_tree(self):
        member = self.add_ztuio_dependency()
        revision, tree = "1" * 40, "2" * 40
        metadata = {
            "workspace_root": str(self.root), "packages": [
                {"id": "root", "name": "ssmcp", "version": "0.5.0", "source": None,
                 "manifest_path": str(self.root / "Cargo.toml")},
                {"id": "core", "name": "ztuio", "version": "0.1.0",
                 "source": f'git+{release.project_z_url("ztuio")}#{revision}', "manifest_path": str(member)},
            ],
            "resolve": {"root": "root", "nodes": [
                {"id": "root", "features": [], "deps": [{"name": "ztuio", "pkg": "core", "dep_kinds": []}]},
                {"id": "core", "features": [], "deps": []},
            ]},
        }
        real_command = release.command

        def command(*args, **options):
            if args[:1] == ("cargo",):
                self.assertEqual(args, ("cargo", "metadata", "--locked", "--format-version", "1"))
                return json.dumps(metadata)
            return real_command(*args, **options)

        with mock.patch.object(release, "command", side_effect=command), \
             mock.patch.object(release, "source_submodules", return_value=[
                 {"path": "crates/ztuio", "commit": revision, "tree": tree}]), \
             mock.patch.object(release, "cargo_git_checkout", return_value=(member.parent, tree)) as checkout:
            graph = release.dependency_graph(self.root)
            self.assertEqual(graph["packages"][1]["commit"], revision)
            checkout.assert_called_once_with(member, revision)
            checkout.return_value = (member.parent, "3" * 40)
            with self.assertRaisesRegex(ValueError, "differs from the ztuio source gitlink"):
                release.dependency_graph(self.root)
            metadata["packages"][1]["source"] = metadata["packages"][1]["source"].replace(revision, "4" * 40)
            with self.assertRaisesRegex(ValueError, "identities differ from Cargo.lock"):
                release.dependency_graph(self.root)

    def test_actual_cargo_checkout_requires_unmodified_locked_content(self):
        cargo_home = self.root / "cargo-home"
        checkout = cargo_home / "git/checkouts/ztuio-fixture/1234567"
        checkout.mkdir(parents=True)
        member = checkout / "Cargo.toml"
        member.write_text('[package]\nname = "ztuio"\nversion = "0.1.0"\n')
        for args in (("init", "--quiet", "--template="), ("add", "Cargo.toml")):
            release.command("git", *args, cwd=checkout)
        release.command("git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "--quiet", "-m", "source", cwd=checkout)
        revision = release.command("git", "rev-parse", "HEAD", cwd=checkout)
        (checkout / ".cargo-ok").touch()
        with mock.patch.dict(os.environ, {"CARGO_HOME": str(cargo_home)}):
            self.assertEqual(release.cargo_git_checkout(member, revision)[0], checkout)
            with self.assertRaisesRegex(ValueError, "differs from locked Git source"):
                release.cargo_git_checkout(member, "0" * 40)
            member.write_text(member.read_text() + "# modified")
            with self.assertRaisesRegex(ValueError, "uncommitted source"):
                release.cargo_git_checkout(member, revision)

    def test_shared_zplugin_requires_both_actual_graph_edges(self):
        packages = {"app": {"name": "ssmcp"}, "harness": {"name": "zharness"}, "plugin": {"name": "zplugin"}}
        nodes = {"app": {"deps": [{"pkg": "plugin"}]}, "harness": {"deps": [{"pkg": "plugin"}]}}
        release.require_shared_zplugin("app", packages, nodes)
        nodes["harness"]["deps"].clear()
        with self.assertRaisesRegex(ValueError, "same single zplugin"):
            release.require_shared_zplugin("app", packages, nodes)
        nodes["harness"]["deps"].append({"pkg": "split"})
        packages["split"] = {"name": "zplugin"}
        with self.assertRaisesRegex(ValueError, "same single zplugin"):
            release.require_shared_zplugin("app", packages, nodes)

    def test_bad_ztuio_lock_prevents_every_bump_write(self):
        member = self.add_ztuio_dependency()
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('version = "0.1.0"', 'version = "0.0.9"'))
        paths = [member, self.root / "Cargo.toml", lock]
        before = [path.read_bytes() for path in paths]
        with self.assertRaisesRegex(ValueError, "ztuio"):
            release.bump("patch", self.root)
        self.assertEqual(before, [path.read_bytes() for path in paths])

    def test_application_publication_never_uploads_ztuio(self):
        self.add_ztuio_dependency()
        archive = release.crate_archive(self.root)
        application = release.digest(archive)
        self.stub_verified_crate(archive)
        uploaded = []

        def registry(version, package):
            if package == "ztuio":
                self.assertEqual(version, "0.1.0")
                return "upstream-ztuio-checksum"
            self.assertEqual((version, package), ("0.5.0", "ssmcp"))
            return application if uploaded else None

        def upload(args, **kwargs):
            self.assertEqual(args[:9], ["cargo", "publish", "--locked", "--no-verify",
                                        "--registry", "crates-io", "--package", "ssmcp", "--target-dir"])
            if "--dry-run" in args:
                self.same_dry_run_bytes(args)
            else:
                uploaded.append(args[7])

        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token",
                                         "CARGO_REGISTRY_DEFAULT": "unrelated-fixture"}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=registry), \
             mock.patch.object(release.subprocess, "run", side_effect=upload):
            release.publish(self.root)
        self.assertEqual(uploaded, ["ssmcp"])

    def test_missing_ztuio_publication_stops_before_application_upload(self):
        self.add_ztuio_dependency()
        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", return_value=None), \
             mock.patch.object(release.subprocess, "run") as upload, \
             self.assertRaisesRegex(ValueError, "publish ztuio 0.1.0 from its independent repository"):
            release.publish(self.root)
        upload.assert_not_called()

    def test_application_collision_stops_after_independent_dependency_check(self):
        self.add_ztuio_dependency()
        self.stub_verified_crate(release.crate_archive(self.root))
        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=["upstream-checksum", "different"]), \
             mock.patch.object(release.subprocess, "run") as upload, \
             self.assertRaisesRegex(ValueError, "different bytes"):
            release.publish(self.root)
        upload.assert_not_called()

    def test_published_application_and_ztuio_retry_without_upload_credentials(self):
        self.add_ztuio_dependency()
        archive = release.crate_archive(self.root)
        self.stub_verified_crate(archive)
        application = release.digest(archive)
        with mock.patch.dict(os.environ, {}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=["upstream-checksum", application]), \
             mock.patch.object(release.subprocess, "run") as upload:
            release.publish(self.root)
        upload.assert_not_called()

    def test_failed_application_upload_never_uploads_the_independent_dependency(self):
        self.add_ztuio_dependency()
        self.stub_verified_crate(release.crate_archive(self.root))

        def upload(args, **kwargs):
            if "--dry-run" in args:
                self.same_dry_run_bytes(args)
            else:
                raise subprocess.CalledProcessError(1, "cargo")

        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=["upstream-checksum", None]), \
             mock.patch.object(release.subprocess, "run", side_effect=upload) as invoked, \
             self.assertRaises(subprocess.CalledProcessError):
            release.publish(self.root)
        self.assertEqual(invoked.call_count, 2)
        self.assertTrue(all(call.args[0][7] == "ssmcp" for call in invoked.call_args_list))
        self.assertIn("--dry-run", invoked.call_args_list[0].args[0])
        self.assertNotIn("--dry-run", invoked.call_args_list[1].args[0])

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
        self.stub_verified_crate(path)
        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", side_effect=[None, "different"]), \
             mock.patch.object(release.subprocess, "run", side_effect=self.same_dry_run_bytes), \
             self.assertRaisesRegex(ValueError, "published crate checksum differs"):
            release.publish(self.root)

    def test_fresh_dry_run_bytes_must_match_before_upload(self):
        path = release.crate_archive(self.root)
        path.parent.mkdir(parents=True)
        path.write_bytes(b"verified application crate")
        self.stub_verified_crate(path)

        def drift(args, **kwargs):
            self.assertIn("--dry-run", args)
            self.same_dry_run_bytes(args)
            fresh = release.crate_archive(self.root, Path(args[args.index("--target-dir") + 1]))
            fresh.write_bytes(b"different fresh crate")

        with mock.patch.dict(os.environ, {"CARGO_REGISTRY_TOKEN": "fixture-token"}, clear=True), \
             mock.patch.object(release, "registry_checksum", return_value=None), \
             mock.patch.object(release.subprocess, "run", side_effect=drift) as invoke, \
             self.assertRaisesRegex(ValueError, "fresh package bytes"):
            release.publish(self.root)
        invoke.assert_called_once()
        self.assertEqual(path.read_bytes(), b"verified application crate")

    def test_normalized_crate_must_match_original_source_and_registry_contract(self):
        self.add_ztuio_dependency()
        commit = self.git("rev-parse", "HEAD")
        src = self.root / "src/lib.rs"
        src.parent.mkdir()
        src.write_bytes(b"pub const FIXTURE: bool = true;\n")
        manifest = (self.root / "Cargo.toml").read_bytes()
        exported = release.registry_export_manifest(self.root)
        normalized = exported
        lock = ('version = 4\n[[package]]\nname = "ssmcp"\nversion = "0.5.0"\n'
                f'[[package]]\nname = "ztuio"\nversion = "0.1.0"\nsource = "{release.CRATES_IO}"\n'
                f'checksum = "{"2"*64}"\n').encode()
        vcs = json.dumps({"git": {"sha1": commit, "dirty": True}, "path_in_vcs": ""}).encode()
        files = {"Cargo.toml": normalized, "Cargo.toml.orig": exported, "Cargo.lock": lock,
                 ".cargo_vcs_info.json": vcs, "src/lib.rs": src.read_bytes()}

        def archive(**changes):
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode="w:gz") as output:
                for name, body in (files | changes).items():
                    member = tarfile.TarInfo(f"ssmcp-0.5.0/{name}")
                    member.size = len(body)
                    output.addfile(member, io.BytesIO(body))
            return data.getvalue()

        report, _ = release.normalized_crate(self.root, archive(), commit)
        self.assertEqual(report["packages"][1]["source"], release.CRATES_IO)
        for changes, error in (
            ({"Cargo.toml": manifest}, "not a crates.io version"),
            ({"Cargo.toml.orig": manifest}, "source identity differs"),
            ({".cargo_vcs_info.json": vcs.replace(b"true", b"false")}, "source identity differs"),
            ({"Cargo.toml": normalized.replace(b"=0.1.0", b"=0.1.1")}, "changed its package"),
            ({"Cargo.lock": lock.replace(release.CRATES_IO.encode(), b"git+https://example.org/pkg")}, "unexpected source"),
            ({".cargo_vcs_info.json": vcs.replace(commit.encode(), b"0" * 40)}, "source identity differs"),
            ({"src/lib.rs": b"// different source"}, "member differs"),
            ({"src\\outside.rs": b"// ambiguous archive path"}, "unsafe normalized package member"),
        ):
            with self.subTest(changes=changes):
                with self.assertRaisesRegex(ValueError, error):
                    release.normalized_crate(self.root, archive(**changes), commit)

        crate = release.crate_archive(self.root)
        crate.write_bytes(archive())
        (crate.parent / release.REGISTRY_EXPORT_LOCK).write_bytes(lock)
        source = {"commit": commit}
        graph = [
            {"name": "ssmcp", "version": "0.5.0", "source": None, "dependencies": [
                {"package": ["ztuio", "0.1.0", release.CRATES_IO]},
            ]},
            {"name": "ztuio", "version": "0.1.0", "source": release.CRATES_IO, "dependencies": []},
        ]
        receipt = {"source": source, "name": crate.name, "sha256": release.digest(crate),
                   "bytes": crate.stat().st_size, "normalized": report, "registry_graph": graph,
                   "export": release.export_receipt(exported, lock)}
        path = crate.parent / release.VERIFIED_CRATE
        with mock.patch.object(release, "release_source", return_value=source), \
             mock.patch.object(release, "normalized_registry_graph", return_value=graph):
            path.write_text(json.dumps(receipt))
            self.assertEqual(release.verified_crate_receipt(self.root), receipt)
            graph[0]["dependencies"].clear()
            path.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(ValueError, "missing direct Project Z dependency edges"):
                release.verified_crate_receipt(self.root)

    def test_published_identical_package_is_idempotent_without_secret(self):
        path = self.root / "target/package/ssmcp-0.5.0.crate"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"crate")
        self.stub_verified_crate(path)
        with mock.patch.dict(os.environ, {}, clear=True), \
             mock.patch.object(release, "registry_checksum", return_value=release.digest(path)), \
             mock.patch.object(release.subprocess, "run") as publish:
            release.publish(self.root)
            publish.assert_not_called()

    def test_registry_collision_and_missing_credentials_prevent_publish(self):
        path = self.root / "target/package/ssmcp-0.5.0.crate"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"crate")
        self.stub_verified_crate(path)
        for remote, message in (("different", "different bytes"), (None, "CARGO_REGISTRY_TOKEN")):
            with mock.patch.dict(os.environ, {}, clear=True), \
                 mock.patch.object(release, "registry_checksum", return_value=remote), \
                 mock.patch.object(release.subprocess, "run") as publish, \
                 self.assertRaisesRegex(ValueError, message):
                release.publish(self.root)
            publish.assert_not_called()



    def add_registry_dependencies(self):
        self.stub_gitlinks("zharness", "zplugin")
        with (self.root / "Cargo.toml").open("a") as stream:
            stream.write('\n[dependencies]\n'
                         'zharness = { git = "https://github.com/LIghtJUNction/zharness.git" }\n'
                         'zplugin = { git = "https://github.com/LIghtJUNction/zplugin.git" }\n'
                         '\n[patch.crates-io]\nzplugin = { git = "https://github.com/LIghtJUNction/zplugin.git" }\n')
        for name, version in (("zharness", "0.5.0"), ("zplugin", "0.3.0")):
            path = self.root / "crates" / name / "Cargo.toml"
            path.parent.mkdir(parents=True)
            path.write_text(f'[package]\nname = "{name}"\nversion = "{version}"\n'
                            + ('\n[dependencies]\nzplugin = { version = "=0.3.0" }\n'
                               if name == "zharness" else ""))
            with (self.root / "Cargo.lock").open("a") as stream:
                stream.write(f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
                             f'source = "git+{release.project_z_url(name)}#{"1"*40}"\n')

    def test_registry_dependency_uses_its_own_inherited_workspace_version(self):
        self.add_registry_dependencies()
        path = self.root / "crates/zplugin/Cargo.toml"
        manifest = path.read_text().replace('version = "0.3.0"', 'version.workspace = true')
        path.write_text(manifest + '\n[workspace.package]\nversion = "0.3.0"\n')
        self.assertEqual(release.development_dependencies(self.root),
                         [("zplugin", "0.3.0"), ("zharness", "0.5.0")])
        for workspace in ('\n[workspace.package]\nversion = "0.3.1"\n', ''):
            with self.subTest(workspace=workspace):
                path.write_text(manifest + workspace)
                with self.assertRaisesRegex(ValueError, "source, gitlink and locked dependency version/commit differ"):
                    release.development_dependencies(self.root)

    def test_registry_zharness_zplugin_requires_root_override(self):
        self.add_registry_dependencies()
        self.assertEqual(release.validate_package("v0.5.0", self.root), "0.5.0")
        manifest = self.root / "Cargo.toml"
        manifest.write_text(manifest.read_text().split("[patch.crates-io]", 1)[0])
        with self.assertRaisesRegex(ValueError, "registry zplugin.*canonical crates.io patch"):
            release.validate_package("v0.5.0", self.root)

    def test_canonical_git_zharness_zplugin_needs_no_root_override(self):
        self.add_registry_dependencies()
        manifest = self.root / "Cargo.toml"
        manifest.write_text(manifest.read_text().split("[patch.crates-io]", 1)[0])
        harness = self.root / "crates/zharness/Cargo.toml"
        upstream = harness.read_text()
        canonical = 'git = "https://github.com/LIghtJUNction/zplugin.git"'
        for source, version, expected in (
            (canonical, "=0.3.0", None),
            (canonical, None, None),
            (canonical.replace("LIghtJUNction", "other"), "=0.3.0", "canonical default-branch Git source"),
            (canonical + ', rev = "' + "2" * 40 + '"', "=0.3.0", "canonical default-branch Git source"),
            (canonical + ', tag = "v0.3.0"', "=0.3.0", "canonical default-branch Git source"),
            (canonical + ', branch = "main"', "=0.3.0", "canonical default-branch Git source"),
            (canonical, "=0.3.1", "requires the same locked zplugin version"),
            ('path = "../zplugin"', "=0.3.0", "must resolve zplugin through its canonical Git source"),
            ('registry = "other"', "=0.3.0", "must resolve zplugin through its canonical Git source"),
        ):
            with self.subTest(source=source, version=version):
                selector = source if version is None else f'version = "{version}", {source}'
                harness.write_text(upstream.replace('version = "=0.3.0"', selector))
                if expected:
                    with self.assertRaisesRegex(ValueError, expected):
                        release.validate_package("v0.5.0", self.root)
                else:
                    self.assertEqual(release.validate_package("v0.5.0", self.root), "0.5.0")

    def test_three_direct_sources_keep_gitlink_lock_and_export_identity(self):
        self.add_registry_dependencies()
        manifest = self.root / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace(
            "[dependencies]\n",
            '[dependencies]\nztuio = { git = "https://github.com/LIghtJUNction/ztuio.git" }\n',
            1))
        source = self.root / "crates/ztuio/Cargo.toml"
        source.parent.mkdir(parents=True)
        source.write_text('[package]\nname = "ztuio"\nversion = "0.1.0"\n')
        with (self.root / "Cargo.lock").open("a") as stream:
            stream.write('\n[[package]]\nname = "ztuio"\nversion = "0.1.0"\n'
                         f'source = "git+{release.project_z_url("ztuio")}#{"1"*40}"\n')
        self.stub_gitlinks("zplugin", "zharness", "ztuio")
        expected = [("zplugin", "0.3.0"), ("zharness", "0.5.0"), ("ztuio", "0.1.0")]
        self.assertEqual(release.development_dependencies(self.root), expected)
        exported = tomllib.loads(release.registry_export_manifest(self.root).decode())["dependencies"]
        for name, version in expected:
            with self.subTest(name=name):
                self.assertEqual(exported[name], {"version": "=" + version})
                modules = {row["path"]: dict(row) for row in release.source_submodules(self.root)}
                modules["crates/" + name]["commit"] = "3" * 40
                with self.assertRaisesRegex(ValueError, "source, gitlink and locked dependency version/commit differ"):
                    release.development_dependencies(self.root, modules=modules)

    def add_zbridge_dependency(self):
        self.stub_gitlinks("zbridge")
        member = self.root / "crates/zbridge/Cargo.toml"
        member.parent.mkdir(parents=True)
        member.write_text('[package]\nname = "zbridge"\nversion = "0.1.0"\n'
                          '\n[features]\nstd = []\n')
        with (self.root / "Cargo.toml").open("a") as stream:
            stream.write('\n[dependencies]\n'
                         'zbridge = { git = "https://github.com/LIghtJUNction/zbridge.git", '
                         'optional = true, default-features = false, features = ["std"] }\n')
        with (self.root / "Cargo.lock").open("a") as stream:
            stream.write('\n[[package]]\nname = "zbridge"\nversion = "0.1.0"\n'
                         f'source = "git+https://github.com/LIghtJUNction/zbridge.git#{"1"*40}"\n')
        return member

    def test_zbridge_gitlink_lock_and_export_preserve_version_and_features(self):
        self.add_zbridge_dependency()
        paths = [self.root / "Cargo.toml", self.root / "Cargo.lock"]
        before = [path.read_bytes() for path in paths]
        self.assertEqual(release.project_z_url("zbridge"),
                         "https://github.com/LIghtJUNction/zbridge.git")
        self.assertEqual(release.development_dependencies(self.root), [("zbridge", "0.1.0")])
        self.assertEqual(release.validate_package("v0.5.0", self.root), "0.5.0")
        self.assertEqual(release.publication_packages(self.root), [("ssmcp", "0.5.0")])
        expected = release.read_toml(paths[0])
        expected["dependencies"]["zbridge"].pop("git")
        expected["dependencies"]["zbridge"]["version"] = "=0.1.0"
        self.assertEqual(tomllib.loads(release.registry_export_manifest(self.root).decode()), expected)
        checksum = "2" * 64
        frozen = before[1].replace(
            f'git+{release.project_z_url("zbridge")}#{"1"*40}'.encode(), release.CRATES_IO.encode()
        ) + f'checksum = "{checksum}"\n'.encode()
        normalized = {"packages": release.registry_lock(frozen, "0.5.0", {"zbridge": "0.1.0"})}
        for published in (checksum, "3" * 64, None):
            with self.subTest(checksum=published), \
                 mock.patch.object(release, "registry_checksum", return_value=published) as registry:
                if published == checksum:
                    release.check_registry_project_z(normalized)
                else:
                    with self.assertRaisesRegex(ValueError, "unpublished or changed: zbridge 0.1.0"):
                        release.check_registry_project_z(normalized)
            registry.assert_called_once_with("0.1.0", "zbridge")
        self.assertEqual(before, [path.read_bytes() for path in paths])

    def test_invalid_zbridge_identity_prevents_both_bump_writes(self):
        member = self.add_zbridge_dependency()
        manifest, lock = self.root / "Cargo.toml", self.root / "Cargo.lock"
        original_manifest, original_lock = manifest.read_text(), lock.read_text()
        original_member = member.read_text()
        modules = [{"path": "crates/zbridge", "commit": "1" * 40, "tree": "2" * 40}]
        cases = (
            ("missing mirror", original_manifest, original_lock, None, modules),
            ("missing gitlink", original_manifest, original_lock, original_member, []),
            ("wrong gitlink", original_manifest, original_lock, original_member,
             [{**modules[0], "commit": "3" * 40}]),
            ("wrong lock commit", original_manifest,
             original_lock.replace("#" + "1" * 40, "#" + "3" * 40), original_member, modules),
            ("wrong lock version", original_manifest,
             original_lock.replace('version = "0.1.0"', 'version = "0.1.1"'), original_member, modules),
            ("wrong mirror version", original_manifest, original_lock,
             original_member.replace('version = "0.1.0"', 'version = "0.1.1"'), modules),
            ("wrong URL", original_manifest.replace("LIghtJUNction/zbridge", "other/zbridge"),
             original_lock, original_member, modules),
        )
        for label, source, locked, checked_out, gitlinks in cases:
            with self.subTest(identity=label):
                manifest.write_text(source)
                lock.write_text(locked)
                if checked_out is None:
                    member.unlink()
                else:
                    member.write_text(checked_out)
                before = [path.read_bytes() for path in (manifest, lock)]
                with mock.patch.object(release, "source_submodules", return_value=gitlinks), \
                     self.assertRaisesRegex(ValueError, "zbridge"):
                    release.bump("patch", self.root)
                self.assertEqual(before, [path.read_bytes() for path in (manifest, lock)])

    def test_zbridge_cannot_enter_the_project_z_patch_allowlist(self):
        self.add_zbridge_dependency()
        self.assertNotIn("zbridge", release.PROJECT_Z_PATCHES)
        manifest = self.root / "Cargo.toml"
        original = manifest.read_text()
        canonical = 'git = "https://github.com/LIghtJUNction/zbridge.git"'
        for direct in (original, original.replace(canonical, 'version = "=0.1.0"')):
            for patch in (
                f'[patch.crates-io]\nzbridge = {{ {canonical} }}\n',
                f'[patch.crates-io]\nbridge = {{ package = "zbridge", {canonical} }}\n',
            ):
                with self.subTest(direct=direct, patch=patch):
                    manifest.write_text(direct + "\n" + patch)
                    before = [path.read_bytes() for path in (manifest, self.root / "Cargo.lock")]
                    with self.assertRaisesRegex(ValueError, "unexpected Project Z patch"):
                        release.bump("patch", self.root)
                    self.assertEqual(before, [path.read_bytes()
                                             for path in (manifest, self.root / "Cargo.lock")])

    def test_unpublished_zbridge_stops_verification_and_publication_before_cargo(self):
        self.add_zbridge_dependency()
        for operation in (release.verify_crate, release.publish):
            with self.subTest(operation=operation.__name__), \
                 mock.patch.object(release, "registry_checksum", return_value=None) as registry, \
                 mock.patch.object(release, "release_source",
                                   side_effect=AssertionError("source verification must not start")) as source, \
                 mock.patch.object(release, "verified_crate_receipt",
                                   side_effect=AssertionError("crate verification must not start")) as receipt, \
                 mock.patch.object(release.subprocess, "run",
                                   side_effect=AssertionError("Cargo must not run")) as upload, \
                 self.assertRaisesRegex(ValueError, "publish zbridge 0.1.0 from its independent repository"):
                operation(self.root)
            registry.assert_called_once_with("0.1.0", "zbridge")
            source.assert_not_called()
            receipt.assert_not_called()
            upload.assert_not_called()

    def submodule_fixture(self, nested=False):
        temporary = tempfile.TemporaryDirectory(prefix="ssmcp-source-module-")
        self.addCleanup(temporary.cleanup)
        source = Path(temporary.name) / "source"
        source.mkdir()

        def git(folder, *args):
            return subprocess.check_output(["git", *args], cwd=folder, env=self.git_env,
                                           text=True, stderr=subprocess.PIPE).strip()

        def initialize(folder):
            git(folder, "init", "--quiet", "--template=", "-b", "main")
            git(folder, "config", "user.name", "Release submodule test")
            git(folder, "config", "user.email", "release-module@example.invalid")
            git(folder, "config", "commit.gpgSign", "false")
            git(folder, "config", "core.hooksPath", os.devnull)
            (folder / "value.txt").write_text("first\n")
            git(folder, "add", "value.txt")
            git(folder, "commit", "--quiet", "-m", "first fixture version")
            (folder / "value.txt").write_text("second\n")
            git(folder, "commit", "--quiet", "-am", "second fixture version")

        initialize(source)
        if nested:
            child = Path(temporary.name) / "child"
            child.mkdir()
            initialize(child)
            git(source, "-c", "protocol.file.allow=always", "submodule", "add",
                "--quiet", str(child), "nested")
            git(source, "commit", "--quiet", "-am", "pin nested fixture")
        self.git("-c", "protocol.file.allow=always", "submodule", "add",
                 "--quiet", str(source), "modules/fixture")
        self.git("commit", "--quiet", "-am", "pin fixture")
        self.git("-c", "protocol.file.allow=always", "submodule", "update", "--init", "--recursive")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("tag", "-d", "v0.5.0")
        self.git("tag", "v0.5.0")
        return self.root / "modules/fixture", git

    def test_custom_version_and_dry_run_do_not_change_any_file(self):
        member = self.add_ztuio_dependency()
        paths = [self.root / "Cargo.toml", self.root / "Cargo.lock", member]
        before = [path.read_bytes() for path in paths]
        for level, custom, expected in (("minor", None, "0.6.0"),
                                        ("custom", "0.10.0-rc.1", "0.10.0-rc.1")):
            with self.subTest(level=level):
                result = release.bump(level, self.root, custom_version=custom, dry_run=True)
                self.assertEqual(result["version"], expected)
                self.assertEqual(result["dry_run"], "true")
                self.assertEqual(before, [path.read_bytes() for path in paths])
        release.bump("custom", self.root, custom_version="0.10.0-rc.1")
        self.assertEqual(release.validate_package("v0.10.0-rc.1", self.root), "0.10.0-rc.1")
        self.assertEqual(member.read_bytes(), before[2])

    def test_invalid_custom_versions_fail_before_writing(self):
        paths = [self.root / "Cargo.toml", self.root / "Cargo.lock"]
        before = [path.read_bytes() for path in paths]
        for custom in (None, "", "v0.6.0", "0.6.00", "0.6.0-01", "0.5.0",
                       "0.5.0-rc.1", "0.4.9", "0.6.0\nmalicious"):
            with self.subTest(custom=custom), self.assertRaises(ValueError):
                release.bump("custom", self.root, custom_version=custom)
            self.assertEqual(before, [path.read_bytes() for path in paths])
        with self.assertRaisesRegex(ValueError, "only valid"):
            release.bump("patch", self.root, custom_version="0.6.0")
        self.assertEqual(before, [path.read_bytes() for path in paths])
        self.assertEqual(release.bumped_version("0.6.0-rc.9", "custom", "0.6.0-rc.10"), "0.6.0-rc.10")
        self.assertEqual(release.bumped_version("0.6.0-rc.10", "custom", "0.6.0"), "0.6.0")

    def test_missing_ztuio_checkout_blocks_application_validation(self):
        member = self.add_ztuio_dependency()
        member.unlink()
        with self.assertRaisesRegex(ValueError, "submodules"):
            release.validate_package("v0.5.0", self.root)

    def test_independent_dependencies_preserve_exact_versions_and_patch_on_bump(self):
        self.add_registry_dependencies()
        manifest = (self.root / "Cargo.toml").read_text()
        source = [(self.root / "crates" / name / "Cargo.toml").read_bytes()
                  for name in ("zharness", "zplugin")]
        self.assertEqual(release.development_dependencies(self.root),
                         [("zplugin", "0.3.0"), ("zharness", "0.5.0")])
        release.bump("minor", self.root)
        updated = (self.root / "Cargo.toml").read_text()
        self.assertEqual(updated.split("[dependencies]", 1)[1], manifest.split("[dependencies]", 1)[1])
        self.assertEqual(source, [(self.root / "crates" / name / "Cargo.toml").read_bytes()
                                  for name in ("zharness", "zplugin")])
        self.assertEqual(release.validate_package("v0.6.0", self.root), "0.6.0")

    def test_wrong_independent_version_or_patch_is_rejected_before_bump(self):
        self.add_registry_dependencies()
        manifest = self.root / "Cargo.toml"
        original = manifest.read_text()
        mutations = (original.replace('zplugin = {', 'zplugin = { version = "0.3.0",', 1),
                     original.replace('[patch.crates-io]\nzplugin = { git = "https://github.com/LIghtJUNction/zplugin.git" }',
                                      '[patch.crates-io]\nzplugin = { path = "other/zplugin" }'),
                     original.replace('zharness = {', 'zharness = { version = "=0.5.1",', 1))
        for text in mutations:
            with self.subTest(manifest=text):
                manifest.write_text(text)
                before = [manifest.read_bytes(), (self.root / "Cargo.lock").read_bytes()]
                with self.assertRaises(ValueError):
                    release.bump("patch", self.root)
                self.assertEqual(before, [manifest.read_bytes(), (self.root / "Cargo.lock").read_bytes()])
        manifest.write_text(original)
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('version = "0.3.0"', 'version = "0.2.9"'))
        with self.assertRaisesRegex(ValueError, "zplugin"):
            release.validate_package("v0.5.0", self.root)

    def test_git_only_v25_binary_proof_and_build_only_need_no_registry_package(self):
        member = self.add_ztuio_dependency(stub=False)
        release.command("git", "init", "--quiet", "--template=", cwd=member.parent)
        release.command("git", "add", "Cargo.toml", cwd=member.parent)
        release.command("git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "--quiet", "-m", "locked core", cwd=member.parent)
        revision = release.command("git", "rev-parse", "HEAD", cwd=member.parent)
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace("1" * 40, revision).replace("version = \"0.5.0\"", "version = \"0.10.25\""))
        manifest = self.root / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace("0.5.0", "0.10.25"))
        (self.root / ".gitignore").write_text("/target/\n/dist/\n")
        output = self.fixtures()
        self.git("add", "Cargo.toml", "Cargo.lock", ".gitignore", "crates/ztuio", "scripts")
        self.git("commit", "--quiet", "-m", "Git-only locked release")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("tag", "v0.10.25")

        cargo_home = self.root / "target/cargo-home"
        checkout = cargo_home / "git/checkouts/ztuio-fixture" / revision[:8]
        checkout.parent.mkdir(parents=True)
        self.git("clone", "--quiet", str(member.parent), str(checkout))
        pkg_source = f'git+{release.project_z_url("ztuio")}#{revision}'
        packages = {"app": {"name": "ssmcp", "version": "0.10.25", "source": None,
                            "manifest_path": str(manifest)},
                    "core": {"name": "ztuio", "version": "0.1.0", "source": pkg_source,
                             "manifest_path": str(checkout / "Cargo.toml")}}
        nodes = {"app": {"features": [], "deps": [{"name": "ztuio", "pkg": "core", "dep_kinds": []}]},
                 "core": {"features": [], "deps": []}}
        original = release.command

        def toolchain(*args, **kwargs):
            if args in (("cargo", "--version"), ("rustc", "--version")):
                return args[0] + " 1.100.0 (fixture)"
            return original(*args, **kwargs)

        with mock.patch.dict(os.environ, {"CARGO_HOME": str(cargo_home)}), \
             mock.patch.object(release, "metadata_graph", return_value=("app", packages, nodes)), \
             mock.patch.object(release, "command", side_effect=toolchain), \
             mock.patch.object(release, "registry_checksum", side_effect=AssertionError("registry unavailable")) as registry:
            self.assertEqual(release.validate_tag("v0.10.25", self.root)["crate_publication"], "deferred")
            self.assertEqual(release.validate_tag(None, self.root, build_only=True)["crate_publication"], "not_requested")
            src = release.verify_source(self.root)
            release.checksums(self.root, build_only=True)
            proof = output / "release-manifest.json"
            self.assertEqual(json.loads(proof.read_bytes())["crate_publication"]["status"], "not_requested")
            release.checksums(self.root)
            release.checksums(self.root, recheck=True)
            data = json.loads(proof.read_bytes())
            self.assertIsNone(data["verified_crate"])
            self.assertEqual(data["crate_publication"]["status"], "deferred")
            self.assertTrue(data["crate_publication"]["reason"])
            self.assertEqual(data["commit"], self.git("rev-parse", "HEAD"))
            self.assertEqual(data["tree"], self.git("rev-parse", "HEAD^{tree}"))
            self.assertEqual(data["submodules"][0]["commit"], revision)
            self.assertEqual(data["dependency_graph"], src["dependency_graph"])
            self.assertEqual(len(list(output.iterdir())), 16)
            self.assertFalse((self.root / "target/package" / release.VERIFIED_CRATE).exists())
            self.assertNotIn("cargo install ssmcp --version 0.10.25", release.release_notes(self.root))
            self.assertIn("is deferred", release.release_notes(self.root))
            for operation in (release.verify_crate, release.publish):
                with self.assertRaisesRegex(ValueError, "crate publication is deferred"):
                    operation(self.root)

            old_proof = proof.read_bytes()
            proof.write_text(old_proof.decode().replace(src["tree"], "0" * 40))
            with self.assertRaisesRegex(ValueError, "finalized release proof differs"):
                release.checksums(self.root, recheck=True)
            proof.write_bytes(old_proof)
            source_receipt = release.source_receipt_path(self.root)
            source_bytes = source_receipt.read_bytes()
            source_receipt.write_text(source_bytes.decode().replace(src["cargo_lock_sha256"], "1" * 64))
            with self.assertRaisesRegex(ValueError, "verified source receipt differs"):
                release.checksums(self.root, recheck=True)
            source_receipt.write_bytes(source_bytes)
            next(output.glob("*.sha256")).unlink()
            with self.assertRaises(FileNotFoundError):
                release.release("v0.10.25", self.root)
            lock.write_text(lock.read_text() + "# drift\n")
            with self.assertRaises(subprocess.CalledProcessError):
                release.verified_source_receipt(self.root)
            registry.assert_not_called()

    def test_frozen_registry_export_keeps_exact_versions_and_does_not_resolve_again(self):
        self.add_ztuio_dependency()
        manifest = self.root / "Cargo.toml"
        original = manifest.read_text().replace(
            'ztuio.git" }', 'ztuio.git", optional = true, default-features = false, features = ["fixture"] }'
        ) + 'base64 = "0.23.1"\n'
        manifest.write_text(original)
        src = self.root / "src"
        src.mkdir()
        (src / "lib.rs").write_text("pub const AS_IS: bool = true;\n")
        self.git("add", "Cargo.toml", "Cargo.lock", "src/lib.rs")
        self.git("commit", "--quiet", "-m", "export fixture")
        commit = self.git("rev-parse", "HEAD")
        derived = release.registry_export_manifest(self.root)
        expected = release.read_toml(manifest)
        spec = expected["dependencies"]["ztuio"]
        spec.pop("git")
        spec["version"] = "=0.1.0"
        self.assertEqual(tomllib.loads(derived.decode()), expected)
        development_lock = (self.root / "Cargo.lock").read_bytes()
        frozen = development_lock.replace(
            f'git+{release.project_z_url("ztuio")}#{"1"*40}'.encode(), release.CRATES_IO.encode()
        ) + (f'checksum = "{"2"*64}"\n').encode()
        release.registry_lock(frozen, "0.5.0", {"ztuio": "0.1.0"})
        real_run = subprocess.run

        def no_resolution(args, **options):
            self.assertNotEqual(args[0], "cargo", "a retry may not regenerate the registry lock")
            return real_run(args, **options)

        with mock.patch.object(release.subprocess, "run", side_effect=no_resolution):
            with release.registry_export(self.root, commit, derived, frozen) as (export, target, lock):
                self.assertEqual(lock, frozen)
                self.assertFalse(target.is_relative_to(self.root))
                self.assertEqual((export / "Cargo.toml").read_bytes(), derived)
                self.assertEqual((export / "Cargo.lock").read_bytes(), frozen)
                self.assertEqual((export / "src/lib.rs").read_bytes(), (src / "lib.rs").read_bytes())
        self.assertEqual(manifest.read_text(), original)
        self.assertEqual((self.root / "Cargo.lock").read_bytes(), development_lock)

    def test_unpublished_independent_dependency_stops_before_upload(self):
        self.add_registry_dependencies()
        for operation in (release.verify_crate, release.publish):
            with mock.patch.object(release, "registry_checksum", return_value=None) as registry, \
                 mock.patch.object(release.subprocess, "run") as upload, \
                 self.assertRaisesRegex(ValueError, "independent repository"):
                operation(self.root)
            registry.assert_called_once_with("0.3.0", "zplugin")
            upload.assert_not_called()

    def test_published_independent_dependencies_are_not_republished(self):
        self.add_registry_dependencies()
        output = self.root / "target/package"
        output.mkdir(parents=True)
        archive = output / "ssmcp-0.5.0.crate"
        archive.write_bytes(b"verified application fixture")
        self.stub_verified_crate(archive)
        checksum = release.digest(archive)
        with mock.patch.object(release, "registry_checksum", side_effect=["plugin-digest", "harness-digest", checksum]) as registry, \
             mock.patch.object(release.subprocess, "run") as upload:
            release.publish(self.root)
        self.assertEqual(registry.call_args_list,
                         [mock.call("0.3.0", "zplugin"), mock.call("0.5.0", "zharness"), mock.call("0.5.0", "ssmcp")])
        upload.assert_not_called()

    def test_pinned_submodules_and_nested_source_are_recorded(self):
        checkout, git = self.submodule_fixture(nested=True)
        modules = release.source_submodules(self.root)
        self.assertEqual([row["path"] for row in modules], ["modules/fixture", "modules/fixture/nested"])
        for row, folder in zip(modules, (checkout, checkout / "nested")):
            self.assertEqual(row["commit"], git(folder, "rev-parse", "HEAD"))
            self.assertEqual(row["tree"], git(folder, "rev-parse", "HEAD^{tree}"))
        self.fixtures()
        self.stub_verified_crate()
        release.checksums(self.root)
        manifest = json.loads((self.root / "dist/release-manifest.json").read_text())
        self.assertEqual(manifest["submodules"], modules)

    def test_uninitialized_submodule_blocks_valid_tag_preflight(self):
        self.submodule_fixture()
        self.git("submodule", "deinit", "--force", "--", "modules/fixture")
        with self.assertRaisesRegex(ValueError, "not initialized"):
            release.validate_tag("v0.5.0", self.root)
        with self.assertRaisesRegex(ValueError, "not initialized"):
            release.validate_tag(None, self.root, build_only=True)

    def test_submodule_checkout_must_match_committed_gitlink(self):
        checkout, git = self.submodule_fixture()
        git(checkout, "checkout", "--quiet", "HEAD^")
        with self.assertRaisesRegex(ValueError, "pinned gitlink"):
            release.source_submodules(self.root)

    def test_submodule_tracked_and_untracked_source_cannot_be_packaged(self):
        checkout, git = self.submodule_fixture()
        (checkout / "value.txt").write_text("changed after validation\n")
        with self.assertRaisesRegex(ValueError, "uncommitted source"):
            release.source_submodules(self.root)
        git(checkout, "checkout", "--", "value.txt")
        (checkout / "unexpected.txt").write_text("not part of the pinned source\n")
        with self.assertRaisesRegex(ValueError, "uncommitted source"):
            release.source_submodules(self.root)


class BumpWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="ssmcp-bump-dispatch-")))
        self.repository = "LIghtJUNction/ssmcp"
        self.baseline = "a" * 40
        self.sha = "b" * 40
        self.details = {
            "workflow_run_id": 123,
            "run_url": f"https://api.github.com/repos/{self.repository}/actions/runs/123",
            "html_url": f"https://github.com/{self.repository}/actions/runs/123",
        }
        self.http = (b"HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n\r\n"
                     + json.dumps(self.details).encode("utf-8") + b"\n")

    def dispatch(self, directory, attempt=1):
        return bump_release.dispatch(
            "v0.10.25", self.sha, self.baseline, 456, attempt, self.root / directory,
            root=self.root, repository=self.repository
        )

    def test_workflow_uses_original_sha_current_default_and_existing_release_contract(self):
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/bump.yml").read_text()
        self.assertIn("default: none", workflow)
        self.assertIn("options: [none, patch, minor, major, custom]", workflow)
        self.assertEqual(workflow.count("ref: ${{ github.sha }}"), 2)
        self.assertNotIn("ref: main", workflow)
        self.assertNotIn("gh workflow run", workflow)
        self.assertIn("--run-attempt \"$GITHUB_RUN_ATTEMPT\"", workflow)
        self.assertIn("if: inputs.dry_run != true && inputs.publish == true", workflow)

    def test_dispatch_preserves_raw_http_and_explicit_no_deploy_inputs(self):
        response = subprocess.CompletedProcess(["gh"], 0, self.http, b"original stderr\n")
        with mock.patch.object(bump_release, "verify_dispatch_source") as verify, \
             mock.patch.object(bump_release.subprocess, "run", return_value=response) as post:
            result = self.dispatch("success")
        verify.assert_called_once_with(self.root, self.baseline, self.sha, "v0.10.25")
        post.assert_called_once()
        argv = post.call_args.args[0]
        self.assertEqual(argv, [
            "gh", "api", "--method", "POST", "--hostname", "github.com",
            "--header", "X-GitHub-Api-Version: 2026-03-10", "--input", "-", "--include",
            "repos/LIghtJUNction/ssmcp/actions/workflows/release.yml/dispatches",
        ])
        stdin = post.call_args.kwargs["input"]
        self.assertEqual(json.loads(stdin), {
            "ref": "main", "inputs": {"tag": "v0.10.25", "build_only": False, "deploy_server": False},
        })
        self.assertEqual(result["status"], "dispatched")
        self.assertEqual(result["source"], "github-actions")
        self.assertEqual(result["workflow_run_id"], 123)
        self.assertEqual(result["bump_run_id"], 456)
        capture_path = self.root / "success/dispatch-capture.json"
        capture = json.loads(capture_path.read_text())
        self.assertEqual(set(capture), {"schema", "argv", "stdin_base64", "stdout_base64",
                                       "stderr_base64", "exit_code", "started_utc", "finished_utc"})
        self.assertEqual(base64.b64decode(capture["stdin_base64"]), stdin)
        self.assertEqual(base64.b64decode(capture["stdout_base64"]), self.http)
        self.assertEqual(base64.b64decode(capture["stderr_base64"]), b"original stderr\n")
        self.assertEqual((self.root / "success/stdout.http").read_bytes(), self.http)
        self.assertEqual(result["capture_sha256"], release.digest(capture_path))
        with mock.patch.object(bump_release.subprocess, "run") as repeated:
            with self.assertRaises(FileExistsError):
                self.dispatch("success")
        repeated.assert_not_called()

    def test_rerun_never_posts_even_without_a_previous_receipt(self):
        with mock.patch.object(bump_release, "verify_dispatch_source") as verify, \
             mock.patch.object(bump_release.subprocess, "run") as post, \
             self.assertRaisesRegex(ValueError, "new explicit Release workflow dispatch"):
            self.dispatch("rerun", attempt=2)
        verify.assert_not_called()
        post.assert_not_called()
        result = json.loads((self.root / "rerun/result.json").read_text())
        self.assertEqual(result["status"], "rerun_blocked")
        self.assertFalse((self.root / "rerun/dispatch-capture.json").exists())

    def test_wrong_prepared_source_stops_before_post(self):
        with mock.patch.object(bump_release, "verify_dispatch_source",
                               side_effect=ValueError("prepared tag differs")), \
             mock.patch.object(bump_release.subprocess, "run") as post, \
             self.assertRaisesRegex(ValueError, "prepared tag differs"):
            self.dispatch("wrong-source")
        post.assert_not_called()
        result = json.loads((self.root / "wrong-source/result.json").read_text())
        self.assertEqual(result["status"], "not_dispatched")

    def test_ambiguous_http_or_cli_failure_is_preserved_without_retry(self):
        cases = (
            ("old-api", 0, b"HTTP/2.0 204 No Content\r\n\r\n"),
            ("failed-cli", 1, self.http),
            ("malformed", 0, b"HTTP/2.0 200 OK\n\nnot-json"),
            ("wrong-run", 0, b"HTTP/2.0 200 OK\n\n" + json.dumps({
                **self.details, "html_url": "https://github.com/another/repo/actions/runs/123"
            }).encode("utf-8")),
        )
        for directory, code, stdout in cases:
            with self.subTest(case=directory), \
                 mock.patch.object(bump_release, "verify_dispatch_source"), \
                 mock.patch.object(bump_release.subprocess, "run", return_value=
                                   subprocess.CompletedProcess(["gh"], code, stdout, b"detail\n")) as post, \
                 self.assertRaises(ValueError):
                self.dispatch(directory)
            post.assert_called_once()
            capture = json.loads((self.root / directory / "dispatch-capture.json").read_text())
            self.assertEqual(capture["exit_code"], code)
            self.assertEqual(base64.b64decode(capture["stdout_base64"]), stdout)
            self.assertEqual(base64.b64decode(capture["stderr_base64"]), b"detail\n")
            self.assertEqual(json.loads((self.root / directory / "result.json").read_text())["status"],
                             "dispatch_unconfirmed")

    def test_post_timeout_keeps_partial_bytes_and_never_reposts(self):
        failure = subprocess.TimeoutExpired(["gh"], 120, output=b"partial HTTP", stderr=b"partial error")
        with mock.patch.object(bump_release, "verify_dispatch_source"), \
             mock.patch.object(bump_release.subprocess, "run", side_effect=failure) as post, \
             self.assertRaisesRegex(ValueError, "delivery is unknown"):
            self.dispatch("timeout")
        post.assert_called_once()
        capture = json.loads((self.root / "timeout/dispatch-capture.json").read_text())
        self.assertIsNone(capture["exit_code"])
        self.assertEqual(base64.b64decode(capture["stdout_base64"]), b"partial HTTP")
        self.assertEqual(base64.b64decode(capture["stderr_base64"]), b"partial error")


class GithubReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="ssmcp-github-release-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "ssmcp"\nversion = "0.5.0"\n')
        (self.root / "dist").mkdir()
        self.archive = self.root / "dist/archive.zip"
        self.archive.write_bytes(b"verified archive")
        self.tag = "v0.5.0"
        self.metadata = {"id": 123, "tag_name": self.tag, "draft": True,
                         "prerelease": False, "assets": []}
        self.releases = [self.metadata]
        self.history = [{"id": 1, "tag_name": "v0.4.0", "draft": False, "prerelease": False}]
        self.calls = []
        self.downloaded = self.archive.read_bytes()
        self.bad_upload = None
        self.hidden_draft_reads = 0

    def candidate(self, version):
        self.tag = "v" + version
        self.metadata["tag_name"] = self.tag
        (self.root / "Cargo.toml").write_text(f'[package]\nname = "ssmcp"\nversion = "{version}"\n')

    def asset(self):
        return {"name": self.archive.name, "size": self.archive.stat().st_size,
                "digest": "sha256:" + release.digest(self.archive)}

    def api(self, *args, **kwargs):
        self.calls.append(args)
        if args == ("gh", "api", "--paginate", "--slurp",
                    "repos/owner/repo/releases?per_page=100"):
            # Include another page so draft discovery must inspect all pages.
            visible = self.releases
            if visible and self.hidden_draft_reads:
                self.hidden_draft_reads -= 1
                visible = []
            return json.dumps([self.history, visible])
        if args == ("gh", "api", "repos/owner/repo/releases/123"):
            return json.dumps(self.metadata)
        if args[:3] == ("gh", "release", "view"):
            return "https://github.com/owner/repo/releases/tag/" + self.tag
        raise AssertionError(f"unexpected release API command: {args}")

    def cli(self, args, **kwargs):
        self.calls.append(tuple(args))
        if args == ["gh", "api", "repos/owner/repo/releases/tags/" + self.tag]:
            if self.releases and not self.metadata["draft"]:
                return subprocess.CompletedProcess(args, 0, json.dumps(self.metadata), "")
            return subprocess.CompletedProcess(args, 1, "", "gh: Not Found (HTTP 404)")
        if args[:3] == ["gh", "release", "create"]:
            self.assertEqual(self.releases, [])
            self.assertIn("--verify-tag", args)
            self.releases = [self.metadata]
        elif args[:3] == ["gh", "release", "upload"]:
            self.assertNotIn("--clobber", args)
            self.metadata["assets"] = [self.asset()]
            if self.bad_upload == "size":
                self.metadata["assets"][0]["size"] += 1
            elif self.bad_upload == "digest":
                self.metadata["assets"][0]["digest"] = "sha256:wrong"
        elif args[:3] == ["gh", "release", "download"]:
            (Path(args[args.index("--dir") + 1]) / self.archive.name).write_bytes(self.downloaded)
        elif args[:3] == ["gh", "release", "edit"]:
            self.assertIn("--draft=false", args)
            self.metadata["draft"] = False
        else:
            raise AssertionError(f"unexpected release CLI command: {args}")
        return subprocess.CompletedProcess(args, 0, "", "")

    def publish_release(self):
        with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "owner/repo"}), \
             mock.patch.object(release, "validate_tag"), \
             mock.patch.object(release, "checksums"), \
             mock.patch.object(release, "command", side_effect=self.api), \
             mock.patch.object(release.subprocess, "run", side_effect=self.cli):
            release.release(self.tag, self.root)

    def operations(self, operation):
        return [call for call in self.calls if call[:3] == ("gh", "release", operation)]

    def test_first_stable_release_becomes_latest_despite_other_drafts_and_prereleases(self):
        for history in ([], [
                {"tag_name": "v99.0.0", "draft": True, "prerelease": False},
                {"tag_name": "v99.0.0-rc.1", "draft": False, "prerelease": True}]):
            with self.subTest(history=history):
                self.history = history
                self.metadata.update(draft=True, assets=[])
                self.calls.clear()
                self.publish_release()
                self.assertIn("--latest=true", self.operations("edit")[0])

    def test_newer_stable_release_uses_numeric_version_order_for_latest(self):
        self.candidate("0.10.0")
        self.history[0]["tag_name"] = "v0.9.999"
        self.publish_release()
        self.assertIn("--latest=true", self.operations("edit")[0])

    def test_older_stable_can_publish_without_claiming_latest_from_a_later_page(self):
        self.candidate("0.10.16")
        self.releases.append({"id": 456, "tag_name": "v0.10.17",
                              "draft": False, "prerelease": False})
        self.publish_release()
        self.assertEqual(self.metadata["assets"], [self.asset()])
        self.assertFalse(self.metadata["draft"])
        self.assertIn("--latest=false", self.operations("edit")[0])
        self.assertIn("--prerelease=false", self.operations("edit")[0])

    def test_prerelease_does_not_claim_latest_or_need_a_stable_comparison(self):
        self.candidate("0.10.18-rc.1")
        self.publish_release()
        self.assertIn("--latest=false", self.operations("edit")[0])
        self.assertIn("--prerelease=true", self.operations("edit")[0])
        # The only listing is draft discovery, before upload.
        self.assertEqual(sum("--paginate" in call for call in self.calls), 1)

    def test_latest_listing_errors_retain_verified_assets_in_the_draft(self):
        real_api = self.api
        for status in (403, 404, 500):
            with self.subTest(status=status):
                self.metadata.update(draft=True, assets=[])
                self.calls.clear()
                error = subprocess.CalledProcessError(1, ["gh", "api"],
                                                     stderr=f"gh: HTTP {status}")

                def unavailable(*args, **kwargs):
                    if "--paginate" in args and self.metadata["assets"]:
                        raise error
                    return real_api(*args, **kwargs)

                with mock.patch.object(self, "api", side_effect=unavailable), \
                     self.assertRaises(subprocess.CalledProcessError) as caught:
                    self.publish_release()
                self.assertIs(caught.exception, error)
                self.assertEqual(self.metadata["assets"], [self.asset()])
                self.assertTrue(self.metadata["draft"])
                self.assertEqual(self.operations("edit"), [])

    def test_malformed_latest_listing_never_guesses_a_publication_policy(self):
        real_api = self.api
        for response in ("not JSON", "{}", "[]", "[{}]",
                         '[[{"tag_name":"v0.10.17","draft":false}]]',
                         '[[{"tag_name":"v0.10.17","draft":"false","prerelease":false}]]',
                         '[[{"tag_name":"release","draft":false,"prerelease":false}]]',
                         '[[{"tag_name":"v0.10.01","draft":false,"prerelease":false}]]'):
            with self.subTest(response=response):
                self.metadata.update(draft=True, assets=[])
                self.calls.clear()

                def malformed(*args, **kwargs):
                    if "--paginate" in args and self.metadata["assets"]:
                        return response
                    return real_api(*args, **kwargs)

                with mock.patch.object(self, "api", side_effect=malformed), \
                     self.assertRaises(ValueError):
                    self.publish_release()
                self.assertEqual(self.metadata["assets"], [self.asset()])
                self.assertTrue(self.metadata["draft"])
                self.assertEqual(self.operations("edit"), [])

    def test_tag_404_reuses_existing_draft_and_reloads_only_by_id(self):
        self.publish_release()
        self.assertEqual(self.operations("create"), [])
        self.assertEqual(len(self.operations("upload")), 1)
        self.assertEqual(len(self.operations("edit")), 1)
        self.assertEqual(self.calls.count(("gh", "api", "repos/owner/repo/releases/123")), 2)
        self.assertEqual(self.calls.count(("gh", "api", "repos/owner/repo/releases/tags/v0.5.0")), 1)

    def test_new_draft_resolves_id_from_listing_before_asset_verification(self):
        self.releases = []
        self.publish_release()
        self.assertEqual(len(self.operations("create")), 1)
        self.assertEqual(self.calls.count(("gh", "api", "repos/owner/repo/releases/123")), 2)
        self.assertFalse(self.metadata["draft"])

    def test_new_draft_can_become_visible_on_a_later_read(self):
        for hidden_reads in (2, 4):
            with self.subTest(hidden_reads=hidden_reads):
                self.releases = []
                self.metadata.update(draft=True, assets=[])
                self.calls.clear()
                self.hidden_draft_reads = hidden_reads

                def waiting(_delay):
                    self.assertEqual(len(self.operations("create")), 1)
                    self.assertFalse(self.operations("upload") or self.operations("edit"))

                with mock.patch.object(release.time, "sleep", side_effect=waiting) as sleep:
                    self.publish_release()
                self.assertEqual(sleep.call_args_list,
                                 [mock.call(delay) for delay in (1, 2, 4, 8)[:hidden_reads]])
                self.assertEqual(len(self.operations("create")), 1)
                self.assertEqual(len(self.operations("upload")), 1)
                self.assertEqual(len(self.operations("edit")), 1)
                self.assertEqual(self.calls.count(("gh", "api", "repos/owner/repo/releases/123")), 2)
                self.assertFalse(self.metadata["draft"])

    def test_invisible_new_draft_stops_after_bounded_reads_and_can_resume(self):
        self.releases = []
        self.hidden_draft_reads = 100
        with mock.patch.object(release.time, "sleep") as sleep, \
             self.assertRaisesRegex(RuntimeError, "draft retained: retry this tag's release job"):
            self.publish_release()
        self.assertEqual(sleep.call_args_list, [mock.call(delay) for delay in (1, 2, 4, 8)])
        self.assertEqual(self.calls.count(("gh", "api", "--paginate", "--slurp",
                                          "repos/owner/repo/releases?per_page=100")), 6)
        self.assertEqual(len(self.operations("create")), 1)
        self.assertFalse(self.operations("upload") or self.operations("edit"))
        self.assertTrue(self.metadata["draft"])
        self.assertEqual(self.metadata["assets"], [])

        self.hidden_draft_reads = 0
        with mock.patch.object(release.time, "sleep") as sleep:
            self.publish_release()
        sleep.assert_not_called()
        self.assertEqual(len(self.operations("create")), 1)
        self.assertEqual(len(self.operations("upload")), 1)
        self.assertEqual(len(self.operations("edit")), 1)
        self.assertEqual(self.releases, [self.metadata])
        self.assertFalse(self.metadata["draft"])

    def test_draft_visibility_wait_propagates_lookup_errors_without_retrying(self):
        for error in (RuntimeError("gh: Forbidden (HTTP 403)"),
                      RuntimeError("gh: server error (HTTP 500)"),
                      ValueError("multiple releases use tag v0.5.0"),
                      json.JSONDecodeError("invalid response", "", 0)):
            with self.subTest(error=str(error)):
                self.releases = []
                self.calls.clear()
                with mock.patch.object(release, "find_release", side_effect=[None, None, error]) as lookup, \
                     mock.patch.object(release.time, "sleep") as sleep, \
                     self.assertRaises(type(error)) as caught:
                    self.publish_release()
                self.assertIs(caught.exception, error)
                self.assertEqual(lookup.call_count, 3)
                sleep.assert_called_once_with(1)
                self.assertEqual(len(self.operations("create")), 1)
                self.assertFalse(self.operations("upload") or self.operations("edit"))
                self.assertTrue(self.metadata["draft"])

    def test_failed_creation_never_enters_visibility_wait(self):
        self.releases = []
        error = subprocess.CalledProcessError(1, ["gh", "release", "create"])
        with mock.patch.object(release, "find_release", return_value=None) as lookup, \
             mock.patch.object(release.subprocess, "run", side_effect=error) as create, \
             mock.patch.object(release.time, "sleep") as sleep, \
             mock.patch.object(release, "validate_tag"), \
             mock.patch.object(release, "checksums"), \
             mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "owner/repo"}), \
             self.assertRaises(subprocess.CalledProcessError):
            release.release("v0.5.0", self.root)
        lookup.assert_called_once()
        create.assert_called_once()
        self.assertEqual(create.call_args.args[0][:3], ["gh", "release", "create"])
        sleep.assert_not_called()
        self.assertEqual(self.releases, [])

    def test_published_release_with_identical_assets_is_idempotent(self):
        self.metadata.update(draft=False, assets=[self.asset()])
        self.publish_release()
        self.assertFalse(self.operations("create") or self.operations("upload") or self.operations("edit"))
        self.assertFalse(any("--paginate" in call for call in self.calls))

    def test_duplicate_drafts_stop_before_creation_or_upload(self):
        self.releases.append({**self.metadata, "id": 456})
        with self.assertRaisesRegex(ValueError, "multiple releases"):
            self.publish_release()
        self.assertFalse(self.operations("create") or self.operations("upload") or self.operations("edit"))

    def test_different_existing_asset_is_never_overwritten(self):
        self.metadata["assets"] = [{**self.asset(), "digest": "sha256:different"}]
        self.downloaded = b"different remote bytes"
        with self.assertRaisesRegex(ValueError, "assets are never overwritten"):
            self.publish_release()
        self.assertFalse(self.operations("upload") or self.operations("edit"))

    def test_existing_asset_without_server_digest_is_downloaded_and_verified(self):
        self.metadata["assets"] = [{**self.asset(), "digest": None}]
        self.publish_release()
        self.assertEqual(len(self.operations("download")), 2)
        self.assertEqual(self.operations("upload"), [])

    def test_unexpected_remote_asset_blocks_publication(self):
        self.metadata["assets"] = [{**self.asset(), "name": "verified-source.json"}]
        with self.assertRaisesRegex(ValueError, "unexpected assets"):
            self.publish_release()
        self.assertFalse(self.operations("upload") or self.operations("edit"))
        self.assertTrue(self.metadata["draft"])

    def test_wrong_uploaded_size_or_digest_keeps_release_draft(self):
        for fault, message in (("size", "upload is incomplete"), ("digest", "upload checksum mismatch")):
            with self.subTest(fault=fault):
                self.metadata["assets"] = []
                self.calls.clear()
                self.bad_upload = fault
                with self.assertRaisesRegex(ValueError, message):
                    self.publish_release()
                self.assertTrue(self.metadata["draft"])
                self.assertEqual(self.operations("edit"), [])



@unittest.skipUnless(os.name == "posix", "The Arch deployment helper requires POSIX")
class DeploymentVersionTests(unittest.TestCase):
    """Exercise the real stage/apply gates without invoking host services."""

    def setUp(self):
        import deploy_server
        self.deploy = deploy_server
        self.directory = self.enterContext(tempfile.TemporaryDirectory())
        self.root = Path(self.directory)
        self.commit = "a" * 40
        self.calls = []
        self.info = "pkgname = ssmcp-git\npkgver = 0.6.0.r123.gaaaaaaa-1\n"
        self.staged_version = "ssmcpd 0.6.0"
        self.installed_version = "ssmcpd 0.6.0"
        self.package = self.write("/incoming/candidate.pkg.tar.zst", b"candidate")
        self.rollback = self.write("/incoming/rollback.pkg.tar.zst", b"rollback")
        self.config = self.write("/etc/ssmcpd/ssmcpd.toml",
            b'[server]\npublic_url = "https://ssmcp.example"\n[auth.github]\nclient_id = "fixture"\n')
        self.binary = self.write("/usr/bin/ssmcpd", b"old server")
        self.path("/run/lock").mkdir(parents=True)
        self.report_path = self.root / "report.json"
        self.receipt = self.path("/var/lib/ssmcp-rollouts") / self.commit / "prepared.json"
        self.package_hash = self.deploy.digest(self.package)
        self.real_chmod = os.chmod

    def path(self, *parts):
        path = Path(*parts)
        if path.is_absolute() and not path.is_relative_to(self.root):
            return self.root / str(path).lstrip("/")
        return path

    def write(self, name, data):
        path = self.path(name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def prepared(self, **changes):
        value = {
            "commit": self.commit, "expected_version": "0.6.0",
            "package_sha": self.package_hash, "proxy_sha": None,
            "before": {"binary": self.deploy.digest(self.binary),
                       "config": self.deploy.digest(self.config),
                       "package": "ssmcp-git 0.4.1.r70.g655342c-1"},
            "prepared_unix": self.deploy.time.time(),
        }
        value.update(changes)
        self.receipt.parent.mkdir(parents=True, exist_ok=True)
        self.receipt.write_text(json.dumps(value))
        return value

    def command(self, argv, timeout=30):
        self.calls.append(list(argv))
        if argv[:2] == ["bsdtar", "-xOf"]:
            return self.info
        if argv[:2] == ["bsdtar", "-xpf"]:
            stage = Path(argv[argv.index("-C") + 1])
            (stage / "usr/bin").mkdir(parents=True, exist_ok=True)
            (stage / "usr/libexec/ssmcp").mkdir(parents=True, exist_ok=True)
        if len(argv) == 2 and argv[1] == "--version":
            return self.installed_version if argv[0] == "/usr/bin/ssmcpd" else self.staged_version
        if argv[:2] == ["pacman", "-Q"]:
            return "ssmcp-git 0.4.1.r70.g655342c-1"
        if argv[:2] == ["systemctl", "show"]:
            return "not-found" if "LoadState" in argv else "inactive"
        if argv[0] == "systemd-run":
            registry = self.write("/var/lib/ssmcp-stage-" + self.commit[:8] + "/oauth-clients.toml", b"fixture")
            self.real_chmod(registry, 0o600)
        if argv[:2] == ["tar", "-cpf"]:
            Path(argv[2]).write_bytes(b"backup")
        return ""

    def invoke(self, phase, expected="0.6.0"):
        argv = ["deploy_server.py", "--phase", phase, "--package", str(self.package),
                "--sha256", self.package_hash, "--rollback-package", str(self.rollback),
                "--commit", self.commit, "--report", str(self.report_path),
                "--config", str(self.config)]
        if expected is not None:
            argv += ["--expected-version", expected]
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch("sys.argv", argv))
            stack.enter_context(mock.patch.object(self.deploy, "Path", self.path))
            stack.enter_context(mock.patch.object(self.deploy, "open",
                lambda path, *args, **kwargs: open(self.path(path), *args, **kwargs), create=True))
            stack.enter_context(mock.patch.object(self.deploy.os, "geteuid", return_value=0))
            stack.enter_context(mock.patch.object(self.deploy.os, "umask", return_value=0o077))
            stack.enter_context(mock.patch.object(self.deploy.os, "chmod",
                lambda path, mode, **kwargs: self.real_chmod(self.path(path), mode, **kwargs)))
            stack.enter_context(mock.patch.object(self.deploy, "run", self.command))
            stack.enter_context(mock.patch.object(self.deploy, "wait_health"))
            stack.enter_context(mock.patch.object(self.deploy, "metadata"))
            stack.enter_context(mock.patch.object(self.deploy, "registration", return_value="/oauth/authorize?fixture"))
            stack.enter_context(mock.patch.object(self.deploy, "request", return_value=(200, "ok")))
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
            result = self.deploy.main()
        return result, json.loads(self.report_path.read_text())

    def test_expected_version_is_required_and_validated_before_commands(self):
        for value in (None, "v0.6.0", "0.6", "00.6.0", "0.6.0;true", "0.6.0-rc.1"):
            with self.subTest(value=value):
                with self.assertRaises(SystemExit) as caught:
                    self.invoke("stage", expected=value)
                self.assertEqual(caught.exception.code, 2)
                self.assertEqual(self.calls, [])
                self.assertFalse(self.path("/run/lock/ssmcp-deployment.lock").exists())

    def test_wrong_or_ambiguous_package_identity_stops_before_staging(self):
        for info, failed in (
            ("pkgname = ssmcp-git\npkgver = 0.5.1.r123.gaaaaaaa-1", "package_version"),
            ("pkgname = ssmcp-git\npkgver = 0.6.00.r123.gaaaaaaa-1", "package_version"),
            ("pkgname = ssmcp-git\npkgver = 0.6.0.r123.gaaaaaaa-1\npkgver = 0.5.1-1", "package_version"),
            ("pkgname = other\npkgver = 0.6.0-1", "package_name"),
            ("pkgname = ssmcp-git\npkgname = other\npkgver = 0.6.0-1", "package_name"),
        ):
            with self.subTest(info=info):
                self.info = info
                self.calls.clear()
                status, report = self.invoke("stage")
                self.assertEqual(status, 1)
                self.assertEqual(report["failed_check"], failed)
                self.assertEqual(report["state"], "failed")
                self.assertEqual([call[:2] for call in self.calls], [["bsdtar", "-xOf"]])
                self.assertFalse(self.receipt.exists())

    def test_staged_binary_must_match_package_release_before_service_start(self):
        self.staged_version = "ssmcpd 0.5.1"
        status, report = self.invoke("stage")
        self.assertEqual(status, 1)
        self.assertEqual(report["failed_check"], "staged_binary_version")
        self.assertFalse(any(call[0] == "systemd-run" for call in self.calls))
        self.assertFalse(self.receipt.exists())

    def test_successful_stage_binds_release_commit_and_digest(self):
        status, report = self.invoke("stage")
        self.assertEqual(status, 0)
        self.assertEqual(report["state"], "prepared")
        self.assertEqual(report["expected_version"], "0.6.0")
        receipt = json.loads(self.receipt.read_text())
        self.assertEqual(receipt["expected_version"], "0.6.0")
        self.assertEqual(receipt["commit"], self.commit)
        self.assertEqual(receipt["package_sha"], self.package_hash)
        self.assertFalse(any(call[:3] == ["systemctl", "restart", "ssmcpd.service"] for call in self.calls))
        self.assertTrue(any(call[:3] == ["systemctl", "stop", "ssmcp-stage-" + self.commit[:8]] for call in self.calls))

    def test_receipt_rejects_other_version_commit_digest_or_expired_stage(self):
        for changes in (
            {"expected_version": "0.5.1"}, {"expected_version": None},
            {"commit": "b" * 40}, {"commit": None},
            {"package_sha": "0" * 64},
            {"prepared_unix": self.deploy.time.time() - 3601},
        ):
            with self.subTest(changes=changes):
                self.prepared(**changes)
                self.calls.clear()
                status, report = self.invoke("apply")
                self.assertEqual(status, 1)
                self.assertEqual(report["failed_check"], "matching_stage_receipt")
                self.assertFalse(any(call[0] in ("pacman", "tar", "systemctl", "systemd-run") for call in self.calls))

    def test_wrong_installed_version_restores_package_and_checkpoint(self):
        self.prepared()
        self.installed_version = "ssmcpd 0.5.1"
        status, report = self.invoke("apply")
        self.assertEqual(status, 1)
        self.assertEqual(report["state"], "rolled_back")
        self.assertEqual(report["failed_check"], "installed_version")
        self.assertTrue(report["checks"]["rollback_package_unchanged"])
        self.assertTrue(report["checks"]["rollback_health"])
        upgrades = [call for call in self.calls if call[:2] == ["pacman", "-U"]]
        self.assertEqual(upgrades, [
            ["pacman", "-U", "--noconfirm", str(self.package)],
            ["pacman", "-U", "--noconfirm", str(self.rollback)],
        ])
        self.assertTrue(any(call[:2] == ["tar", "-xpf"] for call in self.calls))
        self.assertNotIn("server_version", report)


if __name__ == "__main__":
    unittest.main()
