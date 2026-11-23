"""Private-protocol and outcome tests; these never start an SSMCP process."""

import copy
import json
import socket
import unittest
from unittest.mock import Mock, patch

import e2e_output_expiry as expiry


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.binding = {"process_id": 123, "client_id": "fixture-client",
                        "connection_attempt": 1, "store_id": "a0b1"}
        self.expected = {
            name: {"id": letter * 32, "bytes": 1200, "sha256": letter * 64}
            for name, letter in (("target", "a"), ("fresh", "b"), ("active", "c"))
        }
        self.metadata = {
            name: {**descriptor, "expires_at_unix": 100 + index}
            for index, (name, descriptor) in enumerate(self.expected.items())
        }

    def armed(self):
        return {
            "schema_version": 1, "event": "armed", "binding": self.binding,
            **self.expected, "deferred": True, "aging_point": expiry.AGING_POINT,
            "original_expires_at_unix": {
                name: item["expires_at_unix"] for name, item in self.metadata.items()
            },
            "old_expires_at_unix": self.metadata["target"]["expires_at_unix"],
        }

    def witness(self):
        return {
            "schema_version": 1, "event": "timer_removed", "binding": self.binding,
            "origin": "timer", "removed": self.expected["target"],
            "aging_point": expiry.AGING_POINT, "old_expires_at_unix": 100,
            "timer_now_unix": 75, "new_expires_at_unix": 74,
            "controls": {
                "fresh": {"id": self.expected["fresh"]["id"], "present": True, "active": False},
                "active": {"id": self.expected["active"]["id"], "present": True, "active": True},
            },
        }

    def test_bounded_socket_frames_preserve_coalesced_responses_and_reject_partial_eof(self):
        parent, peer = socket.socketpair()
        control = expiry.Control(parent)
        try:
            first = {"schema_version": 1, "event": "ready"}
            second = {"schema_version": 1, "event": "armed"}
            peer.sendall(json.dumps(first).encode() + b"\n"
                         + json.dumps(second).encode() + b"\n"
                         + b'{"schema_version":1')
            self.assertEqual(control.receive(expiry.Deadline(1)), first)
            self.assertEqual(control.receive(expiry.Deadline(1)), second)
            peer.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                control.receive(expiry.Deadline(1))
        finally:
            control.close()
            peer.close()

    def test_invalid_or_unbounded_control_data_is_not_a_success_response(self):
        samples = [
            b'{"schema_version":1,"event":"ready","value":NaN}\n',
            b'\xff\n',
            b'{"schema_version":2,"event":"ready"}\n',
            b'{"schema_version":true,"event":"ready"}\n',
            b'[]\n',
        ]
        for sample in samples:
            with self.subTest(sample=sample):
                parent, peer = socket.socketpair()
                control = expiry.Control(parent)
                try:
                    peer.sendall(sample)
                    with self.assertRaises(AssertionError):
                        control.receive(expiry.Deadline(1))
                finally:
                    control.close()
                    peer.close()
        parent, peer = socket.socketpair()
        control = expiry.Control(parent)
        try:
            with patch.object(expiry, "FRAME_LIMIT", 32):
                peer.sendall(b"x" * 32)
                with self.assertRaisesRegex(AssertionError, "frame limit"):
                    control.receive(expiry.Deadline(1))
        finally:
            control.close()
            peer.close()

    def test_timer_witness_rejects_read_origin_other_store_and_wrong_controls(self):
        self.assertEqual(expiry._timer_removed(
            self.witness(), self.binding, self.expected, self.armed()), self.witness())
        corruptions = [
            lambda value: value.update(origin="read"),
            lambda value: value["binding"].update(store_id="ffff"),
            lambda value: value["binding"].update(client_id="another-client"),
            lambda value: value["binding"].update(connection_attempt=2),
            lambda value: value["removed"].update(bytes=1199),
            lambda value: value["removed"].update(id="d" * 32),
            lambda value: value["removed"].update(sha256="d" * 64),
            lambda value: value["controls"]["active"].update(active=False),
            lambda value: value["controls"]["fresh"].update(present=False),
            lambda value: value["controls"]["fresh"].update(present=1),
            lambda value: value["controls"]["active"].update(active=1),
        ]
        for corrupt in corruptions:
            value = copy.deepcopy(self.witness())
            corrupt(value)
            with self.subTest(value=value):
                with self.assertRaises(AssertionError):
                    expiry._timer_removed(value, self.binding, self.expected, self.armed())

    def test_ready_and_age_require_original_pid_policy_and_target_timestamp(self):
        ready = {"schema_version": 1, "event": "ready", "binding": self.binding,
                 "runtime_bound": True, "retention_secs": 900, "sweep_secs": 60}
        self.assertEqual(expiry._binding(ready, 123), self.binding)
        with self.assertRaises(AssertionError):
            expiry._binding(ready, 124)
        altered = copy.deepcopy(ready)
        altered["retention_secs"] = 1
        with self.assertRaises(AssertionError):
            expiry._binding(altered, 123)
        armed = self.armed()
        self.assertEqual(expiry._armed(armed, self.binding, self.expected, self.metadata), armed)
        changed = copy.deepcopy(armed)
        changed["old_expires_at_unix"] = 101
        with self.assertRaises(AssertionError):
            expiry._armed(changed, self.binding, self.expected, self.metadata)


    def test_arm_requires_deferred_aging_and_unchanged_original_expiries(self):
        self.assertEqual(expiry._armed(
            self.armed(), self.binding, self.expected, self.metadata), self.armed())
        corruptions = [
            lambda value: value.pop("deferred"),
            lambda value: value.update(deferred=False),
            lambda value: value.update(deferred=1),
            lambda value: value.update(aging_point="arm"),
            lambda value: value.update(new_expires_at_unix=0),
            lambda value: value.update(original_expires_at_unix={"target": 100}),
            lambda value: value["original_expires_at_unix"].update(extra=103),
            lambda value: value["original_expires_at_unix"].update(target=99),
            lambda value: value["original_expires_at_unix"].update(fresh=100),
            lambda value: value["original_expires_at_unix"].update(active=101),
            lambda value: value["original_expires_at_unix"].update(active=True),
            lambda value: value["original_expires_at_unix"].update(active=102.0),
            lambda value: value["original_expires_at_unix"].update(active=1 << 64),
        ]
        for corrupt in corruptions:
            value = copy.deepcopy(self.armed())
            corrupt(value)
            with self.subTest(value=value):
                with self.assertRaises(AssertionError):
                    expiry._armed(value, self.binding, self.expected, self.metadata)

    def test_timer_witness_requires_matching_aging_point_and_exact_u64_timestamp(self):
        zero = self.witness()
        zero.update(timer_now_unix=0, new_expires_at_unix=0)
        self.assertEqual(expiry._timer_removed(
            zero, self.binding, self.expected, self.armed()), zero)
        corruptions = [
            lambda value: value.pop("aging_point"),
            lambda value: value.update(aging_point="arm"),
            lambda value: value.update(old_expires_at_unix=101),
            lambda value: value.update(old_expires_at_unix=True),
            lambda value: value.pop("timer_now_unix"),
            lambda value: value.update(timer_now_unix=True),
            lambda value: value.update(timer_now_unix=-1),
            lambda value: value.update(timer_now_unix=75.0),
            lambda value: value.update(timer_now_unix=1 << 64),
            lambda value: value.pop("new_expires_at_unix"),
            lambda value: value.update(new_expires_at_unix=True),
            lambda value: value.update(new_expires_at_unix=-1),
            lambda value: value.update(new_expires_at_unix=73),
            lambda value: value.update(new_expires_at_unix=75),
            lambda value: value.update(new_expires_at_unix=74.0),
            lambda value: value.update(new_expires_at_unix=1 << 64),
            lambda value: value.update(timer_now_unix=101, new_expires_at_unix=100),
        ]
        for corrupt in corruptions:
            value = copy.deepcopy(self.witness())
            corrupt(value)
            with self.subTest(value=value):
                with self.assertRaises(AssertionError):
                    expiry._timer_removed(value, self.binding, self.expected, self.armed())

    def test_arm_rejects_changed_artifact_descriptors_before_timer(self):
        for name in self.expected:
            for field, replacement in (("id", "d" * 32), ("bytes", 1199),
                                       ("sha256", "d" * 64)):
                with self.subTest(name=name, field=field):
                    value = copy.deepcopy(self.armed())
                    value[name][field] = replacement
                    with self.assertRaises(AssertionError):
                        expiry._armed(value, self.binding, self.expected, self.metadata)


