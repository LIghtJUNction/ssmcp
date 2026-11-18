#!/usr/bin/env python3
"""Exercise --tty through the real client with a disposable local wire peer.

No relay, credentials or model is used. The default terminal fixture starts the
actual viewer and records its bytes. --terminal kitty/ztty uses real windows.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time

def wait_for(check, seconds=10):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        value=check()
        if value:return value
        time.sleep(.05)
    raise AssertionError("fixture condition timed out")

def peer():
    root=Path(os.environ["SSMCP_TTY_FIXTURE"])
    checks=[]
    pending=bytearray()
    def receive(deadline):
        while b"\n" not in pending:
            remaining=deadline-time.monotonic()
            if remaining<=0 or not select.select([sys.stdin],[],[],remaining)[0]:
                raise AssertionError("native reply timed out")
            chunk=os.read(sys.stdin.fileno(),65536)
            assert chunk,"client closed before delivering a reply"
            pending.extend(chunk)
        line,_,rest=pending.partition(b"\n")
        pending[:]=rest
        return json.loads(line)
    hello=receive(time.monotonic()+30)
    assert hello["type"]=="hello"
    print(json.dumps({"type":"ready","url":"https://fixture.invalid/mcp","login_code":"",
        "login_expires_in":0,"client_id":"b"*64}),flush=True)
    sequence=0
    def call(name,arguments=None):
        nonlocal sequence
        sequence+=1
        print(json.dumps({"type":"call","id":sequence,
            "operation":{"op":"call_tool","name":name,"arguments":arguments or {}}}),flush=True)
        deadline=time.monotonic()+30
        while True:
            frame=receive(deadline)
            if frame["type"]=="steering":
                assert hello["version"]>=7 and frame["command"]=={"op":"snapshot"},frame
                print(json.dumps({"type":"steering_reply","id":frame["id"],"result":{"Ok":{
                    "schema_version":1,"client_id":"b"*64,"revision":0,
                    "agents":[],"messages":[]}}}),flush=True)
                continue
            assert frame["type"]=="reply" and frame["id"]==sequence,frame
            break
        result=frame["result"]
        assert "Ok" in result,result
        result=result["Ok"]
        assert not result.get("isError"),result
        return result["structuredContent"]
    def start(script,*args,**options):
        return call("ssmcp.exec.start",{"command":[sys.executable,"-u","-c",script,*args],**options})
    def done(job):
        value=call("ssmcp.exec.wait",{"job_id":job["job_id"],"wait_ms":10000})
        assert not value["running"],value
        return value
    def read(job):
        return call("ssmcp.exec.read",{"job_id":job["job_id"]})
    def viewers():
        prefix=f"/tmp/ssmcp-tty-{os.getppid()}-"
        found=[]
        for process in Path("/proc").glob("[0-9]*"):
            try:
                argv=process.joinpath("cmdline").read_bytes().split(b"\0")
                if len(argv)>=4 and argv[1]==b"__tty-view" and argv[2].decode().startswith(prefix):
                    found.append((int(process.name),int(argv[3])))
            except (OSError,UnicodeDecodeError,ValueError):pass
        return found
    try:
        first=start("import os,sys; assert os.isatty(1) and os.isatty(2); print('\\x1b[32m'+sys.argv[1]+'\\x1b[0m'); print('stderr marker',file=sys.stderr); sys.exit(7)",
            "空 格 ' $(literal)")
        assert first["output_mode"]=="pty" and first["tty_slot"]==1
        assert done(first)["exit_code"]==7
        output=read(first)
        assert "空 格 ' $(literal)" in output["stdout"] and "stderr marker" in output["stdout"]
        assert "\x1b[32m" in output["stdout"] and output["stderr"]==""
        checks.append("real PTY, ANSI, literal Unicode/spaced argv, combined output and exact exit 7")
        second=start("print('sequential reuse')")
        assert second["tty_slot"]==1 and done(second)["exit_code"]==0
        assert len(viewers())==1
        checks.append("sequential commands reuse one live viewer")
        a=start("import time; print('parallel A'); time.sleep(2)")
        b=start("import time; print('parallel B'); time.sleep(2)")
        assert a["tty_slot"]!=b["tty_slot"]
        wait_for(lambda:len(viewers())==2)
        assert done(a)["exit_code"]==0 and done(b)["exit_code"]==0
        checks.append("overlapping jobs allocate two real viewers and survive tool return")
        piped=start("import sys; print('stdin:'+sys.stdin.read(),end='')",stdin="pipe")
        call("ssmcp.exec.write",{"job_id":piped["job_id"],"data":"pipe marker\n","close_stdin":True})
        assert done(piped)["exit_code"]==0 and "pipe marker" in read(piped)["stdout"]
        checks.append("pipe stdin and EOF retain execution semantics")
        long=start("import subprocess,time; p=subprocess.Popen(['/bin/sleep','30']); print(p.pid); p.wait()")
        raw=call("ssmcp.exec.read",{"job_id":long["job_id"],"wait_ms":5000})
        pid=int(raw["stdout"].strip())
        cancelled=call("ssmcp.exec.cancel",{"job_id":long["job_id"]})
        assert cancelled["cancelled"] and not cancelled["running"]
        try:os.kill(pid,0)
        except ProcessLookupError:pass
        else:raise AssertionError("cancelled descendant is alive")
        checks.append("cancellation reaps the command and owned child")
        continuing=start("import time; print('long task started'); time.sleep(10); print('long task completed')")
        owned=wait_for(lambda:[pid for pid,slot in viewers() if slot==continuing["tty_slot"]])
        old_pid=owned[0]
        os.kill(old_pid,signal.SIGTERM)
        wait_for(lambda:any(slot==continuing["tty_slot"] and pid!=old_pid for pid,slot in viewers()),12)
        assert done(continuing)["exit_code"]==0
        assert "long task completed" in read(continuing)["stdout"]
        checks.append("manually closed viewer reopens while its command continues")
        call("terminal.write",{"data":"printf 'interactive terminal marker\\n'\n"})
        wait_for(lambda:any("interactive terminal marker" in p.read_text(errors="replace")
            for p in root.glob("view-*.ansi")),5) if os.environ.get("SSMCP_TTY_FAKE") else time.sleep(.5)
        checks.append("existing interactive PTY commands appear in observer windows")
        if os.environ.get("SSMCP_TTY_HOLD"):
            time.sleep(float(os.environ["SSMCP_TTY_HOLD"]))
        report={"checks":checks,"passed":len(checks),"output_mode":first["output_mode"],
            "first_slot":first["tty_slot"],"parallel_slots":[a["tty_slot"],b["tty_slot"]],
            "viewer_pids":[pid for pid,_ in viewers()]}
        (root/"result.json").write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    except Exception as error:
        (root/"result.json").write_text(json.dumps({"error":str(error),"checks":checks},ensure_ascii=False,indent=2))
        raise

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary",type=Path,required=True)
    parser.add_argument("--terminal",help="Open actual terminal windows instead of the recording launcher")
    parser.add_argument("--report",type=Path)
    parser.add_argument("--hold",type=float,default=0,help="Leave final windows visible for screenshot inspection")
    args=parser.parse_args()
    binary=args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="ssmcp-tty-e2e-") as temp:
        root=Path(temp);bin_dir=root/"bin";bin_dir.mkdir()
        peer_script=Path(__file__).resolve()
        ssh=bin_dir/"ssh"
        ssh.write_text("#!"+sys.executable+"\nimport os\nos.execv("+repr(sys.executable)+",["+repr(sys.executable)+","+repr(str(peer_script))+",'--peer'])\n")
        ssh.chmod(0o700)
        (bin_dir/"ssh-keygen").symlink_to("/usr/bin/ssh-keygen")
        terminal=args.terminal
        if terminal is None:
            launcher=bin_dir/"fixture-terminal"
            launcher.write_text("#!"+sys.executable+"\n"+r"""
