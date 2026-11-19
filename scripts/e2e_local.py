#!/usr/bin/env python3
"""Exercise the real Linux local CLI, OAuth and owned physical terminal.

No router fixture, GitHub provider, remote account or model is started. --browser
requires an installed Chromium and Playwright; it verifies real browser cookie
handling instead of simulating that part with HTTP headers. All credentials and
raw terminal output stay in memory or in the private disposable test directory.
"""
from __future__ import annotations

import argparse
import base64
import concurrent.futures
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import pwd
import re
import secrets
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit

from e2e import Harness, wait_for
from e2e_account_gateway import Form
from e2e_dashboard import Frontend, text


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def process_identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return None if fields[0] == "Z" else fields[19]
    except (FileNotFoundError, ProcessLookupError):
        return None


def owned_children(parent):
    found = {}
    pending = [parent]
    while pending:
        pid = pending.pop()
        try:
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
        except (FileNotFoundError, ProcessLookupError):
            continue
        for value in children:
            child = int(value)
            identity = process_identity(child)
            if identity is not None and child not in found:
                found[child] = identity
                pending.append(child)
    return found


class Callback:
    """A real loopback OAuth redirect target, with request logging disabled."""

    def __init__(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(b"Local authorization completed. This test page can close.")

            def log_message(self, *_arguments):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}/callback"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise AssertionError("owned OAuth callback did not stop")


