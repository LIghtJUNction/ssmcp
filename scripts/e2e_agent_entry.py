#!/usr/bin/env python3
"""Exercise --load-harness through the actual CLI using private fake protocol processes.

No coding model, saved account, or external MCP service is contacted.
"""
import argparse
import ast
import hashlib
import http.server
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time


def process_start(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def records(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def owned_alive(row):
    return process_start(row["pid"]) == row["start"]


def stop(child):
    if child.poll() is None:
        os.killpg(child.pid, signal.SIGINT)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)


def fixture_program(program, source, log):
    body = f'''#!{sys.executable}
import json, os, runpy, sys
from pathlib import Path
log = Path({str(log)!r})
row = {{"program":{program!r},"argv":sys.argv[1:],"pid":os.getpid(),
       "start":Path("/proc/self/stat").read_text().split(") ",1)[1].split()[19]}}
with log.open("a") as output: output.write(json.dumps(row)+"\\n")
if sys.argv[1:] == ["--version"]:
    print({program!r}+" 1.0.0 fixture"); sys.exit(0)
assert {program!r} in ("codex", "claude"), "unselected provider started"
class Input:
    def __init__(self, stream, binary=False):
        self.stream=stream
        if not binary: self.buffer=Input(stream.buffer, True)
    def __iter__(self):
        for line in self.stream:
            request=json.loads(line)
            with log.with_suffix(".rpc.jsonl").open("a") as output:
                output.write(json.dumps({{"program":{program!r},"request":request}})+"\\n")
            assert request.get("method") not in ("turn/start", "prompt"), "startup submitted model work"
            yield line
    def __getattr__(self, key): return getattr(self.stream,key)
sys.stdin=Input(sys.stdin)
sys.argv[0]={str(source)!r}
runpy.run_path({str(source)!r},run_name="__main__")
'''
    ast.parse(body)
    return body


class Peer(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        expected = f"http://127.0.0.1:{self.server.server_port}"
        assert self.path == "/fixture"
        assert self.headers["Host"] == expected.removeprefix("http://")
        assert self.headers["Origin"] == expected
        assert self.headers["Authorization"] == "Bearer " + self.server.fixture_token
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.received.append(body)
        if body["method"] == "notifications/initialized":
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if body["method"] == "initialize":
            result = dict(protocolVersion="2025-11-25", capabilities=dict(tools={}),
                          serverInfo=dict(name="ssmcp-private-fixture", version="1"))
        else:
            assert body["method"] == "tools/list", "startup invoked peer work"
            if "cursor" not in body["params"]:
                result = dict(tools=[dict(name="Read", inputSchema=dict(type="object"))], nextCursor="next")
            else:
                assert body["params"]["cursor"] == "next"
                result = dict(tools=[dict(name="Write", inputSchema=dict(type="object"))])
        payload = json.dumps(dict(jsonrpc="2.0", id=body["id"], result=result)).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def run(binary, repository, output):
    checks = []
    with tempfile.TemporaryDirectory(prefix="agent-entry-", dir=output) as directory:
        root = Path(directory)
        home, work, bin_dir = root / "home", root / "work", root / "bin"
        for path in (home, work, bin_dir, root / "runtime"):
            path.mkdir(mode=0o700)
        log = root / "invocations.jsonl"
        config = home / ".config" / "ssmcp"
        config.mkdir(parents=True)
        for name, fixture in [("codex", "codex_rpc.py"), ("claude", "cc-fake.py"),
                              ("pi", "unused"), ("opencode", "unused"), ("playwright-mcp", "unused")]:
            path = bin_dir / name
            path.write_text(fixture_program(name, repository / "tests" / "fixtures" / fixture, log))
            path.chmod(0o700)
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("SSMCP_") and key not in {
                           "DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "HYPRLAND_INSTANCE_SIGNATURE",
                           "SWAYSOCK", "SSH_AUTH_SOCK", "ENV", "BASH_ENV", "ZDOTDIR", "LD_PRELOAD"}}
        environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                           XDG_CACHE_HOME=str(home / ".cache"), XDG_DATA_HOME=str(home / ".local/share"),
                           XDG_STATE_HOME=str(home / ".local/state"), XDG_RUNTIME_DIR=str(root / "runtime"),
                           PATH=f"{bin_dir}:/usr/bin:/bin", SHELL="/bin/sh", NO_COLOR="1", TERM="dumb")
        capture_number = 0

        def cli(arguments, expected=0, env=None):
            nonlocal capture_number
            child = subprocess.Popen([str(binary), *arguments], stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     cwd=work, env=env or environment, start_new_session=True)
            try:
                stdout, stderr = child.communicate(timeout=35)
            except BaseException:
                stop(child)
                raise
            capture_number += 1
            (output / f"cli-{capture_number}.stdout").write_bytes(stdout)
            (output / f"cli-{capture_number}.stderr").write_bytes(stderr)
            if expected == 0:
                assert child.returncode == 0, stderr.decode(errors="replace")
            else:
                assert child.returncode != 0, arguments
            return stdout.decode(), stderr.decode()

        try:
            (config / "config.toml").write_text("invalid TOML would fail configuration loading [")
            bad_context = dict(environment, SSMCP_CONTEXT="does-not-exist")
            before = {str(p.relative_to(home)): p.read_bytes() for p in home.rglob("*") if p.is_file()}
            listed, _ = cli(["--list-harnesses", "--json"], env=bad_context)
            assert {p["name"] for p in json.loads(listed)["providers"]} == {
                "codex", "claude", "pi", "opencode", "ssmcp"}
            assert not log.exists()
            assert before == {str(p.relative_to(home)): p.read_bytes() for p in home.rglob("*") if p.is_file()}
            checks.append("agent_list_needs_no_login_config_probe_or_state_write")
            (config / "config.toml").write_text("[ui]\ndashboard = false\n")

            native, _ = cli(["--list-tools", "--shell", "/bin/sh"])
            assert any(t["name"] == "ssmcp.exec.start" for t in json.loads(native)["tools"])
            assert not log.exists()
            checks.append("default_startup_keeps_optional_providers_unprobed")

            extra = ["--config", "fixture=中文", "--help", "-i", "--agent", "literal"]
            codex, _ = cli(["--list-tools", "--load-harness", "codex", "--", *extra])
            assert any(t["name"] == "codex.thread_start" for t in json.loads(codex)["tools"])
            observed = records(log)
            native_server = [row for row in observed if row["argv"][:3] == ["app-server", "--listen", "stdio://"]]
            assert len(native_server) == 1 and native_server[0]["argv"][3:] == extra
            assert {row["program"] for row in observed} == {"codex"}
            assert not any(owned_alive(row) for row in observed)
            checks.append("codex_native_argv_separator_and_actual_catalog_are_preserved")

            cc_dir = root / "cc"
            cc_dir.mkdir()
            extra = ["--fixture-dir", str(cc_dir), "--model", "fixture-model", "--name", "中文", "--verbose"]
            claude, _ = cli(["--list-tools", "--load-harness=claude-code", "--", *extra])
            assert any(t["name"].startswith("claude.") for t in json.loads(claude)["tools"])
            assert json.loads((cc_dir / "argv.json").read_text()) == ["mcp", "serve", *extra]
            assert {row["program"] for row in records(log)} == {"codex", "claude"}
            assert not any(owned_alive(row) for row in records(log))
            checks.append("claude_code_alias_loads_real_mcp_fixture_and_cleans_owned_process")

            before = log.read_bytes()
            for arguments in (["--load-harness", "opencode", "--", "serve"],
                              ["--load-harness", "pi", "--", "prompt"],
                              ["--load-harness", "playwright", "--", "--headless"],
                              ["--load-harness", "ssmcp"], ["--type", "codex"]):
                cli(["--list-tools", *arguments], expected=1)
            assert log.read_bytes() == before
            checks.append("unsupported_or_legacy_entry_arguments_fail_before_provider_probe")

            _, saved_error = cli([
                "sessions", "create", "selected", "--server", "https://relay.example",
                "--load-harness", "codex", "--allow", "alice@other.example", "--",
                "--cwd", "/nonexistent-ssmcp-native-agent-dir", "--json", "--name", "native",
                "--help", "--server", "https://native.example", "--context", "native",
            ], expected=1)
            assert "allow target belongs to" in saved_error, saved_error
            assert log.read_bytes() == before
            checks.append("saved_session_controls_stop_at_native_separator_before_any_probe")

            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Peer)
            server.received = []
            server.fixture_token = "private-test-only-oauth"
            serving = threading.Thread(target=server.serve_forever, daemon=True)
            serving.start()
            try:
                peer_env = dict(environment, FIXTURE_PEER_TOKEN=server.fixture_token)
                peer_tools, peer_stderr = cli(["--list-tools", "--load-harness", "ssmcp", "--",
                    "--endpoint", f"http://127.0.0.1:{server.server_port}/fixture", "--token-env", "FIXTURE_PEER_TOKEN"], env=peer_env)
                assert {t["name"] for t in json.loads(peer_tools)["tools"] if t["name"].startswith("peer.")} == {"peer.Read", "peer.Write"}
                assert [body["method"] for body in server.received] == ["initialize", "notifications/initialized", "tools/list", "tools/list"]
                assert server.fixture_token not in peer_tools + peer_stderr
                assert log.read_bytes() == before
                assert not any(server.fixture_token.encode() in p.read_bytes() for p in home.rglob("*") if p.is_file())
            finally:
                server.shutdown()
                server.server_close()
                serving.join(timeout=2)
            checks.append("explicit_peer_cli_negotiates_authenticated_pages_without_spawning_ssmcp")

            local_cc = root / "local-cc"
            local_cc.mkdir()
            child = subprocess.Popen([str(binary), "local", "--raw", "--no-watchdog", "--load-harness", "claude-code",
                "--", "--fixture-dir", str(local_cc)], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT, env=environment, cwd=work, start_new_session=True)
            captured = bytearray()
            ready = None
            deadline = time.monotonic() + 25
            try:
                while time.monotonic() < deadline and child.poll() is None and ready is None:
                    readable, _, _ = select.select([child.stdout], [], [], 0.2)
                    if not readable:
                        continue
                    block = os.read(child.stdout.fileno(), 32768)
                    if not block:
                        break
                    captured.extend(block)
                    for line in captured.splitlines():
                        try:
                            row = json.loads(line)
                        except ValueError:
                            continue
                        if isinstance(row, dict) and row.get("client_id") and row.get("transport") == "local":
                            ready = row
                            break
                assert ready and ready["agent"] == "claude" and ready["agent_scope"] == "protocol_catalog", captured.decode(errors="replace")
                assert child.poll() is None
                received = records(local_cc / "received.jsonl")
                assert any(row.get("method") == "tools/list" for row in received)
                assert not any(row.get("method") == "tools/call" for row in received)
            finally:
                stop(child)
                (output / "local-startup.capture").write_bytes(captured)
            assert not any(owned_alive(row) for row in records(log))
            checks.append("local_connection_becomes_ready_with_selected_protocol_and_owned_cleanup")
            rpc = records(log.with_suffix(".rpc.jsonl"))
            assert not any(row["request"].get("method") in ("turn/start", "prompt") for row in rpc)
            checks.append("all_startup_paths_submit_zero_model_turns")
        finally:
            # Only fixture PIDs with their original Linux start identity are eligible.
            for row in records(log):
                if owned_alive(row):
                    os.kill(row["pid"], signal.SIGKILL)
            (output / "fixture-invocations.json").write_text(json.dumps(records(log), indent=2) + "\n")
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    binary = options.binary.resolve(strict=True)
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    repository = Path(__file__).resolve().parent.parent
    report = dict(binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), passed=False)
    try:
        report["checks"] = run(binary, repository, output)
        report["passed"] = True
    finally:
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