import json,os,sys
from pathlib import Path
root=Path(os.environ["SSMCP_TTY_FIXTURE"])
args=sys.argv[1:]
assert args.pop(0)=="-e",args
slot=args[-1]
with (root/"launches.jsonl").open("a") as log:
    log.write(json.dumps({"pid":os.getpid(),"slot":int(slot)})+"\n")
fd=os.open(root/("view-"+slot+".ansi"),os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)
os.dup2(fd,1);os.dup2(fd,2);os.close(fd)
os.execv(args[0],args)
""")
            launcher.chmod(0o700);terminal=str(launcher)
        elif "/" not in terminal:
            import shutil
            terminal=shutil.which(terminal)
            if terminal is None:raise SystemExit("requested terminal is not installed")
        env=os.environ.copy()
        (root/"config.toml").write_text("")
        for key in ("SSMCP_UI_CONFIG","SSMCP_SESSION_GENERATION","SSMCP_CLIENT_OUTBOX","SSMCP_OUTPUT_RELOAD"):
            env.pop(key,None)
        env.update(PATH=str(bin_dir),XDG_CONFIG_HOME=str(root/"config"),XDG_STATE_HOME=str(root/"state"),
            SSMCP_AUTH_HOME=str(root/"auth"),SSMCP_CLIENT_CONFIG=str(root/"config.toml"),
            SSMCP_SESSION_HOME=str(root/"sessions"),
            SSMCP_TTY_FIXTURE=str(root),SSMCP_TTY_HOLD=str(args.hold))
        if not args.terminal:env["SSMCP_TTY_FAKE"]="1"
        log=root/"client.log"
        with log.open("wb") as output:
            process=subprocess.Popen([str(binary),"--ssh","fixture.invalid","--allow","fixture@github",
                "--no-setup","--no-watchdog","--no-color","--shell","/bin/sh","--tty",terminal],
                cwd=root,env=env,stdin=subprocess.DEVNULL,stdout=output,stderr=output,start_new_session=True)
            try:
                result=wait_for(lambda:(root/"result.json").exists() or process.poll() is not None,60+args.hold)
                if not (root/"result.json").exists():
                    raise AssertionError("client exited before acceptance: "+log.read_text(errors="replace")[-5000:])
                report=json.loads((root/"result.json").read_text())
                if "error" in report:raise AssertionError(report)
                if not args.terminal:
                    views="\n".join(p.read_text(errors="replace") for p in root.glob("view-*.ansi"))
                    assert "sequential reuse" in views and "parallel A" in views and "parallel B" in views
                    assert "[exit 7]" in views and "[cancelled]" in views
                    report["recorded_viewers"]=len((root/"launches.jsonl").read_text().splitlines())
                report["terminal"]=args.terminal or "recording launcher with real ssmcp viewer"
                print(json.dumps(report,ensure_ascii=False,indent=2))
                if args.report:
                    args.report.parent.mkdir(parents=True,exist_ok=True)
                    args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    try:process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid,signal.SIGKILL);process.wait()
                if process.returncode not in (0,1):
                    print(log.read_text(errors="replace")[-3000:],file=sys.stderr)

if __name__=="__main__":
    if sys.argv[1:]==["--peer"]:peer()
    else:main()
