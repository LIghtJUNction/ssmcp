#!/usr/bin/env python3
"""Regression checks for bounded acceptance-event consumption."""

import unittest

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


if __name__ == "__main__":
    unittest.main()
