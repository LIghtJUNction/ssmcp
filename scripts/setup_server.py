#!/usr/bin/env python3
"""Configure one account/SSH/MCP gateway; check by default, --apply to migrate.

Requires an installed ssmcpd, systemd, Nginx, Python 3.11+, a private TOML
configuration with GitHub OAuth, and existing TLS certificates. No secrets
are accepted as command-line arguments or printed.
"""
from __future__ import annotations
import argparse
import http.client
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import stat
import subprocess
import tempfile
import time
import tomllib
from urllib.parse import urlsplit


def run(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError('command failed: ' + ' '.join(argv[:2]))
    return result.stdout.strip()


def service_state(name, runner=run):
    try:
        return runner(['systemctl', 'show', name, '-p', 'ActiveState', '--value']) == 'active'
    except RuntimeError:
        return False


def configure_text(text, state_dir):
    """Preserve secrets, comments, and unrelated TOML settings verbatim."""
    data = tomllib.loads(text)
    current = data.get('portal', {}).get('state_dir')
    if current and current != str(state_dir):
        raise ValueError('portal.state_dir differs; select the existing account directory')
    if current:
        return text
    header = re.search(r'(?m)^\s*\[portal\]\s*(?:#.*)?$', text)
    setting = 'state_dir = ' + json.dumps(str(state_dir)) + '\n'
    if header:
        return text[:header.end()] + '\n' + setting + text[header.end():]
    return text.rstrip() + '\n\n[portal]\n' + setting


def configuration(config, state_dir, nginx_site, template):
    original = config.read_text()
    data = tomllib.loads(original)
    if not data.get('auth', {}).get('github'):
        raise ValueError('configure auth.github in the private TOML before setup')
    origin = urlsplit(data['server']['public_url'])
    if (origin.scheme != 'https' or not origin.hostname or origin.username
            or origin.password or origin.path not in ('', '/') or origin.query or origin.fragment
            or not re.fullmatch(r'[A-Za-z0-9.-]+', origin.hostname)):
        raise ValueError('server.public_url must be an HTTPS DNS origin')
    listen = data['server'].get('listen', '127.0.0.1:8080')
    address = urlsplit('http://' + listen)
    if not address.hostname or not ipaddress.ip_address(address.hostname).is_loopback or not address.port:
        raise ValueError('server.listen must be a literal loopback address and port')
    state_dir = state_dir or Path(data.get('portal', {}).get('state_dir', '/var/lib/ssmcpd/portal'))
    if not state_dir.is_absolute() or '..' in state_dir.parts:
        raise ValueError('account directory must be absolute without parent traversal')
    for path in (config, nginx_site, state_dir):
        if any(parent.is_symlink() for parent in (path, *path.parents)):
            raise ValueError('configuration and account paths must not contain symlinks')
    candidate = configure_text(original, state_dir)
    if nginx_site.exists():
        proxy = nginx_site.read_text()
        # Migrate the packaged split-portal template without replacing TLS or logging.
        proxy = proxy.replace('127.0.0.1:8086', listen)
        backends = re.findall(r'proxy_pass\s+http://([^;]+);', proxy)
        if not backends or any(backend not in (listen, '$ssmcp_github_backend') for backend in backends):
            raise ValueError('custom upstreams require an explicit single-gateway Nginx site')
        if '$ssmcp_github_backend' in backends:
            mapping = re.search(r'map\s+\$arg_state\s+\$ssmcp_github_backend\s*\{([^}]+)\}', proxy)
            if not mapping or any(value != listen for value in re.findall(r'\s([^\s;]+);', mapping[1])):
                raise ValueError('GitHub callback map must use the same gateway')
    else:
        if origin.port not in (None, 443):
            raise ValueError('custom HTTPS ports require an existing Nginx site')
        proxy = template.read_text().replace('ssmcp.example.com', origin.hostname)
        proxy = proxy.replace('127.0.0.1:8080', listen)
    return {config: candidate, nginx_site: proxy}, state_dir, 'http://' + listen


def atomic_write(path, content, metadata=None):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.ssmcp-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, stat.S_IMODE(metadata.st_mode) if metadata else 0o644)
        if metadata:
            os.chown(temporary, metadata.st_uid, metadata.st_gid)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def probe(base):
    address = urlsplit(base)
    def request(route, body=None):
        transport = http.client.HTTPSConnection if address.scheme == 'https' else http.client.HTTPConnection
        connection = transport(address.hostname, address.port, timeout=5)
        try:
            connection.request('GET' if body is None else 'POST', route,
                               body=None if body is None else json.dumps(body),
                               headers={'Content-Type': 'application/json'})
            reply = connection.getresponse()
            return reply.status, reply.read(65537)
        finally:
            connection.close()
    deadline = time.monotonic() + 20
    while True:
        try:
            status, body = request('/healthz')
            if status == 200 and body.strip() == b'ok':
                break
        except OSError:
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError('gateway health check failed')
        time.sleep(0.25)
    status, body = request('/api/client/login/start', {'name': 'deployment health check'})
    if status != 200:
        raise RuntimeError('client account login probe failed; account storage must share the gateway')
    grant = json.loads(body)
    try:
        if request('/api/client/whoami')[0] != 401 or request('/api/client/sessions')[0] != 401:
            raise RuntimeError('unauthenticated account endpoints did not reject access')
    finally:
        request('/api/client/login/cancel', {'device_code': grant['device_code']})


