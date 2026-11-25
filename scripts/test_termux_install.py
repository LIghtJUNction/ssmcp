"""Offline dispatch tests. Set SSMCP_UPSTREAM_INSTALLER to exercise a real release install.sh."""
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("install-ssmcp-termux.sh")
DISPATCH = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
p = pathlib.Path
state = p(os.environ['FIXTURE'])
args = sys.argv[1:]
tool = p(sys.argv[0]).name
with (state/'calls').open('a') as log:
    log.write(json.dumps([tool, *args])+'\n')
if tool == 'proot-distro':
    runtime = p(os.environ.get('TERMUX__PREFIX') or os.environ['PREFIX']) / 'var/lib/proot-distro'
    rootfs = runtime / 'containers/ssmcp-debian/rootfs'
    legacy = runtime / 'installed-rootfs/ssmcp-debian'
    if args == ['install','debian:bookworm','--name','ssmcp-debian']:
        rootfs.mkdir(parents=True)
    elif args[:5] == ['login','ssmcp-debian','--','/bin/sh','-s']:
        if legacy.is_dir() and not rootfs.exists():
            rootfs.parent.mkdir(parents=True, exist_ok=True)
            legacy.rename(rootfs)
        if not rootfs.is_dir(): sys.exit(1)
        os.execv('/bin/sh', ['/bin/sh', *args[4:]])
    else: sys.exit(2)
elif tool == 'curl':
    url = next(a for a in args if a.startswith('https://'))
    if url.endswith('/releases/latest'):
        print('https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.39',end='')
    else:
        flag = '-o' if '-o' in args else '--output'
        shutil.copyfile(state/'release'/url.split('/')[-1], args[args.index(flag)+1])
elif tool == 'git':
    if args==['init','-q','source']:
        p('source').mkdir()
        (p('source')/'rust-toolchain.toml').write_text('[toolchain]\nchannel = "1.100.0"\n')
    elif args==['rev-parse','HEAD']: print('1234abcd')
    elif args not in (['remote','add','origin','https://github.com/LIghtJUNction/ssmcp.git'],
                       ['fetch','--depth','1','origin','refs/tags/v0.10.39'],
                       ['checkout','-q','--detach','FETCH_HEAD'],['submodule','update','--init','--recursive','--depth','1']): sys.exit(2)
elif tool == 'rustup':
    assert args==['run','--install','1.100.0','cargo','build','--locked','--release','--bin','ssmcp']
    if os.getenv('BUILD_FAIL'): sys.exit(44)
    dest=p('target/release/ssmcp')
    dest.parent.mkdir(parents=True)
    dest.write_bytes((state/'client').read_bytes())
    dest.chmod(0o755)
elif tool not in ('apt-get','pkg'): sys.exit(2)
'''
CLIENT = b'#!/bin/sh\nprintf "ssmcp 0.10.39\\n"\n'
STUB = b'''set -eu
[ "$1 $2 $3" = "--version v0.10.39 --dir" ]
cp "$FIXTURE/client" "$4/ssmcp"
chmod 755 "$4/ssmcp"
'''


@unittest.skipUnless(platform.system() == "Linux" and platform.libc_ver()[0] == "glibc",
                     "the guest control-flow fixture uses GNU/Linux command-line tools")
class TermuxInstall(unittest.TestCase):
    def setUp(self):
        cleanup = tempfile.TemporaryDirectory()
        self.addCleanup(cleanup.cleanup)
        self.base = Path(cleanup.name)
        self.release = self.base / 'release'
        self.release.mkdir()
        tools = self.base / 'bin'
        tools.mkdir()
        dispatcher = tools / 'dispatch'
        dispatcher.write_text(DISPATCH)
        dispatcher.chmod(0o755)
        for name in ['pkg', 'apt-get', 'rustup', 'git', 'curl', 'proot-distro']:
            (tools / name).symlink_to(dispatcher.name)
        (self.base / 'client').write_bytes(CLIENT)
        upstream = os.getenv('SSMCP_UPSTREAM_INSTALLER')
        (self.release / 'install.sh').write_bytes(Path(upstream).read_bytes() if upstream else STUB)
        arch = {'arm64':'aarch64','amd64':'x86_64'}.get(platform.machine(),platform.machine())
        archive = self.release / f'ssmcp-{arch}-unknown-linux-gnu.tar.gz'
        with tarfile.open(archive, 'w:gz') as tf:
            entry = tarfile.TarInfo('ssmcp')
            entry.mode, entry.size = 0o755, len(CLIENT)
            tf.addfile(entry, io.BytesIO(CLIENT))
        (self.release / 'SHA256SUMS').write_text(''.join(
            f'{hashlib.sha256(f.read_bytes()).hexdigest()}  {f.name}\n'
            for f in [self.release/'install.sh', archive]))
        home = self.base / 'home'
        home.mkdir()
        self.dest = home / '.local/bin/ssmcp'
        self.env = dict(os.environ, HOME=str(home), PREFIX=str(self.base / 'prefix'),
                        FIXTURE=str(self.base), PATH=f'{tools}:/usr/bin:/bin')
        self.env.pop('TERMUX__PREFIX', None)
        self.calls = self.base / 'calls'

    def install(self, *args, success=True):
        run = subprocess.run(['/bin/sh', str(SCRIPT), *args], env=self.env,
                             text=True, capture_output=True, timeout=10)
        self.assertEqual(run.returncode == 0, success, run.stdout + run.stderr)

    def test_latest_binary_and_reuse(self):
        self.install()
        self.install('bin','v0.10.39')
        self.assertEqual(self.dest.read_bytes(),CLIENT)
        cmds = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(sum(c[:2]==['proot-distro','install'] for c in cmds),1)
        self.assertFalse(any(c[0] in ('git','rustup') for c in cmds))

    def test_legacy_guest_is_migrated_by_login_without_reinstalling(self):
        prefix = self.base / 'custom-termux-prefix'
        self.env['TERMUX__PREFIX'] = str(prefix)
        runtime = prefix / 'var/lib/proot-distro'
        legacy = runtime / 'installed-rootfs/ssmcp-debian'
        legacy.mkdir(parents=True)
        (legacy / 'existing-data').write_text('keep')
        self.install('bin','v0.10.39')
        self.install('bin','v0.10.39')
        self.assertEqual((runtime / 'containers/ssmcp-debian/rootfs/existing-data').read_text(), 'keep')
        self.assertFalse(any(json.loads(line)[:2] == ['proot-distro','install']
                             for line in self.calls.read_text().splitlines()))

    def test_checksum_failure_preserves_client(self):
        self.dest.parent.mkdir(parents=True)
        self.dest.write_text('keep')
        (self.release/'install.sh').write_text('exit 0')
        self.install('bin','v0.10.39',success=False)
        self.assertEqual(self.dest.read_text(),'keep')

    def test_locked_source_and_failure_preserves_client(self):
        self.install('source','v0.10.39')
        self.assertEqual(self.dest.read_bytes(),CLIENT)
        self.dest.write_text('previous')
        self.env['BUILD_FAIL']='1'
        self.install('source','v0.10.39',success=False)
        self.assertEqual(self.dest.read_text(),'previous')

    def test_invalid_version_is_rejected_before_environment_changes(self):
        self.install('bin','v1.2.3; echo oops',success=False)
        self.install('bin','v1.2.3\nextra',success=False)
        self.assertFalse(self.calls.exists())


if __name__ == '__main__':
    unittest.main()
