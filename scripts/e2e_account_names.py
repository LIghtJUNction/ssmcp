#!/usr/bin/env python3
"""Exercise preseeded account rename and old-name login through real HTTP routes.

Uses the existing private Rust router fixture and its owned loopback GitHub
provider. No client CLI, new-account generation, cache or restart is exercised.
An already-built fixture is required; this script never invokes Cargo.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import time

from e2e import Harness
from e2e_account_gateway import PortalPages


CONTRACT_SOURCE = "edf171d27b0d8400f0129af6542001e7c085a210"


class AccountNames(PortalPages, Harness):
    def accounts(self):
        path = self.account_gateway.directory / "accounts" / "accounts.json"
        value = json.loads(path.read_text(encoding="utf-8"))
        accounts = value.get("accounts") if isinstance(value, dict) else None
        if not isinstance(accounts, dict) or set(accounts) != {"123", "124"}:
            raise AssertionError("the private fixture no longer contains its two seeded accounts")
        return accounts

    def verify_identity(self, value, account):
        aliases = account.get("handle_aliases", [])
        generated = account.get("generated_handle", False)
        self.check(account.get("subject") == self.identity["subject"]
                   and account.get("username") == "gh-" + self.identity["subject"]
                   and account.get("github_login") == self.identity["login"]
                   and isinstance(aliases, list) and type(generated) is bool and not generated,
                   "the ledger retains the original subject, internal ID and explicit account")
        expected = {**self.identity, "username": account["handle"],
                    "username_aliases": aliases, "username_generated": generated}
        self.check(isinstance(value, dict) and type(value.get("username_generated")) is bool
                   and value == expected,
                   "the authenticated full identity matches the current private account ledger")

    def whoami(self, token):
        status, body, _ = self.request("GET", path="/api/client/whoami", secret=token)
        self.check(status == 200 and isinstance(body, dict) and isinstance(body.get("identity"), dict),
                   "the real account token authenticates through whoami")
        return body["identity"]

    def run(self):
        self.http_timeout = 5
        self.fixture_timeout_seconds = 120
        self.cookie = None
        self.account_gateway.prepare()
        self.loopback_origin(self.origin)
        self.loopback_origin(self.github_origin)
        self.account_gateway.validate_account_identity(self.whoami(self.account_token))
        initial = self.accounts()
        owner = initial[self.identity["subject"]]
        other = initial["124"]
        other_aliases = other.get("handle_aliases")
        self.check(other.get("subject") == "124" and other.get("username") == "gh-124"
                   and other.get("handle") == "otherrelayfixture"
                   and isinstance(other_aliases, list) and len(other_aliases) == 1
                   and isinstance(other_aliases[0], str)
                   and re.fullmatch(r"member-[0-9a-f]{16}", other_aliases[0]) is not None,
                   "the foreign account has an actual reserved prior name in the private ledger")
        old_name = owner["handle"]
        new_name = old_name + "-renamed"
        self.check(all(new_name != account.get("handle")
                       and new_name not in account.get("handle_aliases", [])
                       for account in initial.values()), "the new platform name is unused in the private fixture")
        self.login_portal()
        form = self.home(old_name)
        month_before = datetime.now(timezone.utc).strftime("%Y-%m")
        status, _, headers = self.portal("/account/username", "POST",
                                         {"csrf": form.fields["csrf"], "username": new_name})
        month_after = datetime.now(timezone.utc).strftime("%Y-%m")
        self.check(status == 303 and headers.get("location") == "/#account",
                   "the authenticated browser renames its own canonical account")
        form = self.home(new_name)
        renamed = self.accounts()
        usage = renamed[self.identity["subject"]].get("username_renames")
        self.check(usage in ({"period": month_before, "count": 1},
                             {"period": month_after, "count": 1}),
                   "the first successful rename persists one use in the current UTC month")
        expected_owner = {**owner, "handle": new_name,
                          "username_renames": usage,
                          "handle_aliases": sorted(set(owner["handle_aliases"]) | {old_name})}
        self.check(renamed[self.identity["subject"]] == expected_owner and renamed["124"] == other,
                   "rename updates the owner's handle and quota while retaining prior names")
        current = self.whoami(self.account_token)
        self.verify_identity(current, renamed[self.identity["subject"]])
        status, _, _ = self.portal("/account/username", "POST",
                                  {"csrf": form.fields["csrf"], "username": other_aliases[0]})
        self.check(status == 409 and self.accounts() == renamed,
                   "a foreign account's old alias remains reserved and a rejected rename changes nothing")
        self.home(new_name)
        self.check(self.whoami(self.account_token) == current,
                   "the original account token and canonical identity survive the rejected rename")

        status, grant, _ = self.request("POST", path="/api/client/login/start", secret="",
                                        body={"name": "account-names-http", "username": old_name})
        if isinstance(grant, dict):
            self.remember(grant.get("device_code"), grant.get("user_code"))
        self.check(status == 200 and isinstance(grant, dict)
                   and grant.get("username") == old_name and grant.get("username_available") is False
                   and isinstance(grant.get("device_code"), str) and bool(grant["device_code"])
                   and isinstance(grant.get("user_code"), str) and bool(grant["user_code"]),
                   "an old reserved name still starts a real device-login grant")
        setup, query = self.owned_url(grant.get("verification_uri_complete", ""), self.origin, "/setup")
        self.check(query == {"code": [grant["user_code"]]},
                   "the device grant points to its exact owned setup page and public code")
        status, page, _ = self.portal(setup.path + "?" + setup.query)
        self.check(status == 200, "the existing portal session opens the real device consent page")
        form = self.page_form(page)
        self.check(form.fields.get("code") == grant["user_code"]
                   and 'action="/setup/authorize"' in page
                   and f"<p>客户端请求使用用户名：<strong>{old_name}</strong>" in page,
                   "the consent form contains the actual old-name grant and CSRF token")
        status, _, headers = self.portal("/setup/authorize", "POST", {
            "csrf": form.fields["csrf"], "code": form.fields["code"], "decision": "allow"})
        self.check(status == 303, "the owning browser authorizes the old-name device grant")
        _, query = self.owned_url(self.origin + headers.get("location", ""), self.origin, "/setup")
        self.check(query == {"code": [grant["user_code"]]},
                   "consent returns to the same owned device grant")
        status, login, _ = self.request("POST", path="/api/client/login/poll", secret="",
                                        body={"device_code": grant["device_code"]})
        if isinstance(login, dict):
            self.remember(login.get("access_token"))
        self.check(status == 200 and isinstance(login, dict) and login.get("status") == "authorized"
                   and isinstance(login.get("access_token"), str) and bool(login["access_token"])
                   and login["access_token"] != self.account_token,
                   "the first poll issues a new real account token after browser consent")
        self.verify_identity(login.get("identity"), renamed[self.identity["subject"]])
        self.check(self.whoami(login["access_token"]) == current and self.accounts() == renamed,
                   "the old-name grant returns the same subject and canonical identity without renaming again")


def redacted(harness, value):
    for secret in [harness.secret, *harness.sensitive]:
        if secret:
            value = value.replace(secret, "[redacted]")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--fixture-binary", type=Path, default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"))
    parser.add_argument("--report", type=Path, help="write a credential-free JSON report")
    args = parser.parse_args()
    started = time.monotonic()
    report = {"status": "failed", "contract_source_commit": CONTRACT_SOURCE,
              "scope": "preseeded accounts: browser rename, reserved alias and device-grant HTTP chain",
              "transport": "production Broker/router fixture + owned loopback HTTPS",
              "synthetic_github_provider": True, "tls_verified": False,
              "real_github_account": False, "client_cli_verified": False,
              "new_account_generation_verified": False, "restart_verified": False,
              "models_started": False}
    with tempfile.TemporaryDirectory(prefix="ssmcp-account-names-") as directory:
        harness = AccountNames(Path(directory), args.bin_dir)
        harness.fixture_binary = args.fixture_binary
        failure = None
        try:
            harness.run()
        except (Exception, KeyboardInterrupt) as error:
            failure = redacted(harness, f"{type(error).__name__}: {error}")
        finally:
            try:
                harness.cleanup()
            except Exception as error:
                detail = redacted(harness, f"cleanup {type(error).__name__}: {error}")
                failure = detail if failure is None else failure + "; " + detail
        report.update(tls_verified=harness.account_gateway.tls_verified, checks=harness.checks, fixture_exit_code=getattr(harness, "fixture_exit_code", None),
                      duration_seconds=round(time.monotonic() - started, 3))
        if harness.account_gateway.prepared:
            try:
                digest = hashlib.sha256()
                with Path(harness.fixture_binary).open("rb") as stream:
                    for block in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(block)
                report["fixture_binary_sha256"] = digest.hexdigest()
            except Exception as error:
                detail = redacted(harness, f"fixture hash {type(error).__name__}: {error}")
                failure = detail if failure is None else failure + "; " + detail
        if failure is None and report["fixture_exit_code"] == 0:
            report["status"] = "passed"
        else:
            report["error"] = failure or "the owned fixture did not exit successfully"
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
