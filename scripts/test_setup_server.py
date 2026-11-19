#!/usr/bin/env python3
import json
import tempfile
import unittest
from unittest.mock import Mock, patch
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
            template = Path(__file__).resolve().parents[1]/'packaging/nginx-ssmcp-portal.conf.example'
            proxy.write_text(template.read_text())
            changes, state, base = setup.configuration(config, None, proxy, template)
            self.assertNotIn('8086', changes[proxy])
            self.assertEqual(base, 'http://127.0.0.1:8080')
            self.assertEqual(state, Path('/var/lib/ssmcpd/portal'))
            proxy.write_text('location / { proxy_pass http://custom-upstream; }')
            with self.assertRaises(ValueError):
                setup.configuration(config, None, proxy, template)

    def test_new_site_uses_origin_and_loopback_port(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config, proxy = root/'config', root/'proxy'
            config.write_text(CONFIG)
            template = Path(__file__).resolve().parents[1]/'packaging/nginx-ssmcp.conf.example'
            changes, _, _ = setup.configuration(config, None, proxy, template)
            self.assertIn('server_name relay.example.com;', changes[proxy])
            self.assertNotIn('ssmcp.example.com', changes[proxy])

    def test_health_probe_cancels_its_client_login_on_success_and_failure(self):
        for unauthorized in (True, False):
            with self.subTest(unauthorized=unauthorized):
                connection = Mock()
                replies = [
                    (200, b"ok"),
                    (200, json.dumps({"device_code":"health-check-fixture"}).encode()),
                    (401 if unauthorized else 200, b"{}"),
                ]
                if unauthorized:
                    replies.append((401, b"{}"))
                replies.append((200, b"{}"))
                connection.getresponse.side_effect = [
                    Mock(status=status, read=Mock(return_value=body)) for status, body in replies
                ]
                with patch.object(setup.http.client, "HTTPConnection", return_value=connection):
                    if unauthorized:
                        setup.probe("http://127.0.0.1:8080")
                    else:
                        with self.assertRaises(RuntimeError):
                            setup.probe("http://127.0.0.1:8080")
                calls = connection.request.call_args_list
                self.assertEqual(calls[1].args[:2], ("POST", "/api/client/login/start"))
                self.assertEqual(calls[-1].args[:2], ("POST", "/api/client/login/cancel"))
                self.assertEqual(json.loads(calls[-1].kwargs["body"]),
                                 {"device_code":"health-check-fixture"})
                self.assertEqual(connection.close.call_count, len(calls))

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
                self.assertIn(['systemctl', 'disable', 'ssmcp-portal.service'], calls)

    def test_migration_stops_legacy_service_and_enables_gateway(self):
        self.transaction()

    def test_failed_health_probe_restores_files_and_legacy_service(self):
        self.transaction(fail=True)

    def test_repeated_setup_does_not_restart_healthy_gateway(self):
        self.transaction(unchanged=True, portal=False)


class StageIsolationTests(unittest.TestCase):
    def test_stage_checks_runtime_as_service_user_before_starting_gateway(self):
        import deploy_server
        binary = Path('/opt/ssmcp/releases') / ('a' * 40) / 'usr/bin/ssmcpd'
        command = deploy_server.stage_service_command(
            'ssmcp-stage-test', 'ssmcp-stage-test', binary, Path('/private/config.toml'))
        self.assertIn('--uid=ssmcpd', command)
        self.assertIn('--gid=ssmcp', command)
        self.assertIn('--setenv=PATH=/usr/bin:/bin', command)
        self.assertIn('--property=ExecStartPre='+str(binary)+' check-runtime', command)
        self.assertEqual(command[command.index(str(binary))+1], 'serve')
        self.assertIn('/run/ssmcp-stage-test/control.sock', command)
        self.assertIn('/var/lib/ssmcp-stage-test/oauth-clients.toml', command)

    def test_stage_preserves_packaged_service_runtime_restrictions(self):
        import deploy_server
        service = Path(__file__).resolve().parents[1] / 'packaging/ssmcpd.service'
        restrictions = {'NoNewPrivileges', 'PrivateTmp', 'PrivateDevices', 'ProtectSystem',
                        'ProtectHome', 'ProtectKernelTunables', 'ProtectControlGroups',
                        'RestrictAddressFamilies', 'ReadWritePaths', 'UMask'}
        command = deploy_server.stage_service_command(
            'ssmcp-stage-test', 'ssmcp-stage-test', Path('/usr/bin/ssmcpd'), Path('/config'))
        for line in service.read_text().splitlines():
            if line.partition('=')[0] in restrictions:
                self.assertIn('--property='+line, command)
        self.assertIn('ExecStartPre=/usr/local/bin/ssmcpd check-runtime', service.read_text())

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
