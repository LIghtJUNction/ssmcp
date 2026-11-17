#!/usr/bin/env python3
import tempfile
import unittest
from pathlib import Path
import setup_server as setup

CONFIG = '''[server]
public_url = "https://relay.example.com"
listen = "127.0.0.1:8080"
[auth.github]
client_id = "example"
client_secret = "private-placeholder"
[portal]
gateway_socket = "/run/ssmcpd/management.sock"
'''


class SetupTests(unittest.TestCase):
    def test_state_insertion_preserves_secrets_and_is_idempotent(self):
        result = setup.configure_text(CONFIG, Path('/var/lib/ssmcpd/portal'))
        self.assertIn('client_secret = "private-placeholder"', result)
        self.assertEqual(result, setup.configure_text(result, Path('/var/lib/ssmcpd/portal')))
        with self.assertRaises(ValueError):
            setup.configure_text(result, Path('/different'))

    def test_state_can_be_added_without_existing_portal_section(self):
        result = setup.configure_text(CONFIG.split('[portal]')[0], Path('/accounts'))
        self.assertEqual(setup.tomllib.loads(result)['portal']['state_dir'], '/accounts')

    def test_packaged_split_proxy_is_migrated_and_custom_upstream_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config, proxy = root/'config', root/'proxy'
            config.write_text(CONFIG)
            template = Path(__file__).resolve().parents[1]/'packaging/nginx-ssmcp.conf.example'
            proxy.write_text('location / { proxy_pass http://127.0.0.1:8086; }')
            changes, state, base = setup.configuration(config, None, proxy, template, extract_key=lambda _: 'public key fixture')
            self.assertNotIn('8086', changes[proxy])
            self.assertEqual(base, 'http://127.0.0.1:8080')
            self.assertEqual(state, Path('/var/lib/ssmcpd/portal'))
            proxy.write_text('location / { proxy_pass http://custom-upstream; }')
            with self.assertRaises(ValueError):
                setup.configuration(config, None, proxy, template, extract_key=lambda _: 'public key fixture')

    def test_new_site_uses_origin_and_loopback_port(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config, proxy = root/'config', root/'proxy'
            config.write_text(CONFIG)
            template = Path(__file__).resolve().parents[1]/'packaging/nginx-ssmcp.conf.example'
            changes, _, _ = setup.configuration(config, None, proxy, template, extract_key=lambda _: 'public key fixture')
            self.assertIn('server_name relay.example.com;', changes[proxy])
            self.assertEqual(changes[Path('/var/lib/ssmcp/server-key.pem')], 'public key fixture')
            self.assertEqual(setup.tomllib.loads(changes[config])['server']['identity_public_key'], '/var/lib/ssmcp/server-key.pem')
            self.assertNotIn('ssmcp.example.com', changes[proxy])

    def test_public_identity_is_exported_from_certificate_without_reading_private_key(self):
        calls = []
        def runner(argv):
            calls.append(argv)
            return '-----BEGIN PUBLIC KEY-----\nfixture\n-----END PUBLIC KEY-----'
        key = setup.certificate_public_key('ssl_certificate /cert/fullchain.pem;\nssl_certificate_key /cert/private.pem;', runner)
        self.assertIn('BEGIN PUBLIC KEY', key)
        self.assertEqual(calls, [['openssl', 'x509', '-in', '/cert/fullchain.pem', '-pubkey', '-noout']])
        with self.assertRaises(ValueError):
            setup.certificate_public_key('ssl_certificate /a;\nssl_certificate /b;', runner)

    def transaction(self, fail=False, unchanged=False, portal=True):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config, proxy = root/'config', root/'proxy'
            config.write_text('old-config')
            proxy.write_text('old-proxy')
            calls = []
            def runner(args):
                calls.append(args)
                if args[:2] == ['systemctl', 'show']:
                    if 'LoadState' in args:
                        return 'loaded'
                    return 'active' if args[2] != 'ssmcp-portal.service' or portal else 'inactive'
                return ''
            def health(_):
                if fail:
                    raise RuntimeError('probe failed')
            changes = {config:'old-config' if unchanged else 'new-config',
                       proxy:'old-proxy' if unchanged else 'new-proxy'}
            if fail:
                with self.assertRaises(RuntimeError):
                    setup.apply(changes, 'http://127.0.0.1:8080', root/'checkpoints', runner, health)
                self.assertEqual(config.read_text(), 'old-config')
                self.assertEqual(proxy.read_text(), 'old-proxy')
                self.assertIn(['systemctl', 'start', 'ssmcp-portal.service'], calls)
            else:
                result = setup.apply(changes, 'http://127.0.0.1:8080', root/'checkpoints', runner, health)
                self.assertEqual(result, 'unchanged' if unchanged and not portal else 'configured')
                if unchanged and not portal:
                    self.assertFalse(any('restart' in call for call in calls))
                else:
                    self.assertIn(['systemctl', 'stop', 'ssmcp-portal.service'], calls)
                self.assertIn(['systemctl', 'disable', '--now', 'ssmcp-portal.service'], calls)
                self.assertIn(['systemctl', 'disable', '--now', 'ssmcp-portal-ssh.timer'], calls)
                self.assertFalse(any(call[:3] == ['systemctl', 'enable', '--now'] and any('ssh' in part for part in call) for call in calls))

    def test_migration_stops_legacy_service_and_enables_gateway(self):
        self.transaction()

    def test_failed_health_probe_restores_files_and_legacy_service(self):
        self.transaction(fail=True)

    def test_repeated_setup_does_not_restart_healthy_gateway(self):
        self.transaction(unchanged=True, portal=False)


class StageIsolationTests(unittest.TestCase):
    def test_staging_moves_accounts_audit_and_management_into_its_own_lease(self):
        import deploy_server
        text = CONFIG + 'state_dir = "/production/accounts"\naudit_dir = "/production/audit"\n'
        result = deploy_server.isolated_configuration(text, Path('/stage/state'), Path('/stage/run'))
        portal = setup.tomllib.loads(result)['portal']
        self.assertEqual(portal['state_dir'], '/stage/state/portal')
        self.assertEqual(portal['audit_dir'], '/stage/state/audit')
        self.assertEqual(portal['gateway_socket'], '/stage/run/management.sock')
        self.assertIn('client_secret = "private-placeholder"', result)
        self.assertIn('/production/accounts', text)


if __name__ == '__main__':
    unittest.main()
