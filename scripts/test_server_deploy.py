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


if __name__ == "__main__":
    unittest.main(verbosity=2)
