#!/usr/bin/env python3
"""Private, offline fixtures. No real SSH, production filesystem, or extraction."""

import argparse
import contextlib
import copy
import datetime as dt
import errno
import gzip
import io
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

import backup_ssmcp as backup


class StrictBackup(unittest.TestCase):
    def setUp(self):
        previous_umask = os.umask(0o077)
        self.addCleanup(os.umask, previous_umask)
        self.temp = tempfile.TemporaryDirectory(prefix="ssmcp-backup-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.source = self.folder / "source"
        self.source.mkdir(mode=0o700)
        directories = {backup.ROOTS[i] for i in (0, 1, 2, 5)}
        for name in backup.ROOTS:
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            if name in directories:
                path.mkdir()
            else:
                path.write_bytes(b"fictional service fixture\n")
                path.chmod(0o640)
        self.data = self.source / "etc/ssmcpd/settings.toml"
        self.data.write_bytes(b"test_only=true\n")
        self.data.chmod(0o600)
        os.utime(self.data, ns=(1_700_000_000_123456789, 1_700_000_000_123456789))
        self.newline = self.source / "var/lib/ssmcpd/line\nbreak.json"
        self.newline.write_bytes(b'{"fictional":true}\n')
        self.newline.chmod(0o600)
        os.link(self.data, self.source / "var/lib/ssmcpd/settings-hardlink")
        (self.source / "etc/ssmcpd/current").symlink_to("settings.toml")
        os.setxattr(self.data, "user.fixture", b"\x00\xfffixture")
        self.value = backup.inventory(self.source)
        self.archive = self.folder / "fixture.tar.gz"
        with self.archive.open("xb") as stream:
            backup.capture_stream(stream, backup.digest(backup.encoded(self.value)), self.source)

    def sha(self, path=None):
        return backup.digest((path or self.archive).read_bytes())

    def verify(self, path=None, value=None):
        return backup.verify_archive(path or self.archive, value or self.value, self.sha(path))

    def rewritten(self, mutate=None, extra=None):
        target = self.folder / "modified.tar.gz"
        with tarfile.open(self.archive, "r:gz") as source, tarfile.open(target, "w:gz", format=tarfile.PAX_FORMAT) as destination:
            members = list(source)
            for i, original in enumerate(members):
                member = copy.deepcopy(original)
                data = source.extractfile(original).read() if original.isreg() else None
                if mutate:
                    member, data = mutate(member, data, i)
                destination.addfile(member, io.BytesIO(data) if data is not None else None)
            if extra:
                member, data = extra(members, source)
                destination.addfile(member, io.BytesIO(data) if data is not None else None)
        return target

    def window(self):
        now = dt.datetime.now(dt.timezone.utc)
        return {"schema": "ssmcp-controlled-write-window-v1", "host": "archczy", "roots": list(backup.ROOTS),
                "accounts_orders_authorization_writes_blocked": True, "archive_execution_authorized": True,
                "service_switch_authorized": False, "approval_reference": "OFFLINE FIXTURE ONLY",
                "write_fence_evidence": "Mock source owned by this test, no production fence assertion",
                "source_identity": self.identity(), "preflight_sha256": "0" * 64, "preflight_at_utc": now.isoformat(),
                "started_at_utc": (now - dt.timedelta(seconds=2)).isoformat(),
                "expires_at_utc": (now + dt.timedelta(minutes=10)).isoformat()}

    def identity(self):
        return {"machine_id_sha256": "0" * 64, "boot_id": "00000000-0000-0000-0000-000000000000",
                "pid": 123, "start_ticks": 456, "exe_path": "/etc/ssmcpd/settings.toml", "exe_sha256": backup.digest(self.data.read_bytes())}

    def args(self):
        window = self.folder / "window.json"
        window.write_bytes(backup.encoded(self.window()))
        window.chmod(0o600)
        known = self.folder / "known-hosts"
        known.write_text("fictional fixture, never used by ssh\n")
        known.chmod(0o600)
        return argparse.Namespace(window=str(window), host="archczy", known_hosts=str(known),
                                  output=str(self.folder / "bundle"), timeout=10)

    def fake_stream(self, command, output, errors, maximum, directory, timeout):
        value = dict(self.value, source_identity=self.identity())
        data = backup.encoded(value) if command == ["inventory"] else self.archive.read_bytes()
        self.assertLessEqual(len(data), maximum)
        output.write(data)
        return len(data)

    def test_roundtrip_links_newlines_xattrs_and_exact_metadata(self):
        result = self.verify()
        self.assertFalse(result["service_switch_authorized"])
        names = backup.validate_inventory(self.value)
        self.assertIn("var/lib/ssmcpd/line\nbreak.json", names)
        self.assertEqual(names["var/lib/ssmcpd/settings-hardlink"]["hardlink"], "etc/ssmcpd/settings.toml")
        self.assertEqual(names["etc/ssmcpd/current"]["target"], "settings.toml")
        self.assertEqual(names["etc/ssmcpd/settings.toml"]["mtime_ns"], 1_700_000_000_123456789)
        self.assertIn("user.fixture", names["etc/ssmcpd/settings.toml"]["xattrs"])

    def test_full_gzip_truncation_and_corruption_rejected_even_with_new_sha(self):
        data = self.archive.read_bytes()
        for label, broken in (("truncated", data[:-5]), ("bad_crc", data[:-8] + bytes([data[-8] ^ 1]) + data[-7:])):
            with self.subTest(label=label):
                path = self.folder / (label + ".gz")
                path.write_bytes(broken)
                with self.assertRaises((BackupErrorTypes)):
                    self.verify(path)

    def test_whole_archive_digest_checked(self):
        with self.assertRaisesRegex(backup.BackupError, "archive_digest_mismatch"):
            backup.verify_archive(self.archive, self.value, "0" * 64)

    def test_extra_duplicate_and_traversal_members_rejected(self):
        for name in ("etc/ssmcpd/unlisted", "../escape", "/etc/ssmcpd/absolute", "etc/ssmcpd/../escape"):
            with self.subTest(name=name):
                path = self.rewritten(extra=lambda members, source: (tarfile.TarInfo(name), b""))
                with self.assertRaises(backup.BackupError):
                    self.verify(path)
        path = self.rewritten(extra=lambda members, source: (copy.deepcopy(members[0]), None))
        with self.assertRaisesRegex(backup.BackupError, "duplicate"):
            self.verify(path)

    def test_content_and_metadata_drift_rejected(self):
        def content(member, data, i):
            if member.name == "etc/ssmcpd/settings.toml":
                data = bytes([data[0] ^ 1]) + data[1:]
            return member, data
        with self.assertRaisesRegex(backup.BackupError, "content_mismatch"):
            self.verify(self.rewritten(mutate=content))
        def metadata(member, data, i):
            if i == 0:
                member.mode ^= 1
            return member, data
        with self.assertRaisesRegex(backup.BackupError, "metadata_mismatch"):
            self.verify(self.rewritten(mutate=metadata))

    def test_tar_tail_and_concatenated_archive_rejected(self):
        raw = gzip.decompress(self.archive.read_bytes())
        for suffix in (b"X" * 512, raw, b"\0" * 20480):
            path = self.folder / "tail.gz"
            path.write_bytes(gzip.compress(raw + suffix))
            with self.assertRaises(backup.BackupError):
                self.verify(path)

    def test_source_content_drift_and_post_capture_drift_fail(self):
        expected = backup.digest(backup.encoded(self.value))
        self.data.write_bytes(b"altered source\n")
        with self.assertRaisesRegex(backup.BackupError, "source_changed_since_inventory"):
            backup.capture_stream(io.BytesIO(), expected, self.source)
        changed = copy.deepcopy(self.value)
        changed["entries"][0]["mode"] ^= 1
        with mock.patch.object(backup, "inventory", side_effect=[self.value, changed]):
            with self.assertRaisesRegex(backup.BackupError, "source_changed_during_archive"):
                backup.capture_stream(io.BytesIO(), expected, self.source)

    def test_escaping_symlink_and_ancestor_symlink_fail_closed(self):
        link = self.source / "etc/ssmcpd/current"
        link.unlink()
        link.symlink_to("/etc/passwd")
        with self.assertRaisesRegex(backup.BackupError, "outside_scope"):
            backup.inventory(self.source)
        link.unlink()
        folder = self.source / "var/lib/ssmcpd"
        shutil.rmtree(folder)
        folder.symlink_to(self.source / "etc/ssmcpd", target_is_directory=True)
        with self.assertRaises(BackupErrorTypes):
            backup.inventory(self.source)

    def test_unsupported_socket_or_fifo_is_rejected(self):
        os.mkfifo(self.source / "var/lib/ssmcpd/pipe")
        with self.assertRaisesRegex(backup.BackupError, "unsupported_source_type"):
            backup.inventory(self.source)

    def test_default_and_capture_without_execute_have_no_side_effects(self):
        for args in ([], ["capture"]):
            with mock.patch.object(backup.subprocess, "Popen", side_effect=AssertionError("network invoked")), mock.patch.object(backup.os, "mkdir", side_effect=AssertionError("write invoked")):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    self.assertEqual(backup.main(args), 0)
                self.assertEqual(json.loads(output.getvalue())["mode"], "plan")

    def test_private_bundle_roundtrip_and_no_overwrite(self):
        args = self.args()
        commands = [["inventory"], ["archive"]]
        with mock.patch.object(backup, "ssh_command", side_effect=commands), mock.patch.object(backup, "run_stream", side_effect=self.fake_stream):
            result = backup.capture(args)
        self.assertFalse(result["service_switch_authorized"])
        self.assertEqual(backup.verify_bundle(args.output)["members"], len(self.value["entries"]))
        self.assertEqual(stat.S_IMODE(os.stat(args.output).st_mode), 0o700)
        for path in Path(args.output).iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        with mock.patch.object(backup, "run_stream", side_effect=AssertionError("must not run")):
            with self.assertRaises(FileExistsError):
                backup.capture(args)

    def test_producer_nonzero_partial_never_verified(self):
        with backup.directory_fd(self.folder) as directory:
            with backup.private_file(directory, "partial") as out, backup.private_file(directory, "errors") as err:
                with self.assertRaisesRegex(backup.BackupError, "producer_nonzero_exit"):
                    backup.run_stream([sys.executable, "-c", "import sys;sys.stdout.buffer.write(b'partial');sys.exit(7)"], out, err, 100, directory, 10)
        self.assertEqual((self.folder / "partial").read_bytes(), b"partial")
        self.assertFalse((self.folder / "verified.json").exists())

    def test_stderr_is_bounded_and_drained_with_stdout(self):
        with backup.directory_fd(self.folder) as directory:
            with backup.private_file(directory, "bounded-output") as out, backup.private_file(directory, "bounded-errors") as err:
                command = [sys.executable, "-c", f"import sys;sys.stderr.buffer.write(b'x'*{backup.MAX_STDERR + 65536});sys.stderr.flush()"]
                with self.assertRaisesRegex(backup.BackupError, "stderr_size_limit"):
                    backup.run_stream(command, out, err, 1024, directory, 10)
        self.assertLessEqual((self.folder / "bounded-errors").stat().st_size, backup.MAX_STDERR)
        self.assertFalse((self.folder / "verified.json").exists())

    def test_nonempty_stderr_does_not_deadlock_stdout_capture(self):
        with backup.directory_fd(self.folder) as directory:
            with backup.private_file(directory, "mixed-output") as out, backup.private_file(directory, "mixed-errors") as err:
                command = [sys.executable, "-c", "import sys;sys.stderr.buffer.write(b'd'*131072);sys.stderr.flush();sys.stdout.buffer.write(b'payload')"]
                self.assertEqual(backup.run_stream(command, out, err, 100, directory, 10), 7)
        self.assertEqual((self.folder / "mixed-output").read_bytes(), b"payload")
        self.assertEqual((self.folder / "mixed-errors").stat().st_size, 131072)

    def test_capture_failure_retains_private_partial_without_receipt(self):
        args = self.args()
        def fail_archive(command, *values):
            count = self.fake_stream(command, *values)
            if command == ["archive"]:
                raise backup.BackupError("producer_nonzero_exit")
            return count
        with mock.patch.object(backup, "ssh_command", side_effect=[["inventory"], ["archive"]]), mock.patch.object(backup, "run_stream", side_effect=fail_archive):
            with self.assertRaisesRegex(backup.BackupError, "producer_nonzero_exit"):
                backup.capture(args)
        bundle = Path(args.output)
        self.assertTrue((bundle / "archive.partial.tar.gz").exists())
        self.assertFalse((bundle / "verified.json").exists())
        self.assertFalse((bundle / "archive.tar.gz").exists())

    def test_receipt_and_directory_sync_fail_before_success_marker(self):
        args = self.args()
        real_fsync = os.fsync
        for failure in ("receipt", "directory"):
            args.output = str(self.folder / ("sync-failure-" + failure))
            hit = []
            def failing_fsync(fd):
                path = os.readlink(f"/proc/self/fd/{fd}")
                receipt = path.endswith(("/verification.ready.json", "/verified.json"))
                if (failure == "receipt" and receipt) or (failure == "directory" and path == args.output):
                    hit.append(path)
                    raise OSError(errno.EIO, "injected private fixture sync failure")
                return real_fsync(fd)
            with self.subTest(failure=failure):
                with mock.patch.object(backup, "ssh_command", side_effect=[["inventory"], ["archive"]]), mock.patch.object(backup, "run_stream", side_effect=self.fake_stream), mock.patch.object(backup.os, "fsync", side_effect=failing_fsync):
                    with self.assertRaises(OSError):
                        backup.capture(args)
                self.assertTrue(hit)
                self.assertFalse((Path(args.output) / "verified.json").exists())
                with self.assertRaises(BackupErrorTypes):
                    backup.verify_bundle(args.output)

    def test_success_marker_publication_failure_is_not_verified(self):
        args = self.args()
        real_link = os.link
        def failing_publish(source, target, **options):
            if target == "verified.json":
                raise OSError(errno.EIO, "injected private fixture publication failure")
            return real_link(source, target, **options)
        with mock.patch.object(backup, "ssh_command", side_effect=[["inventory"], ["archive"]]), mock.patch.object(backup, "run_stream", side_effect=self.fake_stream), mock.patch.object(backup.os, "link", side_effect=failing_publish):
            with self.assertRaises(OSError):
                backup.capture(args)
        self.assertTrue((Path(args.output) / "verification.ready.json").exists())
        self.assertFalse((Path(args.output) / "verified.json").exists())
        with self.assertRaises(BackupErrorTypes):
            backup.verify_bundle(args.output)

    def test_window_expiry_and_missing_fence_are_rejected(self):
        for field in ("accounts_orders_authorization_writes_blocked", "archive_execution_authorized"):
            value = self.window()
            value[field] = False
            with self.assertRaises(backup.BackupError):
                backup.validate_window(value, "archczy")
        value = self.window()
        value["expires_at_utc"] = "2000-01-01T00:00:00+00:00"
        with self.assertRaises(backup.BackupError):
            backup.validate_window(value, "archczy")


    def test_changed_process_identity_rejected_before_archive(self):
        args = self.args()
        value = self.window()
        value["source_identity"]["start_ticks"] += 1
        Path(args.window).write_bytes(backup.encoded(value))
        with mock.patch.object(backup, "ssh_command", side_effect=[["inventory"]]) as command, mock.patch.object(backup, "run_stream", side_effect=self.fake_stream):
            with self.assertRaisesRegex(backup.BackupError, "source_identity_changed_since_preflight"):
                backup.capture(args)
        self.assertEqual(command.call_count, 1)
        self.assertFalse((Path(args.output) / "archive.partial.tar.gz").exists())

    def test_negative_mtime_roundtrip_preserves_nanoseconds(self):
        os.utime(self.data, ns=(-500000001, -500000001))
        value = backup.inventory(self.source)
        path = self.folder / "negative-mtime.tar.gz"
        with path.open("xb") as stream:
            backup.capture_stream(stream, backup.digest(backup.encoded(value)), self.source)
        self.verify(path, value)
        with tarfile.open(path, "r:gz") as archive:
            self.assertEqual(archive.getmember("etc/ssmcpd/settings.toml").pax_headers["mtime"], "-0.500000001")

    def test_owner_names_cannot_override_numeric_ownership(self):
        for field in ("uname", "gname"):
            def renamed(member, data, i):
                if i == 0:
                    setattr(member, field, "unexpected-owner")
                return member, data
            with self.subTest(field=field):
                with self.assertRaisesRegex(backup.BackupError, "metadata_mismatch"):
                    self.verify(self.rewritten(mutate=renamed))


BackupErrorTypes = (backup.BackupError, OSError, EOFError, tarfile.TarError)

if __name__ == "__main__":
    unittest.main()
