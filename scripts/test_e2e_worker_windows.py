"""Regression controls for the authenticated worker-window directory projection."""
import json
import subprocess
import unittest
from unittest.mock import Mock

from e2e_worker_windows import CHAT_DISABLED, REPLY_UNSUPPORTED, directory_chat_enabled, directory_pair, WorkerWindows


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


class PolicyDenialTests(unittest.TestCase):
    def setUp(self):
        self.harness = WorkerWindows.__new__(WorkerWindows)
        self.participant = {"agent_id": "verified-worker", "token": "private-fixture-token"}

    def test_exact_expected_policy_exception_is_asserted_at_call_boundary(self):
        denial = {"rejected": True, "error": "native file mutation blocked by safety policy"}
        self.harness.public_call = Mock(return_value={"result": denial})
        self.assertEqual(self.harness.policy_denial(
            "owned-client", self.participant, "ssmcp.fs.mkdir", {"path": "denied"}), denial)
        args = self.harness.public_call.call_args.args[1]
        self.assertEqual(args["agent"], {"client_id": "owned-client", "id": "verified-worker",
                                         "token": "private-fixture-token"})
        self.assertIn('await call("owned-client", "ssmcp.fs.mkdir", {"path": "denied"})', args["code"])

    def test_generated_js_observes_real_rejection_and_does_not_invent_success(self):
        self.harness.public_call = Mock(return_value={"result": {
            "rejected": True, "error": "native file mutation blocked by safety policy"}})
        self.harness.policy_denial("owned-client", self.participant, "ssmcp.fs.mkdir", {})
        code = self.harness.public_call.call_args.args[1]["code"]
        runner = ("const fs=require('node:fs');const p=JSON.parse(fs.readFileSync(0,'utf8'));"
                  "const F=Object.getPrototypeOf(async function(){}).constructor;"
                  "new F('call',p.code)(async()=>{if(p.error)throw new Error(p.error);"
                  "return {ok:true};}).then(r=>process.stdout.write(JSON.stringify(r)));")
        for error in ["native file mutation blocked by safety policy", "connection lost", None]:
            with self.subTest(error=error):
                result = subprocess.run(["node", "-e", runner], input=json.dumps({"code": code, "error": error}),
                                        text=True, capture_output=True, check=True)
                observed = json.loads(result.stdout)
                self.assertEqual(observed, {"rejected": True, "error": error} if error
                                 else {"rejected": False, "response": {"ok": True}})

    def test_success_wrong_errors_and_nested_error_envelopes_cannot_pass(self):
        for result in [{"rejected": False, "response": {}},
                       {"rejected": True, "error": "connection lost"},
                       {"rejected": True, "error": "workspace is busy; retry the file operation"},
                       {"isError": True}, {}, {"rejected": 1,
                        "error": "native file mutation blocked by safety policy"}]:
            with self.subTest(result=result):
                self.harness.public_call = Mock(return_value={"result": result})
                with self.assertRaisesRegex(AssertionError, "exact native safety policy denial"):
                    self.harness.policy_denial("owned-client", self.participant, "ssmcp.fs.mkdir", {})

    def test_orchestration_failure_is_never_reclassified_as_policy_denial(self):
        self.harness.public_call = Mock(side_effect=AssertionError("runtime isolation unavailable"))
        with self.assertRaisesRegex(AssertionError, "runtime isolation unavailable"):
            self.harness.policy_denial("owned-client", self.participant, "ssmcp.fs.mkdir", {})


if __name__ == "__main__":
    unittest.main()
