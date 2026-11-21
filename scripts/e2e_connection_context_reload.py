#!/usr/bin/env python3
"""Verify named local connections stay local across an actual client reexec.

Reuse the existing private local OAuth/MCP fixture. Each case starts only its
own client, changes its private context store, reloads it through the real
public tool, and verifies the replacement. No browser, remote account, model,
installed service or existing client is used. HTTPS argv is covered by the
Rust reload tests; the explicit-context case also exercises a fake Claude
protocol over the real local MCP Unix socket before and after reload.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import selectors
import shutil
import signal
import stat
import subprocess
import tempfile
import time

from e2e import free_port
from e2e_agent_entry import fixture_program, owned_alive, records
from e2e_local import Local, process_identity, sha256
from e2e_local_socket import SocketLocal


class ContextReload(Local):
    def __init__(self, directory, binary_dir, selection):
        super().__init__(directory, binary_dir, None, False, None)
        self.selection = selection
        self.listen_port = free_port()
        self.extra_root = directory / "extra context"
        self.extra_root.mkdir(mode=0o700)
        self.cross_features = selection == "command_line"
        self.socket_path = None
        self.agent_log = directory / "agent-invocations.jsonl"
        self.agent_directory = directory / "fake-claude"
        self.native_arguments = ["--fixture-dir", str(self.agent_directory),
            "--server", "native-server", "--context", "native-context",
            "--title", "native title 中文", "--help", "--socket", "native.sock", ""]
        if self.cross_features:
            self.agent_directory.mkdir(mode=0o700)

    def client_command(self, *, name="context-reload", **_kwargs):
        selector = ["--context", "original"] if self.selection == "command_line" else []
        command = [str(self.executable), *selector, "--name", name,
                "--title", "--context", "--context-root", str(self.extra_root),
                "--listen", f"127.0.0.1:{self.listen_port}", "--shell", "/bin/sh",
                "--theme", "nord", "--no-color", "--no-watchdog", "--no-browser"]
        if self.cross_features:
            # The optional socket path must not consume the following --load-harness.
            command += ["--socket", "--load-harness", "claude-code", "--", *self.native_arguments]
        return command

    def request(self, *args, **kwargs):
        if self.cross_features:
            # Reuse the socket fixture's real HTTP transport and canonical Host.
            return SocketLocal.request(self, *args, **kwargs)
        return super().request(*args, **kwargs)

    def configure(self, environment, arguments):
        result = subprocess.run([str(self.executable), "context", *arguments],
            env=environment, cwd=self.workspace, capture_output=True, text=True, timeout=5)
        if result.returncode:
            raise AssertionError(f"private context command failed: {result.stderr}")

    def client_environment(self, label, **kwargs):
        environment = super().client_environment(label, **kwargs)
        environment.pop("SSMCP_CONTEXT", None)
        environment["SSMCP_CONTEXT_RELOAD_MARKER"] = self.selection
        for name in ("git", "node", "bwrap", "prlimit"):
            executable = shutil.which(name)
            if not executable:
                raise RuntimeError(f"local reload fixture requires installed {name}")
            target = Path(environment["PATH"]) / name
            if not target.exists():
                target.symlink_to(Path(executable).absolute())
        self.configure(environment, ["create", "original", "--local"])
        self.configure(environment, ["create", "other", "--server", "https://other.invalid"])
        self.configure(environment, ["use", "original"])
        if self.selection == "environment":
            environment["SSMCP_CONTEXT"] = "original"
        if self.cross_features:
            self.socket_path = Path(environment["XDG_RUNTIME_DIR"]) / "ssmcp/mcp.sock"
            fixture = Path(__file__).resolve().parent.parent / "tests/fixtures/cc-fake.py"
            wrapper = Path(environment["PATH"]) / "claude"
            assert not wrapper.exists(), "the isolated PATH must contain no installed Claude"
            wrapper.write_text(fixture_program("claude", fixture, self.agent_log))
            wrapper.chmod(0o700)
        self.environments[label] = environment
        return environment

    def check_agent_generation(self, credentials, generation):
        self.check(credentials.get("agent") == "claude"
                   and credentials.get("agent_scope") == "protocol_catalog",
                   "the real connection reports its selected protocol only after initialization")
        metadata = self.socket_path.lstat()
        self.check(stat.S_ISSOCK(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600
                   and metadata.st_uid == os.geteuid(),
                   "the combined agent connection serves its owned private MCP socket")
        invocations = [row for row in records(self.agent_log) if row["argv"][:2] == ["mcp", "serve"]]
        self.check(len(invocations) == generation
                   and all(row["argv"] == ["mcp", "serve", *self.native_arguments] for row in invocations),
                   "each real provider process receives the exact native arguments after --")
        self.check(owned_alive(invocations[-1])
                   and not any(owned_alive(row) for row in invocations[:-1]),
                   "reload replaces only its owned provider process and leaves the current one live")
        requests = [row["request"] for row in records(self.agent_log.with_suffix(".rpc.jsonl"))]
        self.check(sum(row.get("method") == "initialize" for row in requests) == generation
                   and sum(row.get("method") == "tools/list" for row in requests) == 2 * generation
                   and sum(row.get("method") == "tools/call" for row in requests) == generation - 1
                   and not any(row.get("method") in ("turn/start", "prompt") for row in requests),
                   "each startup performs protocol/catalog work and submits no task")
        # This is an explicit call to the fixture's real advertised native tool.
        arguments = {"marker": "cross-feature 中文", "generation": generation}
        result = self.call("claude.Dynamic1", arguments)
        self.check(result == {"native_name": "Dynamic1", "arguments": arguments},
                   "authenticated MCP over the Unix socket returns the actual selected provider result")
        argv = Path(f"/proc/{self.client.pid}/cmdline").read_bytes().split(b"\0")[:-1]
        self.check(argv[argv.index(b"--") + 1:] == [arg.encode() for arg in self.native_arguments],
                   "the live client argv retains native selector-like flags and the empty argument")

    def read_replacement(self):
        with selectors.DefaultSelector() as selector:
            selector.register(self.client.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=20):
                raise AssertionError("owned client did not report a replacement connection")
        line = self.client.stdout.readline()
        if not line:
            raise AssertionError("owned client exited instead of preserving its resolved target")
        return self.validate_connection_record(json.loads(line))

    def current_client(self):
        clients = self.public_call("list")["clients"]
        matching = [client for client in clients if client["name"] == "context-reload"
                    and client["cwd"] == str(self.workspace.resolve())]
        if len(matching) != 1:
            raise AssertionError("expected exactly the owned local context client")
        return matching[0]

    def run(self):
        self.client, credentials = self.start_client(name="context-reload", cwd=self.workspace)
        environment = self.environments["client"]
        self.oauth_login(credentials)
        initial = self.current_client()
        self.client_id = initial["id"]
        if self.cross_features:
            self.check_agent_generation(credentials, 1)
        self.check(initial.get("title") == "--context", "literal --title value is preserved at startup")
        roots = self.call("ssmcp.context.roots")["roots"]
        self.check(any(root["path"] == str(self.extra_root.resolve()) for root in roots),
                   "the explicit extra context root is granted before reload")
        pid = self.client.pid
        identity = process_identity(pid)
        self.configure(environment, ["use", "other"])
        self.configure(environment, ["rm", "original"])
        if self.selection == "command_line":
            self.configure(environment, ["create", "original", "--server", "https://edited.invalid"])
        changed_config = Path(environment["SSMCP_CLIENT_CONFIG"]).read_bytes()
        self.check(b'current = "other"' in changed_config,
                   "the private persisted default now selects a different HTTPS target")

        acknowledgement = self.call("ssmcp.client.reload", {})
        self.check(acknowledgement["accepted"], "the real native reload tool acknowledges the owned client")
        replacement = self.read_replacement()
        self.check(self.client.pid == pid and process_identity(pid) == identity,
                   "the same owned process actually reexecutes instead of being replaced by the fixture")
        self.check(self.http_port == self.listen_port,
                   "the replacement stays in local mode and preserves its explicit loopback listener")
        self.oauth_login(replacement)
        current = self.current_client()
        self.client_id = current["id"]
        if self.cross_features:
            self.check_agent_generation(replacement, 2)
        self.check(current["id"] != initial["id"], "reexec obtains a new real connection identity")
        self.check(current.get("title") == initial.get("title") and current["cwd"] == initial["cwd"],
                   "reexec preserves the literal title and launch cwd")
        self.check(self.call("ssmcp.context.roots")["roots"] == roots,
                   "reexec preserves the exact file-access roots")
        argv = Path(f"/proc/{pid}/cmdline").read_bytes().rstrip(b"\0").split(b"\0")
        self.check(argv[1] == b"local" and argv[argv.index(b"--title") + 1] == b"--context",
                   "the actual replacement argv pins local without deleting the literal selector-like title")
        inherited = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
        self.check(f"SSMCP_CONTEXT_RELOAD_MARKER={self.selection}".encode() in inherited,
                   "reload preserves the original environment")
        if self.selection == "environment":
            self.check(b"SSMCP_CONTEXT=original" in inherited,
                       "the original environment selector is retained but cannot redirect the fixed target")
        self.check(Path(environment["SSMCP_CLIENT_CONFIG"]).read_bytes() == changed_config,
                   "reload never rewrites the changed context store")
        self.check(not list(Path(environment["SSMCP_AUTH_HOME"]).iterdir()),
                   "local reload never creates or borrows a remote account credential")

    def close(self):
        try:
            super().close()
        finally:
            if self.cross_features:
                survivors = [row for row in records(self.agent_log) if owned_alive(row)]
                for row in survivors:
                    if owned_alive(row):
                        os.kill(row["pid"], signal.SIGKILL)
                self.check(not survivors, "normal client cleanup stops every owned fake provider")
                if self.socket_path is not None:
                    self.check(not self.socket_path.exists(),
                               "normal client cleanup removes the combined connection's MCP socket")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("this process regression exercises the Linux local transport")
    binary = args.bin_dir.resolve() / "ssmcp"
    if not binary.is_file():
        parser.error(f"missing {binary}; build the candidate CLI first")

    def interrupted(_signal, _frame):
        raise TimeoutError("context reload regression interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    started = time.monotonic()
    report = {"status": "failed", "scope": "actual Linux local context reload; no remote account or model",
              "binary_sha256": sha256(binary), "cases": [],
              "sources": {name: sha256(Path(__file__).with_name(name)) for name in (
                  "e2e_connection_context_reload.py", "e2e_local.py", "e2e.py", "e2e_account_gateway.py",
                  "e2e_agent_entry.py", "e2e_local_socket.py")}}
    report["sources"]["cc-fake.py"] = sha256(Path(__file__).resolve().parent.parent / "tests/fixtures/cc-fake.py")
    try:
        for selection in ("command_line", "environment", "current"):
            with tempfile.TemporaryDirectory(prefix="ssmcp-context-reload-") as directory:
                harness = ContextReload(Path(directory), args.bin_dir, selection)
                case = {"selection": selection, "status": "failed"}
                report["cases"].append(case)
                try:
                    harness.run()
                    case["status"] = "passed"
                except Exception as error:
                    description = f"{type(error).__name__}: {error}"
                    for secret in [harness.secret, *harness.sensitive]:
                        if secret:
                            description = description.replace(secret, "[redacted]")
                    case["error"] = description
                    harness.diagnostics()
                    raise RuntimeError(description) from None
                finally:
                    try:
                        harness.close()
                    except Exception:
                        case["status"] = "failed"
                        raise
                    finally:
                        case["checks"] = harness.checks
        report["status"] = "passed"
    finally:
        report["elapsed_seconds"] = round(time.monotonic() - started, 3)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Passed {len(report['cases'])} real connection-context reload cases", flush=True)


if __name__ == "__main__":
    main()