class OutcomeTests(unittest.TestCase):
    def test_shared_deadline_is_not_renewed_by_a_new_stage_or_request(self):
        current = [10.0]
        deadline = expiry.Deadline(75, clock=lambda: current[0])
        current[0] = 40
        self.assertEqual(deadline.remaining(), 45)
        self.assertEqual(deadline.remaining(10), 10)
        current[0] = 84.5
        self.assertEqual(deadline.remaining(10), 0.5)
        current[0] = 85
        with self.assertRaises(TimeoutError):
            deadline.remaining()

    def test_scoped_alarm_restores_original_absolute_end_handler_and_interval(self):
        current = [100.0]
        original_handler = lambda _signum, _frame: None
        deadline = expiry.Deadline(75, clock=lambda: current[0])
        with patch.object(expiry.signal, "getitimer", return_value=(140.0, 0.25)), \
                patch.object(expiry.signal, "getsignal", return_value=original_handler), \
                patch.object(expiry.signal, "signal") as set_handler, \
                patch.object(expiry.signal, "setitimer") as set_timer:
            alarm = expiry.ScenarioAlarm(deadline)
            alarm.arm()
            self.assertEqual(set_timer.call_args.args, (expiry.signal.ITIMER_REAL, 75.0))
            current[0] = 130.0
            alarm.restore(failure_pending=False)
            self.assertEqual(set_handler.call_args.args,
                             (expiry.signal.SIGALRM, original_handler))
            self.assertEqual(set_timer.call_args.args,
                             (expiry.signal.ITIMER_REAL, 110.0, 0.25))

    def test_scenario_timeout_keeps_unexpired_overall_deadline_for_cleanup(self):
        current = [100.0]
        deadline = expiry.Deadline(75, clock=lambda: current[0])
        with patch.object(expiry.signal, "getitimer", return_value=(140.0, 0.0)), \
                patch.object(expiry.signal, "getsignal", return_value=lambda *_: None), \
                patch.object(expiry.signal, "signal"), \
                patch.object(expiry.signal, "setitimer") as set_timer:
            alarm = expiry.ScenarioAlarm(deadline)
            alarm.arm()
            current[0] = 175.0
            with self.assertRaisesRegex(TimeoutError, "output expiry scenario"):
                alarm._expired(expiry.signal.SIGALRM, None)
            alarm.restore(failure_pending=True)
            self.assertEqual(alarm.observation()["timeout_origin"], "scenario")
            self.assertEqual(set_timer.call_args.args, (expiry.signal.ITIMER_REAL, 65.0, 0.0))

    def test_fired_overall_alarm_is_not_rearmed_to_interrupt_cleanup_again(self):
        current = [100.0]
        deadline = expiry.Deadline(75, clock=lambda: current[0])

        def original_handler(_signum, _frame):
            raise TimeoutError("unified acceptance exceeded its 240-second deadline")

        with patch.object(expiry.signal, "getitimer", return_value=(20.0, 0.0)), \
                patch.object(expiry.signal, "getsignal", return_value=original_handler), \
                patch.object(expiry.signal, "signal") as set_handler, \
                patch.object(expiry.signal, "setitimer") as set_timer:
            alarm = expiry.ScenarioAlarm(deadline)
            alarm.arm()
            self.assertEqual(set_timer.call_args.args, (expiry.signal.ITIMER_REAL, 20.0))
            current[0] = 120.0
            with self.assertRaisesRegex(TimeoutError, "240-second"):
                alarm._expired(expiry.signal.SIGALRM, None)
            current[0] = 121.0
            alarm.restore(failure_pending=True)
            self.assertEqual(set_handler.call_args.args,
                             (expiry.signal.SIGALRM, original_handler))
            self.assertEqual(set_timer.call_args.args, (expiry.signal.ITIMER_REAL, 0, 0.0))
            self.assertEqual(alarm.observation()["timeout_origin"], "overall")
            self.assertTrue(alarm.observation()["overall_alarm_fired"])

    def test_primary_failure_and_cleanup_failure_cannot_become_passed(self):
        evidence = {"status": "running", "cleanup": {"errors": ["stop failed"]}}
        with self.assertRaises(expiry.OutputExpiryFailure) as caught:
            expiry._finish(evidence, "AssertionError: public read returned wrong bytes")
        self.assertEqual(str(caught.exception), "AssertionError: public read returned wrong bytes")
        self.assertEqual(caught.exception.evidence["cleanup"]["errors"], ["stop failed"])
        self.assertEqual(caught.exception.evidence["status"], "failed")
        with self.assertRaises(expiry.OutputExpiryFailure):
            expiry._finish({"cleanup": {"errors": ["stop failed"]}}, None)
        self.assertEqual(expiry._finish({"cleanup": {"errors": []}}, None)["status"], "passed")

    def test_public_request_restores_the_callers_http_timeout_after_failure(self):
        class Harness:
            http_timeout = 9

        harness = Harness()
        deadline = expiry.Deadline(0.5)

        def rejected():
            self.assertGreater(harness.http_timeout, 0)
            self.assertLessEqual(harness.http_timeout, 0.5)
            raise AssertionError("public native call failed")

        with self.assertRaisesRegex(AssertionError, "public native call failed"):
            expiry._request(harness, deadline, rejected)
        self.assertEqual(harness.http_timeout, 9)

    def test_cleanup_error_frame_is_not_erased_by_a_later_zero_exit(self):
        binding = {"process_id": 123, "client_id": "fixture-client",
                   "connection_attempt": 1, "store_id": "a0b1"}

        class Process:
            code = None

            def poll(self):
                return self.code

            def wait(self, timeout):
                self.code = 0
                return 0

        class Control:
            frames = [
                {"schema_version": 1, "event": "error", "binding": binding,
                 "message": "original shutdown error"},
                {"schema_version": 1, "event": "stopped", "binding": binding,
                 "cleanup_completed": True},
            ]

            def send(self, value, deadline):
                pass

            def receive(self, deadline):
                return self.frames.pop(0)

            def close(self):
                pass

        owned = Mock()
        owned.refresh.return_value = True
        owned.snapshot.return_value = []
        owned.remaining.return_value = []
        cleanup = expiry._cleanup(Process(), Control(), binding, None, owned)
        self.assertTrue(cleanup["stopped_acknowledged"])
        self.assertEqual(cleanup["exit_code"], 0)
        self.assertTrue(cleanup["reaped"])
        self.assertTrue(cleanup["errors"])
        with self.assertRaises(expiry.OutputExpiryFailure):
            expiry._finish({"cleanup": cleanup}, None)


