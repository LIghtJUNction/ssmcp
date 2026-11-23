#!/usr/bin/env python3
"""Exercise the real composer journal assertion with opaque event values."""

import copy
import unittest

from e2e_composer import Composer


class Journal:
    def __init__(self, events):
        self.events = events

    def observe(self, name, arguments):
        assert name == "ssmcp.ui.message"
        assert arguments == {"text": "fresh"}
        return {"recorded": True, "storage": "memory", "message_id": "probe"}

    def live_events(self, *, limit):
        assert limit == 16
        return {"generation": "generation", "scope": {"client_id": "client"},
                "next_cursor": 20, "events": self.events}

    def check(self, condition, message):
        if not condition:
            raise AssertionError(message)


class ComposerJournalTests(unittest.TestCase):
    def setUp(self):
        self.baseline = {"generation": "generation", "scope": {"client_id": "client"},
                         "next_cursor": 10}
        values = [None, "opaque", ["opaque"], 17, True, {"type": "other"}]
        self.events = [{"sequence": 11 + index, "value": value}
                       for index, value in enumerate(values)]
        self.events.append({"sequence": 17, "value": {
            "message_id": "probe", "type": "ssmcp.message", "text": "fresh"}})

    def verify(self):
        return Composer.check_post_delivery_journal(
            Journal(self.events), self.baseline, {"discarded-body"}, marker="fresh")

    def test_mixed_event_values_keep_the_exact_native_probe(self):
        current = self.verify()
        self.assertEqual(current["events"], self.events)
        self.assertEqual(current["events"][-1]["sequence"], 17)

    def test_duplicate_probe_is_still_rejected(self):
        self.events.append(copy.deepcopy(self.events[-1]))
        with self.assertRaisesRegex(AssertionError, "matching_ids=2"):
            self.verify()

    def test_old_body_in_non_object_value_is_still_rejected(self):
        self.events[1]["value"] = "opaque discarded-body"
        with self.assertRaisesRegex(AssertionError, "excludes old bodies"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