class Local(Harness):
    def __init__(self, directory, binary_dir, decoder, browser):
        super().__init__(directory, binary_dir)
        self.decoder = decoder
        self.browser = browser
        self.front = None
        self.environments = {}
        self.workspace = directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        self.safe_git_env = {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                             "GIT_TERMINAL_PROMPT": "0"}
        self.callback = None
        self.cleanup_verified = False
        self.browser_cookie_verified = False
        self.http_timeout = 30
        self.wait_user_verified = False

    def client_command(self, *, name="local-fixture", shell="/bin/sh", extra_args=()):
        return [str(self.binary_dir / "ssmcp"), "local", "--name", name,
                "--shell", shell, "--no-watchdog", "--no-browser", *extra_args]

    def client_environment(self, label, **_kwargs):
        # The shared helper exposes only system utilities, never coding models.
        # Its account router is never prepared, so it does not seed a login.
        environment = super().client_environment(label)
        home = self.directory / f"{label}-home"
        home.mkdir(mode=0o700)
        for name in ("config", "state", "cache", "runtime", "output"):
            (home / name).mkdir(mode=0o700)
        environment.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_STATE_HOME": str(home / "state"), "XDG_CACHE_HOME": str(home / "cache"),
            "XDG_RUNTIME_DIR": str(home / "runtime"), "SSMCP_OUTPUT_DIR": str(home / "output")})
        for name in ("SSMCP_RELAY_SERVER", "SSMCP_RELAY_TOKEN", "SSMCP_RELAY_CAPABILITIES",
                     "DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XDG_CURRENT_DESKTOP"):
            environment.pop(name, None)
        self.environments[label] = environment
        return environment

    def validate_connection_record(self, value):
        code = value.get("login_code", "")
        if not re.fullmatch(r"[0-9a-f]{64}", code):
            raise AssertionError("local CLI did not provide a one-use login code")
        self.sensitive.append(code)
        resource = urlsplit(value.get("url", ""))
        self.check(value.get("transport") == "local" and value.get("auth") == "local_user",
                   "the actual CLI identifies local OS-user transport")
        self.check(resource.scheme == "http" and resource.hostname == "127.0.0.1"
                   and resource.port and resource.path == "/" + pwd.getpwuid(os.geteuid()).pw_name,
                   "the real local endpoint is loopback and bound to the effective OS user")
        self.check(not value.get("allow") and "secret" not in value,
                   "local connection metadata does not invent an external identity")
        self.origin = self.issuer = f"http://127.0.0.1:{resource.port}"
        self.http_port = resource.port
        self.user = resource.path[1:]
        return value

    def request(self, *args, **kwargs):
        try:
            return super().request(*args, **kwargs)
        finally:
            if self.front is not None and threading.current_thread() is threading.main_thread():
                self.front.pump(0.01)

    def prepare(self):
        config = self.directory / "local-ui.toml"
        config.write_text('[ui]\ndashboard=true\ncolor=false\nsidebar=false\nlayout="chat"\n')
        self.front = Frontend(self, "local-fixture", config, self.decoder, rows=34, cols=120)
        self.client = self.front.process
        self.client_id = self.front.client_id
        self.credentials = self.front.credentials
        self.check(not self.account_gateway.prepared and self.account_gateway.process is None,
                   "the local CLI starts without an account-router fixture or GitHub provider")

    def authorization_entry(self):
        front = self.front
        snapshot = front.until(lambda value: "本地会话" in text(value) and "/logs" in text(value),
                               "local authorization instructions")
        self.check(self.origin in text(snapshot) and "本月剩余" not in text(snapshot)
                   and "未登录" not in text(snapshot),
                   "actual PTY shows local origin and authorization without a cloud-account quota")
        front.send("/login\r")
        front.until(lambda value: "无需 GitHub" in text(value), "local login command explanation")
        auth = Path(self.environments["local-fixture"]["SSMCP_AUTH_HOME"])
        self.check(not list(auth.iterdir()), "local /login does not create a remote account credential")
        front.send(b"\x15")
        front.send("/logs\r")
        # The entry is read only in this owned PTY. Never persist its secret or a
        # screenshot of the real code in the machine-readable report.
        seen = False
        for _ in range(8):
            snapshot = front.snapshot()
            compact = "".join(text(snapshot).split())
            if self.credentials["login_code"] in compact and self.origin in compact:
                seen = True
                break
            front.send(b"\x1b[5~")
            front.pump(0.1)
        self.check(seen, "the user can reach the current MCP URL and one-use code through /logs")
        front.send(b"\x1b")
        front.pump(0.15)

    def oauth_login(self, credentials, *, attacks=False):
        self.callback = Callback()
        status, metadata, _ = self.oauth_request("/.well-known/oauth-authorization-server")
        self.check(status == 200 and metadata["issuer"] == self.origin,
                   "OAuth discovery advertises the exact real loopback issuer")
        registration = {"client_name": "Local mode acceptance", "application_type": "native",
            "redirect_uris": [self.callback.url], "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"]}
        status, registered, _ = self.oauth_request("/oauth/register", "POST", registration)
        self.check(status == 201 and bool(registered.get("client_id")), "local OAuth performs real dynamic client registration")
        self.oauth_client = registered
        verifier = secrets.token_urlsafe(32)
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        state = secrets.token_urlsafe(24)
        params = {"client_id": registered["client_id"], "redirect_uri": self.callback.url,
                  "response_type": "code", "resource": credentials["url"], "scope": "mcp",
                  "code_challenge": challenge, "code_challenge_method": "S256", "state": state}
        authorize = self.origin + "/oauth/authorize?" + urlencode(params)
        if self.browser:
            node = shutil.which("node")
            if not node:
                raise RuntimeError("--browser requires an already-installed Node.js")
            driver = Path(__file__).parent.parent / "tests/fixtures/local_oauth_browser.mjs"
            result = subprocess.run([node, str(driver)], input=json.dumps({
                "authorize_url": authorize, "login_code": credentials["login_code"],
                "origin": self.origin, "callback_url": self.callback.url,
            }), text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
            self.check(result.returncode == 0, "isolated real Chromium completed local authorization")
            reply = json.loads(result.stdout)
            location = reply["callback_url"]
            cookie = reply["cookie"]
            self.check(cookie == {"name": "ssmcp-local-oauth", "secure": False,
                                  "httpOnly": True, "sameSite": "Lax"},
                       "real Chromium accepts the scoped HttpOnly loopback cookie and submits it")
            self.browser_cookie_verified = True
        else:
            path = "/oauth/authorize?" + urlencode(params)
            status, body, headers = self.oauth_request(path)
            form = Form()
            form.feed(body)
            self.check(status == 200 and "transaction_id" in form.fields and "login_code" in form.fields,
                       "local consent requests the connection code without a provider login")
            cookie_header = headers["set-cookie"]
            self.check(cookie_header.startswith("ssmcp-local-oauth=") and "; Secure;" not in cookie_header,
                       "HTTP local consent emits its separate cookie name")
            cookie = cookie_header.split(";", 1)[0]
            status, _, headers = self.oauth_request("/oauth/authorize", "POST", {
                "transaction_id": form.fields["transaction_id"], "login_code": credentials["login_code"],
            }, cookie=cookie, headers={"Origin": self.origin}, form=True)
            self.check(status == 302, "one-use local connection code authorizes the actual session")
            location = headers["location"]
        callback = urlsplit(location)
        expected = urlsplit(self.callback.url)
        query = parse_qs(callback.query)
        self.check((callback.scheme, callback.netloc, callback.path) == (expected.scheme, expected.netloc, expected.path)
                   and query.get("state") == [state] and query.get("iss") == [self.origin]
                   and len(query.get("code", [])) == 1,
                   "OAuth callback preserves the exact redirect, state and issuer")
        self.sensitive.append(query["code"][0])
        exchange = {"client_id": registered["client_id"], "grant_type": "authorization_code",
                    "code": query["code"][0], "code_verifier": verifier,
                    "redirect_uri": self.callback.url, "resource": credentials["url"]}
        status, tokens, _ = self.oauth_request("/oauth/token", "POST", exchange, form=True)
        self.check(status == 200 and tokens.get("token_type") == "Bearer" and bool(tokens.get("access_token")),
                   "PKCE exchange returns a real local MCP access token")
        self.secret = tokens["access_token"]
        self.sensitive.extend(value for key, value in tokens.items() if "token" in key and isinstance(value, str))
        self.check(self.oauth_request("/oauth/token", "POST", exchange, form=True)[0] == 400,
                   "the authorization code cannot be exchanged twice")
        self.callback.close()
        self.callback = None

    def run(self):
        self.prepare()
        self.authorization_entry()
        for path in ("/healthz", "/oauth/register", "/.well-known/oauth-authorization-server"):
            self.check(self.request("GET", path=path, headers={"Host": "attacker.invalid"}, secret="")[0] == 403,
                       "local route rejects a mismatched Host")
            self.check(self.request("GET", path=path, headers={"Origin": "https://attacker.invalid"}, secret="")[0] == 403,
                       "local route rejects a foreign Origin")
        for path in ("/account/login", "/api/client/whoami", "/api/client/connect"):
            self.check(self.request("GET", path=path, secret="")[0] == 404,
                       "local gateway does not expose portal or device-account routes")
        self.check(self.request(body={}, secret="")[0] == 401, "loopback proximity does not bypass MCP authentication")
        self.oauth_login(self.credentials)
        status, initialized, _ = self.rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                                       "clientInfo": {"name": "local-acceptance", "version": "1"}})
        self.check(status == 200 and "result" in initialized, "the authenticated local client initializes MCP")
        clients = self.public_call("list", {})
        self.check(any(item["id"] == self.client_id for item in clients["clients"]),
                   "local authorization discovers the actual foreground client")
        content = "Pure local MCP\n中文与空格 remain intact.\n"
        self.call("ssmcp.fs.write_text", {"path": "local-check.txt", "text": content})
        read = self.call("ssmcp.fs.read_text", {"path": "local-check.txt"})
        self.check(read["text"] == content and (self.workspace / "local-check.txt").read_text() == content,
                   "real native filesystem calls round-trip through local OAuth/MCP")
        job = self.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", "printf LOCAL_EXEC_OK"], "timeout_ms": 5000})
        output = ""
        stdout_cursor = stderr_cursor = 0
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            result = self.call("ssmcp.exec.read", {"job_id": job["job_id"], "wait_ms": 1000, "max_bytes": 4096,
                                                  "stdout_cursor": stdout_cursor, "stderr_cursor": stderr_cursor})
            output += result["stdout"]
            stdout_cursor, stderr_cursor = result["stdout_cursor"], result["stderr_cursor"]
            if not result["running"] and result["stdout_eof"]:
                break
        self.check(not result["running"] and result["exit_code"] == 0 and output == "LOCAL_EXEC_OK",
                   "the real local execution backend returns process output and exit status")
        self.wait_user_roundtrip()
        children = owned_children(self.client.pid)
        socket_path = None
        for pid in children:
            try:
                args = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            except (FileNotFoundError, ProcessLookupError):
                continue
            if len(args) >= 3 and args[1] == b"__local-relay":
                socket_path = Path(os.fsdecode(args[2]))
        self.check(socket_path is not None and socket_path.exists(), "the actual transport uses its owned private Unix socket")
        self.check(socket_path.parent.stat().st_uid == os.geteuid()
                   and socket_path.parent.stat().st_mode & 0o777 == 0o700,
                   "the actual local transport directory is private to the effective UID")
        self.front.disconnect()
        wait_for(lambda: all(process_identity(pid) != identity for pid, identity in children.items()),
                 timeout=8, description="owned local transport and backend descendants to exit")
        self.check(not socket_path.exists() and not socket_path.parent.exists(),
                   "local shutdown removes only its owned socket and private directory")
        try:
            self.request("GET", path="/healthz", secret="")
        except (ConnectionError, OSError):
            pass
        else:
            raise AssertionError("local gateway continued listening after the client stopped")
        self.check(True, "local gateway and its in-memory authorization lifetime end with the client")
        self.cleanup_verified = True

    def wait_user_roundtrip(self):
        code = f"return await ui.wait({json.dumps(self.client_id)}, {{timeout_ms:10000,intent:'need_user',reason:'Synthetic local wait',notify:false}});"

        def waiting():
            return self.rpc("tools/call", {"name": "code_mode", "arguments": {"code": code, "timeout_ms": 20000}}, modern=True)

        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(waiting)
            active = None
            deadline = time.monotonic() + 6
            while time.monotonic() < deadline and not future.done():
                snapshot = self.call("ssmcp.ui.wait", {"op": "status"})
                active = next((item for item in snapshot["state"]["active"] if item.get("reason") == "Synthetic local wait"), None)
                if active:
                    break
                self.front.pump(0.05)
            self.check(active is not None and not future.done(),
                       "authenticated local wait blocks and reaches the actual native wait registry")
            message = "SYNTHETIC_LOCAL_USER_WAIT_REPLY"
            self.front.send(b"\x15")
            self.front.send(message + "\r")
            deadline = time.monotonic() + 12
            while not future.done() and time.monotonic() < deadline:
                self.front.pump(0.05)
            status, response, _ = future.result(timeout=1)
        self.check(status == 200 and not response.get("error"), "real local user input releases the authenticated waiting HTTP call")
        structured = response["result"]["structuredContent"]
        receipt, = structured["user_wait"]
        messages = structured["user_messages"]
        self.check(receipt["client_id"] == self.client_id and receipt["wait_id"] == active["wait_id"]
                   and receipt["status"] == "received" and receipt["user_consent_to_end"] is None,
                   "the final local wait receipt matches its server-owned wait without inferring consent to end")
        self.check(len(messages) == 1 and messages[0]["text"] == message
                   and receipt["messages"] == [{"message_id": item["message_id"], "revision": item["revision"]} for item in messages],
                   "the final local response delivers the exact PTY message and committed queue revision")
        timed = self.public_call("code_mode", {"code": f"return await ui.wait({json.dumps(self.client_id)}, {{timeout_ms:1000,notify:false}});", "timeout_ms": 10000})
        receipt, = timed["user_wait"]
        self.check(receipt["status"] == "timed_out" and receipt["continue_wait"] is True and not timed.get("user_messages"),
                   "an empty bounded local wait times out and asks the caller to continue waiting")
        self.wait_user_verified = True

    def close(self):
        if self.callback is not None:
            self.callback.close()
            self.callback = None
        if self.front is not None:
            self.front.close()
        self.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--browser", action="store_true", help="require real installed Chromium/Playwright for OAuth")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("the local host first implementation is Linux-only")
    decoder = args.decoder or args.bin_dir / "vt_snapshot"
    if not decoder.is_file():
        parser.error("supply the already-built vt_snapshot with --decoder")
    started = time.monotonic()
    report = {"schema_version": 1, "status": "failed", "platform": "Linux",
              "transport": "real foreground CLI + private Unix socket + loopback HTTP + OAuth/PKCE + owned PTY",
              "external_account_used": False, "models_started": False, "mdns_published": False,
              "named_tls_proxy_verified": False, "browser_requested": args.browser,
              "binaries": {"ssmcp": sha256(args.bin_dir / "ssmcp"), "vt_snapshot": sha256(decoder)},
              "sources": {"e2e_local.py": sha256(__file__),
                          "e2e.py": sha256(Path(__file__).with_name("e2e.py")),
                          "e2e_dashboard.py": sha256(Path(__file__).with_name("e2e_dashboard.py")),
                          "e2e_account_gateway.py": sha256(Path(__file__).with_name("e2e_account_gateway.py")),
                          "local_oauth_browser.mjs": sha256(Path(__file__).parent.parent / "tests/fixtures/local_oauth_browser.mjs")}}
    runner = Path(os.environ.get("SSMCP_CODE_MODE_RUNNER", args.bin_dir / "code-mode.js"))
    if runner.is_file():
        report["binaries"]["code-mode.js"] = sha256(runner)

    def interrupted(_signum, _frame):
        raise TimeoutError("local E2E interrupted; cleaning up owned resources")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(120)
    with tempfile.TemporaryDirectory(prefix="ssmcp-local-e2e-") as directory:
        harness = Local(Path(directory), args.bin_dir, decoder, args.browser)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            # Keep failure evidence useful without copying real codes, tokens,
            # callback URLs or raw terminal frames into a shareable report.
            report["error_type"] = type(error).__name__
            print(f"Local acceptance failed: {type(error).__name__}", flush=True)
            raise
        finally:
            try:
                harness.close()
            except Exception as error:
                report["status"] = "failed"
                report["cleanup_error_type"] = type(error).__name__
                raise
            finally:
                report.update(checks=harness.checks, browser_cookie_verified=harness.browser_cookie_verified,
                              wait_user_verified=harness.wait_user_verified, cleanup_verified=harness.cleanup_verified,
                              duration_seconds=round(time.monotonic() - started, 3))
                if args.report:
                    args.report.parent.mkdir(parents=True, exist_ok=True)
                    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
                signal.alarm(0)
    print(f"Passed {len(report['checks'])} local checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
