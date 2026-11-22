"""Shared real-router account transport for isolated end-to-end fixtures.

Only the upstream GitHub provider is synthetic. The compiled Rust test fixture
uses the production Broker, HTTP router, account ledger, OAuth and WebSocket
implementations. No installed account, SSH entry point, or model is used.
This module deliberately imports no scenario modules.
"""

import base64
import hashlib
from html.parser import HTMLParser
import http.client
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import time
from urllib.parse import parse_qs, urlencode, urlsplit


FIXTURE_TEST = "http::relay_composer_fixture::relay_composer_server_fixture"
FIXTURE_ENV = "SSMCP_E2E_FIXTURE_BINARY"
PROXY_VARIABLES = {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
IDENTITY = {"provider": "github", "subject": "123", "login": "fixture",
            "principal": "fixture@github", "username": "relayfixture"}


def has_platform_consent(page, origin, identity):
    # The test router's public origin is literal loopback HTTP. Its canonical
    # account address retains that scheme, unlike an HTTPS relay address.
    platform_address = f"{identity['username']}@{origin}"
    return (isinstance(page, str) and f"<strong>{platform_address}</strong>" in page
            and "@github" not in page
            and "name=decision value=allow" in page
            and "name=decision value=deny" in page)


def direct_loopback(environment):
    result = {key: value for key, value in environment.items()
              if key.lower() not in PROXY_VARIABLES}
    result["NO_PROXY"] = "127.0.0.1,localhost,::1"
    return result


def private_json(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.fchmod(descriptor, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False)
        stream.write("\n")


class Form(HTMLParser):
    def __init__(self):
        super().__init__()
        self.fields = {}
        self.visible = []
        self.links = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "input" and attrs.get("name"):
            self.fields[attrs["name"]] = attrs.get("value", "")
            if attrs.get("type") != "hidden":
                self.visible.append(attrs["name"])
        if tag == "a" and attrs.get("href"):
            self.links.append(attrs["href"])


class AccountGateway:
    def __init__(self, harness):
        self.harness = harness
        self.process = None
        self.directory = None
        self.exit_code = None
        self.pending_auth_homes = []

    @property
    def prepared(self):
        return self.process is not None

    def prepare(self):
        h = self.harness
        if self.prepared:
            raise RuntimeError("the owned account gateway was already prepared")
        if not (h.binary_dir / "ssmcp").is_file():
            raise RuntimeError(f"missing client executable {h.binary_dir / 'ssmcp'}")
        supplied = getattr(h, "fixture_binary", None) or os.environ.get(FIXTURE_ENV)
        if not supplied:
            raise RuntimeError(
                f"set {FIXTURE_ENV} to the already-built library test executable "
                f"containing {FIXTURE_TEST}; this fixture never invokes cargo"
            )
        binary = Path(supplied).resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError("the account-router fixture must be an executable file")
        h.fixture_binary = binary
        self.directory = h.directory.resolve() / "account-gateway"
        self.directory.mkdir(mode=0o700)
        inherited = getattr(h, "fixture_environment", os.environ)
        environment = direct_loopback({key: value for key, value in inherited.items()
                                       if not key.startswith("SSMCP_")})
        environment["SSMCP_RELAY_COMPOSER_FIXTURE"] = str(self.directory)
        lifetime = getattr(h, "fixture_timeout_seconds", None)
        if lifetime is not None:
            if type(lifetime) is not int or not 30 <= lifetime <= 1800:
                raise ValueError("fixture_timeout_seconds must be an integer in 30..=1800")
            environment["SSMCP_RELAY_COMPOSER_FIXTURE_TIMEOUT_SECS"] = str(lifetime)
        self.process = h.spawn("account-router", [
            str(binary), "--exact", FIXTURE_TEST, "--ignored", "--nocapture",
        ], env=environment)
        ready = self.directory / "ready.json"
        deadline = time.monotonic() + 12
        while not ready.is_file():
            if self.process.poll() is not None:
                raise AssertionError("the real account router exited before its private ready record")
            if time.monotonic() >= deadline:
                raise AssertionError("the real account router did not publish its ready record")
            time.sleep(0.02)
        h.check(stat.S_IMODE(ready.stat().st_mode) == 0o600,
                "the owned account router keeps its credentials in a private ready record")
        data = json.loads(ready.read_text())
        if data.get("schema_version") != 1:
            raise AssertionError("unsupported account-router fixture ready schema")
        h.origin = data["server_url"]
        h.issuer = data["issuer"]
        h.github_origin = data["github_test_origin"]
        h.identity = data["identity"]
        h.account_token = data["account_token"]
        h.account_expires_at = data["account_expires_at"]
        h.sensitive.append(h.account_token)
        address = urlsplit(h.origin)
        if (address.scheme != "http" or address.hostname != "127.0.0.1" or not address.port
                or address.path or address.query or address.fragment or address.username):
            raise AssertionError("the account-router fixture must bind an explicit loopback HTTP origin")
        h.http_port = address.port
        h.user = "mcp"
        h.check(h.identity == IDENTITY,
                "the isolated account has the expected stable provider identity and handle")
        status, result, _ = h.request("GET", path="/healthz", secret="")
        h.check(status == 200 and result.strip() == "ok", "the real production account router is healthy")
        for root in self.pending_auth_homes:
            self.write_login(root)
        self.pending_auth_homes.clear()

    def validate_account_identity(self, identity):
        """Validate this freshly seeded fixture's one alias and explicit handle."""
        h = self.harness
        ledger = json.loads((self.directory / "accounts" / "accounts.json").read_text(encoding="utf-8"))
        h.check(isinstance(ledger, dict) and isinstance(ledger.get("accounts"), dict),
                "the isolated account fixture has a valid private ledger")
        account = ledger["accounts"].get(h.identity["subject"])
        h.check(isinstance(account, dict)
                and account.get("subject") == h.identity["subject"]
                and account.get("github_login") == h.identity["login"]
                and account.get("username") == "gh-" + h.identity["subject"]
                and account.get("handle") == h.identity["username"],
                "the seeded account preserves its verified subject, login, internal ID and current handle")
        aliases = account.get("handle_aliases")
        # Serde omits generated_handle when false; the HTTP claim is always present.
        generated = account.get("generated_handle", False)
        h.check(isinstance(aliases, list) and len(aliases) == 1
                and isinstance(aliases[0], str)
                and re.fullmatch(r"member-[0-9a-f]{16}", aliases[0]) is not None
                and type(generated) is bool and generated is False,
                "the fresh fixture retains its generated name as one alias after the explicit claim")
        expected = {**h.identity, "username_aliases": aliases, "username_generated": generated}
        h.check(isinstance(identity, dict) and type(identity.get("username_generated")) is bool
                and identity == expected,
                "the authenticated account API returns the exact identity and claims stored in the private ledger")
        return identity

    def seed_login(self, environment):
        """Write only this fixture's real account credential into this child home."""
        root = Path(environment["SSMCP_AUTH_HOME"])
        if not self.prepared:
            if root not in self.pending_auth_homes:
                self.pending_auth_homes.append(root)
            return environment
        self.write_login(root)
        return environment

    def write_login(self, root):
        h = self.harness
        private_json(root / (hashlib.sha256(h.origin.encode()).hexdigest() + ".json"), {
            "server": h.origin, "token": h.account_token,
            "principal": h.identity["principal"], "subject": h.identity["subject"],
            "username": h.identity["username"], "expires_at": h.account_expires_at,
        })
        selected = root / "default"
        with selected.open("x", encoding="utf-8") as stream:
            os.chmod(selected, 0o600)
            stream.write(h.origin)

    def client_command(self, *, name="workstation", shell="/bin/sh", extra_args=()):
        h = self.harness
        if not getattr(h, "origin", ""):
            raise RuntimeError("prepare the owned account router before creating a client")
        return [str(h.binary_dir / "ssmcp"), "--server", h.origin, "--name", name,
                "--shell", shell, "--no-setup", "--reconnect-delay", "1", *extra_args]

    def validate_connection_record(self, value):
        if (not isinstance(value, dict) or not value.get("client_id")
                or value.get("login_code") or value.get("secret")):
            raise AssertionError("account relay must return a client identity without a connection login secret")
        return value

    def authenticate_mcp(self, credentials, *, attacks=False):
        """Real discovery, DCR, independent provider/consumer PKCE and consent."""
        h = self.harness
        self.validate_connection_record(credentials)
        resource = h.issuer + "/mcp"
        if "url" in credentials:
            h.check(credentials["url"] == resource,
                    "the account connection advertises its exact identity-scoped MCP resource")
        else:
            # Saved session status exposes public connection fields only.
            # Bind its real ID to this fixture account without inventing a URL.
            status, sessions, _ = h.request(
                "GET", path="/api/client/sessions", secret=h.account_token)
            h.check(status == 200 and isinstance(sessions, dict)
                    and any(client.get("id") == credentials["client_id"]
                            for client in sessions.get("clients", [])),
                    "the saved connection belongs to the authenticated account session list")
        status, _, challenge = h.request(body={}, secret="")
        metadata_url = h.issuer + "/.well-known/oauth-protected-resource/mcp"
        h.check(status == 401 and metadata_url in challenge.get("www-authenticate", ""),
                "MCP 401 advertises protected-resource metadata")
        status, metadata, _ = h.oauth_request(urlsplit(metadata_url).path)
        h.check(status == 200 and metadata["resource"] == resource
                and metadata["authorization_servers"] == [h.issuer],
                "resource metadata binds the exact account endpoint and OAuth issuer")
        status, server, _ = h.oauth_request("/.well-known/oauth-authorization-server")
        h.check(status == 200 and server["issuer"] == h.issuer
                and "S256" in server["code_challenge_methods_supported"],
                "OAuth discovery advertises authorization-code PKCE S256")
        if h.oauth_client is None:
            status, registered, _ = h.oauth_request(urlsplit(server["registration_endpoint"]).path, "POST", {
                "client_name": "ssmcp account end-to-end", "redirect_uris": ["http://127.0.0.1:37401/callback"],
                "token_endpoint_auth_method": "none", "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"], "application_type": "native",
            })
            h.check(status == 201 and registered.get("client_id"), "public MCP client registers through OAuth DCR")
            h.oauth_client = registered
        client_id = h.oauth_client["client_id"]
        redirect = h.oauth_client["redirect_uris"][0]
        verifier = base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode()
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
        state = base64.urlsafe_b64encode(os.urandom(18)).decode()
        authorize = urlsplit(server["authorization_endpoint"]).path
        token_path = urlsplit(server["token_endpoint"]).path
        params = {"client_id": client_id, "redirect_uri": redirect, "response_type": "code", "scope": "mcp",
                  "resource": resource, "state": state, "code_challenge": challenge, "code_challenge_method": "S256"}
        if attacks:
            status, _, headers = h.oauth_request(authorize + "?" + urlencode(dict(params, redirect_uri="https://attacker.example/callback")))
            h.check(status == 400 and "location" not in headers, "OAuth never redirects to an unregistered callback")
            status, _, headers = h.oauth_request(authorize + "?" + urlencode(dict(params, code_challenge_method="plain")))
            returned = parse_qs(urlsplit(headers.get("location", "")).query)
            h.check(status >= 400 or bool(returned.get("error")), "OAuth rejects a PKCE downgrade to plain")
        status, page, headers = h.oauth_request(authorize + "?" + urlencode(params), headers={"Accept": "text/html"})
        form = Form()
        if isinstance(page, str):
            form.feed(page)
        h.check(status == 200 and "transaction_id" in form.fields and "login_code" not in form.fields,
                "the account OAuth endpoint requires provider consent instead of a connection login code")
        cookie_header = headers.get("set-cookie", "")
        h.check(all(flag in cookie_header.lower() for flag in ("secure", "httponly", "samesite=lax")),
                "the OAuth browser binding is Secure, HttpOnly and SameSite Lax")
        h.check("no-store" in headers.get("cache-control", "") and headers.get("referrer-policy") == "same-origin",
                "OAuth pages cannot be cached and bind same-origin form submission")
        cookie = cookie_header.split(";", 1)[0]
        transaction = form.fields["transaction_id"]
        h.sensitive.extend([cookie, transaction, verifier])
        origin = {"Origin": h.issuer, "Accept": "text/html"}
        action = {"transaction_id": transaction, "provider": "github"}
        if attacks:
            status, _, headers = h.oauth_request(authorize, "POST", action, headers=origin, form=True)
            h.check(status in (400, 403) and "location" not in headers,
                    "provider login rejects a missing browser binding cookie")
            status, _, headers = h.oauth_request(authorize, "POST", action, cookie=cookie,
                headers={**origin, "Origin": "https://attacker.example"}, form=True)
            h.check(status in (400, 403) and "location" not in headers,
                    "provider login rejects a cross-origin form submission")
        status, page, _ = h.oauth_request(authorize, "POST", action, cookie=cookie, headers=origin, form=True)
        navigation = Form()
        if isinstance(page, str):
            navigation.feed(page)
        targets = [link for link in navigation.links
                   if link.startswith(h.github_origin + "/login/oauth/authorize?")]
        if status != 200 or len(targets) != 1:
            raise AssertionError("OAuth did not return its isolated GitHub provider navigation")
        upstream = urlsplit(h.github_origin)
        if upstream.scheme != "http" or upstream.hostname != "127.0.0.1" or not upstream.port:
            raise AssertionError("the synthetic GitHub provider must be an owned loopback endpoint")
        target = urlsplit(targets[0])
        provider = http.client.HTTPConnection("127.0.0.1", upstream.port, timeout=5)
        try:
            provider.request("GET", target.path + "?" + target.query)
            response = provider.getresponse()
            callback = response.getheader("Location", "")
            response.read(8192)
            if response.status not in (302, 303):
                raise AssertionError("the synthetic provider did not issue a one-use authorization code")
        finally:
            provider.close()
        parsed = urlsplit(callback)
        if callback.split("?", 1)[0] != h.issuer + "/oauth/callback/github":
            raise AssertionError("the provider callback differs from the real router issuer")
        h.sensitive.extend(value for values in parse_qs(parsed.query).values() for value in values)
        status, consent, _ = h.oauth_request(parsed.path + "?" + parsed.query, cookie=cookie)
        h.check(status == 200 and has_platform_consent(consent, h.origin, h.identity),
                "real identity exchange validates provider PKCE and requires explicit consent")
        status, _, headers = h.oauth_request(authorize, "POST",
            {"transaction_id": transaction, "decision": "allow"}, cookie=cookie, headers=origin, form=True)
        callback = urlsplit(headers.get("location", ""))
        query = parse_qs(callback.query)
        h.check(status == 303 and callback._replace(query="").geturl() == redirect
                and query.get("state") == [state] and query.get("iss") == [h.issuer],
                "OAuth consent preserves the registered callback, state and issuer")
        code = query["code"][0]
        h.sensitive.append(code)
        exchange = {"grant_type": "authorization_code", "client_id": client_id, "code": code,
                    "code_verifier": verifier, "redirect_uri": redirect, "resource": resource}
        if attacks:
            status, result, _ = h.oauth_request(token_path, "POST", {**exchange, "code_verifier": "x" * 43}, form=True)
            h.check(status == 400 and result.get("error") == "invalid_grant",
                    "authorization code rejects an incorrect PKCE verifier")
            status, result, _ = h.oauth_request(token_path, "POST", {**exchange, "resource": h.issuer + "/different-user"}, form=True)
            h.check(status == 400 and result.get("error"), "authorization code cannot be exchanged for another resource")
        status, tokens, token_headers = h.oauth_request(token_path, "POST", exchange, form=True)
        h.check(status == 200 and tokens.get("token_type", "").lower() == "bearer",
                "the real OAuth endpoint issues an identity-scoped MCP access token")
        h.check(tokens.get("expires_in") == 28800 and tokens.get("refresh_token"),
                "OAuth access expires in eight hours and supplies a refresh token")
        h.check("no-store" in token_headers.get("cache-control", ""), "OAuth token responses cannot be cached")
        h.secret = tokens["access_token"]
        h.sensitive.extend([tokens["access_token"], tokens["refresh_token"]])
        h.oauth_grant = {"tokens": tokens, "exchange": exchange, "token_path": token_path,
                         "authorize": authorize, "authorize_params": params}
        h.check(h.rpc("ping", modern=True, secret=h.account_token)[0] == 401,
                "the account relay credential cannot authenticate as an MCP access token")
        return h.oauth_grant

    def disconnect_client(self, client_id, timeout=5):
        """Request one owned WSS close; the acknowledgment is not reconnection proof."""
        if (not isinstance(client_id, str) or len(client_id) != 64
                or any(character not in "0123456789abcdef" for character in client_id)):
            raise ValueError("disconnect_client requires an exact 64-character hexadecimal client ID")
        if not isinstance(timeout, (int, float)) or not 0 < timeout <= 60:
            raise ValueError("disconnect timeout must be greater than zero and at most 60 seconds")
        if not self.prepared or self.process.poll() is not None:
            raise RuntimeError("the owned account fixture is not running")
        request = self.directory / "disconnect.json"
        acknowledgment = self.directory / "disconnect-ack.json"
        lock = self.directory / "disconnect-inflight"
        try:
            descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError as error:
            raise RuntimeError("another owned disconnect request is still awaiting acknowledgment") from error
        os.close(descriptor)
        nonce = os.urandom(16).hex()
        temporary = self.directory / f"disconnect-{nonce}.tmp"
        try:
            if request.exists():
                raise RuntimeError("an earlier owned disconnect request has not been consumed")
            acknowledgment.unlink(missing_ok=True)
            private_json(temporary, {"schema_version": 1, "nonce": nonce, "client_id": client_id})
            os.replace(temporary, request)
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise AssertionError("the account fixture exited before acknowledging the disconnect request")
                if acknowledgment.is_file():
                    result = json.loads(acknowledgment.read_text())
                    if (result.get("schema_version") != 1 or result.get("nonce") != nonce
                            or result.get("client_id") != client_id or result.get("accepted") is not True):
                        raise AssertionError("the account fixture rejected or mismatched the exact disconnect request")
                    return result
                time.sleep(0.02)
            raise TimeoutError("owned disconnect acknowledgment was not confirmed; inspect the client before retrying")
        finally:
            temporary.unlink(missing_ok=True)
            lock.unlink(missing_ok=True)

    def stop(self):
        if self.process is None:
            return
        if self.process.poll() is None:
            (self.directory / "stop").touch()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.harness.stop(self.process)
        self.exit_code = self.process.returncode
        self.harness.fixture_exit_code = self.exit_code
