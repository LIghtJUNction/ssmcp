"""Read-only server inspection must never turn an observation into a switch lease."""
import json
import os
from pathlib import Path
import socket
import struct
import tempfile
import threading
import unittest

import inspect_server_upgrade as probe


class InspectionTests(unittest.TestCase):
    def test_live_and_empty_global_snapshots_both_block_switching(self):
        for count in (0, 2):
            result = probe.decision({"is_admin": True, "scope": "all",
                                     "sessions": [{}] * count,
                                     "stats": {"active_sessions": count}})
            self.assertFalse(result["switch_permitted"])
            self.assertFalse(result["admission_fence_verified"])
            self.assertEqual(result["allowed_next_step"], "isolated_staging")

    def test_partial_or_inconsistent_snapshots_fail_closed(self):
        valid = {"is_admin": True, "scope": "all", "sessions": [],
                 "stats": {"active_sessions": 0}}
        for changes in ({"is_admin": False}, {"scope": "own"},
                        {"stats": {"active_sessions": True}},
                        {"stats": {"active_sessions": 1}},
                        {"sessions": None}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                probe.decision({**valid, **changes})

    def exchange(self, payload, *, peer_pid=None, size=None):
        with tempfile.TemporaryDirectory(prefix="ssmcp-upgrade-probe-") as temporary:
            endpoint = Path(temporary) / "management.sock"
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
                server.bind(str(endpoint))
                endpoint.chmod(0o600)
                server.listen(1)
                server.settimeout(5)
                requests = []
                def serve():
                    with server.accept()[0] as connection:
                        connection.settimeout(5)
                        header = connection.recv(4)
                        if not header:
                            return
                        request = json.loads(probe.read_exact(connection, struct.unpack("!I", header)[0]))
                        requests.append(request)
                        data = json.dumps(payload).encode()
                        connection.sendall(struct.pack("!I", size if size is not None else len(data)) + data)
                worker = threading.Thread(target=serve, daemon=True)
                worker.start()
                try:
                    result = probe.snapshot(endpoint, {"provider": "github", "subject": "123", "login": "fixture"},
                                            {"pid": peer_pid or os.getpid(), "uid": os.geteuid()})
                finally:
                    worker.join(6)
                    self.assertFalse(worker.is_alive())
                self.assertEqual(requests[0]["operation"], "snapshot")
                self.assertEqual(requests[0]["scope"], "all")
                return result

    def test_real_private_socket_requests_global_snapshot(self):
        snapshot = {"is_admin": True, "scope": "all", "sessions": [],
                    "stats": {"active_sessions": 0}}
        self.assertEqual(self.exchange({"result": "snapshot", "snapshot": snapshot}), snapshot)

    def test_socket_peer_must_be_the_pinned_daemon_process(self):
        with self.assertRaisesRegex(ValueError, "management_peer"):
            self.exchange({}, peer_pid=os.getpid() + 1000000)

    def test_oversized_or_truncated_frames_are_rejected(self):
        for size in (0, probe.MAX_RESPONSE + 1, 100):
            with self.subTest(size=size), self.assertRaises(ValueError):
                self.exchange({}, size=size)


if __name__ == "__main__":
    unittest.main()
