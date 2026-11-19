#!/usr/bin/env python3
"""Headless fetch through the existing local account relay fixture.

Uses disposable repositories, configuration and a synthetic login. Only local
file Git remotes and loopback HTTP/WebSocket are involved.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

from e2e_account_relay import Harness, wait_for
from e2e_account_gateway import direct_loopback


class FetchHarness(Harness):
    def __init__(self, directory, binary):
        git = shutil.which("git")
        if not git:
            raise RuntimeError("git is required for the local fetch fixture")
        self.git = str(Path(git).resolve())
        super().__init__(directory, binary)
        self.fetch_log = directory / "fetch-attempts.log"
        wrapper = directory / "empty-path" / "git"
        wrapper.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            f"log = {str(self.fetch_log)!r}\n"
            "if 'fetch' in sys.argv[1:]:\n"
            "    with open(log, 'a', encoding='utf-8') as stream:\n"
            "        stream.write('fetch\\n')\n"
            f"os.execv({self.git!r}, ['git', *sys.argv[1:]])\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o700)

    def env(self, device):
        result = {key: value for key, value in direct_loopback(super().env(device)).items()
                  if not key.startswith("GIT_")}
        result.update(
            GIT_CONFIG_GLOBAL=str(self.root / "no-global-config"),
            GIT_CONFIG_SYSTEM=str(self.root / "no-system-config"),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_TERMINAL_PROMPT="0",
            GIT_ALLOW_PROTOCOL="file",
            GIT_AUTHOR_NAME="fetch-fixture",
            GIT_AUTHOR_EMAIL="fetch-fixture@example.invalid",
            GIT_COMMITTER_NAME="fetch-fixture",
            GIT_COMMITTER_EMAIL="fetch-fixture@example.invalid",
        )
        return result

    def git_command(self, cwd, *args):
        result = subprocess.run(
            [self.git, *args], cwd=cwd, env=self.env("A"),
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
            timeout=10,
        )
        if result.returncode:
            raise AssertionError(self.redacted(result.stderr))
        return result.stdout.strip()

    def remote_update(self, text):
        source = self.root / "source"
        (source / "tracked").write_text(text + "\n")
        self.git_command(source, "add", "tracked")
        self.git_command(source, "commit", "-qm", text)
        self.git_command(source, "push", "-q", "origin", "main")
        return self.git_command(source, "rev-parse", "HEAD")

    def fetched_head(self):
        return self.git_command(self.root / "A", "rev-parse", "refs/remotes/origin/main")

    def fetch_count(self):
        return len(self.fetch_log.read_text().splitlines()) if self.fetch_log.exists() else 0

    def connect_headless(self, log_format):
        token = self.state.account_token
        previous = len(self.state.connections.get(token, []))
        process = self.spawn(
            "A", "--name", "periodic-fetch-fixture", "--no-setup",
            "--shell", "/bin/sh", "--no-watchdog", "--log-format", log_format,
        )
        wait_for(lambda: len(self.state.connections.get(token, [])) > previous,
                 "headless account connection")
        connection = self.state.connections[token][-1]
        connection.call("terminal.status", {})
        self.check(process.poll() is None, f"{log_format} session is connected without a TTY")
        return process

    def run(self):
        bare = self.root / "remote.git"
        source = self.root / "source"
        source.mkdir()
        self.git_command(self.root, "init", "--bare", "--initial-branch=main", str(bare))
        self.git_command(source, "init", "--initial-branch=main")
        self.git_command(source, "remote", "add", "origin", str(bare))
        initial = self.remote_update("initial")
        workspace = self.root / "A"
        self.git_command(workspace, "init", "--initial-branch=main")
        self.git_command(workspace, "remote", "add", "origin", str(bare))
        self.git_command(workspace, "fetch", "-q", "origin")
        self.git_command(workspace, "checkout", "-q", "-B", "main", "--track", "origin/main")
        self.cli("A", "login", "--server", self.state.origin, "--no-browser", "--json")
        default = self.cli("A", "config", "get", "git-fetch-interval")
        self.check(default.stdout.strip() == "300", "new configurations default to 300 seconds")
        configured = self.cli("A", "config", "set", "git-fetch-interval", "60")
        self.check("Applies to new connections" in configured.stdout,
                   "configuration reports when the setting takes effect")

        remote = self.remote_update("first-remote-update")
        first = self.connect_headless("jsonl")
        wait_for(lambda: self.fetched_head() == remote, "JSONL connection fetches its upstream")
        self.check(self.fetch_count() == 1, "JSONL connection starts exactly one initial fetch")
        self.check(self.git_command(workspace, "rev-parse", "HEAD") == initial,
                   "background fetch preserves the local branch")
        self.stop(first)

        remote = self.remote_update("second-remote-update")
        self.cli("A", "config", "set", "git-fetch-interval", "0")
        # This is the disposable fixture's schedule, never a user repository.
        schedule = workspace / ".git" / "ssmcp-fetch.json"
        schedule.unlink()
        before = self.fetch_count()
        disabled = self.connect_headless("jsonl")
        time.sleep(0.25)
        self.stop(disabled)
        self.check(self.fetch_count() == before and not schedule.exists()
                   and self.fetched_head() != remote,
                   "a new connection reads interval 0 and starts no fetch")

        self.cli("A", "config", "set", "git-fetch-interval", "60")
        plain = self.connect_headless("pretty")
        wait_for(lambda: self.fetched_head() == remote, "non-TTY pretty connection fetches its upstream")
        self.stop(plain)
        self.check(self.fetch_count() == before + 1,
                   "re-enabling fetch takes effect on a new non-TTY connection")
        self.check(self.git_command(workspace, "rev-parse", "HEAD") == initial,
                   "all headless connections preserve the local HEAD")
        self.check(not self.state.errors, "loopback gateway observed valid protocol frames")
        return {
            "status": "passed", "checks": len(self.checks),
            "transport": "loopback WebSocket", "git_remote": "disposable local bare repository",
            "formats": ["jsonl", "pretty without TTY"], "models_started": False,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ssmcp"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ssmcp-periodic-fetch-") as temporary:
        harness = FetchHarness(Path(temporary), binary)
        try:
            result = harness.run()
            if args.report:
                args.report.write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result))
        except Exception as error:
            harness.diagnostics()
            raise SystemExit(harness.redacted(f"{type(error).__name__}: {error}")) from None
        finally:
            harness.cleanup()


if __name__ == "__main__":
    main()
