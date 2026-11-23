#!/usr/bin/env python3
"""Exercise the real account relay, composer queue, and local reexec in a PTY.

Requires the client, vt_snapshot, and an already-built library test executable:
  python3 scripts/e2e_relay_composer.py --bin-dir target/debug \
    --fixture-binary target/debug/deps/ssmcp-HASH --report /tmp/relay-composer.json

The ignored Rust fixture hosts the production Broker and HTTP router inside the
test executable. Only its upstream GitHub provider is synthetic; OAuth PKCE,
account authentication, WebSocket frames, queue CAS, and code_mode responses use
the real implementations. All transport is loopback HTTP/WS. This is not a TLS
or real GitHub account test, and no installed model or live session is used.
"""

import argparse
import base64
import fcntl
import hashlib
from html.parser import HTMLParser
import http.client
import json
import os
from pathlib import Path
import platform
import re
import secrets
import shutil
import signal
import stat
import struct
import subprocess
import tempfile
import termios
import time
from urllib.parse import parse_qs, urlencode, urlsplit

from e2e_account_gateway import has_platform_consent
from e2e_account_relay import IDENTITY, wait_for
from e2e_composer import Composer, ENTER, LEFT, UP, editing_queue, input_text, visible_record
from e2e_client_ui import empty_composer_ready
from e2e_dashboard import Decoder, Frontend, sha256, text


FIXTURE_TEST = "http::relay_composer_fixture::relay_composer_server_fixture"
PROXY_VARIABLES = ("http_proxy", "https_proxy", "all_proxy", "no_proxy")


def direct_loopback(environment):
    result = {key: value for key, value in environment.items()
              if key.lower() not in PROXY_VARIABLES}
    result["NO_PROXY"] = "127.0.0.1,localhost,::1"
    return result


class Form(HTMLParser):
    def __init__(self):
        super().__init__()
        self.fields = {}
        self.links = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "input" and attrs.get("name"):
            self.fields[attrs["name"]] = attrs.get("value", "")
        if tag == "a" and attrs.get("href"):
            self.links.append(attrs["href"])


def private_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        os.chmod(path, 0o600)
        json.dump(value, stream, ensure_ascii=False)
        stream.write("\n")


class RelayFrontend(Frontend):
    """Reuse the physical terminal/decoder, with an account relay entry point."""

    def __init__(self, harness, label, config, decoder, *, rows=58, cols=106):
        self.harness = harness
        self.decoder = Decoder(decoder)
        self.master, self.slave = os.openpty()
        self.original_termios = termios.tcgetattr(self.slave)
        self.transcript = bytearray()
        self.stdout = bytearray()
        self.closed = False
        self.decoder.request(op="resize", rows=rows, cols=cols)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(self.master, False)
        environment = direct_loopback(harness.client_environment(label))
        git = shutil.which("git")
        if not git:
            raise RuntimeError("relay composer E2E requires an already-installed Git")
        supplied_git = Path(environment["PATH"]) / "git"
        if not supplied_git.is_symlink() or supplied_git.resolve(strict=True) != Path(git).resolve(strict=True):
            raise RuntimeError("relay composer E2E requires the shared fixture Git identity")
        environment.update(harness.safe_git_env)
        environment.update({
            "SSMCP_UI_CONFIG": str(config), "TERM": "xterm-256color",
            "NO_COLOR": "1", "COLUMNS": str(cols), "LINES": str(rows),
        })
        root = Path(environment["SSMCP_AUTH_HOME"])
        private_json(root / (hashlib.sha256(harness.origin.encode()).hexdigest() + ".json"), {
            "server": harness.origin, "token": harness.account_token,
            "principal": harness.identity["principal"], "subject": harness.identity["subject"],
            "username": harness.identity["username"], "expires_at": harness.account_expires_at,
        })
        selected = root / "default"
        selected.write_text(harness.origin)
        selected.chmod(0o600)

        def own_controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        self.process = subprocess.Popen(
            [str(harness.binary_dir / "ssmcp"), "--server", harness.origin,
             "--name", label, "--shell", "/bin/sh", "--no-setup", "--reconnect-delay", "1"],
            stdin=self.slave, stdout=subprocess.PIPE, stderr=self.slave,
            cwd=harness.workspace, env=environment, preexec_fn=own_controlling_terminal,
        )
        harness.processes.append(self.process)
        os.set_blocking(self.process.stdout.fileno(), False)
        try:
            self.credentials = harness.connection_record(self, "initial account relay connection")
        except BaseException:
            self.close()
            raise
        self.client_id = self.credentials["client_id"]


