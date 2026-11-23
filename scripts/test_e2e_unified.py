#!/usr/bin/env python3
"""Regression checks for bounded acceptance-event consumption."""

import unittest
from unittest.mock import Mock, patch

from e2e_output_expiry import OutputExpiryFailure

from e2e_unified import Unified


class Feed:
    def __init__(self, count, *, append=0, page_limit=8):
        self.rows = [{"cursor":i, "event":{"index":i}} for i in range(count)]
        self.append = append
        self.page_limit = page_limit
        self.calls = []

    def call(self, tool, arguments, **kwargs):
        self.calls.append((tool, arguments, kwargs))
        if len(self.calls) > 32:
            raise AssertionError("consumer followed the live tail indefinitely")
        for _ in range(self.append):
            cursor = len(self.rows)
            self.rows.append({"cursor":cursor, "event":{"index":cursor}})
        cursor = arguments["cursor"]
        batch = self.rows[cursor:cursor + min(arguments["max_events"], self.page_limit)]
        return {"events":batch, "next_cursor":cursor + len(batch),
                "end_cursor":len(self.rows), "truncated":False}


class EventSnapshotTests(unittest.TestCase):
    def test_audited_reads_stop_at_first_boundary_and_resume_without_gaps(self):
        feed = Feed(19, append=4)
        rows, cursor = Unified.events(feed, "ssmcp.audit.events", client_id="sibling", secret="fixture")
        self.assertEqual([row["cursor"] for row in rows], list(range(23)))
        self.assertEqual(cursor, 23)
        self.assertEqual(len(feed.calls), 3)
        self.assertEqual(feed.calls[-1][1]["max_events"], 7)
        self.assertTrue(all(call[2] == {"client_id":"sibling", "secret":"fixture"} for call in feed.calls))

        resumed, next_cursor = Unified.events(feed, "ssmcp.audit.events", cursor=cursor)
        self.assertEqual([row["cursor"] for row in resumed], list(range(23, 35)))
        self.assertEqual(next_cursor, 35)

    def test_short_pages_do_not_end_the_snapshot_early(self):
        # The transport may return fewer events to respect its byte budget.
        feed = Feed(17, append=2, page_limit=3)
        rows, cursor = Unified.events(feed, "ssmcp.exec.events")
        self.assertEqual([row["cursor"] for row in rows], list(range(19)))
        self.assertEqual(cursor, 19)
        self.assertEqual(len(feed.calls), 7)
        self.assertEqual(feed.calls[-1][1]["max_events"], 1)

    def test_empty_snapshot_needs_one_read(self):
        feed = Feed(0)
        self.assertEqual(Unified.events(feed, "ssmcp.fs.events"), ([], 0))
        self.assertEqual(len(feed.calls), 1)

    def test_truncation_gaps_and_invalid_boundaries_still_fail(self):
        base = {"events":[{"cursor":0, "event":{}}],
                "next_cursor":1, "end_cursor":1, "truncated":False}
        cases = [
            {"truncated":True},
            {"events":[{"cursor":1, "event":{}}]},
            {"next_cursor":2},
            {"end_cursor":0},
            {"events":[], "next_cursor":0},
        ]
        for override in cases:
            with self.subTest(override=override):
                class BrokenFeed:
                    def call(self, *_args, **_kwargs):
                        return dict(base, **override)
                with self.assertRaises(AssertionError):
                    Unified.events(BrokenFeed(), "ssmcp.audit.events")



class ExpiryIntegrationTests(unittest.TestCase):
    def harness(self):
        harness = Unified.__new__(Unified)
        harness.output_expiry_evidence = None
        harness.output_expiry_record = None
        harness.output_expiry_observed = False
        harness.save = Mock(return_value={"path": "fixture-expiry.json"})
        harness.scrub = lambda value: value
        return harness

    def evidence(self):
        return {"schema": "ssmcp.public-output-expiry.v1", "status": "passed",
                "clock_mode": "fixture_timestamp",
                "aging_point": "timer_sweep_before_expiry", "aging_deferred": True,
                "armed": {"deferred": True, "aging_point": "timer_sweep_before_expiry"},
                "public_path_expiry_observed": True, "automatic_cleanup_observed": True,
                "timer_witness": {"origin": "timer", "aging_point": "timer_sweep_before_expiry"},
                "cleanup": {"stopped_acknowledged": True, "reaped": True,
                            "forced": False, "exit_code": 0, "errors": []}}

    def test_successful_evidence_is_saved_without_changing_stage_return_contract(self):
        harness, evidence = self.harness(), self.evidence()
        with patch("e2e_unified.verify_output_expiry", return_value=evidence) as verify:
            self.assertIsNone(Unified.expiration(harness))
        verify.assert_called_once_with(harness, timeout_seconds=75.0)
        harness.save.assert_called_once_with("automatic-expiration.json", evidence)
        self.assertIs(harness.output_expiry_evidence, evidence)
        self.assertTrue(harness.output_expiry_observed)

    def test_partial_public_or_forced_cleanup_evidence_cannot_pass(self):
        incomplete = [
            {"status": "passed"},
            {**self.evidence(), "automatic_cleanup_observed": False},
            {**self.evidence(), "timer_witness": {"origin": "read"}},
            {**self.evidence(), "cleanup": {**self.evidence()["cleanup"], "forced": True}},
            {**self.evidence(), "cleanup": {**self.evidence()["cleanup"], "errors": ["residual child"]}},
        ]
        for evidence in incomplete:
            with self.subTest(evidence=evidence):
                harness = self.harness()
                with patch("e2e_unified.verify_output_expiry", return_value=evidence):
                    with self.assertRaises(AssertionError):
                        Unified.expiration(harness)
                self.assertFalse(harness.output_expiry_observed)

    def test_immediate_or_unstaged_aging_evidence_cannot_pass(self):
        evidence = self.evidence()
        cases = [
            {**evidence, "aging_deferred": False},
            {**evidence, "aging_point": "at_arm"},
            {**evidence, "armed": {**evidence["armed"], "deferred": False}},
            {**evidence, "timer_witness": {**evidence["timer_witness"], "aging_point": "at_arm"}},
            {key: value for key, value in evidence.items() if key != "aging_point"},
        ]
        for outcome in cases:
            with self.subTest(outcome=outcome):
                harness = self.harness()
                with patch("e2e_unified.verify_output_expiry", return_value=outcome):
                    with self.assertRaises(AssertionError):
                        Unified.expiration(harness)
                self.assertFalse(harness.output_expiry_observed)

    def test_failure_evidence_is_retained_and_original_error_survives_save_failure(self):
        for save_failure in (False, True):
            with self.subTest(save_failure=save_failure):
                harness = self.harness()
                evidence = {"status": "failed", "failure": "timer did not remove the exact target",
                            "cleanup": {"errors": ["owned child did not exit"]}}
                failure = OutputExpiryFailure(evidence["failure"], evidence)
                if save_failure:
                    harness.save.side_effect = OSError("evidence destination unavailable")
                with patch("e2e_unified.verify_output_expiry", side_effect=failure):
                    with self.assertRaises(OutputExpiryFailure) as caught:
                        Unified.expiration(harness)
                self.assertIs(caught.exception, failure)
                self.assertIs(harness.output_expiry_evidence, evidence)
                self.assertFalse(harness.output_expiry_observed)
                harness.save.assert_called_once_with("automatic-expiration.json", evidence)
                if save_failure:
                    self.assertIn("evidence_write_errors", evidence)


if __name__ == "__main__":
    unittest.main()
