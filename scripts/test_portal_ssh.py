#!/usr/bin/env python3
"""Unprivileged safety tests: system users and SSH configuration are never changed."""
import base64
import hashlib
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch
import sync_portal_ssh as sync


def fixture():
    blob = b'\0\0\0\x0bssh-ed25519\0\0\0\x20' + bytes([1]) * 32
    key = {'public_key': 'ssh-ed25519 ' + base64.b64encode(blob).decode(),
           'fingerprint': 'SHA256:' + base64.b64encode(hashlib.sha256(blob).digest()).decode().rstrip('=')}
    return {'accounts': {'123': {'subject': '123', 'username': 'gh-123', 'keys': [key]}}}


class SyncTests(unittest.TestCase):
    def test_parser_rejects_injection_bad_blob_and_binding(self):
        self.assertIn('gh-123', sync.validate(fixture()))
        for value in ['../123', '-1', '01', '0', '18446744073709551616']:
            data = fixture()
            data['accounts'][value] = data['accounts'].pop('123')
            with self.assertRaises(ValueError):
                sync.validate(data)
        for key in ['command="id" ssh-ed25519 AAAA', 'ssh-ed25519 AAAA', 'ssh-ed25519 AAAA\n']:
            data = fixture()
            data['accounts']['123']['keys'][0]['public_key'] = key
            with self.assertRaises(ValueError):
                sync.validate(data)

    def test_collision_preserves_existing_files_and_never_runs_useradd(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            old = directory / 'gh-123'
            old.write_text('legacy key')
            user = SimpleNamespace(pw_uid=1001, pw_dir='/home/old', pw_shell='/bin/bash')
            with patch.object(sync.subprocess, 'run') as run:
                with self.assertRaises(ValueError):
                    sync.sync(sync.validate(fixture()), directory, lookup=lambda _: user, run=run)
                run.assert_not_called()
            self.assertEqual(old.read_text(), 'legacy key')

    def test_missing_ssh_restrictions_prevent_key_publication(self):
        with patch.object(sync.subprocess, 'check_output', return_value='ForceCommand none\n'):
            with self.assertRaises(ValueError):
                sync.verify_sshd('gh-123')

    def test_creation_rotation_and_removal_use_only_dedicated_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            users = {}
            def lookup(name):
                if name not in users:
                    raise KeyError(name)
                return users[name]
            def run(args, check):
                self.assertEqual(args[-1], 'gh-123')
                self.assertTrue(check)
                self.assertIn('ssmcp,ssmcp-portal', args)
                users[args[-1]] = SimpleNamespace(pw_uid=1001, pw_dir='/nonexistent', pw_shell='/bin/sh')
            # Production registry requires root ownership; test as the current test user.
            original = sync.read_json
            with patch.object(sync, 'read_json', side_effect=lambda p, uid, mode: original(p, sync.os.geteuid(), mode)):
                sync.sync(sync.validate(fixture()), directory, lookup, run, verify=lambda _: None)
                self.assertTrue((directory / 'gh-123').read_text().startswith('restrict ssh-ed25519 '))
                self.assertEqual(json.loads((directory / 'owners.json').read_text()), {'gh-123': 1001})
                sync.sync({'gh-123': []}, directory, lookup, run, verify=lambda _: None)
                self.assertEqual((directory / 'gh-123').read_text(), '')
                sync.sync({}, directory, lookup, run, verify=lambda _: None)
                self.assertIn('gh-123', users)
                users['gh-123'].pw_uid = 1002
                with self.assertRaises(ValueError):
                    sync.sync({'gh-123': []}, directory, lookup, run, verify=lambda _: None)


if __name__ == '__main__':
    unittest.main()
