#!/usr/bin/env python3
"""Exercise the real native UI in owned PTYs; uses only private fixture config."""
import argparse, hashlib, json, os, pathlib, select, signal, struct
import subprocess, tempfile, termios, time, tomllib, fcntl

class Terminal:
    def __init__(self, binary, config, root, rows=30, cols=120):
        self.output = bytearray()
        self.pid, self.fd = os.forkpty()
        self.status = None
        if self.pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                env = {k:v for k,v in os.environ.items() if not k.startswith("SSMCP_")}
                env.update(TERM="xterm-256color", NO_COLOR="1",
                    XDG_CONFIG_HOME=str(root/"config"), XDG_STATE_HOME=str(root/"state"),
                    XDG_CACHE_HOME=str(root/"cache"))
                os.chdir(root)
                os.execve(binary, [binary,"ui","preview","--ui-config",str(config)], env)
            finally:
                os._exit(127)
        os.set_blocking(self.fd, False)

    def drain(self, seconds=0.1):
        until=time.monotonic()+seconds
        while time.monotonic()<until:
            if select.select([self.fd],[],[],max(0,until-time.monotonic()))[0]:
                try: block=os.read(self.fd,65536)
                except (BlockingIOError, OSError): break
                if not block: break
                self.output.extend(block)
                if len(self.output)>4*1024*1024:
                    raise AssertionError("PTY fixture output exceeded its bound")
            else: break

    def wait_text(self, text, start=0, timeout=8):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            self.drain(0.05)
            if text.encode() in self.output[start:]: return
            ended,status=os.waitpid(self.pid,os.WNOHANG)
            if ended:
                self.status=status
                raise AssertionError(f"UI exited before {text!r}: status={status}")
        raise AssertionError(f"UI did not display {text!r}; tail={bytes(self.output[-1600:])!r}")

    def send(self, text):
        data=text.encode() if isinstance(text,str) else text
        while data:
            count=os.write(self.fd,data);data=data[count:]

    def command(self, text):
        start=len(self.output);self.send(text+"\r");return start

    def close(self):
        if self.status is None:
            try: self.send(b"\x1d")
            except OSError: pass
            deadline=time.monotonic()+3
            while time.monotonic()<deadline:
                self.drain(0.03)
                ended,status=os.waitpid(self.pid,os.WNOHANG)
                if ended: self.status=status;break
            if self.status is None:
                # Only the unreaped child created above owns this process group.
                os.killpg(self.pid,signal.SIGTERM)
                deadline=time.monotonic()+2
                while time.monotonic()<deadline:
                    ended,status=os.waitpid(self.pid,os.WNOHANG)
                    if ended: self.status=status;break
                    time.sleep(0.02)
                if self.status is None:
                    os.killpg(self.pid,signal.SIGKILL)
                    _,self.status=os.waitpid(self.pid,0)
        os.close(self.fd)

    def __enter__(self): return self
    def __exit__(self,*args): self.close()

def saved(path):
    return tomllib.loads(path.read_text())

def wait_language(path, value):
    deadline=time.monotonic()+5
    while time.monotonic()<deadline:
        if saved(path).get("ui",{}).get("language")==value: return
        time.sleep(0.025)
    raise AssertionError(f"language {value} was not persisted")

def verify(binary, report):
    result={"binary":str(binary),"sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"scope":"owned PTYs with isolated preview config; no backend or real account","checks":[],"exit_code":None}
    def passed(name):
        result["checks"].append(name)
        report.write_text(json.dumps(result,indent=2))
        print("PASS",name,flush=True)
    with tempfile.TemporaryDirectory(prefix="ssmcp-ui-language-") as tmp:
        root=pathlib.Path(tmp)
        config=root/"settings.toml"
        original="[client]\nserver='https://relay.example.invalid'\n[git]\nfetch_interval_seconds=600\n[ui]\ncolor=false\nsidebar=false\n"
        config.write_text(original)
        with Terminal(str(binary),config,root) as terminal:
            terminal.wait_text("Message the agent")
            passed("default_english_real_pty")
            start=terminal.command("/language zh")
            wait_language(config,"zh")
            terminal.wait_text("语言：中文",start)
            terminal.wait_text("向智能体发送消息",start)
            doc=saved(config)
            assert doc["client"]["server"]=="https://relay.example.invalid"
            assert doc["git"]["fetch_interval_seconds"]==600
            assert config.stat().st_mode&0o777==0o600
            passed("immediate_chinese_private_atomic_save_preserves_other_sections")
            start=terminal.command("/language fr")
            terminal.wait_text("请使用 /language en 或 /language zh",start)
            assert saved(config)["ui"]["language"]=="zh"
            terminal.send(b"\x15")
            passed("invalid_language_keeps_active_and_saved_language")
        with Terminal(str(binary),config,root) as terminal:
            terminal.wait_text("向智能体发送消息")
            passed("chinese_survives_process_restart")
            start=terminal.command("/language en")
            wait_language(config,"en")
            terminal.wait_text("Language: English",start)
            terminal.wait_text("Message the agent",start)
            passed("switch_back_to_english_real_pty")
        with Terminal(str(binary),config,root) as terminal:
            terminal.wait_text("Message the agent")
            passed("english_survives_process_restart")
            config.write_text("malformed = [")
            start=terminal.command("/language zh")
            terminal.wait_text("Language unchanged: could not save settings",start)
            assert config.read_text()=="malformed = ["
            terminal.send(b"\x1bOP")
            terminal.wait_text("Write and send",start)
            passed("failed_save_keeps_english_and_malformed_document")
        config.write_text(original)
        env={k:v for k,v in os.environ.items() if not k.startswith("SSMCP_")}
        env.update(NO_COLOR="1", XDG_CONFIG_HOME=str(root/"config"),
            XDG_STATE_HOME=str(root/"state"), XDG_CACHE_HOME=str(root/"cache"))
        for language in ["en","zh"]:
            for cols in [1,10,24,45,80,120]:
                run=subprocess.run([str(binary),"ui","preview","--static","--ui-config",str(config),
                    "--language",language,"--rows","30","--cols",str(cols)],
                    cwd=root,env=env,capture_output=True,timeout=10,check=True)
                run.stdout.decode("utf-8")
                assert len(run.stdout)<256*1024 and not run.stderr
            passed(f"{language}_static_narrow_unicode_frames")
        run=subprocess.run([str(binary),"ui","show","--ui-config",str(config)],
            cwd=root,env=env,capture_output=True,timeout=10,check=True)
        shown=tomllib.loads(run.stdout.decode())
        assert shown["ui"]["language"]=="en" and not run.stderr
        passed("machine_readable_show_stays_clean_toml")
    result["exit_code"]=0
    report.write_text(json.dumps(result,indent=2))
    print(json.dumps(result,indent=2),flush=True)

if __name__=="__main__":
    def stop(signum, frame):
        raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, stop)
    parser=argparse.ArgumentParser()
    parser.add_argument("binary",type=pathlib.Path)
    parser.add_argument("--report",type=pathlib.Path,required=True)
    args=parser.parse_args()
    try:
        verify(args.binary.resolve(),args.report)
    except BaseException as error:
        result=json.loads(args.report.read_text()) if args.report.exists() else {"checks":[]}
        result.update(exit_code=1,error=f"{type(error).__name__}: {error}")
        args.report.write_text(json.dumps(result,indent=2))
        raise
