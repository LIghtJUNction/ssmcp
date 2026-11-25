#!/usr/bin/env python3
"""Connect an independent MCP caller to the local harness CLI with real OAuth."""
import argparse
import re
import shutil
import tempfile
from pathlib import Path

from e2e import free_port, wait_for
from e2e_local import Local


def verify(binary_dir):
    with tempfile.TemporaryDirectory(prefix="ssmcp-local-harness-") as work:
        harness = Local(Path(work), binary_dir, None, False, None)
        try:
            env = harness.client_environment("harness")
            env.update(harness.safe_git_env)
            for utility in ("node", "bwrap", "prlimit"):
                executable = shutil.which(utility)
                if not executable:
                    raise RuntimeError(f"local harness acceptance requires {utility}")
                (Path(env["PATH"]) / utility).symlink_to(Path(executable).absolute())
            port = free_port()
            process = harness.spawn("local", [
                str(harness.executable), "mcp", "--listen", f"127.0.0.1:{port}",
                "--input", "pipe", "--name", "local-harness", "--no-watchdog", "--shell", "/bin/sh",
            ], cwd=harness.workspace, env=env)

            def credentials():
                if process.poll() is not None:
                    raise AssertionError("local MCP exited before registration")
                output = (harness.directory / "local.log").read_text()
                url = re.search(r"Native MCP endpoint: (http://[^\s]+)", output)
                code = re.search(r"Local OAuth login: ([a-f0-9]{64})\b", output)
                if url and code:
                    return {"url": url[1], "login_code": code[1]}

            login = harness.validate_connection_record(wait_for(credentials, timeout=20))
            harness.check(harness.http_port == port, "MCP uses the selected install port")
            harness.check(harness.request(body={}, secret="")[0] == 401, "local MCP requires authorization")
            harness.oauth_login(login)
            clients = harness.public_call("list")["clients"]
            assert len(clients) == 1 and clients[0]["name"] == "local-harness"
            harness.client_id = clients[0]["id"]
            now = harness.call("ssmcp.time.now")
            harness.check(now["unix_ms"] > 0, "local code_mode reaches the registered native client")
        finally:
            harness.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    verify(parser.parse_args().bin_dir)
