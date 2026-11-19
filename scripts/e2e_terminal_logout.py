#!/usr/bin/env python3
"""Real logout UI teardown against owned PTYs and a synthetic account gateway."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--repo', type=Path, required=True)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--decoder', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--source', default='unspecified')
args = parser.parse_args()
sys.path.insert(0, str(args.repo / 'scripts'))
import e2e_account_relay as relay
from e2e_dashboard import Frontend as DashboardFrontend, Decoder, text


class LogoutHandler(relay.Handler):
    def do_POST(self):
        if self.path != '/api/client/logout':
            return super().do_POST()
        self.state.requests.append(('POST', self.path))
        size = int(self.headers.get('Content-Length', '0'))
        assert 0 <= size <= 65536
        self.rfile.read(size)
        assert self.bearer() == self.state.account_token
        self.state.logout_calls += 1
        # Exercise the failure ordering seen in a real revoked WebSocket:
        # transport ends before the HTTP result is available to the UI worker.
        with self.state.lock:
            connections = list(self.state.sessions.values())
        for connection in connections:
            connection.close(4001)
        self.state.logout_seen.set()
        if not self.state.logout_release.wait(8):
            self.state.errors.append('fixture logout response was not released')
            return self.respond(500, {'error': 'fixture_timeout'})
        if self.state.logout_status == 200:
            self.state.logged_out = True
            return self.respond(200, {'logged_out': True})
        self.respond(503, {'error': 'fixture_remote_unavailable'})


class Frontend(DashboardFrontend):
    def __init__(self, harness, config):
        self.harness = harness
        self.decoder = Decoder(args.decoder)
        self.master, self.slave = os.openpty()
        self.original_termios = termios.tcgetattr(self.slave)
        self.transcript, self.stdout = bytearray(), bytearray()
        self.closed = False
        self.decoder.request(op='resize', rows=28, cols=100)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', 28, 100, 0, 0))
        os.set_blocking(self.master, False)
        env = harness.env('A')
        env.update(SSMCP_UI_CONFIG=str(config), TERM='xterm-256color', COLUMNS='100', LINES='28')
        env.pop('NO_COLOR', None)

        def own_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        self.process = subprocess.Popen(
            [harness.binary, '--server', harness.state.origin, '--name', 'Logout fixture',
             '--no-setup', '--shell', '/bin/sh', '--reconnect-delay', '1'],
            stdin=self.slave, stdout=subprocess.PIPE, stderr=self.slave,
            cwd=harness.root / 'A', env=env, preexec_fn=own_terminal)
        harness.processes.append(self.process)
        os.set_blocking(self.process.stdout.fileno(), False)

    def wait_exit(self):
        deadline = time.monotonic() + 8
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump(0.05)
        assert self.process.poll() is not None, self.failure_detail()
        self.pump(0.1)


def login_count(state):
    return state.requests.count(('POST', '/api/client/login/start'))


args.output.mkdir(parents=True, exist_ok=True)
relay.Handler = LogoutHandler
results = []


def write_report(passed):
    with args.binary.open('rb') as stream:
        binary_sha = hashlib.file_digest(stream, 'sha256').hexdigest()
    report = {
        'passed': passed,
        'mode': 'real_cli_and_pty_with_synthetic_loopback_logout_gateway',
        'source': args.source,
        'binary_sha256': binary_sha,
        'scenarios': results,
    }
    (args.output / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return report


for scenario in ['remote_success', 'remote_unconfirmed', 'newer_saved_login']:
    with tempfile.TemporaryDirectory(prefix='ssmcp-logout-pty-') as tmp:
        harness = relay.Harness(Path(tmp), args.binary)
        harness.secret, harness.sensitive = '', [harness.state.account_token, harness.state.device_code]
        state = harness.state
        state.logout_calls = 0
        state.logout_seen, state.logout_release = threading.Event(), threading.Event()
        state.logout_status = 200 if scenario == 'remote_success' else 503
        front = None
        try:
            harness.cli('A', 'login', '--server', state.origin, '--no-browser', '--json')
            config = Path(tmp) / 'ui.toml'
            config.write_text('[ui]\ndashboard=true\nsidebar=false\n')
            front = Frontend(harness, config)
            front.until(lambda s: 'conversation' in text(s).lower(), 'conversation screen')
            relay.wait_for(lambda: state.sessions, 'authenticated account WebSocket')
            credential = harness.root / 'A/auth' / (hashlib.sha256(state.origin.encode()).hexdigest() + '.json')
            harness.check(json.loads(credential.read_text()).get('username') is None,
                          'the authenticated account has no server username')
            original_login_count = login_count(state)
            if scenario == 'newer_saved_login':
                data = json.loads(credential.read_text())
                replacement = 'cl_' + secrets.token_hex(32)
                harness.sensitive.append(replacement)
                data['token'] = replacement
                credential.write_text(json.dumps(data))
                credential.chmod(0o600)
                front.send('/logout\r')
                front.until(lambda s: 'Logout failed' in text(s), 'connection snapshot mismatch')
                harness.check(front.process.poll() is None, 'snapshot mismatch keeps the current UI open')
                harness.check(json.loads(credential.read_text())['token'] == replacement,
                              'UI logout preserves a newer saved login')
                harness.check(state.logout_calls == 0, 'snapshot mismatch sends no logout HTTP request')
                front.disconnect()
            else:
                front.send('/logout\r')
                relay.wait_for(state.logout_seen.is_set, 'logout closes WebSocket before its response')
                deadline = time.monotonic() + 1.3
                while time.monotonic() < deadline:
                    front.pump(0.05)
                harness.check(front.process.poll() is None, 'teardown waits for the outstanding logout HTTP result')
                harness.check(not credential.exists(), 'selected local credential is cleared before waiting for HTTP')
                harness.check(login_count(state) == original_login_count,
                              'early WebSocket revocation does not start account reauthorization')
                harness.check(state.connect_attempts[state.account_token] == 1,
                              'early WebSocket revocation does not reconnect')
                state.logout_release.set()
                front.wait_exit()
                expected = 'were revoked' if scenario == 'remote_success' else 'could not be confirmed'
                marker = bytes(front.transcript).rfind(b'\x1b[?1049l')
                assert marker >= 0, 'the UI did not leave the alternate screen'
                final = bytes(front.transcript)[marker:].decode(errors='replace')
                harness.check(expected in final, 'the real logout outcome remains visible after terminal restoration')
                harness.check(termios.tcgetattr(front.slave) == front.original_termios,
                              'logout restores the owned terminal input mode')
                harness.check(state.logout_calls == 1, 'the UI sends exactly one logout request')
            harness.check(front.process.returncode == 0, 'owned client stops cleanly')
            harness.check(login_count(state) == original_login_count, 'no extra login starts after completion')
            harness.check(state.connect_attempts[state.account_token] == 1, 'watchdog never opens a replacement session')
            harness.check(not state.errors, 'fixture protocol observed no unexpected frames')
            results.append({'scenario': scenario, 'passed': True, 'checks': harness.checks})
        except Exception as error:
            detail = f'{type(error).__name__}: {error}'
            if front is not None:
                detail += '\n' + front.failure_detail()
            for secret in harness.sensitive:
                if secret:
                    detail = detail.replace(secret, '[redacted]')
            (args.output / f'{scenario}-failure.txt').write_text(detail)
            results.append({'scenario': scenario, 'passed': False, 'checks': harness.checks,
                            'error': detail})
            write_report(False)
            raise
        finally:
            state.logout_release.set()
            try:
                harness.cleanup()
            finally:
                if front is not None:
                    front.decoder.close()
                    os.close(front.master)
                    os.close(front.slave)

report = write_report(True)
print(json.dumps(report, ensure_ascii=False))