class OwnedProcessTests(unittest.TestCase):
    def test_pidfd_refuses_a_reused_instance_without_sending_a_signal(self):
        original = {"pid": 4242, "start_ticks": 17, "state": "S",
                    "parent_pid": 10, "session_pid": 4242}
        with patch.object(expiry, "_same_instance", side_effect=[original, None]), \
                patch.object(expiry.os, "pidfd_open", return_value=55), \
                patch.object(expiry.os, "close") as close, \
                patch.object(expiry.Path, "read_text", return_value="Pid:\t4242\n"), \
                patch.object(expiry.signal, "pidfd_send_signal") as send:
            self.assertFalse(expiry._signal_instance(original, expiry.signal.SIGKILL))
            send.assert_not_called()
            close.assert_called_once_with(55)

    def test_owned_tree_and_job_binding_keep_separate_sessions_and_reject_foreign_pid(self):
        process = Mock(pid=11)
        records = {
            11: {"pid": 11, "start_ticks": 1, "state": "S", "parent_pid": 7, "session_pid": 11},
            22: {"pid": 22, "start_ticks": 2, "state": "S", "parent_pid": 11, "session_pid": 22},
            33: {"pid": 33, "start_ticks": 3, "state": "S", "parent_pid": 22, "session_pid": 22},
            44: {"pid": 44, "start_ticks": 4, "state": "S", "parent_pid": 99, "session_pid": 44},
            55: {"pid": 55, "start_ticks": 5, "state": "S", "parent_pid": 44, "session_pid": 44},
        }
        children = {
            "/proc/11/task/11/children": "22",
            "/proc/22/task/22/children": "33",
            "/proc/33/task/33/children": "",
        }

        def glob(path, pattern):
            self.assertEqual(pattern, "*/children")
            pid = str(path).split("/")[2]
            self.assertIn(int(pid), (11, 22, 33))
            return [expiry.Path(f"/proc/{pid}/task/{pid}/children")]

        def read(path, *args, **kwargs):
            return children[str(path)]

        registry = []
        with patch.object(expiry, "_process_identity", side_effect=lambda pid: records.get(pid)), \
                patch.object(expiry.Path, "glob", glob), \
                patch.object(expiry.Path, "read_text", read):
            owned = expiry.OwnedProcesses(process, registry=registry)
            self.assertTrue(owned.refresh(expiry.Deadline(1)))
            self.assertEqual({row["pid"] for row in owned.snapshot()}, {11, 22, 33})
            bound = owned.bind_job({"job_id": "owned", "pid": 22, "pid_role": "supervisor"},
                                   33, expiry.Deadline(1))
            self.assertEqual(bound["supervisor"]["session_pid"], 22)
            self.assertEqual(bound["command"]["start_ticks"], 3)
            with self.assertRaises(AssertionError):
                owned.bind_job({"job_id": "foreign", "pid": 44, "pid_role": "supervisor"},
                               55, expiry.Deadline(1))
        self.assertEqual({row["pid"] for row in registry}, {11, 22, 33})

    def test_zero_fixture_exit_cannot_hide_an_unreaped_observed_descendant(self):
        binding = {"process_id": 11, "client_id": "fixture-client",
                   "connection_attempt": 1, "store_id": "a0b1"}
        descendant = {"pid": 33, "start_ticks": 3, "state": "S", "parent_pid": 22,
                      "session_pid": 22, "state_now": "Z"}
        process = Mock()
        process.poll.side_effect = lambda: 0 if process.wait.called else None
        process.wait.return_value = 0
        control = Mock()
        control.receive.return_value = {"schema_version": 1, "event": "stopped",
                                        "binding": binding, "cleanup_completed": True}
        owned = Mock()
        owned.refresh.return_value = True
        owned.snapshot.return_value = [descendant]
        owned.remaining.return_value = [descendant]
        owned.signal.return_value = False
        current = [0.0]

        def clock():
            current[0] += 0.1
            return current[0]

        with patch.object(expiry.time, "monotonic", side_effect=clock), \
                patch.object(expiry.time, "sleep"), \
                patch.object(expiry.signal, "pidfd_send_signal") as signal_real:
            cleanup = expiry._cleanup(process, control, binding, None, owned)
        signal_real.assert_not_called()
        self.assertEqual(cleanup["exit_code"], 0)
        self.assertTrue(cleanup["reaped"])
        self.assertTrue(cleanup["stopped_acknowledged"])
        self.assertFalse(cleanup["all_observed_instances_gone"])
        self.assertEqual(cleanup["remaining_instances"], [descendant])
        self.assertTrue(cleanup["errors"])
        with self.assertRaises(expiry.OutputExpiryFailure):
            expiry._finish({"cleanup": cleanup}, None)

    def test_successful_exact_fallback_still_preserves_the_runtime_cleanup_failure(self):
        binding = {"process_id": 11, "client_id": "fixture-client",
                   "connection_attempt": 1, "store_id": "a0b1"}
        instance = {"pid": 33, "start_ticks": 3, "state": "S", "parent_pid": 22,
                    "session_pid": 22, "state_now": "S"}
        process = Mock()
        process.poll.side_effect = lambda: 0 if process.wait.called else None
        process.wait.return_value = 0
        control = Mock()
        control.receive.return_value = {"schema_version": 1, "event": "stopped",
                                        "binding": binding, "cleanup_completed": True}
        owned = Mock()
        owned.refresh.return_value = True
        owned.snapshot.return_value = [instance]
        pending, calls = [True], []

        def signal_one(identity, signum):
            self.assertEqual((identity["pid"], identity["start_ticks"]), (33, 3))
            calls.append(signum)
            if signum == expiry.signal.SIGKILL:
                pending[0] = False
            return True

        owned.signal.side_effect = signal_one
        owned.remaining.side_effect = lambda: [instance] if pending[0] else []
        current = [0.0]

        def clock():
            current[0] += 0.1
            return current[0]

        with patch.object(expiry.time, "monotonic", side_effect=clock), \
                patch.object(expiry.time, "sleep"):
            cleanup = expiry._cleanup(process, control, binding, None, owned)
        self.assertEqual(calls, [expiry.signal.SIGTERM, expiry.signal.SIGKILL])
        self.assertTrue(cleanup["all_observed_instances_gone"])
        self.assertTrue(cleanup["forced"])
        self.assertTrue(cleanup["errors"])
        with self.assertRaises(expiry.OutputExpiryFailure):
            expiry._finish({"cleanup": cleanup}, None)


    def test_final_empty_observation_after_cleanup_deadline_is_still_a_failure(self):
        binding = {"process_id": 11, "client_id": "fixture-client",
                   "connection_attempt": 1, "store_id": "a0b1"}
        process = Mock()
        process.poll.side_effect = lambda: 0 if process.wait.called else None
        process.wait.return_value = 0
        control = Mock()
        control.receive.return_value = {"schema_version": 1, "event": "stopped",
                                        "binding": binding, "cleanup_completed": True}
        owned = Mock()
        owned.refresh.return_value = True
        owned.snapshot.return_value = []
        current, observations = [0.0], [0]

        def remaining():
            observations[0] += 1
            if observations[0] == 2:
                current[0] = 20.001
            return []

        owned.remaining.side_effect = remaining
        with patch.object(expiry.time, "monotonic", side_effect=lambda: current[0]):
            cleanup = expiry._cleanup(process, control, binding, None, owned)
        self.assertEqual(observations[0], 2)
        self.assertTrue(cleanup["all_observed_instances_gone"])
        self.assertEqual(cleanup["exit_code"], 0)
        self.assertTrue(cleanup["stopped_acknowledged"])
        self.assertEqual(cleanup["duration_seconds"], 20.001)
        self.assertIn("owned output expiry cleanup exceeded its 20-second absolute deadline",
                      cleanup["errors"])
        with self.assertRaises(expiry.OutputExpiryFailure):
            expiry._finish({"cleanup": cleanup}, None)
        with self.assertRaises(expiry.OutputExpiryFailure) as caught:
            expiry._finish({"cleanup": cleanup}, "AssertionError: original public failure")
        self.assertEqual(str(caught.exception), "AssertionError: original public failure")
        self.assertEqual(caught.exception.evidence["cleanup"]["errors"], cleanup["errors"])


if __name__ == "__main__":
    unittest.main()
