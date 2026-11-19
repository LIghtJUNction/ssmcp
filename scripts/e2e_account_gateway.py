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
import stat
import subprocess
import time
from urllib.parse import parse_qs, urlencode, urlsplit


FIXTURE_TEST = "http::relay_composer_fixture::relay_composer_server_fixture"
FIXTURE_ENV = "SSMCP_E2E_FIXTURE_BINARY"
PROXY_VARIABLES = {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
IDENTITY = {"provider": "github", "subject": "123", "login": "fixture",
            "principal": "fixture@github", "username": "relayfixture"}


def direct_loopback(environment):
    result = {key: value for key, value in environment.items()
              if key.lower() not in PROXY_VARIABLES}
    result["NO_PROXY"] = "127.0.0.1,localhost,::1"
    return result


def private_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        os.chmod(path, 0o600)
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
        environment = direct_loopback({key: value for key, value in os.environ.items()
                                       if not key.startswith("SSMCP_")})
        environment["SSMCP_RELAY_COMPOSER_FIXTURE"] = str(self.directory)
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
        h.check(credentials.get("url") == resource,
                "the account connection advertises its exact identity-scoped MCP resource")
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
        h.check(status == 200 and isinstance(consent, str)
                and "name=decision value=allow" in consent and "fixture@github" in consent,
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
