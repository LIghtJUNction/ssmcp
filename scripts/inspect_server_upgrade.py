#!/usr/bin/env python3
"""Inspect relay activity without changing routes, state or service processes.

Exit 3 means inspection succeeded but switching remains blocked. A zero-session
snapshot is not an admission fence; this tool never authorizes a restart.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import stat
import struct
import subprocess
import sys
import tomllib

MAX_RESPONSE = 8 * 1024 * 1024


def require(ok, message):
    if not ok:
        raise ValueError(message)


def process_identity(pid):
    proc = Path("/proc") / str(pid)
    fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
    with (proc / "exe").open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return {"pid": pid, "start_ticks": int(fields[19]),
            "uid": proc.stat().st_uid, "sha256": digest}


def read_exact(stream, size):
    result = bytearray()
    while len(result) < size:
        chunk = stream.recv(size - len(result))
        require(bool(chunk), "truncated_management_response")
        result.extend(chunk)
    return bytes(result)


def snapshot(path, actor, identity):
    metadata = path.lstat()
    parent = path.parent.lstat()
    require(stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == identity["uid"]
            and metadata.st_mode & 0o077 == 0, "untrusted_management_socket")
    require(stat.S_ISDIR(parent.st_mode) and parent.st_uid in (0, identity["uid"])
            and parent.st_mode & 0o022 == 0, "untrusted_management_directory")
    original_uid = os.geteuid()
    require(original_uid in (0, identity["uid"]), "daemon_uid_or_root_required")
    try:
        if original_uid != identity["uid"]:
            os.seteuid(identity["uid"])
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(5)
            stream.connect(str(path))
            peer_pid, peer_uid, _ = struct.unpack(
                "3i", stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(peer_pid == identity["pid"] and peer_uid == identity["uid"],
                    "management_peer_does_not_match_live_daemon")
            request = json.dumps({"operation": "snapshot", "actor": actor, "scope": "all"}).encode()
            stream.sendall(struct.pack("!I", len(request)) + request)
            length = struct.unpack("!I", read_exact(stream, 4))[0]
            require(0 < length <= MAX_RESPONSE, "management_response_size_invalid")
            response = json.loads(read_exact(stream, length))
    finally:
        if os.geteuid() != original_uid:
            os.seteuid(original_uid)
    require(isinstance(response, dict) and response.get("result") == "snapshot",
            "management_snapshot_unavailable")
    return response.get("snapshot")


def decision(value):
    require(isinstance(value, dict) and value.get("is_admin") is True
            and value.get("scope") == "all", "global_admin_snapshot_required")
    sessions = value.get("sessions")
    stats = value.get("stats")
    count = stats.get("active_sessions") if isinstance(stats, dict) else None
    require(isinstance(sessions, list) and type(count) is int
            and count >= 0 and count == len(sessions), "inconsistent_session_count")
    return {"active_sessions": count, "global_admin_snapshot_verified": True,
            "status": "blocked_active_sessions" if count else "blocked_no_admission_fence",
            "admission_fence_verified": False, "switch_permitted": False,
            "allowed_next_step": "isolated_staging"}


def inspect(pid, config, expected_sha256):
    before = process_identity(pid)
    require(before["sha256"] == expected_sha256, "unexpected_live_daemon")
    original_config = config.read_bytes()
    portal = tomllib.loads(original_config.decode())["portal"]
    ids = portal.get("admin_github_ids", [])
    require(isinstance(ids, list) and ids
            and all(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]*", value)
                    for value in ids), "registered_numeric_admin_configuration_required")
    state = Path(portal["state_dir"])
    endpoint = Path(portal["gateway_socket"])
    require(state.is_absolute() and endpoint.is_absolute(), "absolute_portal_paths_required")
    accounts = json.loads((state / "accounts.json").read_bytes())["accounts"]
    require(isinstance(accounts, dict) and all(isinstance(value, dict) for value in accounts.values()),
            "invalid_account_registry")
    actors = [{"provider": "github", "subject": account["subject"],
               "login": account["github_login"]}
              for account in accounts.values()
              if account.get("subject") in ids and account.get("github_login")
              and not account.get("banned", False)]
    require(bool(actors), "registered_administrator_unavailable")
    result = decision(snapshot(endpoint, actors[0], before))
    require(process_identity(pid) == before and config.read_bytes() == original_config,
            "live_process_or_configuration_changed_during_inspection")
    result.update(schema="ssmcp.upgrade-inspection.v1", live_pid=pid,
                  daemon_sha256=before["sha256"], same_process_verified=True,
                  reason="The current daemon has no verified atomic admission fence or drain lease.")
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group()
    target.add_argument("--pid", type=int)
    target.add_argument("--service", default="ssmcpd.service")
    parser.add_argument("--config", type=Path, default=Path("/etc/ssmcpd/ssmcpd.toml"))
    parser.add_argument("--expected-sha256", required=True)
    args = parser.parse_args(argv)
    try:
        require(re.fullmatch(r"[0-9a-f]{64}", args.expected_sha256), "expected_sha256_required")
        pid = args.pid
        if pid is None:
            require(re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", args.service), "invalid_service")
            pid = int(subprocess.check_output(
                ["systemctl", "show", args.service, "--property=MainPID", "--value"],
                text=True, timeout=5).strip())
        require(pid > 1, "live_daemon_pid_required")
        result = inspect(pid, args.config, args.expected_sha256)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        # Never print configuration, account records, tokens or raw IPC responses.
        print(json.dumps({"schema": "ssmcp.upgrade-inspection.v1",
                          "status": "inspection_failed", "switch_permitted": False,
                          "failure_type": type(error).__name__}))
        return 2
    print(json.dumps(result, sort_keys=True))
    return 3


if __name__ == "__main__":
    sys.exit(main())