def disable_legacy(runner):
    try:
        loaded = runner(['systemctl', 'show', 'ssmcp-portal.service', '-p', 'LoadState', '--value'])
    except RuntimeError:
        return
    if loaded not in ('not-found', ''):
        runner(['systemctl', 'disable', 'ssmcp-portal.service'])


def apply(changes, base, checkpoint_root, runner=run, health=probe, public_health=None):
    portal_active = service_state('ssmcp-portal.service', runner)
    gateway_active = service_state('ssmcpd.service', runner)
    nginx_active = service_state('nginx.service', runner)
    changed = {path: text for path, text in changes.items()
               if not path.exists() or path.read_text() != text}
    if not changed and not portal_active and gateway_active:
        health(base)
        if public_health:
            public_health()
        runner(['systemctl', 'enable', '--now', 'ssmcp-portal-ssh.timer'])
        runner(['systemctl', 'start', 'ssmcp-portal-ssh.service'])
        disable_legacy(runner)
        return 'unchanged'
    checkpoint_root.mkdir(parents=True, mode=0o700, exist_ok=True)
    checkpoint = Path(tempfile.mkdtemp(prefix='setup-', dir=checkpoint_root))
    os.chmod(checkpoint, 0o700)
    originals = {}
    for index, path in enumerate(changed):
        originals[path] = (path.read_text(), path.stat()) if path.exists() else None
        if path.exists():
            saved = checkpoint / str(index)
            saved.write_bytes(path.read_bytes())
            saved.chmod(0o600)
    try:
        for path, text in changed.items():
            previous = originals[path]
            atomic_write(path, text, previous[1] if previous else None)
        runner(['nginx', '-t'])
        if portal_active:
            runner(['systemctl', 'stop', 'ssmcp-portal.service'])
        runner(['systemctl', 'restart', 'ssmcpd.service'])
        health(base)
        runner(['systemctl', 'enable', 'ssmcpd.service'])
        if nginx_active:
            runner(['systemctl', 'reload', 'nginx.service'])
        else:
            runner(['systemctl', 'enable', '--now', 'nginx.service'])
        if public_health:
            public_health()
        runner(['systemctl', 'enable', '--now', 'ssmcp-portal-ssh.timer'])
        runner(['systemctl', 'start', 'ssmcp-portal-ssh.service'])
        disable_legacy(runner)
        return 'configured'
    except Exception:
        for path, previous in originals.items():
            if previous is None:
                path.unlink(missing_ok=True)
            else:
                atomic_write(path, previous[0], previous[1])
        runner(['systemctl', 'restart' if gateway_active else 'stop', 'ssmcpd.service'])
        if portal_active:
            runner(['systemctl', 'start', 'ssmcp-portal.service'])
        runner(['nginx', '-t'])
        runner(['systemctl', 'reload' if nginx_active else 'stop', 'nginx.service'])
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path('/etc/ssmcpd/ssmcpd.toml'))
    parser.add_argument('--nginx-site', type=Path, required=True)
    parser.add_argument('--state-dir', type=Path)
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error('run as root to inspect private configuration')
    here = Path(__file__).resolve()
    templates = [here.parent/'packaging/nginx-ssmcp.conf.example',
                 here.parents[1]/'packaging/nginx-ssmcp.conf.example',
                 here.parents[2]/'share/doc/ssmcp/examples/nginx-ssmcp.conf.example']
    template = next((path for path in templates if path.is_file()), templates[-1])
    try:
        changes, state, base = configuration(args.config, args.state_dir, args.nginx_site, template)
        if not args.apply:
            print('Configuration valid; run the same command with --apply to configure the unified gateway.')
            return 0
        account = pwd.getpwnam('ssmcpd')
        if not state.parent.exists():
            state.parent.mkdir(mode=0o700)
            os.chown(state.parent, account.pw_uid, account.pw_gid)
        if not state.exists():
            state.mkdir(mode=0o700)
            os.chown(state, account.pw_uid, account.pw_gid)
        metadata = state.stat()
        if metadata.st_uid != account.pw_uid or stat.S_IMODE(metadata.st_mode) != 0o700:
            raise ValueError('existing account directory must be service-owned with mode 0700')
        public = tomllib.loads(changes[args.config])['server']['public_url'].rstrip('/')
        print(apply(changes, base, Path('/var/lib/ssmcp-rollouts'), public_health=lambda: probe(public)))
        return 0
    except Exception as error:
        print('Setup failed (' + type(error).__name__ + '): ' + str(error))
        return 1

if __name__ == '__main__':
    raise SystemExit(main())
