#!/usr/bin/python3
"""Provision only portal-owned accounts; never edit existing users or their key files."""
import base64
import binascii
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import stat
import subprocess
import tempfile

STATE = Path('/var/lib/ssmcpd/portal/accounts.json')
OUTPUT = Path('/var/lib/ssmcp-ssh')


def validate(database):
    if set(database) != {'accounts'} or not isinstance(database['accounts'], dict) or len(database['accounts']) > 4096:
        raise ValueError('invalid account database')
    result = {}
    for subject, account in database['accounts'].items():
        if not re.fullmatch(r'[1-9][0-9]{0,19}', subject) or int(subject) > 2**64 - 1:
            raise ValueError('invalid numeric subject')
        username = 'gh-' + subject
        if account['subject'] != subject or account['username'] != username or len(account['keys']) > 20:
            raise ValueError('invalid account binding')
        keys = []
        for key in account['keys']:
            text = key['public_key']
            if not isinstance(text, str) or len(text) > 100 or not text.startswith('ssh-ed25519 '):
                raise ValueError('invalid public key')
            encoded = text[12:]
            blob = base64.b64decode(encoded, validate=True)
            fingerprint = 'SHA256:' + base64.b64encode(hashlib.sha256(blob).digest()).decode().rstrip('=')
            if (len(blob) != 51 or blob[:19] != b'\0\0\0\x0bssh-ed25519\0\0\0\x20'
                    or blob[19:] == bytes(32) or base64.b64encode(blob).decode() != encoded
                    or key['fingerprint'] != fingerprint or text in keys):
                raise ValueError('invalid public key blob or fingerprint')
            keys.append(text)
        result[username] = keys
    return result


def private_directory(path, uid, mode):
    for item in (path, *path.parents):
        m = item.lstat()
        if not stat.S_ISDIR(m.st_mode) or m.st_uid not in (0, uid) or m.st_mode & 0o022:
            raise ValueError('unsafe directory ownership, permissions or symlink')
    m = path.stat()
    if m.st_uid != uid or stat.S_IMODE(m.st_mode) != mode:
        raise ValueError('incorrect private directory owner or mode')


def read_json(path, uid, mode):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        m = os.fstat(stream.fileno())
        if not stat.S_ISREG(m.st_mode) or m.st_uid != uid or stat.S_IMODE(m.st_mode) != mode or m.st_nlink != 1 or m.st_size > 8 * 1024 * 1024:
            raise ValueError('unsafe state file')
        data = stream.read(8 * 1024 * 1024 + 1)
        if len(data) > 8 * 1024 * 1024:
            raise ValueError('state too large')
        return json.loads(data)


def atomic(path, data, mode):
    fd, temporary = tempfile.mkstemp(prefix='.sync-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def verify_sshd(name):
    effective = subprocess.check_output([
        '/usr/sbin/sshd', '-T', '-C',
        f'user={name},host=localhost,addr=127.0.0.1'], text=True)
    settings = dict(line.split(' ', 1) for line in effective.lower().splitlines() if ' ' in line)
    required = {
        'forcecommand': '/usr/local/bin/ssmcpd attach',
        'authorizedkeysfile': '/var/lib/ssmcp-ssh/%u',
        'authenticationmethods': 'publickey', 'passwordauthentication': 'no',
        'kbdinteractiveauthentication': 'no', 'disableforwarding': 'yes',
        'permittty': 'no', 'permituserrc': 'no',
    }
    if any(settings.get(key) != value for key, value in required.items()):
        raise ValueError('portal SSH restrictions must be active before publishing keys')


def sync(accounts, directory, lookup=pwd.getpwnam, run=subprocess.run, verify=verify_sshd):
    registry_path = directory / 'owners.json'
    registry = read_json(registry_path, 0, 0o600) if registry_path.exists() else {}
    # Never use unvalidated registry values as paths or commands.
    if not isinstance(registry, dict) or any(not re.fullmatch(r'gh-[1-9][0-9]{0,19}', name) or not isinstance(uid, int) or uid <= 0 for name, uid in registry.items()):
        raise ValueError('invalid SSH ownership registry')
    for name, keys in accounts.items():
        try:
            user = lookup(name)
        except KeyError:
            if name in registry:
                raise ValueError('managed account disappeared; refusing to recreate')
            # useradd refuses existing names and never invokes a shell with user input.
            run(['/usr/sbin/useradd', '--no-create-home', '--home-dir', '/nonexistent',
                 '--shell', '/bin/sh', '--password', '*', '--groups', 'ssmcp,ssmcp-portal',
                 '--comment', 'ssmcp portal account', '--', name], check=True)
            user = lookup(name)
            registry[name] = user.pw_uid
            atomic(registry_path, json.dumps(registry), 0o600)
        if registry.get(name) != user.pw_uid or user.pw_uid == 0 or user.pw_dir != '/nonexistent' or user.pw_shell != '/bin/sh':
            raise ValueError('existing account collision or changed managed account')
        verify(name)
        atomic(directory / name, ''.join('restrict ' + key + '\n' for key in keys), 0o644)
    # Account removal from a restored snapshot revokes its managed key file, not the OS user.
    for name in registry.keys() - accounts.keys():
        atomic(directory / name, '', 0o644)


def main():
    if os.geteuid() != 0:
        raise ValueError('synchronizer must run as root')
    os.umask(0o077)
    uid = pwd.getpwnam('ssmcpd').pw_uid
    private_directory(STATE.parent, uid, 0o700)
    try:
        OUTPUT.mkdir(mode=0o755)
        OUTPUT.chmod(0o755)  # OpenSSH must traverse it despite the private process umask.
    except FileExistsError:
        pass
    private_directory(OUTPUT, 0, 0o755)
    fd = os.open(OUTPUT / 'sync.lock', os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        accounts = validate(read_json(STATE, uid, 0o600))
        sync(accounts, OUTPUT)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, binascii.Error, subprocess.SubprocessError):
        # No raw input, account contents or credentials in logs.
        raise SystemExit('portal SSH sync failed; check private state, account collisions and permissions')
