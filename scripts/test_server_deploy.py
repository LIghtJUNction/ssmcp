#!/usr/bin/env python3
"""Offline deployment contracts: no SSH, HTTP requests or service operations."""
import base64
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


deploy = load("deploy_release")
entry = load("deploy_entry")
COMMIT = "a" * 40
TAG = "v0.10.3"
ARCHIVE = "ssmcp-x86_64-unknown-linux-gnu.tar.gz"
PREFIX = ""
URL = "https://github.com/LIghtJUNction/ssmcp/releases/download/" + TAG + "/" + ARCHIVE


class DeploymentContracts(unittest.TestCase):
    def setUp(self):
        for target in ("subprocess.run", "urllib.request.urlopen", "socket.socket"):
            patcher = mock.patch(target, side_effect=AssertionError("offline test attempted a process or network call"))
            patcher.start()
            self.addCleanup(patcher.stop)

    def release(self):
        manifest = {"package": "ssmcp", "version": "0.10.3", "commit": COMMIT,
                    "artifacts": [{"name": ARCHIVE, "sha256": "b" * 64, "bytes": 200}]}
        release = {"tag_name": TAG, "draft": False, "prerelease": False, "published_at": "2026-11-19T00:00:00Z",
                   "assets": [{"name": ARCHIVE, "size": 200, "digest": "sha256:" + "b" * 64, "browser_download_url": URL}]}
        return release, manifest

    def test_fixed_repository_release_identity(self):
        release, manifest = self.release()
        record = deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")
        self.assertEqual(record["archive_url"], URL)
        self.assertEqual(record["commit"], COMMIT)

    def test_source_version_and_commit_must_match(self):
        for field, value in [("package", "other"), ("version", "0.10.2"), ("commit", "c" * 40)]:
            release, manifest = self.release()
            manifest[field] = value
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
                deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")

    def test_unpublished_or_preview_release_is_rejected(self):
        for field, value in [("draft", True), ("prerelease", True), ("published_at", None)]:
            release, manifest = self.release()
            release[field] = value
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, "published_stable_release_required"):
                deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")

    def test_archive_size_digest_and_origin_must_match(self):
        for field, value in [("size", 201), ("digest", "sha256:" + "c" * 64), ("browser_download_url", "https://example.test/file")]:
            release, manifest = self.release()
            release["assets"][0][field] = value
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")

    def test_duplicate_manifest_archive_is_rejected(self):
        release, manifest = self.release()
        manifest["artifacts"].append(copy.deepcopy(manifest["artifacts"][0]))
        with self.assertRaisesRegex(RuntimeError, "one_server_archive_required"):
            deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")

    def test_stable_release_command_is_the_only_write_entry(self):
        args = entry.command("deploy " + TAG + " " + COMMIT + " 123-1")
        self.assertEqual(args, ["/usr/bin/sudo", "-n", entry.HELPER, "--tag", TAG,
                                "--commit", COMMIT, "--run-id", "123-1"])
        self.assertEqual(entry.command("status"), ["/usr/bin/sudo", "-n", entry.HELPER, "--status"])

    def test_ssh_entry_rejects_shell_and_nonrelease_inputs(self):
        valid = "deploy " + TAG + " " + COMMIT + " 123-1"
        for value in [valid + ";id", valid + "\n", "sh", "scp -t /tmp", valid.replace(TAG, "main"),
                      valid.replace(TAG, "v0.10.3-rc.1"), valid.replace(COMMIT, "a" * 7), valid.replace("123-1", "../1")]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                entry.command(value)

    def archive(self, root, extra=None):
        archive = root / "package.tar.gz"
        prefix = PREFIX
        with tarfile.open(archive, "w:gz") as output:
            for name, data in [(prefix + "ssmcpd", b"fake executable"), (prefix + "code-mode.js", b"fake matched runner")]:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                output.addfile(info, io.BytesIO(data))
            if extra is not None:
                output.addfile(extra, io.BytesIO(b"x") if extra.isfile() else None)
        return archive, {"archive": ARCHIVE, "archive_prefix": PREFIX, "archive_bytes": archive.stat().st_size,
                         "archive_sha256": deploy.sha(archive)}

    def test_only_verified_runtime_members_are_installed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive, record = self.archive(root)
            deploy.extract_runtime(archive, root, record)
            self.assertEqual((root / "candidate").read_bytes(), b"fake executable")
            self.assertEqual((root / "runner.js").read_bytes(), b"fake matched runner")
            self.assertEqual((root / "candidate").stat().st_mode & 0o777, 0o755)

    def test_corrupted_archive_fails_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive, record = self.archive(root)
            data = bytearray(archive.read_bytes())
            data[-1] ^= 1
            archive.write_bytes(data)
            with self.assertRaisesRegex(RuntimeError, "downloaded_archive_mismatch"):
                deploy.extract_runtime(archive, root, record)
            self.assertFalse((root / "candidate").exists())

    def test_traversal_and_symlink_members_are_rejected(self):
        for kind in ["traversal", "symlink"]:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                info = tarfile.TarInfo(PREFIX + ("../escape" if kind == "traversal" else "alias"))
                if kind == "symlink":
                    info.type, info.linkname = tarfile.SYMTYPE, "/etc/passwd"
                else:
                    info.size = 1
                archive, record = self.archive(root, info)
                with self.assertRaisesRegex(RuntimeError, "unsafe_archive_member"):
                    deploy.extract_runtime(archive, root, record)

    def test_old_runner_validation_uses_recorded_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            old, new = Path(directory) / "old.js", Path(directory) / "new.js"
            old.write_bytes(b"old runner")
            saved = {str(old): deploy.sha(old)}
            new.write_bytes(b"new runner")
            deploy.verify_original_runners(saved)
            old.write_bytes(b"bad runner")
            with self.assertRaisesRegex(RuntimeError, "original_runners_changed"):
                deploy.verify_original_runners(saved)
            old.unlink()
            with self.assertRaisesRegex(RuntimeError, "original_runners_changed"):
                deploy.verify_original_runners(saved)

    def asset_fixture(self, brand="favicon.svg", corrupt=False, legacy=False, wrong_mime=False):
        bodies = {name: ("fixture " + name).encode() for name in deploy.CORE_ASSET_NAMES | {brand}}
        urls = {name: "/account/assets/" + hashlib.sha256(body).hexdigest() + "/" + name for name, body in bodies.items()}
        tags = [('<link rel="stylesheet" href="' + urls[name] + '">') if name.endswith(".css") else
                ('<script src="' + urls[name] + '"></script>') for name in sorted(deploy.CORE_ASSET_NAMES)]
        tags.append('<link rel="icon" href="' + urls[brand] + '">')
        if legacy:
            tags.append('<script src="/copy-command.js"></script>')
        html = ("<body>" + "".join(tags) + "</body>").encode()
        request = {"public_url": "https://example.test", "assets": {
            name: {"sha256": hashlib.sha256(bodies[name]).hexdigest(), "bytes": len(bodies[name]),
                   "content_types": deploy.asset_types(name)} for name in deploy.CORE_ASSET_NAMES}}
        def http(_base, path, **options):
            if path == "/":
                return 200, html
            if path == "/mcp":
                return 401, b""
            if path == "/account/admin":
                return 303, b""
            if path == "/.well-known/oauth-authorization-server":
                return 200, b'{"issuer":"https://example.test"}'
            name = next(name for name, url in urls.items() if url == path)
            body = bodies[name]
            if corrupt and name == brand:
                body = b"x" + body[1:]
            mime = "text/plain" if wrong_mime and name == brand else deploy.asset_types(name)[0]
            return 200, body, {"content-type": mime}
        return request, http

    def test_brand_image_types_are_discovered_from_candidate_html(self):
        for brand in ["favicon.svg", "favicon.png", "brand-mark.png", "brand-symbol.webp"]:
            with self.subTest(brand=brand):
                request, http = self.asset_fixture(brand)
                with mock.patch.object(deploy, "health"), mock.patch.object(deploy, "request", side_effect=http):
                    result = deploy.checks("http://127.0.0.1:8080", request)
                self.assertIn(brand, request["assets"])
                self.assertTrue(result["assets"][brand]["html_url_verified"])

    def test_same_length_corrupt_brand_image_is_rejected(self):
        request, http = self.asset_fixture("brand-mark.png", corrupt=True)
        with mock.patch.object(deploy, "health"), mock.patch.object(deploy, "request", side_effect=http), \
             self.assertRaisesRegex(RuntimeError, "asset_sha256_brand-mark.png"):
            deploy.checks("http://127.0.0.1:8080", request)

    def test_brand_image_mime_must_match(self):
        request, http = self.asset_fixture("brand-mark.png", wrong_mime=True)
        with mock.patch.object(deploy, "health"), mock.patch.object(deploy, "request", side_effect=http), \
             self.assertRaisesRegex(RuntimeError, "asset_content_type_brand-mark.png"):
            deploy.checks("http://127.0.0.1:8080", request)

    def test_legacy_resource_is_rejected_even_with_all_fingerprinted_assets(self):
        request, http = self.asset_fixture(legacy=True)
        with mock.patch.object(deploy, "health"), mock.patch.object(deploy, "request", side_effect=http), \
             self.assertRaisesRegex(RuntimeError, "unversioned_page_resource"):
            deploy.checks("http://127.0.0.1:8080", request)

    def test_asset_set_cannot_expand_after_stage(self):
        request, http = self.asset_fixture("brand-mark.png")
        request["assets_frozen"] = True
        with mock.patch.object(deploy, "health"), mock.patch.object(deploy, "request", side_effect=http), \
             self.assertRaisesRegex(RuntimeError, "page_asset_set_changed_after_stage"):
            deploy.checks("http://127.0.0.1:8080", request)

    def simulate_switch(self, *, had_managed_unit, public_failure=False, stage_only=False, changed_pid=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            install, units, folder = root / "install", root / "units", root / "run"
            for path in [install, units, folder]:
                path.mkdir()
            old_binary, old_runner = root / "old-ssmcpd", root / "old-runner.js"
            old_binary.write_bytes(b"current production binary")
            old_runner.write_bytes(b"current production runner")
            original_unit = units / "97-manual.conf"
            original_unit.write_text("[Service]\nExecStart=\nExecStart=" + str(old_binary) + " serve\n")
            managed = units / "zzz-release.conf"
            original_managed = ("[Service]\n# previous managed release\nExecStart=\nExecStart=" + str(old_binary) +
                                " serve\nExecStartPre=\nExecStartPre=" + str(old_binary) + " check-runtime\n").encode()
            saved = None
            if had_managed_unit:
                managed.write_bytes(original_managed)
                managed.chmod(0o640)
                saved = {"content": base64.b64encode(original_managed).decode(), "sha256": deploy.sha(managed), "mode": 0o640}
            for name, body in [("candidate", b"new release binary"), ("runner.js", b"new release runner"), ("backup.tar", b"checkpoint")]:
                (folder / name).write_bytes(body)
            before = {"pid": 123, "exe": str(old_binary), "argv": [str(old_binary), "serve"],
                      "live_sha256": deploy.sha(old_binary), "units": {str(path): deploy.sha(path) for path in units.iterdir()},
                      "config": str(root / "config"), "private_files": {}, "runtime_files": {str(old_runner): deploy.sha(old_runner)},
                      "proxy_files": {}, "sandbox": {}, "start_commands": [str(old_binary) + " serve"],
                      "prestart_commands": [str(old_binary) + " check-runtime"]}
            old_html = b'<body><section id="pricing">price</section><footer><nav aria-label="\xe7\xbd\x91\xe7\xab\x99\xe6\x94\xbf\xe7\xad\x96"><a href="/legal/privacy">p</a><a href="/legal/terms">t</a><a href="/legal/refund">r</a></nav></footer></body>'
            request = {"before": before, "run_id": "123-1", "commit": COMMIT, "version": TAG[1:], "public_url": "https://example.test",
                       "expected_live_sha256": before["live_sha256"], "expected_live_exe": before["exe"],
                       "daemon_sha256": deploy.sha(folder / "candidate"), "runner_sha256": deploy.sha(folder / "runner.js"),
                       "backup_sha256": deploy.sha(folder / "backup.tar"), "helper_sha256": deploy.sha(deploy.__file__),
                       "assets": {}, "release": {"repository": deploy.REPOSITORY, "tag": TAG, "commit": COMMIT},
                       "managed_dropin_before": saved,
                       "public_baseline": {"footer_count": 1, "proxy_fragment_sha256": hashlib.sha256(deploy.proxy_fragment(old_html)).hexdigest()}}
            if stage_only:
                request["stage_only"] = True
                del request["backup_sha256"]
                (folder / "backup.tar").unlink()
            request_path = folder / "request.json"
            request_path.write_text(json.dumps(request))
            prefix = install / ("release-" + COMMIT[:12] + "-" + request["daemon_sha256"][:12])
            new_binary, new_runner = prefix / "bin/ssmcpd", prefix / "libexec/ssmcp/code-mode.js"
            state = {"new": False, "restarts": 0, "staged": False}
            def stage(_candidate, _request):
                state["staged"] = True
                return {"fresh_state": True}
            def snapshot(**_):
                if not state["new"]:
                    current = copy.deepcopy(before)
                    if state["staged"] and changed_pid:
                        current["pid"] += 1
                    return current
                after = copy.deepcopy(before)
                after.update(exe=str(new_binary), argv=[str(new_binary), "serve"], live_sha256=request["daemon_sha256"],
                             units={str(path): deploy.sha(path) for path in units.iterdir()},
                             runtime_files={str(new_runner): request["runner_sha256"]},
                             start_commands=[str(new_binary) + " serve"], prestart_commands=[str(new_binary) + " check-runtime"])
                return after
            def run(argv, **_):
                if argv[-1] == "--version":
                    return "ssmcpd " + TAG[1:]
                if argv[:2] == ["systemctl", "restart"]:
                    state["new"] = managed.exists() and str(new_binary) in managed.read_text()
                    state["restarts"] += 1
                    return ""
                if argv == ["systemctl", "daemon-reload"]:
                    return ""
                raise AssertionError("unexpected simulated command")
            def checks(_base, _request, **options):
                if options.get("public") and public_failure:
                    raise RuntimeError("public_asset_mismatch")
                return {"health": True, "homepage": True}
            original_open = open
            def local_open(path, *args, **kwargs):
                return original_open(root / "deployment.lock" if str(path) == "/run/lock/ssmcp-deployment.lock" else path, *args, **kwargs)
            def mapped_path(path):
                return install if str(path) == "/usr/local/lib/ssmcp" else Path(path)
            with mock.patch.object(deploy, "Path", side_effect=mapped_path), mock.patch.object(deploy, "MANAGED_DROPIN", managed), \
                 mock.patch("builtins.open", side_effect=local_open), mock.patch.object(deploy, "snapshot", side_effect=snapshot), \
                 mock.patch.object(deploy, "run", side_effect=run), mock.patch.object(deploy, "stage", side_effect=stage), \
                 mock.patch.object(deploy, "checks", side_effect=checks), mock.patch.object(deploy, "health"), \
                 mock.patch.object(deploy, "request", return_value=(200, old_html)), \
                 mock.patch.object(deploy, "current_sha", side_effect=lambda **_: snapshot()["live_sha256"]), \
                 mock.patch.object(deploy.shutil, "disk_usage", return_value=SimpleNamespace(free=1_000_000_000)):
                exit_code = deploy.apply(request_path)
            receipt = json.loads((folder / "receipt.json").read_text())
            self.assertEqual(deploy.sha(original_unit), before["units"][str(original_unit)])
            self.assertEqual(old_runner.read_bytes(), b"current production runner")
            self.assertEqual(old_binary.read_bytes(), b"current production binary")
            if stage_only:
                self.assertEqual(exit_code, int(changed_pid))
                self.assertEqual(receipt["status"], "failed" if changed_pid else "staged")
                self.assertEqual(receipt["stage_checks"], {"fresh_state": True})
                self.assertEqual(state["restarts"], 0)
                self.assertEqual(managed.read_bytes(), original_managed)
                self.assertFalse(prefix.exists())
                self.assertNotIn("rollback_checkpoint_verified", receipt)
                if changed_pid:
                    self.assertEqual(receipt["failure"]["reason"], "production_changed_after_stage")
                else:
                    self.assertEqual(receipt["running_sha256"], before["live_sha256"])
            elif public_failure:
                self.assertEqual(exit_code, 1)
                self.assertEqual(receipt["status"], "rolled_back")
                self.assertTrue(receipt["rollback_runtime_fingerprint_verified"])
                self.assertFalse(state["new"])
                self.assertEqual(state["restarts"], 2)
                self.assertFalse(prefix.exists())
                if had_managed_unit:
                    self.assertEqual(managed.read_bytes(), original_managed)
                    self.assertEqual(managed.stat().st_mode & 0o777, 0o640)
                else:
                    self.assertFalse(managed.exists())
            else:
                self.assertEqual(exit_code, 0)
                self.assertEqual(receipt["status"], "deployed")
                self.assertTrue(state["new"])
                self.assertEqual(state["restarts"], 1)
                self.assertIn("ExecStartPre=\nExecStartPre=" + str(new_binary) + " check-runtime", managed.read_text())
                self.assertEqual(new_runner.read_bytes(), b"new release runner")

    def test_stage_only_uses_no_backup_and_does_not_restart(self):
        self.simulate_switch(had_managed_unit=True, stage_only=True)

    def test_stage_only_requires_same_live_process(self):
        self.simulate_switch(had_managed_unit=True, stage_only=True, changed_pid=True)

    def test_first_managed_deployment_preserves_manual_dropins(self):
        self.simulate_switch(had_managed_unit=False, public_failure=False)

    def test_next_managed_deployment_replaces_existing_preflight(self):
        self.simulate_switch(had_managed_unit=True, public_failure=False)

    def test_failed_first_deployment_removes_only_its_dropin(self):
        self.simulate_switch(had_managed_unit=False, public_failure=True)

    def test_failed_next_deployment_restores_exact_previous_dropin(self):
        self.simulate_switch(had_managed_unit=True, public_failure=True)



class ReleaseHistoryRewriteContracts(unittest.TestCase):
    setUp = DeploymentContracts.setUp
    release = DeploymentContracts.release
    SOURCE = "c" * 40

    def fixture(self):
        release, manifest = self.release()
        raw = (json.dumps(manifest, indent=2) + "\n").encode()
        retained = hashlib.sha256(b"synthetic retained git entries").hexdigest()
        table = {"schema": 1, "entries": [{
            "repository": deploy.REPOSITORY, "tag": TAG,
            "build_commit": COMMIT, "source_commit": self.SOURCE,
            "manifest_sha256": hashlib.sha256(raw).hexdigest(),
            "proof": {
                "kind": "retained-git-tree-v1",
                "build_tree": "d" * 40, "source_tree": "e" * 40,
                "build_retained_sha256": retained, "source_retained_sha256": retained,
                "removed_paths_sha256": hashlib.sha256(b'["docs/old-screenshot.png"]').hexdigest(),
                "removed_path_count": 1,
                "audit_sha256": hashlib.sha256(b"synthetic independently reviewed proof").hexdigest(),
            },
        }]}
        return release, manifest, raw, table

    def trusted(self, table, checksum=None):
        encoded = json.dumps(table, sort_keys=True, separators=(",", ":"),
                             ensure_ascii=True, allow_nan=False).encode("ascii")
        return mock.patch.multiple(
            deploy, RELEASE_SOURCE_REWRITES=table,
            RELEASE_SOURCE_REWRITES_SHA256=checksum or hashlib.sha256(encoded).hexdigest())

    def select(self, release, manifest, raw, source=None):
        return deploy.select_release(TAG, source or self.SOURCE, release, manifest,
                                     "x86_64", manifest_bytes=raw)

    def test_equal_source_release_does_not_consult_history_exceptions(self):
        release, manifest = self.release()
        with mock.patch.object(deploy, "release_source_rewrite",
                               side_effect=AssertionError("ordinary release used exception path")):
            record = deploy.select_release(TAG, COMMIT, release, manifest, "x86_64")
        self.assertEqual(record["commit"], COMMIT)
        self.assertEqual(record["build_commit"], COMMIT)
        self.assertEqual(record["source_commit"], COMMIT)
        self.assertNotIn("source_history_rewrite", record)

    def test_reviewed_history_map_retains_original_build_and_clean_source(self):
        release, manifest, raw, table = self.fixture()
        original = copy.deepcopy(manifest)
        with self.trusted(table):
            record = self.select(release, manifest, raw)
        self.assertEqual(record["commit"], self.SOURCE)
        self.assertEqual(record["source_commit"], self.SOURCE)
        self.assertEqual(record["build_commit"], COMMIT)
        self.assertEqual(record["archive_sha256"], "b" * 64)
        self.assertEqual(record["archive_url"], URL)
        self.assertFalse(record["source_history_rewrite"]["artifact_rebuilt"])
        self.assertEqual(record["source_history_rewrite"]["manifest_sha256"],
                         hashlib.sha256(raw).hexdigest())
        self.assertEqual(record["source_history_rewrite"]["audit_sha256"],
                         table["entries"][0]["proof"]["audit_sha256"])
        self.assertEqual(manifest, original)

    def test_release_metadata_cannot_advertise_its_own_source_exception(self):
        release, manifest, raw, table = self.fixture()
        release["source_rewrites"] = table
        release["source_commit"] = self.SOURCE
        with self.trusted({"schema": 1, "entries": []}), \
             self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
            self.select(release, manifest, raw)

    def test_wrong_tag_build_source_or_manifest_digest_is_rejected(self):
        for field, value in [
            ("tag", "v0.10.4"), ("build_commit", "f" * 40),
            ("source_commit", "f" * 40), ("manifest_sha256", "f" * 64)
        ]:
            release, manifest, raw, table = self.fixture()
            table["entries"][0][field] = value
            with self.subTest(field=field), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
                self.select(release, manifest, raw)

    def test_wrong_requested_source_and_changed_original_build_are_rejected(self):
        release, manifest, raw, table = self.fixture()
        with self.trusted(table), self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
            self.select(release, manifest, raw, source="f" * 40)
        manifest["commit"] = "f" * 40
        with self.trusted(table), self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
            self.select(release, manifest, json.dumps(manifest).encode())

    def test_exact_original_manifest_bytes_are_required(self):
        release, manifest, raw, table = self.fixture()
        for changed in (None, bytearray(raw), raw + b" ", b"{invalid"):
            with self.subTest(raw_type=type(changed).__name__), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
                self.select(release, manifest, changed)

    def test_manifest_object_cannot_diverge_from_the_pinned_bytes(self):
        release, manifest, raw, table = self.fixture()
        # Python's ordinary dict equality considers 200.0 == 200; JSON identity must not.
        for changed in ({"untrusted": True}, {"bytes": 200.0}):
            supplied = copy.deepcopy(manifest)
            supplied["artifacts"][0].update(changed)
            with self.subTest(changed=changed), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "release_source_mismatch"):
                self.select(release, supplied, raw)

    def test_unreviewed_allowlist_mutation_fails_the_canonical_pin(self):
        release, manifest, raw, table = self.fixture()
        with self.trusted(table):
            table["entries"][0]["source_commit"] = "f" * 40
            with self.assertRaisesRegex(RuntimeError, "release_source_map_invalid"):
                self.select(release, manifest, raw, source="f" * 40)

    def test_other_repository_and_duplicate_tag_are_not_authorized(self):
        for kind in ("repository", "duplicate"):
            release, manifest, raw, table = self.fixture()
            if kind == "repository":
                table["entries"][0]["repository"] = "other/ssmcp"
            else:
                table["entries"].append(copy.deepcopy(table["entries"][0]))
            with self.subTest(kind=kind), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "release_source_map_invalid"):
                self.select(release, manifest, raw)

    def test_invalid_or_unequal_retained_tree_proof_is_rejected(self):
        changes = [
            ("kind", "unverified"),
            ("build_tree", "short"),
            ("source_retained_sha256", "f" * 64),
            ("removed_paths_sha256", ""),
            ("removed_path_count", True),
            ("removed_path_count", -1),
            ("removed_path_count", 0),
            ("source_tree", "d" * 40),
            ("audit_sha256", None),
        ]
        for field, value in changes:
            release, manifest, raw, table = self.fixture()
            table["entries"][0]["proof"][field] = value
            with self.subTest(field=field, value=value), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "release_source_map_invalid"):
                self.select(release, manifest, raw)

    def test_ancestor_only_rewrite_requires_the_same_tree(self):
        release, manifest, raw, table = self.fixture()
        proof = table["entries"][0]["proof"]
        proof["source_tree"] = proof["build_tree"]
        proof["removed_path_count"] = 0
        proof["removed_paths_sha256"] = hashlib.sha256(b"[]").hexdigest()
        with self.trusted(table):
            record = self.select(release, manifest, raw)
        self.assertEqual(record["source_history_rewrite"]["removed_path_count"], 0)
        self.assertFalse(record["source_history_rewrite"]["artifact_rebuilt"])

    def test_mapped_release_keeps_published_and_archive_integrity_gates(self):
        for field, value in (("draft", True), ("prerelease", True), ("published_at", None)):
            release, manifest, raw, table = self.fixture()
            release[field] = value
            with self.subTest(field=field), self.trusted(table), \
                 self.assertRaisesRegex(RuntimeError, "published_stable_release_required"):
                self.select(release, manifest, raw)
        for field, value in [
            ("size", 201), ("digest", "sha256:" + "c" * 64),
            ("browser_download_url", "https://example.test/file")
        ]:
            release, manifest, raw, table = self.fixture()
            release["assets"][0][field] = value
            with self.subTest(field=field), self.trusted(table), self.assertRaises(RuntimeError):
                self.select(release, manifest, raw)
        release, manifest, raw, table = self.fixture()
        release["assets"].append(copy.deepcopy(release["assets"][0]))
        with self.trusted(table), self.assertRaisesRegex(RuntimeError, "one_server_archive_required"):
            self.select(release, manifest, raw)

    def test_prepare_uses_exact_manifest_bytes_and_clean_source_for_runtime_files(self):
        release, manifest, raw, table = self.fixture()
        api = "https://api.github.com/repos/" + deploy.REPOSITORY + "/releases/tags/" + TAG
        manifest_url = "https://github.com/" + deploy.REPOSITORY + "/releases/download/" + TAG + "/release-manifest.json"
        release["assets"].append({
            "name": "release-manifest.json", "browser_download_url": manifest_url,
            "digest": "sha256:" + hashlib.sha256(raw).hexdigest()
        })
        source_url = "https://raw.githubusercontent.com/" + deploy.REPOSITORY + "/" + self.SOURCE
        runner = b"synthetic retained code-mode runner"
        fetched = []

        def fetch(url, maximum, destination=None):
            fetched.append(url)
            if destination is not None:
                self.assertEqual(url, URL)
                self.assertEqual(maximum, 200)
                destination.write_bytes(b"synthetic archive")
                return destination.stat().st_size
            if url == api:
                return json.dumps(release).encode()
            if url == manifest_url:
                return raw
            if url == source_url + "/src/code_mode.js":
                return runner
            for name in deploy.CORE_ASSET_NAMES:
                if url == source_url + "/src/portal/" + name:
                    return ("synthetic " + name).encode()
            raise AssertionError("unexpected source fetch: " + url)

        def extract(_archive, folder, record):
            self.assertEqual(record["build_commit"], COMMIT)
            (folder / "candidate").write_bytes(b"synthetic never-executed binary")
            (folder / "runner.js").write_bytes(runner)

        before = {"exe": "/synthetic/live-ssmcpd", "live_sha256": "d" * 64, "prestart_commands": []}
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)

            def version(argv):
                self.assertIn(argv, [[str(folder / "candidate"), "--version"],
                                     [before["exe"], "--version"]])
                return "ssmcpd 0.10.3"

            intent = {"tag": TAG, "commit": self.SOURCE, "run_id": "123-1", "stage_only": True}
            with self.trusted(table), \
                 mock.patch.object(deploy, "configure_target", return_value="https://example.test"), \
                 mock.patch.object(deploy, "fetch", side_effect=fetch), \
                 mock.patch.object(deploy, "extract_runtime", side_effect=extract), \
                 mock.patch.object(deploy, "snapshot", return_value=before), \
                 mock.patch.object(deploy, "run", side_effect=version), \
                 mock.patch.object(deploy.platform, "machine", return_value="x86_64"):
                request = deploy.prepare(intent, folder)
            saved = json.loads((folder / "request.json").read_text())
            self.assertEqual(request["release"], saved["release"])
            self.assertEqual(saved["release"]["build_commit"], COMMIT)
            self.assertEqual(saved["release"]["source_commit"], self.SOURCE)
            self.assertEqual(saved["release"]["source_history_rewrite"]["manifest_sha256"],
                             hashlib.sha256(raw).hexdigest())
            self.assertFalse((folder / "backup.tar").exists())
        self.assertEqual(fetched.count(manifest_url), 1)
        self.assertIn(source_url + "/src/code_mode.js", fetched)
        self.assertTrue(all(source_url + "/src/portal/" + name in fetched
                            for name in deploy.CORE_ASSET_NAMES))
        self.assertFalse(any("/" + COMMIT + "/" in url for url in fetched))


if __name__ == "__main__":
    unittest.main(verbosity=2)
