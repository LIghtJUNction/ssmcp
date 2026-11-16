#!/usr/bin/env python3
"""Exercise DCR across real daemon restarts without touching an installed service."""
from __future__ import annotations
import argparse
import hashlib
import base64
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from urllib.parse import urlencode

def request(port, path, payload=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        conn.request("GET" if payload is None else "POST",path,
                     body=None if payload is None else json.dumps(payload),
                     headers={"Content-Type":"application/json"})
        response=conn.getresponse()
        body=response.read().decode()
        return response.status, body
    finally:
        conn.close()

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir",type=Path,default=Path("target/debug"))
    parser.add_argument("--report",type=Path)
    args=parser.parse_args()
    binary=(args.bin_dir/"ssmcpd").resolve()
    checks=[]
    def check(value,name):
        if not value: raise AssertionError(name)
        checks.append(name)
        print("PASS",name,flush=True)
    with tempfile.TemporaryDirectory(prefix="ssmcp-registry-http-") as directory:
        root=Path(directory).resolve()
        root.chmod(0o700)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1",0));port=sock.getsockname()[1]
        command=[str(binary),"serve","--public-url","https://ssmcp.example",
                 "--listen",f"127.0.0.1:{port}","--socket",str(root/"control.sock"),
                 "--client-registry",str(root/"clients.toml")]
        env={k:v for k,v in os.environ.items() if not k.startswith("SSMCP_") and k!="STATE_DIRECTORY"}
        config=root/"empty.toml";config.write_text("")
        command += ["--config",str(config)]
        def start():
            process=subprocess.Popen(command,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,env=env)
            try:
                for _ in range(100):
                    if process.poll() is not None: raise AssertionError("daemon startup failed")
                    try:
                        if request(port,"/healthz")[0]==200:return process
                    except (OSError,http.client.HTTPException):pass
                    time.sleep(0.03)
                raise AssertionError("daemon startup timed out")
            except BaseException:
                stop(process);raise
        def stop(process):
            if process.poll() is None:
                process.terminate()
                try:process.wait(timeout=8)
                except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
        p=start()
        try:
            status,text=request(port,"/oauth/register",{
                "client_name":"Restart HTTP regression",
                "redirect_uris":["https://client.example/callback"],
                "token_endpoint_auth_method":"client_secret_post"})
            check(status==201,"register confidential client over real HTTP")
            client=json.loads(text)
            persisted=(root/"clients.toml").read_text()
            check(client["client_secret"] not in persisted,"plaintext client secret is not stored")
            check((root/"clients.toml").stat().st_mode & 0o777 == 0o600,"registry has mode 0600")
            query=urlencode({"client_id":client["client_id"],"redirect_uri":"https://client.example/callback",
                "response_type":"code","resource":"https://ssmcp.example/alice","scope":"mcp",
                "code_challenge_method":"S256",
                "code_challenge":base64.urlsafe_b64encode(hashlib.sha256(b"x"*43).digest()).decode().rstrip("=")})
            check(request(port,"/oauth/authorize?"+query)[0]==200,"registered client can start authorization")
            stop(p)
            p=start()
            status,text=request(port,"/oauth/authorize?"+query)
            check(status==200 and "Register a client with this issuer" not in text,
                  "same client ID can authorize after full daemon restart")
            check((root/"clients.toml").read_text()==persisted,"restart does not renew or rewrite registration expiry")
            status,_=request(port,"/oauth/authorize?"+query.replace("https%3A%2F%2Fclient.example%2Fcallback","https%3A%2F%2Fevil.example%2Fcallback"))
            check(status==400,"saved redirect URI remains exact-match only")
        finally:stop(p)
        (root/"clients.toml").write_text("invalid = [")
        p=subprocess.Popen(command,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,env=env)
        try:check(p.wait(timeout=8)!=0,"corrupt persisted registry fails startup closed")
        finally:stop(p)
    report={"status":"passed","checks":checks,"transport":"real daemon + HTTP + full process restart"}
    if args.report:args.report.write_text(json.dumps(report,indent=2)+"\n")
    print(f"Passed {len(checks)} registry restart checks")
if __name__=="__main__":main()
