"""Account identity regression controls; no clients or external accounts are used."""

import copy
import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace

from e2e_account_gateway import AccountGateway, IDENTITY, validate_account_identity


class AccountIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.ledger_path = self.directory / "accounts" / "accounts.json"
        self.ledger_path.parent.mkdir()
        self.alias = "member-0123456789abcdef"
        self.ledger = {"accounts": {"123": {
            "subject": "123", "github_login": "fixture", "username": "gh-123",
            "handle": "relayfixture", "handle_aliases": [self.alias],
        }}}
        self.write_ledger()
        self.harness = SimpleNamespace(identity=dict(IDENTITY), check=self.check)
        self.identity = {**IDENTITY, "username_aliases": [self.alias], "username_generated": False}

    @staticmethod
    def check(condition, description):
        if not condition:
            raise AssertionError(description)

    def write_ledger(self):
        self.ledger_path.write_text(json.dumps(self.ledger), encoding="utf-8")

    def test_current_response_passes_but_old_full_dict_comparison_fails(self):
        self.assertNotEqual(self.identity, IDENTITY)
        self.assertEqual(validate_account_identity(self.harness, self.directory, self.identity), self.identity)
        gateway = AccountGateway(self.harness)
        gateway.directory = self.directory
        self.assertEqual(gateway.validate_account_identity(self.identity), self.identity)

    def test_wrong_identity_or_claims_never_pass(self):
        mutations = [
            {"provider": "other"}, {"subject": "124"}, {"login": "otherfixture"},
            {"principal": "otherfixture@github"}, {"username": "otherrelayfixture"},
            {"username_aliases": []}, {"username_aliases": ["member-fedcba9876543210"]},
            {"username_generated": True}, {"username_generated": 0}, {"extra": "unexpected"},
        ]
        responses = [IDENTITY, None] + [{**self.identity, **mutation} for mutation in mutations]
        for response in responses:
            with self.subTest(response=response), self.assertRaises(AssertionError):
                validate_account_identity(self.harness, self.directory, response)

    def test_private_ledger_must_match_independent_fixture_identity(self):
        for field, value in [("subject", "124"), ("github_login", "otherfixture"),
                             ("username", "gh-124"), ("handle", "otherrelayfixture"),
                             ("handle_aliases", []), ("generated_handle", True)]:
            with self.subTest(field=field):
                ledger = copy.deepcopy(self.ledger)
                ledger["accounts"]["123"][field] = value
                self.ledger_path.write_text(json.dumps(ledger), encoding="utf-8")
                with self.assertRaises(AssertionError):
                    validate_account_identity(self.harness, self.directory, self.identity)


if __name__ == "__main__":
    unittest.main()