class RelayComposer(Composer):
    def __init__(self, directory, binary_dir, decoder, fixture_binary):
        super().__init__(directory, binary_dir, decoder)
        self.fixture_binary = fixture_binary.resolve()
        self.fixture_directory = directory / "server"
        self.fixture_process = None
        self.fixture_exit_code = None
        self.origin = ""
        self.issuer = ""
        self.identity = {}
        self.account_token = ""
        self.account_expires_at = 0
        self.user = "mcp"
        # These are the actual HOME/XDG locations of this fixture's child, not
        # aliases for host state. Reexec inherits the same private directories.
        self.private_environment = {
            "HOME": str(directory / "home"),
            "XDG_STATE_HOME": str(directory / "state"),
            "XDG_CONFIG_HOME": str(directory / "config"),
            "XDG_DATA_HOME": str(directory / "data"),
            "XDG_CACHE_HOME": str(directory / "cache"),
            "XDG_RUNTIME_DIR": str(directory / "runtime"),
        }
        for path in self.private_environment.values():
            Path(path).mkdir(mode=0o700)
        self.state_home = Path(self.private_environment["XDG_STATE_HOME"])

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        environment.update(self.private_environment)
        return environment

    def assert_current_context(self, value):
        retired = getattr(self, "retired_context", ())
        if retired:
            encoded = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)
            if any(marker in encoded for marker in retired):
                raise AssertionError("the new relay exposed retired fixture text or a retired message/context identity")

    def rpc(self, method, params=None, **options):
        status, body, headers = super().rpc(method, params, **options)
        if method == "tools/call" and isinstance(params, dict) and params.get("name") == "code_mode":
            # Cover UI projections and event/artifact reads as well as work().
            # Composer.rpc separately records the actual user-message receipts.
            self.assert_current_context(body)
        return status, body, headers

    def live_events(self, **options):
        page = super().live_events(**options)
        if getattr(self, "retired_context", ()):
            self.assert_current_context(page)
            self.check(page["generation"] != self.retired_generation
                       and page["scope"]["client_id"] == self.client_id
                       and all(event.get("source") is None
                               or event["source"].get("client_id") in (None, self.client_id)
                               for event in page["events"]),
                       "every post-reload event snapshot has the new process scope and no old-client provenance")
        return page

    def seed_legacy_state(self):
        conversations = self.state_home / "ssmcp" / "conversations"
        memory = self.state_home / "ssmcp" / "context-memory"
        self.check(not any(path.is_file() for root in (conversations, memory)
                           for path in root.rglob("*")),
                   "ordinary relay work creates no conversation archives or memory checkpoints")
        legacy_id = "conversation_" + "a" * 32
        message_id = "message_" + "b" * 32
        context_id = "context_" + "c" * 32
        workspace = str(self.workspace.resolve())
        markers = ("RELAY_LEGACY_CONVERSATION_MUST_NOT_LOAD",
                   legacy_id,
                   "RELAY_LEGACY_CHECKPOINT_MUST_NOT_LOAD")
        rows = [
            {"type": "session", "schema": "ssmcp.conversation.v1", "schema_version": 1,
             "id": legacy_id, "workspace": workspace, "created_at_ms": 1,
             "truncated_messages": 0},
            {"type": "message", "id": message_id, "role": "assistant",
             "kind": "agent_message", "content": markers[0], "created_at_ms": 2,
             "truncated": False},
        ]
        # All bodies are synthetic. The JSONL and archive index follow their
        # historical formats; the checkpoint follows Document/Branch/Source
        # and Store::at in the retired context_memory.rs, including the real
        # workspace SHA-256 key. No real saved user history is copied here.
        workspace_key = hashlib.sha256(workspace.encode()).hexdigest()
        for directory in (self.state_home / "ssmcp", conversations, memory, memory / workspace_key):
            directory.mkdir(mode=0o700, exist_ok=True)
            directory.chmod(0o700)
        checkpoint = {
            "schema": "ssmcp.context-memory.v1", "schema_version": 1,
            "workspace": workspace, "workspace_id": workspace_key, "revision": 1,
            "active_id": context_id,
            "branches": [{
                "id": context_id, "name": "synthetic fixture", "parent_id": None,
                "revision": 1, "created_at_ms": 3, "updated_at_ms": 3,
                "checkpoint": {
                    "mode": "provided_summary", "text": markers[2], "created_at_ms": 3,
                    "sources": [{"conversation_id": legacy_id, "message_id": message_id,
                                 "sha256": hashlib.sha256(markers[0].encode()).hexdigest()}],
                    "omitted_sources": 0,
                },
                "recent": [], "omitted_recent": 0,
            }],
        }
        contents = {
            conversations / (legacy_id + ".jsonl"):
                ("\n".join(json.dumps(row) for row in rows) + "\n").encode(),
            conversations / "archives.json":
                (json.dumps({"schema_version": 1, "conversations": [legacy_id]}) + "\n").encode(),
            memory / workspace_key / "state.json":
                (json.dumps(checkpoint) + "\n").encode(),
        }
        records = {}
        for path, raw in contents.items():
            with path.open("xb") as stream:
                os.fchmod(stream.fileno(), 0o600)
                stream.write(raw)
            metadata = path.stat()
            records[path] = {"bytes": raw,
                             "identity": (metadata.st_dev, metadata.st_ino, metadata.st_mtime_ns)}
        return records, markers, {"conversation_id": legacy_id, "message_id": message_id,
                                  "context_id": context_id}

    def assert_legacy_state_unchanged(self, records):
        for path, saved in records.items():
            metadata = path.stat()
            self.check(not path.is_symlink() and path.read_bytes() == saved["bytes"]
                       and (metadata.st_dev, metadata.st_ino, metadata.st_mtime_ns) == saved["identity"],
                       "the synthetic legacy file is neither rewritten nor replaced: "
                       + str(path.relative_to(self.state_home)))
        actual = {path for namespace in ("conversations", "context-memory")
                  for path in (self.state_home / "ssmcp" / namespace).rglob("*") if path.is_file()}
        self.check(actual == set(records),
                   "reload and new work add no automatic archive or checkpoint files")

    def prepare(self):
        self.fixture_directory.mkdir(mode=0o700)
        environment = direct_loopback({key: value for key, value in os.environ.items()
                                       if not key.startswith("SSMCP_")})
        environment["SSMCP_RELAY_COMPOSER_FIXTURE"] = str(self.fixture_directory)
        self.fixture_process = self.spawn("relay-router", [
            str(self.fixture_binary), "--exact", FIXTURE_TEST, "--ignored", "--nocapture",
        ], env=environment)
        ready = self.fixture_directory / "ready.json"

        def started():
            if self.fixture_process.poll() is not None:
                raise AssertionError("real router fixture exited before its private ready record")
            return ready.is_file()

        wait_for(started, "real test-router ready record", timeout=12)
        self.check(stat.S_IMODE(ready.stat().st_mode) == 0o600,
                   "the owned router fixture exposes credentials only in a private ready file")
        data = json.loads(ready.read_text())
        if data.get("schema_version") != 1:
            raise AssertionError("unsupported relay fixture ready schema")
        self.origin = data["server_url"]
        self.issuer = data["issuer"]
        self.github_origin = data["github_test_origin"]
        self.identity = data["identity"]
        self.account_token = data["account_token"]
        self.account_expires_at = data["account_expires_at"]
        self.sensitive.append(self.account_token)
        address = urlsplit(self.origin)
        if (address.scheme != "http" or address.hostname != "127.0.0.1" or not address.port
                or address.path or address.query or address.fragment or address.username):
            raise AssertionError("router fixture must bind an explicit loopback HTTP origin")
        self.http_port = address.port
        self.check(self.identity == (IDENTITY | {"username": "relayfixture"}),
                   "the isolated named account has the expected stable identity and handle")
        status, result, _ = self.request("GET", path="/healthz", secret="")
        self.check(status == 200 and result.strip() == "ok", "the real production router is healthy")

    def frontend(self, label, config):
        front = RelayFrontend(self, label, config, self.decoder)
        self.frontends.append(front)
        self.last_frontend = front
        self.client_id = front.client_id
        return front

    def connection_record(self, front, description):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            front.pump(0.05)
            if front.process.poll() is not None:
                raise AssertionError(f"client exited during {description}\n{front.failure_detail()}")
            if b"\n" not in front.stdout:
                continue
            line, _, rest = front.stdout.partition(b"\n")
            front.stdout = bytearray(rest)
            value = json.loads(line)
            if not value.get("client_id") or value.get("login_code"):
                raise AssertionError("account relay must emit an identity connection record without a login code")
            return value
        raise AssertionError(f"timed out during {description}\n{front.failure_detail()}")

    def account(self, path):
        status, body, _ = self.request("GET", path=path, secret=self.account_token)
        if status != 200:
            raise AssertionError(f"owned account endpoint failed: {path}, HTTP {status}")
        return body

    def authenticate_mcp(self):
        """Real DCR, independent consumer/provider PKCE, and explicit consent."""
        status, registered, _ = self.oauth_request("/oauth/register", "POST", {
            "client_name": "relay composer E2E", "redirect_uris": ["http://127.0.0.1:37402/callback"],
            "token_endpoint_auth_method": "none", "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"], "application_type": "native",
        })
        self.check(status == 201 and registered.get("client_id"), "the real router registers an MCP OAuth client")
        verifier = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
        state = secrets.token_hex(16)
        redirect = registered["redirect_uris"][0]
        resource = self.issuer + "/mcp"
        parameters = {"client_id": registered["client_id"], "redirect_uri": redirect,
                      "response_type": "code", "scope": "mcp", "resource": resource,
                      "state": state, "code_challenge": challenge, "code_challenge_method": "S256"}
        status, page, headers = self.oauth_request("/oauth/authorize?" + urlencode(parameters))
        form = Form()
        if isinstance(page, str):
            form.feed(page)
        self.check(status == 200 and "transaction_id" in form.fields
                   and "login_code" not in form.fields,
                   "the shared identity endpoint requires provider consent instead of a connection login code")
        cookie = headers.get("set-cookie", "").split(";", 1)[0]
        if not cookie:
            raise AssertionError("OAuth consent has no browser-binding cookie")
        transaction = form.fields["transaction_id"]
        self.sensitive.extend([cookie, transaction, verifier])
        action = {"transaction_id": transaction, "provider": "github"}
        status, page, _ = self.oauth_request("/oauth/authorize", "POST", action, cookie=cookie,
                                           headers={"Origin": self.issuer}, form=True)
        navigation = Form()
        if isinstance(page, str):
            navigation.feed(page)
        targets = [link for link in navigation.links
                   if link.startswith(self.github_origin + "/login/oauth/authorize?")]
        if status != 200 or len(targets) != 1:
            raise AssertionError("OAuth did not return its test-only GitHub provider navigation")
        target = urlsplit(targets[0])
        upstream = urlsplit(self.github_origin)
        if upstream.scheme != "http" or upstream.hostname != "127.0.0.1" or not upstream.port:
            raise AssertionError("synthetic GitHub provider is not an owned loopback endpoint")
        provider = http.client.HTTPConnection("127.0.0.1", upstream.port, timeout=5)
        try:
            provider.request("GET", target.path + "?" + target.query)
            response = provider.getresponse()
            callback = response.getheader("Location", "")
            response.read(8192)
            if response.status not in (302, 303):
                raise AssertionError("synthetic provider did not issue a one-use authorization code")
        finally:
            provider.close()
        parsed = urlsplit(callback)
        if callback.split("?", 1)[0] != self.issuer + "/oauth/callback/github":
            raise AssertionError("provider callback differs from the real router's issuer")
        self.sensitive.extend(value for values in parse_qs(parsed.query).values() for value in values)
        status, consent, _ = self.oauth_request(parsed.path + "?" + parsed.query, cookie=cookie)
        self.check(status == 200 and has_platform_consent(consent, self.origin, self.identity),
                   "the real identity exchange validates synthetic GitHub PKCE and requires explicit consent")
        status, _, headers = self.oauth_request("/oauth/authorize", "POST",
            {"transaction_id": transaction, "decision": "allow"}, cookie=cookie,
            headers={"Origin": self.issuer}, form=True)
        granted = urlsplit(headers.get("location", ""))
        query = parse_qs(granted.query)
        self.check(status == 303 and granted._replace(query="").geturl() == redirect
                   and query.get("state") == [state] and query.get("iss") == [self.issuer],
                   "OAuth consent returns the registered consumer callback with state and issuer binding")
        code = query["code"][0]
        self.sensitive.append(code)
        status, tokens, _ = self.oauth_request("/oauth/token", "POST", {
            "grant_type": "authorization_code", "client_id": registered["client_id"],
            "code": code, "code_verifier": verifier, "redirect_uri": redirect, "resource": resource,
        }, form=True)
        self.check(status == 200 and tokens.get("access_token", "").startswith("at_"),
                   "the real OAuth endpoint issues an identity-scoped MCP access token")
        self.secret = tokens["access_token"]
        self.sensitive.extend([tokens["access_token"], tokens.get("refresh_token", "")])
        self.check(self.rpc("ping", modern=True, secret=self.account_token)[0] == 401,
                   "the account relay credential is not mistaken for an MCP access token")

    def focus_input(self, front):
        # When focus already belongs to the composer, no Escape is needed.
        # Otherwise observe the actual transition, including both decoders.
        if self.ui("/display/conversation")["focus"] == "input":
            return
        front.send(b"\x1b")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.ui("/display/conversation")["focus"] == "input":
                return
        raise AssertionError("Escape did not return input focus to the relay composer")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.private_environment, **self.safe_git_env},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.frontend("relay-composer", self.config("relay-composer"))
        front.until(empty_composer_ready,
                    "the named-account chat composer over a real WebSocket relay")
        whoami = self.account("/api/client/whoami")["identity"]
        self.check(whoami == self.identity, "the client relay belongs to the isolated named account")
        self.check(any(item["id"] == self.client_id for item in self.account("/api/client/sessions")["clients"]),
                   "the real account session API sees the outbound WebSocket client")
        self.authenticate_mcp()
        self.check(any(item["id"] == self.client_id for item in self.public_call("list")["clients"]),
                   "the independently authorized MCP identity sees the same account relay session")
        self.command(front, "/queue")
        initial_queue = self.queue()
        self.check(initial_queue["supported"], "the real WebSocket relay negotiated the v7 composer protocol")
        initial_queue_target = initial_queue["draft_context"]["target"]
        initial_composition = self.ui("/display/conversation")
        initial_routing = {key: initial_composition[key] for key in ("target", "native_recipient")}

        first = self.submit(front, "touch relay-composer-hidden-shell-leak")
        self.check(not (self.workspace / "relay-composer-hidden-shell-leak").exists(),
                   "typed account-session requirements do not execute in a hidden shell")
        self.check(self.record(self.queue(), message_id=first["id"])["status"] == "queued",
                   "UI-only code_mode observations leave the relay request pending")
        self.expect_delivery(self.work(), first)
        self.wait_delivered(front, first)
        self.check(not self.messages(self.work()), "a delivered relay requirement is not sent twice")

        editable = self.submit(front, "RELAY_COMPOSER_EDIT_ORIGINAL")
        front.send(UP)
        front.until(lambda shot: editing_queue(shot, "next caller")
                                 and editable["text"] in input_text(shot),
                    "server-confirmed relay takeback before editing")
        self.check(self.record(self.queue(), message_id=editable["id"])["status"] == "editing",
                   "real relay takeback places the queued identity into editing state")
        self.check(not self.messages(self.work()), "an editing relay requirement is withheld at the actual response boundary")
        revised = "RELAY_COMPOSER_EDIT_REVISED 界"
        front.send(b"\x01\x0b" + revised.encode() + ENTER)
        self.wait_submitted(front, revised)
        self.activate_column(front, "queue")
        front.until(lambda shot: visible_record(shot, revised, "queued", editable["id"]),
                    "revised relay requirement acknowledged under the same identity")
        accepted = self.record(self.queue(), message_id=editable["id"])
        self.check(accepted["text"] == revised and accepted["revision"] > editable["revision"],
                   "republishing through WebSocket preserves the ID and advances the queue revision")
        self.expect_delivery(self.work(), editable, revised)
        self.wait_delivered(front, editable, revised)

        old_reply = "RELAY_COMPOSER_OLD_RAM_REPLY 界"
        reply_receipt = self.observe("ssmcp.ui.message", {"text": old_reply, "kind": "progress"})
        before_events = self.live_events()
        self.check(reply_receipt["recorded"] is True and reply_receipt["storage"] == "memory"
                   and sum(event.get("value", {}).get("message_id") == reply_receipt["message_id"]
                           for event in before_events["events"]) == 1,
                   "the pre-reload reply belongs to one real event in the current process generation")
        legacy_records, legacy_markers, legacy_ids = self.seed_legacy_state()

        # After this last submission, use only non-consuming UI observations
        # until real reexec. Native work or artifact reads could claim it.
        pending = self.submit(front, "RELAY_COMPOSER_PENDING_MUST_NOT_REPLAY")
        parked = "RELAY_COMPOSER_UNSENT_DRAFT 界"
        self.focus_input(front)
        front.send(parked.encode() + LEFT * 2)
        front.until(lambda shot: parked in input_text(shot), "an unsent relay draft before reexec")
        before = self.queue()
        cursor = before["draft"]["cursor"]
        self.check(before["draft"]["text"] == parked
                   and cursor == len(parked.encode()) - len(" 界".encode()),
                   "the live draft retains its exact interior UTF-8 cursor before parking")
        self.select_request(front, first["id"])
        front.send(b"r")
        front.until(lambda shot: first["text"] in input_text(shot), "a delivered history copy before relay reload")
        copied = self.queue()
        self.check(copied["draft_context"]["parked"]["text"] == before["draft"]["text"]
                   and copied["draft_context"]["parked"]["cursor"] == before["draft"]["cursor"],
                   "copying a delivered request preserves the unrelated live draft and cursor")
        front.send(b"\x1b")
        front.until(lambda shot: parked in input_text(shot), "Escape restores the draft within the same process")
        escaped = self.queue()
        self.check(escaped["draft"] == before["draft"]
                   and escaped["draft_context"]["parked"] is None
                   and escaped["draft_context"]["target"] == "next",
                   "same-process navigation preserves the exact draft, cursor and recipient")
        self.select_request(front, first["id"])
        front.send(b"r")
        front.until(lambda shot: first["text"] in input_text(shot), "a fresh parked copy before real reload")
        waiting = [row for row in self.queue()["queue"] if row["status"] in ("queued", "sending")]
        self.check(len(waiting) == 1 and waiting[0]["id"] == pending["id"]
                   and waiting[0]["text"] == pending["text"] and waiting[0]["target"] == "next"
                   and waiting[0]["status"] == "queued",
                   "one exact next-caller request remains unclaimed immediately before reexec")
        old_texts = (first["text"], editable["text"], revised, pending["text"], parked,
                     old_reply, *legacy_markers)
        # These are message/request/context identities, not the event_N journal
        # projection IDs whose sequence numbers may restart in a new generation.
        # If a receipt ever uses that local namespace too, its old identity is
        # (generation, id); the new generation may legitimately reuse the id.
        reusable_reply_id = re.fullmatch(r"event_[0-9]+", reply_receipt["message_id"]) is not None
        old_ids = (first["id"], editable["id"], pending["id"],
                   *(() if reusable_reply_id else (reply_receipt["message_id"],)),
                   *legacy_ids.values())
        old_context = tuple(dict.fromkeys((*old_texts, *old_ids)))
        old_id, old_pid, old_token = self.client_id, front.process.pid, self.secret
        self.check(first["room_id"] == old_id and accepted["room_id"] == old_id
                   and pending["room_id"] == old_id,
                   "all pre-reload request identities belong to the actual original relay room")
        front.send(b"\x01\x0b/reload\r")
        credentials = self.connection_record(front, "account relay reexec")
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "local relay /reload reexecs the same foreground process with a new client ID")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        self.retired_context = old_context
        self.retired_generation = before_events["generation"]
        fresh_screen = front.until(
            lambda shot: empty_composer_ready(shot)
            and all(marker not in text(shot) for marker in old_context),
            "a fresh empty composer after real relay reexec",
        )
        self.check(self.account("/api/client/whoami")["identity"] == whoami,
                   "the replacement relay remains authenticated as the same named account")

        def account_reconnected():
            ids = {item["id"] for item in self.account("/api/client/sessions")["clients"]}
            return self.client_id in ids and old_id not in ids

        wait_for(account_reconnected, "the account listing replacing its old relay connection")
        self.check(True, "the account listing exposes the new session and removes the disconnected ID")
        self.check(self.secret == old_token and self.rpc("ping", modern=True, secret=old_token)[0] == 200,
                   "the existing identity OAuth grant remains valid across this client-only reload")
        listed = self.public_call("list", secret=old_token)["clients"]
        self.check(any(item["id"] == self.client_id for item in listed)
                   and all(item["id"] != old_id for item in listed),
                   "the same MCP grant controls the replacement account relay without new login")
        fresh = self.queue()
        self.check(fresh["queue"] == [] and fresh["draft"] == {"text": "", "cursor": 0}
                   and fresh["draft_context"]["parked"] is None
                   and fresh["draft_context"]["target"] == initial_queue_target,
                   "real reexec starts with no delivered history, pending queue, draft or parked editor")
        composition = self.ui("/display/conversation")
        self.check(composition["draft"] == fresh["draft"] and composition["editing"] is None
                   and {key: composition[key] for key in initial_routing} == initial_routing,
                   "the new composer has its original default routing and an empty editor")
        fresh_response = self.work()
        self.check(not self.messages(fresh_response),
                   "the new relay never automatically redelivers the old pending or delivered requirements")
        after_events = self.live_events()
        serialized = json.dumps(after_events["events"], ensure_ascii=False)
        self.check(after_events["generation"] != before_events["generation"]
                   and after_events["scope"]["client_id"] == self.client_id
                   and all(marker not in serialized and marker not in text(fresh_screen) for marker in old_context)
                   and not any(event.get("value", {}).get("message_id") == reply_receipt["message_id"]
                               and (not reusable_reply_id or event["value"].get("text") == old_reply)
                               for event in after_events["events"]),
                   "the replacement event generation contains no old RAM body or legacy file contents")
        self.assert_legacy_state_unchanged(legacy_records)

        members = []
        for role in ("implementation", "review"):
            member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                                "name": "Relay member", "role": role})
            self.sensitive.append(member["token"])
            members.append(member)

        def registered():
            front.pump(0.05)
            return {member["agent_id"] for member in self.queue()["agents"]} == {
                member["agent_id"] for member in members}

        wait_for(registered, "both exact post-reload member identities in the current relay roster")
        self.recipient(front, members[0])
        continuation = self.submit(front, "RELAY_COMPOSER_FRESH_EXACT_REQUEST 界")
        self.check(continuation["target"] == members[0]["agent_id"]
                   and continuation["room_id"] == self.client_id
                   and continuation["id"] not in {first["id"], editable["id"], pending["id"]},
                   "new input has its own identity and exact authenticated recipient in the new relay room")
        current_draft = "RELAY_POST_RELOAD_UNSENT_DRAFT 界"
        self.focus_input(front)
        front.send(current_draft.encode() + LEFT * 2)
        front.until(lambda shot: current_draft in input_text(shot), "an independent draft beside the new exact request")
        current_composition = self.ui("/display/conversation")
        self.check(current_composition["draft"] == {
            "text": current_draft, "cursor": len(current_draft.encode()) - len(" 界".encode())},
            "the new exact-recipient request coexists with an independent UTF-8 draft")
        controller_response = self.work(controller={"name": "Relay member"})
        self.check(not self.messages(controller_response),
                   "a matching display name cannot claim the new exact-recipient requirement")
        other_response = self.work(member=members[1])
        self.check(not self.messages(other_response),
                   "another authenticated post-reload member cannot claim the selected member's requirement")
        unclaimed = self.record(self.queue(), message_id=continuation["id"])
        self.check(unclaimed["status"] == "queued" and unclaimed["target"] == members[0]["agent_id"],
                   "wrong-recipient calls leave the exact queued identity and target unchanged")
        exact_response = self.work(member=members[0])
        self.expect_delivery(exact_response, continuation, agent_id=members[0]["agent_id"])
        self.wait_delivered(front, continuation)
        duplicate_response = self.work(member=members[0])
        self.check(not self.messages(duplicate_response),
                   "the authenticated recipient receives the new requirement only once")
        self.check(all(marker not in json.dumps(response, ensure_ascii=False)
                       for response in (fresh_response, controller_response, other_response,
                                        exact_response, duplicate_response)
                       for marker in old_context),
                   "post-reload work responses contain no old RAM or legacy checkpoint context")
        delivered = self.queue()
        self.check({row["id"] for row in delivered["queue"]} == {continuation["id"]}
                   and self.record(delivered, message_id=continuation["id"])["status"] == "delivered",
                   "only explicitly submitted post-reload input enters the new room's queue")
        after_composition = self.ui("/display/conversation")
        self.check(after_composition["draft"] == current_composition["draft"]
                   and after_composition["target"] == members[0]["agent_id"],
                   "same-process recipient checks and delivery preserve the fresh draft and cursor")
        expected_deliveries = [
            {"observed_target_client_id": old_id, "message_id": first["id"],
             "text": first["text"], "agent_id": None},
            {"observed_target_client_id": old_id, "message_id": editable["id"],
             "text": revised, "agent_id": None},
            {"observed_target_client_id": self.client_id, "message_id": continuation["id"],
             "text": continuation["text"], "agent_id": members[0]["agent_id"]},
        ]
        self.check(self.gateway_deliveries == expected_deliveries
                   and all(row["message_id"] != pending["id"] and row["text"] != pending["text"]
                           for row in self.gateway_deliveries),
                   "gateway responses deliver only the three explicit inputs, never the pre-reload pending request")
        self.check_post_delivery_journal(after_events, old_context,
                                         marker="RELAY_POST_DELIVERY_JOURNAL_PROBE")
        continued_screen = front.until(
            lambda shot: visible_record(shot, continuation["text"], "delivered", continuation["id"])
            and current_draft in input_text(shot)
            and all(marker not in text(shot) for marker in old_context),
            "the new delivered request and fresh draft without old RAM or legacy markers",
        )
        self.check(all(marker not in json.dumps(after_composition, ensure_ascii=False)
                       and marker not in text(continued_screen) for marker in old_context),
                   "the relay composer still excludes pre-reload bodies and identities after fresh delivery")
        # Explicit export is tested only after the new requirement has been
        # delivered. Clearing this owned draft is not the reload-freshness test.
        handoff_before = self.queue()["queue"]
        note = "RELAY_EXPLICIT_HANDOFF_NOTE 界"
        self.focus_input(front)
        front.send(b"\x01\x0b/handoff " + note.encode() + ENTER)

        def exported_handoff():
            front.pump(0.05)
            state = self.ui("/display/conversation")
            match = re.fullmatch(r'\n@"(\.ssmcp/inputs/handoff-[0-9a-f]{32}\.md)"\n', state["draft"]["text"])
            return (state, match.group(1)) if match else None

        handoff_state, handoff_path = wait_for(exported_handoff, "the explicit private handoff reference")
        handoff_file = self.workspace / handoff_path
        handoff_bytes = handoff_file.read_bytes()
        handoff_text = handoff_bytes.decode("utf-8")
        self.check(not handoff_file.is_symlink() and stat.S_IMODE(handoff_file.stat().st_mode) == 0o600
                   and 0 < len(handoff_bytes) <= 2 * 1024 * 1024
                   and note in handoff_text and continuation["text"] in handoff_text
                   and current_draft not in handoff_text
                   and all(marker not in handoff_text for marker in old_context),
                   "explicit handoff exports only the fresh context into a bounded private Markdown file")
        self.check(self.queue()["queue"] == handoff_before
                   and handoff_state["target"] == members[0]["agent_id"]
                   and handoff_state["native_recipient"] is None,
                   "handoff preserves the exact recipient and neither queues nor sends its editable reference")
        handoff_response = self.work(member=members[0])
        self.check(not self.messages(handoff_response)
                   and all(marker not in json.dumps(handoff_response, ensure_ascii=False) for marker in old_context),
                   "the explicit handoff reference is never delivered without a separate submission")
        handoff_events = self.live_events()
        self.check(handoff_events["generation"] == after_events["generation"],
                   "explicit handoff retains the fresh event generation and exposes no retired context")
        self.check(self.gateway_deliveries == expected_deliveries,
                   "explicit handoff causes no additional gateway user-message delivery")
        self.assert_legacy_state_unchanged(legacy_records)
        self.check(not (self.workspace / "relay-composer-hidden-shell-leak").exists(),
                   "all relay composer interactions keep request text separate from shell execution")
        final, restored_termios = front.disconnect()
        self.check(front.process.returncode == 0 and not final["alternate_screen"]
                   and restored_termios == front.original_termios,
                   "the reexecuted relay exits cleanly and restores the owned physical terminal")
        self.check(all(secret.encode() not in front.transcript for secret in self.sensitive if secret),
                   "the physical terminal transcript contains no fixture account or OAuth credentials")
        self.metrics["relay_composer"] = {"bytes": len(front.transcript),
            "sha256": hashlib.sha256(front.transcript).hexdigest(), "request_count": len(delivered["queue"]),
            "before_generation": before_events["generation"], "after_generation": after_events["generation"],
            "old_reply_identity": {"generation": before_events["generation"],
                                   "message_id": reply_receipt["message_id"]},
            "gateway_response_deliveries": self.gateway_deliveries,
            "handoff_bytes": len(handoff_bytes), "handoff_sha256": hashlib.sha256(handoff_bytes).hexdigest(),
            "legacy_sha256": {str(path.relative_to(self.state_home)): hashlib.sha256(saved["bytes"]).hexdigest()
                              for path, saved in legacy_records.items()}}
        front.close()

    def cleanup(self):
        if self.fixture_process is not None and self.fixture_process.poll() is None:
            (self.fixture_directory / "stop").touch()
            try:
                self.fixture_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        super().cleanup()
        if self.fixture_process is not None:
            self.fixture_exit_code = self.fixture_process.returncode


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("relay composer E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--fixture-binary", type=Path, required=True,
                        help="already-compiled library test binary containing the ignored real-router fixture")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    args.bin_dir = args.bin_dir.resolve()
    decoder = (args.decoder or args.bin_dir / "examples/vt_snapshot").resolve()
    for executable in (args.bin_dir / "ssmcp", decoder, args.fixture_binary):
        if not executable.is_file():
            parser.error(f"missing executable {executable}; build it before this test")
    started = time.monotonic()
    source_directory = Path(__file__).resolve().parent
    sources = [source_directory / name for name in (
        "e2e_relay_composer.py", "e2e_composer.py", "e2e_dashboard.py", "e2e.py", "e2e_account_relay.py")]
    sources += [source_directory.parent / "src/http/relay_composer_fixture.rs"]
    report = {"status": "failed", "platform": platform.system(),
        "transport": "real production Broker/router in compiled Rust test executable + loopback HTTP/WS account relay + owned PTY",
        "oauth": "actual DCR/PKCE/consent/token exchange with an isolated synthetic GitHub provider",
        "tls_verified": False, "real_github_account": False, "models_started": False,
        "scope": "account queue delivery, CAS editing, same-account reexec with fresh RAM, legacy files left untouched, exact-recipient continuation and explicit handoff",
        "binaries": {"ssmcp": sha256(args.bin_dir / "ssmcp"), "vt_snapshot": sha256(decoder),
                     "router_test_fixture": sha256(args.fixture_binary)},
        "sources": {str(path.relative_to(source_directory.parent)): sha256(path) for path in sources}}
    with tempfile.TemporaryDirectory(prefix="ssmcp-relay-composer-") as directory:
        harness = RelayComposer(Path(directory), args.bin_dir, decoder, args.fixture_binary)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            description = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    description = description.replace(secret, "[redacted]")
            report["error"] = description[:10000]
            print(report["error"], flush=True)
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            if report["status"] == "passed" and harness.fixture_exit_code != 0:
                report["status"] = "failed"
                report["error"] = "the owned router fixture did not terminate successfully"
            report.update(checks=harness.checks, metrics=harness.metrics,
                          fixture_exit_code=harness.fixture_exit_code,
                          duration_seconds=round(time.monotonic() - started, 3))
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} relay composer checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
