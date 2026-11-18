#!/usr/bin/env python3
"""Stage and deploy an Arch ssmcp package with a private rollback checkpoint.

Run as root on the server. Never prints provider secrets or OAuth credentials.
Staging does not restart production. Apply requires the matching stage receipt
for the expected release version, commit and package digest.
"""
from __future__ import annotations
import argparse, base64, fcntl, hashlib, http.client, json, os, pwd, re, shutil
import subprocess, tempfile, time, tomllib
from pathlib import Path
from urllib.parse import urlencode, urlsplit


def run(argv, timeout=30):
    result=subprocess.run(argv,stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=timeout)
    if result.returncode:
        raise RuntimeError('command failed: '+Path(argv[0]).name+' '+(argv[1] if len(argv)>1 else ''))
    return result.stdout.strip()


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream,'sha256').hexdigest()


def save(path, value):
    data=(json.dumps(value,indent=2,sort_keys=True)+'\n').encode()
    fd,temp=tempfile.mkstemp(prefix='.report-',dir=path.parent)
    try:
        with os.fdopen(fd,'wb') as stream:
            stream.write(data);stream.flush();os.fsync(stream.fileno())
        os.replace(temp,path)
    finally:
        if os.path.exists(temp):os.unlink(temp)


def request(base, path, payload=None):
    parsed=urlsplit(base)
    cls=http.client.HTTPSConnection if parsed.scheme=='https' else http.client.HTTPConnection
    conn=cls(parsed.hostname,parsed.port,timeout=10)
    try:
        conn.request('GET' if payload is None else 'POST',path,
                     body=None if payload is None else json.dumps(payload),
                     headers={'Content-Type':'application/json','Accept':'application/json,text/html'})
        response=conn.getresponse()
        return response.status,response.read(131073).decode(errors='replace')
    finally:conn.close()


def wait_health(base):
    for _ in range(80):
        try:
            status,body=request(base,'/healthz')
            if status==200 and body.strip()=='ok':return
        except (OSError,http.client.HTTPException):pass
        time.sleep(0.25)
    raise RuntimeError('health check failed')


def stage_service_command(stage_unit, state_name, binary, config):
    state = Path('/var/lib') / state_name
    return ['systemd-run', '--quiet', '--collect', '--unit='+stage_unit,
        '--uid=ssmcpd', '--gid=ssmcp',
        '--property=StateDirectory='+state_name, '--property=StateDirectoryMode=0700',
        '--property=RuntimeDirectory='+state_name, '--property=RuntimeDirectoryMode=0750',
        '--property=NoNewPrivileges=yes', '--property=ProtectSystem=strict',
        '--property=ProtectHome=yes', '--property=PrivateTmp=yes',
        '--property=PrivateDevices=yes', '--property=ProtectKernelTunables=yes',
        '--property=ProtectControlGroups=yes', '--property=ReadWritePaths=/proc/sys/user',
        '--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK',
        '--property=UMask=0077', '--property=TimeoutStopSec=15',
        '--property=ExecStartPre='+str(binary)+' check-runtime',
        '--setenv=PATH=/usr/bin:/bin', str(binary), 'serve',
        '--config', str(config), '--listen', '127.0.0.1:18087',
        '--socket', '/run/'+state_name+'/control.sock',
        '--client-registry', str(state/'oauth-clients.toml')]


def registration(base,issuer):
    status,body=request(base,'/oauth/register',{'client_name':'ssmcp deployment restart check',
        'redirect_uris':['https://client.example/ssmcp-deploy-callback'],
        'token_endpoint_auth_method':'none'})
    if status!=201:raise RuntimeError('DCR registration failed')
    client=json.loads(body)['client_id']
    query=urlencode({'client_id':client,'redirect_uri':'https://client.example/ssmcp-deploy-callback',
        'response_type':'code','resource':issuer+'/mcp','scope':'mcp','code_challenge_method':'S256',
        'code_challenge':base64.urlsafe_b64encode(hashlib.sha256(b'x'*43).digest()).decode().rstrip('=')})
    return '/oauth/authorize?'+query


