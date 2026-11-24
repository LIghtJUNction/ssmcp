"""Regression controls for the authenticated worker-window directory projection."""
import unittest

from e2e_worker_windows import CHAT_DISABLED, REPLY_UNSUPPORTED, directory_chat_enabled, directory_pair


def client(identity, reason, reply=False, cwd="/owned workspace"):
    return {"id": identity, "kind": "process", "chat_io_version": 1, "cwd": cwd,
            "unavailable_reason": reason, "reply_native_supported": reply}


class DirectoryPairTests(unittest.TestCase):
    def setUp(self):
        self.parent = client("parent", REPLY_UNSUPPORTED)
        self.worker = client("worker", CHAT_DISABLED)

    def test_current_directory_pairs_without_retired_chat_enabled_field(self):
        self.assertNotIn("chat_enabled", self.parent)
        self.assertNotIn("chat_enabled", self.worker)
        self.assertEqual(directory_pair([self.worker, self.parent], "/owned workspace"),
                         (self.parent, self.worker))
        modern_parent = client("parent", None, reply=True)
        self.assertEqual(directory_pair([modern_parent, self.worker], "/owned workspace"),
                         (modern_parent, self.worker))

    def test_exact_counts_workspace_and_exclusions_remain_required(self):
        for clients, cwd, excluded in [
            ([self.parent], "/owned workspace", ()),
            ([self.worker], "/owned workspace", ()),
            ([self.parent, self.worker, client("extra", CHAT_DISABLED)], "/owned workspace", ()),
            ([self.parent, client("second-parent", REPLY_UNSUPPORTED)], "/owned workspace", ()),
            ([self.parent, self.worker], "/other workspace", ()),
            ([self.parent, self.worker], "/owned workspace", ("worker",)),
        ]:
            with self.subTest(clients=clients, cwd=cwd, excluded=excluded):
                self.assertIsNone(directory_pair(clients, cwd, excluded))

    def test_missing_unknown_or_conflicting_roles_never_become_parents(self):
        for malformed in [
            {"id": "legacy", "cwd": "/owned workspace", "chat_enabled": True},
            {**self.parent, "unavailable_reason": "unknown"},
            {**self.parent, "reply_native_supported": 0},
            {**self.parent, "unavailable_reason": None},
            {**self.worker, "reply_native_supported": True},
            {**self.parent, "kind": "thread"},
            {**self.parent, "chat_io_version": 2},
            {key: value for key, value in self.parent.items() if key != "unavailable_reason"},
        ]:
            with self.subTest(malformed=malformed):
                self.assertIsNone(directory_chat_enabled(malformed))
                self.assertIsNone(directory_pair([malformed, self.worker], "/owned workspace"))


if __name__ == "__main__":
    unittest.main()