def metadata(base,issuer):
    status,text=request(base,'/.well-known/oauth-authorization-server')
    data=json.loads(text)
    if status!=200 or data.get('issuer')!=issuer or 'refresh_token' not in data.get('grant_types_supported',[]):
        raise RuntimeError('OAuth metadata mismatch')
    status,_=request(base,'/mcp')
    if status!=401:raise RuntimeError('unauthenticated MCP did not reject request')


def package_field(info, name):
    """A package identity field must occur exactly once in .PKGINFO."""
    values=[line.split(' = ',1)[1] for line in info.splitlines()
            if line.startswith(name+' = ')]
    return values[0] if len(values)==1 else None


def package_version_matches(value, expected):
    """Accept the release or this repository's git package, including pkgrel."""
    return isinstance(value,str) and re.fullmatch(
        re.escape(expected)+r'(?:\.r[0-9]+\.g[0-9a-f]+)?-[0-9]+(?:\.[0-9]+)*',value) is not None


def isolated_configuration(text, state, runtime):
    config = tomllib.loads(text)
    for field, destination in [('state_dir', state/'portal'),
                               ('audit_dir', state/'audit'),
                               ('gateway_socket', runtime/'management.sock')]:
        if config.get('portal', {}).get(field):
            text, count = re.subn(r'(?m)^(\s*'+field+r'\s*=\s*).+$',
                lambda match: match[1]+json.dumps(str(destination)), text)
            if count != 1:
                raise ValueError('cannot isolate staging '+field)
    return text


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--phase',choices=['stage','apply'],required=True)
    parser.add_argument('--package',type=Path,required=True)
    parser.add_argument('--sha256',required=True)
    parser.add_argument('--rollback-package',type=Path,required=True)
    parser.add_argument('--commit',required=True)
    parser.add_argument('--expected-version',required=True,help='Exact stable release version, for example 0.6.0')
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--config',type=Path,default=Path('/etc/ssmcpd/ssmcpd.toml'))
    parser.add_argument('--proxy-config',type=Path,help='Optional replacement for the existing Nginx site')
    parser.add_argument('--proxy-target',type=Path,default=Path('/etc/nginx/conf.d/ssmcp.conf'))
    args=parser.parse_args()
    if not re.fullmatch(r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)',args.expected_version):
        parser.error('expected-version must be a stable major.minor.patch version without a v prefix')
    expected_binary='ssmcpd '+args.expected_version
    if not re.fullmatch(r'[0-9a-f]{40}',args.commit) or not re.fullmatch(r'[0-9a-f]{64}',args.sha256):
        parser.error('commit and sha256 must be full lowercase hexadecimal values')
    if args.proxy_config and (not args.proxy_config.is_absolute() or args.proxy_target.parent!=Path('/etc/nginx/conf.d') or args.proxy_target.suffix!='.conf'):
        parser.error('proxy paths must identify an absolute candidate and one existing conf.d site')
    if os.geteuid()!=0:raise SystemExit('root required')
    os.umask(0o077)
    checkpoint=Path('/var/lib/ssmcp-rollouts')/args.commit
    checkpoint.mkdir(parents=True,exist_ok=True,mode=0o700)
    lock=open('/run/lock/ssmcp-deployment.lock','a')
    fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    report={'phase':args.phase,'state':'running','commit':args.commit,'expected_version':args.expected_version,'checks':{},'started_unix':int(time.time())}
    args.report=args.report.resolve()
    def check(name,value=True):
        report['checks'][name]=bool(value)
        save(args.report,report)
        if not value:raise RuntimeError(name)
    try:
        check('package_sha256',digest(args.package)==args.sha256)
        info=run(['bsdtar','-xOf',str(args.package),'.PKGINFO'])
        check('package_name',package_field(info,'pkgname')=='ssmcp-git')
        check('package_version',package_version_matches(package_field(info,'pkgver'),args.expected_version))
        proxy_digest=digest(args.proxy_config) if args.proxy_config else None
        if args.proxy_config:
            check('proxy_target_exists',args.proxy_target.is_file())
            with tempfile.TemporaryDirectory(prefix='ssmcp-nginx-') as directory:
                candidate=Path(directory)/'nginx.conf'
                candidate.write_text('error_log stderr; events {} http { include '+str(args.proxy_config)+'; }\n')
                run(['nginx','-t','-c',str(candidate)])
            check('proxy_candidate_syntax')
        config=tomllib.loads(args.config.read_text())
        issuer=config['server']['public_url'].rstrip('/')
        check('issuer_https',urlsplit(issuer).scheme=='https')
        check('github_configured',bool(config.get('auth',{}).get('github')))
        stage_root=Path('/opt/ssmcp/releases')/args.commit
        stage_unit='ssmcp-stage-'+args.commit[:8]
        state_name='ssmcp-stage-'+args.commit[:8]
        state=Path('/var/lib')/state_name
        receipt=checkpoint/'prepared.json'
        stage_base='http://127.0.0.1:18087'
        if args.phase=='stage':
            before={'binary':digest('/usr/bin/ssmcpd'),'config':digest(args.config),
                    'package':run(['pacman','-Q','ssmcp-git'])}
            if args.proxy_config:before['proxy']=digest(args.proxy_target)
            stage_root.mkdir(parents=True,exist_ok=True,mode=0o755)
            os.chmod('/opt/ssmcp',0o755);os.chmod('/opt/ssmcp/releases',0o755);stage_root.chmod(0o755)
            run(['bsdtar','-xpf',str(args.package),'-C',str(stage_root),'usr/bin/ssmcp','usr/bin/ssmcpd','usr/libexec/ssmcp/code-mode.js'])
            for directory in ['usr','usr/bin','usr/libexec','usr/libexec/ssmcp']:
                (stage_root/directory).chmod(0o755)
            check('staged_binary_version',run([str(stage_root/'usr/bin/ssmcpd'),'--version'])==expected_binary)
            command=stage_service_command(stage_unit,state_name,stage_root/'usr/bin/ssmcpd',args.config)
            # A staged portal uses a fresh store, never the production account data.
            if config.get('portal',{}).get('state_dir'):
                command += ['--portal-state-dir',str(state/'portal')]
            if config.get('portal', {}).get('gateway_socket') or config.get('portal', {}).get('audit_dir'):
                stage_config = stage_root/'stage-config.toml'
                stage_config.write_text(isolated_configuration(args.config.read_text(), state, Path('/run')/state_name))
                account = pwd.getpwnam('ssmcpd')
                os.chown(stage_config, account.pw_uid, account.pw_gid)
                stage_config.chmod(0o600)
                command[command.index('--config')+1] = str(stage_config)
            owned=False
            try:
                check('stage_unit_unused',run(['systemctl','show',stage_unit,'-p','LoadState','--value'])=='not-found')
                run(command);owned=True;wait_health(stage_base)
                check('staged_runtime_preflight')
                metadata(stage_base,issuer);check('staged_health_and_discovery')
                url=registration(stage_base,issuer)
                check('staged_authorization_before_restart',request(stage_base,url)[0]==200)
                run(['systemctl','restart',stage_unit],timeout=45);wait_health(stage_base)
                check('staged_DCR_survives_restart',request(stage_base,url)[0]==200)
                check('staged_private_registry',state.joinpath('oauth-clients.toml').stat().st_mode&0o777==0o600)
            finally:
                if owned:run(['systemctl','stop',stage_unit],timeout=45)
            check('production_not_changed',before['binary']==digest('/usr/bin/ssmcpd') and before['config']==digest(args.config))
            save(receipt,{'commit':args.commit,'expected_version':args.expected_version,
                          'package_sha':args.sha256,'proxy_sha':proxy_digest,'before':before,'prepared_unix':time.time()})
            report['state']='prepared'
        else:
            saved=json.loads(receipt.read_text())
            check('matching_stage_receipt',saved.get('commit')==args.commit and saved.get('expected_version')==args.expected_version
                  and saved.get('package_sha')==args.sha256 and saved.get('proxy_sha')==proxy_digest
                  and 0<=time.time()-saved['prepared_unix']<3600)
            if args.proxy_config:check('proxy_production_lease',saved['before'].get('proxy')==digest(args.proxy_target))
            check('production_lease',saved['before']['binary']==digest('/usr/bin/ssmcpd') and saved['before']['config']==digest(args.config)
                  and saved['before']['package']==run(['pacman','-Q','ssmcp-git']))
            rollback=args.rollback_package.resolve()
            check('rollback_available',rollback.is_file())
            rollback_hash=digest(rollback)
            # Preserve any existing hotfixes that differ from the cached package.
            paths=['usr/bin/ssmcp','usr/bin/ssmcpd','usr/libexec/ssmcp','usr/lib/systemd/system/ssmcpd.service',
                'usr/lib/systemd/system/ssmcp-portal.service','usr/lib/systemd/system/ssmcp-portal-ssh.service',
                'usr/lib/systemd/system/ssmcp-portal-ssh.timer','usr/libexec/ssmcp-portal-ssh-sync']
            if args.proxy_config:paths.append(str(args.proxy_target).lstrip('/'))
            existing=[path for path in paths if Path('/'+path).exists()]
            backup=checkpoint/'files-before.tar'
            check('checkpoint_unused',not backup.exists())
            run(['tar','-cpf',str(backup),'-C','/',*existing])
            check('checkpoint_created')
            portal_active=run(['systemctl','show','ssmcp-portal.service','-p','ActiveState','--value'])=='active'
            try:
                run(['pacman','-U','--noconfirm',str(args.package)],timeout=120)
                check('installed_version',run(['/usr/bin/ssmcpd','--version'])==expected_binary)
                if args.proxy_config:
                    temporary=args.proxy_target.with_suffix('.conf.ssmcp-new')
                    try:
                        shutil.copyfile(args.proxy_config,temporary);temporary.chmod(0o644)
                        os.replace(temporary,args.proxy_target)
                        run(['nginx','-t']);run(['systemctl','reload','nginx'])
                        check('proxy_installed',digest(args.proxy_target)==proxy_digest)
                    finally:
                        temporary.unlink(missing_ok=True)
                run(['systemctl','daemon-reload'])
                run(['systemctl','restart','ssmcpd.service'],timeout=45)
                wait_health('http://127.0.0.1:8080')
                metadata('http://127.0.0.1:8080',issuer)
                url=registration('http://127.0.0.1:8080',issuer)
                run(['systemctl','restart','ssmcpd.service'],timeout=45)
                wait_health('http://127.0.0.1:8080')
                check('production_DCR_survives_restart',request('http://127.0.0.1:8080',url)[0]==200)
                if portal_active:
                    run(['systemctl','restart','ssmcp-portal.service'],timeout=45)
                    wait_health('http://127.0.0.1:8086')
                    check('portal_remains_healthy')
                wait_health(issuer);metadata(issuer,issuer)
                check('public_health_discovery_and_auth_rejection')
                check('public_homepage',request(issuer,'/')[0]==200)
                check('configuration_unchanged',digest(args.config)==saved['before']['config'])
                registry=Path('/var/lib/ssmcpd/oauth-clients.toml')
                check('production_registry_permissions',registry.stat().st_mode&0o777==0o600
                      and registry.stat().st_uid==pwd.getpwnam('ssmcpd').pw_uid)
                pid=run(['systemctl','show','ssmcpd','-p','MainPID','--value'])
                check('running_binary_matches_install',digest('/proc/'+pid+'/exe')==digest('/usr/bin/ssmcpd'))
                report.update(state='deployed',server_version=run(['/usr/bin/ssmcpd','--version']),
                    package=run(['pacman','-Q','ssmcp-git']),binary_sha256=digest('/usr/bin/ssmcpd'),
                    registry=str(registry),rollback_checkpoint=str(checkpoint))
            except Exception:
                check('rollback_package_unchanged',digest(rollback)==rollback_hash)
                run(['pacman','-U','--noconfirm',str(rollback)],timeout=120)
                run(['tar','-xpf',str(backup),'-C','/'])
                if args.proxy_config:
                    run(['nginx','-t']);run(['systemctl','reload','nginx'])
                run(['systemctl','daemon-reload']);run(['systemctl','restart','ssmcpd'],timeout=45)
                if portal_active:run(['systemctl','restart','ssmcp-portal'],timeout=45)
                wait_health(issuer)
                report['state']='rolled_back';check('rollback_health')
                raise
    except Exception as error:
        if report['state']!='rolled_back':report['state']='failed'
        report['error_type']=type(error).__name__
        report['failed_check']=next((k for k,v in report['checks'].items() if not v),None)
    finally:
        report['finished_unix']=int(time.time());save(args.report,report)
        print(json.dumps(report,sort_keys=True))
        lock.close()
    return 0 if report['state'] in ('prepared','deployed') else 1

if __name__=='__main__':raise SystemExit(main())
