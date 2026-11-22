#!/usr/bin/python3
"""Deploy one verified GitHub Release, with an isolated runtime gate and automatic rollback."""
import argparse
import base64
import fcntl
import hashlib
import http.client
from html.parser import HTMLParser
import io
import json
import os
import platform
from pathlib import Path
import pwd
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tarfile
import time
import tomllib
import urllib.request
from urllib.parse import urlencode, urlsplit

SERVICE = "ssmcpd.service"
BASE = Path("/var/lib/ssmcp-deployments")
MANAGED_DROPIN = Path("/etc/systemd/system/ssmcpd.service.d/zzz-release.conf")
REPOSITORY = "LIghtJUNction/ssmcp"

# Reviewed exceptions for generated-artifact history removal only. This table
# travels with the standalone helper; release metadata, CLI and environment
# cannot supply or extend it. Original published artifacts were not rebuilt.
RELEASE_SOURCE_REWRITES = {
    "schema": 1,
    "entries": [
        {
            "build_commit": "52a531a59e9ec9236a76eafc0f4f068799e44b15",
            "manifest_sha256": "b6e4a14bf019bdc3be4d440864fcdef00302696e73c430c928384fd7ccfab15d",
            "proof": {
                "audit_sha256": "5bb6c36d5755fc0c6e6b4f18b37a9be0e0b6fc96073557e9012a800b1315f258",
                "build_retained_sha256": "3eced2ae1de334db27ecd6a078f5b3993bf8f9d464f44d44106655725c4a0e7e",
                "build_tree": "17dff7b6dddbe1169d9211dc05f686fc44d2fc21",
                "kind": "retained-git-tree-v1",
                "removed_path_count": 76,
                "removed_paths_sha256": "965f085434e25db1a75dd949008388634a3427b771aefa291507f6fb6738b60e",
                "source_retained_sha256": "3eced2ae1de334db27ecd6a078f5b3993bf8f9d464f44d44106655725c4a0e7e",
                "source_tree": "9dad00b18582eee3f4dda2e83e1f9d62fb629ff1"
            },
            "repository": "LIghtJUNction/ssmcp",
            "source_commit": "961879e26ddf6697efbcc9acb4d39affb9968435",
            "tag": "v0.10.10"
        },
        {
            "build_commit": "4e3132d60600dca5053af81cdefa6962c3058ac4",
            "manifest_sha256": "18c21b28dd523bf5ce0e4c8c5758a95fe68947bc09a68fa01320bdc7c6c82d33",
            "proof": {
                "audit_sha256": "7be4a1f75538e743d5d703d1aecf418240d7a77d5dc5491c7dee8cdcbe02c0ad",
                "build_retained_sha256": "4f0f47862758e34da1b556985be8cd2609408b9ac79381e752f96eb3b4deb3cb",
                "build_tree": "f122be555b8e0ea597b4561b63d2acb574057e57",
                "kind": "retained-git-tree-v1",
                "removed_path_count": 76,
                "removed_paths_sha256": "965f085434e25db1a75dd949008388634a3427b771aefa291507f6fb6738b60e",
                "source_retained_sha256": "4f0f47862758e34da1b556985be8cd2609408b9ac79381e752f96eb3b4deb3cb",
                "source_tree": "1194828778ea2f1c5ad92bf5fdf9fac6b8ea595a"
            },
            "repository": "LIghtJUNction/ssmcp",
            "source_commit": "ed067954d5279eb23245805e14e761d2e6971e5e",
            "tag": "v0.10.13"
        },
        {
            "build_commit": "4f4dd88275c205e0e61a7601b4e0d11f56038769",
            "manifest_sha256": "52bf95a1cbd624b8532fddb6d5ee4895480c3b5bad901eab16cac1af8ee29f67",
            "proof": {
                "audit_sha256": "36e89477e5d4ac74eba04b01df92e5a44b4d926982e5dafe13c33915bc7d09a7",
                "build_retained_sha256": "58a1566c31bfcdf40257b0e5eaf2b179269969383aa37a97275dc913b7335a6e",
                "build_tree": "33943217ac15be08e5064d105db89cc9bbd03206",
                "kind": "retained-git-tree-v1",
                "removed_path_count": 91,
                "removed_paths_sha256": "59113edc17cd474609277d88de9d5aabc0f5452b4215ced3a47977f497860e1f",
                "source_retained_sha256": "58a1566c31bfcdf40257b0e5eaf2b179269969383aa37a97275dc913b7335a6e",
                "source_tree": "70b066d2c4d6df30ce84b91ab2ecd1fd1be7f5d4"
            },
            "repository": "LIghtJUNction/ssmcp",
            "source_commit": "54532e0916ce3dbb9609796684c24488824b2a73",
            "tag": "v0.10.20"
        },
        {
            "build_commit": "c75309a9acab27c684c0beaaf9d22fd991200760",
            "manifest_sha256": "c6fbf596377412d0479548cc69f382f29677da9f2caaedcebcffda568e5d12e6",
            "proof": {
                "audit_sha256": "96f79ed94f4ebc368632c234a93ee93a6aa454cc1b56d447ecf9b7d29611e0d9",
                "build_retained_sha256": "ea3700a1bdddf5d454366e85415c99b652109a4f81530c4186ec2c51c8190187",
                "build_tree": "76a99d265fbf3711d273ba38b66239878adbb219",
                "kind": "retained-git-tree-v1",
                "removed_path_count": 2,
                "removed_paths_sha256": "a1d4e11e0e7dd4ea4230fc57ab3f50ad1777cf47d4270c5d6e4c353b1dd8f7e6",
                "source_retained_sha256": "ea3700a1bdddf5d454366e85415c99b652109a4f81530c4186ec2c51c8190187",
                "source_tree": "d7f6779bb853906e2b00c7afd2fd5a5bf332f63e"
            },
            "repository": "LIghtJUNction/ssmcp",
            "source_commit": "52187740ed00f538e2c3e40eda733f2cb075df40",
            "tag": "v0.5.1"
        },
        {
            "build_commit": "3e3b7b6f8cce30b77e83a11d7cc01e595a59a849",
            "manifest_sha256": "82885ec6fa3b69b8ee857ba3d279a1bf61dcad4ecaf7e5cc4c585ed559733d76",
            "proof": {
                "audit_sha256": "36b968a813b94df12fa273c9aa810bcae730204f3c79cf150a2be7e0295684d5",
                "build_retained_sha256": "ee050bd329ee96c1defbf966c0ad94e862bfc29a4c60847cb997005db12f3ab2",
                "build_tree": "e4f016a08709dad012887886797fc27df8b110e6",
                "kind": "retained-git-tree-v1",
                "removed_path_count": 12,
                "removed_paths_sha256": "8ab229561218a83f2039520b1f96595658e2411d721078e1c350c91cefd53b6e",
                "source_retained_sha256": "ee050bd329ee96c1defbf966c0ad94e862bfc29a4c60847cb997005db12f3ab2",
                "source_tree": "cdc487a2373fcef42477e0ed8aa4845827dd6488"
            },
            "repository": "LIghtJUNction/ssmcp",
            "source_commit": "740782217036bdeb47261576e11414d51222b862",
            "tag": "v0.6.3"
        }
    ]
}
RELEASE_SOURCE_REWRITES_SHA256 = 'd9cd1a5c0bccf2d6bd495d4bcc34bc764d9b120e6baa8caa730979e4b3e61b5e'
CORE_ASSET_NAMES = {"portal.css", "portal-refresh.css", "traffic-meter.js", "client-install-copy.js", "copy-command.js"}
LEGAL_PATHS = ("/legal/terms", "/legal/privacy", "/legal/refund")
PROXY_CONFIG = "/etc/nginx/conf.d/ssmcp.lmm.best.conf"
MAINTENANCE_HEADER = "X-SSMCP-Maintenance"
SANDBOX_PROPERTIES = ("User", "Group", "NoNewPrivileges", "PrivateTmp", "PrivateDevices", "ProtectKernelTunables", "ProtectControlGroups", "ProtectHome", "ProtectSystem", "ReadWritePaths", "RestrictAddressFamilies", "PrivateNetwork", "UMask")


def require(ok, name):
    if not ok:
        raise RuntimeError(name)


def asset_types(name):
    if name in CORE_ASSET_NAMES:
        return ["text/css"] if name.endswith(".css") else ["text/javascript", "application/javascript"]
    image = re.fullmatch(r"(?:brand|favicon)(?:-[a-z0-9]+)*\.(svg|png|webp)", name)
    require(image is not None, "unexpected_asset_reference")
    return [{"svg": "image/svg+xml", "png": "image/png", "webp": "image/webp"}[image[1]]]


class StageCleanupDeferred(RuntimeError):
    """The stage may still be running; its state and executable must remain."""


def sha(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(args, timeout=45, *, deadline=None):
    if deadline is not None:
        remaining = deadline - time.monotonic()
        require(remaining > 0, "rollback_deadline_exhausted")
        timeout = min(timeout, remaining)
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    require(result.returncode == 0, "command_failed_" + Path(args[0]).name)
    return result.stdout.strip()


def prop(name, *, deadline=None):
    return run(["systemctl", "show", SERVICE, "-p", name, "--value"], deadline=deadline)


def current_sha(*, deadline=None):
    pid = prop("MainPID", deadline=deadline)
    return sha("/proc/" + pid + "/exe") if pid.isdigit() and int(pid) else None


def snapshot(*, deadline=None):
    pid = prop("MainPID", deadline=deadline)
    require(prop("ActiveState", deadline=deadline) == "active" and pid.isdigit() and int(pid) > 0, "live_service_not_active")
    proc = Path("/proc") / pid
    exe = os.readlink(proc / "exe")
    require(exe.startswith("/") and not exe.endswith(" (deleted)"), "running_path_requires_review")
    argv = [v.decode() for v in (proc / "cmdline").read_bytes().split(b"\0") if v]
    env = dict(v.decode().split("=", 1) for v in (proc / "environ").read_bytes().split(b"\0") if b"=" in v)
    require(argv[1:] == ["serve"], "startup_arguments_changed_review_required")
    require(not any(k.startswith("SSMCP_") for k in env), "environment_overrides_changed_review_required")
    require(prop("NeedDaemonReload", deadline=deadline) == "no", "unit_reload_pending_review_required")
    start = prop("ExecStart", deadline=deadline)
    start_match = re.fullmatch(r"\{\s*path=(/[A-Za-z0-9_./-]+)\s*;\s*argv\[\]=([^;{}]+)\s*;[^{}]*\}", start)
    require(start_match is not None, "start_metadata_requires_review")
    start_commands = [start_match[2].strip()]
    require(re.fullmatch(r"/[A-Za-z0-9_./-]+ serve", start_commands[0]) and
            start_match[1] == argv[0] and start_commands[0].split(" ") == argv,
            "startup_unit_runtime_mismatch")
    prestart = prop("ExecStartPre", deadline=deadline)
    prestart_commands = [v.strip() for v in re.findall(r"argv\[\]=([^;]+)", prestart)]
    require(not prestart or prestart_commands, "prestart_metadata_requires_review")
    config = "/etc/ssmcpd/ssmcpd.toml"
    units = {prop("FragmentPath", deadline=deadline), *prop("DropInPaths", deadline=deadline).split()}
    for parent in ["/etc/systemd/system", "/run/systemd/system", "/usr/lib/systemd/system"]:
        units.update(str(p) for p in (Path(parent) / (SERVICE + ".d")).glob("*.conf"))
    units.discard("")
    files = {p: sha(p) for p in sorted(units)}
    extra = [config, "/etc/ssmcpd/ssmcpd.env"]
    runtime_paths = {"/usr/libexec/ssmcp/code-mode.js", "/usr/local/libexec/ssmcp/code-mode.js",
                     str(Path(exe).parent.parent / "libexec/ssmcp/code-mode.js")}
    require(Path(PROXY_CONFIG).is_file(), "reviewed_proxy_config_missing")
    return {"pid": int(pid), "exe": exe, "argv": argv,
            "live_sha256": sha(proc / "exe"), "units": files,
            "start_commands": start_commands, "prestart_commands": prestart_commands,
            "sandbox": {name: prop(name, deadline=deadline) for name in SANDBOX_PROPERTIES},
            "config": config, "private_files": {p: sha(p) for p in extra if Path(p).is_file()},
            "runtime_files": {p: sha(p) for p in sorted(runtime_paths) if Path(p).is_file()},
            "proxy_files": {PROXY_CONFIG: sha(PROXY_CONFIG)}}

def same_lease(a, b):
    return all(a[k] == b[k] for k in ["exe", "argv", "live_sha256", "units", "config", "private_files",
                                      "runtime_files", "proxy_files", "start_commands", "prestart_commands", "sandbox"])


def verify_original_runners(expected_files):
    # Snapshot discovery follows the current executable, so inspect the original paths directly.
    for path, digest in expected_files.items():
        require(Path(path).is_file() and sha(path) == digest, "original_runners_changed")


def backup(expected, output=None):
    before = snapshot()
    require(before["live_sha256"] == expected, "live_sha_mismatch")
    files = {"runtime/ssmcpd": Path("/proc") / str(before["pid"]) / "exe"}
    files.update({"files/" + p.lstrip("/"): Path(p) for p in [*before["units"], *before["private_files"], *before["runtime_files"]]})
    entries, payloads = [], {}
    for name, path in files.items():
        data = path.read_bytes()
        stat = path.stat()
        payloads[name] = data
        entries.append({"name": name, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data),
                        "mode": stat.st_mode & 0o777, "uid": stat.st_uid, "gid": stat.st_gid})
    require(same_lease(before, snapshot()), "live_changed_during_backup")
    payloads["manifest.json"] = json.dumps({"snapshot": before, "entries": entries}, sort_keys=True).encode()
    with tarfile.open(fileobj=output or sys.stdout.buffer, mode="w|") as archive:
        for name, data in payloads.items():
            item = tarfile.TarInfo(name)
            item.size, item.mode = len(data), 0o600
            archive.addfile(item, io.BytesIO(data))


def read_maintenance_token(path):
    require(isinstance(path, str) and Path(path).is_absolute(), "maintenance_token_absolute_path")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
        try:
            metadata = os.fstat(fd)
            require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid == 0 and
                    stat.S_IMODE(metadata.st_mode) == 0o600, "maintenance_token_private_file")
            data = os.read(fd, 66)
        finally:
            os.close(fd)
    except OSError:
        raise RuntimeError("maintenance_token_unavailable") from None
    require(re.fullmatch(rb"[0-9a-f]{64}\n?", data), "maintenance_token_format")
    return data


def maintenance_token(base, maintenance):
    require(re.fullmatch(r"https://[A-Za-z0-9.-]+(?::[0-9]+)?", base) and
            base == maintenance["origin"], "maintenance_public_origin")
    data = read_maintenance_token(maintenance["token_file"])
    require(hashlib.sha256(data).hexdigest() == maintenance["token_sha256"], "maintenance_token_changed")
    return data.rstrip(b"\n").decode("ascii")


def request(base, path, payload=None, *, timeout=12, include_headers=False, maintenance=None):
    headers = {"Content-Type": "application/json", "Cache-Control": "no-cache", "Accept-Encoding": "identity"}
    if maintenance is not None:
        require(payload is None and path.startswith("/") and not path.startswith("//") and
                "\r" not in path and "\n" not in path, "maintenance_probe_path")
        headers[MAINTENANCE_HEADER] = maintenance_token(base, maintenance)
    url = urlsplit(base)
    cls = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    conn = cls(url.hostname, url.port, timeout=timeout)
    try:
        conn.request("GET" if payload is None else "POST", path,
                     body=None if payload is None else json.dumps(payload),
                     headers=headers)
        # http.client returns redirects as responses; never forward the private header.
        response = conn.getresponse()
        body = response.read(8_388_609)
        require(len(body) <= 8_388_608, "response_exceeds_probe_limit")
        if include_headers:
            return response.status, body, {key.lower(): value for key, value in response.getheaders()}
        return response.status, body
    finally:
        conn.close()

def health(base, *, deadline=None, maintenance=None):
    limit = time.monotonic() + 30
    deadline = limit if deadline is None else min(limit, deadline)
    while (remaining := deadline - time.monotonic()) > 0:
        try:
            status, body = request(base, "/healthz", timeout=min(3, remaining), maintenance=maintenance)
            if status == 200 and body.strip() == b"ok" and time.monotonic() <= deadline:
                return
        except (OSError, http.client.HTTPException):
            pass
        pause = min(0.25, max(0, deadline - time.monotonic()))
        if pause:
            time.sleep(pause)
    raise RuntimeError("health_timeout")



class Page(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.assets, self.footers, self.pricing, self.policy_navs = {}, 0, 0, 0
        self.links, self.unversioned_resources = set(), set()
        self.feed(html.decode("utf-8"))

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        self.footers += tag == "footer"
        self.pricing += attributes.get("id") == "pricing"
        self.policy_navs += tag == "nav" and attributes.get("aria-label") == "网站政策"
        for key in ("href", "src"):
            value = attributes.get(key, "")
            if key == "href":
                self.links.add(value)
            if value.startswith("/account/assets/"):
                match = re.fullmatch(r"/account/assets/([0-9a-f]{64})/([a-z0-9.-]+)", value)
                require(match is not None, "unexpected_asset_reference")
                name = match[2]
                asset_types(name)
                require(name not in self.assets or self.assets[name] == value, "conflicting_asset_reference")
                self.assets[name] = value
            elif value and ((key == "src" and tag in ["script", "img"]) or
                            (key == "href" and tag == "link" and
                             set(attributes.get("rel", "").split()) & {"stylesheet", "icon"})):
                self.unversioned_resources.add(value)


def proxy_fragment(html):
    match = re.search(rb'(<section\b[^>]*\bid=["\']pricing["\'][\s\S]*?)(?=</body\s*>)', html, re.I)
    require(match is not None, "public_pricing_fragment_missing")
    fragment = match[1]
    page = Page(fragment)
    require(page.pricing == 1 and page.footers == 1 and page.policy_navs == 1, "public_policy_fragment_shape")
    require(set(LEGAL_PATHS).issubset(page.links), "public_policy_links")
    return fragment


def public_baseline(base, *, maintenance=None):
    require(re.fullmatch(r"https://[A-Za-z0-9.-]+(?::[0-9]+)?", base), "baseline_public_origin")
    status, html = request(base, "/", maintenance=maintenance)
    require(status == 200, "baseline_homepage")
    fragment = proxy_fragment(html)
    legal = {}
    for path in LEGAL_PATHS:
        status, body = request(base, path, maintenance=maintenance)
        require(status == 200, "baseline_legal_page")
        legal[path] = {"sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body)}
    return {"proxy_fragment_sha256": hashlib.sha256(fragment).hexdigest(),
            "proxy_fragment_bytes": len(fragment), "footer_count": Page(html).footers,
            "legal_pages": legal}

def checks(base, r, *, public=False):
    maintenance = r.get("maintenance") if public else None
    health(base, maintenance=maintenance)
    status, html = request(base, "/", maintenance=maintenance)
    require(status == 200, "homepage")
    page = Page(html)
    require(not page.unversioned_resources, "unversioned_page_resource")
    require(CORE_ASSET_NAMES.issubset(page.assets) and set(page.assets) - CORE_ASSET_NAMES,
            "core_and_brand_assets_required")
    if r.get("assets_frozen"):
        require(set(page.assets) == set(r["assets"]), "page_asset_set_changed_after_stage")
    asset_checks = {}
    for name, url in sorted(page.assets.items()):
        status, body, headers = request(base, url, include_headers=True, maintenance=maintenance)
        require(status == 200, "asset_status_" + name)
        if name not in r["assets"]:
            require(not public and not r.get("assets_frozen") and name not in CORE_ASSET_NAMES,
                    "asset_not_verified_in_stage")
            r["assets"][name] = {"sha256": url.split("/")[3], "bytes": len(body), "content_types": asset_types(name)}
        expected = r["assets"][name]
        require(url == "/account/assets/" + expected["sha256"] + "/" + name, "html_asset_digest_" + name)
        require(len(body) == expected["bytes"], "asset_length_" + name)
        require(hashlib.sha256(body).hexdigest() == expected["sha256"], "asset_sha256_" + name)
        mime = headers.get("content-type", "").split(";", 1)[0].strip().lower()
        require(mime in expected["content_types"], "asset_content_type_" + name)
        asset_checks[name] = {"sha256": expected["sha256"], "bytes": len(body), "html_url_verified": True}
    if public:
        baseline = r["public_baseline"]
        fragment = proxy_fragment(html)
        require(len(fragment) == baseline["proxy_fragment_bytes"] and
                hashlib.sha256(fragment).hexdigest() == baseline["proxy_fragment_sha256"], "public_pricing_policy_changed")
        require(page.footers == 1, "single_public_footer")
        require(page.pricing == 1 and page.policy_navs == 1, "single_public_pricing_and_policy")
        for path, expected in baseline["legal_pages"].items():
            status, body = request(base, path, maintenance=maintenance)
            require(status == 200 and len(body) == expected["bytes"] and
                    hashlib.sha256(body).hexdigest() == expected["sha256"], "legal_page_changed")
    else:
        require(page.footers == 0 and page.pricing == 0 and page.policy_navs == 0, "direct_page_has_no_proxy_footer")
    require(request(base, "/mcp", maintenance=maintenance)[0] == 401, "mcp_rejects_unauthenticated")
    require(request(base, "/account/admin", maintenance=maintenance)[0] in [302, 303, 401, 403], "admin_rejects_unauthenticated")
    status, metadata = request(base, "/.well-known/oauth-authorization-server", maintenance=maintenance)
    require(status == 200 and json.loads(metadata)["issuer"] == r["public_url"], "oauth_issuer")
    return {"health": True, "homepage": True, "assets": asset_checks,
            "proxy_pricing_policy_preserved": public, "single_public_footer": public,
            "mcp_rejects_unauthenticated": True, "admin_rejects_unauthenticated": True, "oauth_issuer": True}

def set_toml(text, section, key, value):
    lines = text.splitlines()
    start = next((i for i, line in enumerate(lines) if line.strip() == "[" + section + "]"), None)
    if start is None:
        lines.extend(["", "[" + section + "]", key + " = " + json.dumps(value)])
    else:
        end = next((i for i in range(start + 1, len(lines)) if lines[i].lstrip().startswith("[")), len(lines))
        matches = [i for i in range(start + 1, end) if re.match(r"\s*" + re.escape(key) + r"\s*=", lines[i])]
        require(len(matches) <= 1, "ambiguous_stage_config")
        line = key + " = " + json.dumps(value)
        if matches:
            lines[matches[0]] = line
        else:
            lines.insert(end, line)
    result = "\n".join(lines) + "\n"
    require(tomllib.loads(result)[section][key] == value, "stage_config_rewrite")
    return result


def stage(binary, r):
    name = "ssmcp-release-stage-" + r["run_id"]
    state, runtime = Path("/var/lib") / name, Path("/run") / name
    require(not state.exists(), "stage_directory_exists")
    require(run(["systemctl", "show", name, "-p", "LoadState", "--value"]) == "not-found", "stage_unit_exists")
    account = pwd.getpwnam("ssmcpd")
    state.mkdir(mode=0o700)
    os.chown(state, account.pw_uid, account.pw_gid)
    text = Path(r["before"]["config"]).read_text()
    original = tomllib.loads(text)
    for section, key, value in [("portal", "state_dir", str(state / "portal")),
                                ("portal", "audit_dir", str(state / "audit")),
                                ("portal", "gateway_socket", str(runtime / "management.sock")),
                                ("oauth", "client_registry", str(state / "oauth-clients.toml")),
                                ("commerce", "enabled", False)]:
        text = set_toml(text, section, key, value)
    config = state / "stage.toml"
    config.write_text(text)
    config.chmod(0o600)
    os.chown(config, account.pw_uid, account.pw_gid)
    forbidden = {"/var/lib/ssmcpd"}
    forbidden.update(str(original.get(section, {}).get(key, "")) for section, key in
                     [("portal", "state_dir"), ("portal", "audit_dir"), ("oauth", "client_registry")])
    forbidden.discard("")
    require(all(re.fullmatch(r"/[A-Za-z0-9_./-]+", p) and p not in ["/", "/var", "/var/lib"] for p in forbidden), "unsupported_state_path")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    base = "http://127.0.0.1:" + str(port)
    command = ["systemd-run", "--quiet", "--collect", "--unit=" + name, "--uid=ssmcpd", "--gid=ssmcp",
               "--property=RuntimeMaxSec=300s", "--property=TimeoutStopSec=30s", "--property=KillMode=control-group",
               "--property=StateDirectory=" + name, "--property=StateDirectoryMode=0700",
               "--property=RuntimeDirectory=" + name, "--property=RuntimeDirectoryMode=0750",
               "--property=ProtectSystem=strict", "--property=ProtectHome=yes", "--property=PrivateTmp=yes",
               "--property=NoNewPrivileges=yes", "--property=UMask=0077",
               "--property=PrivateDevices=yes", "--property=ProtectKernelTunables=yes",
               "--property=ProtectControlGroups=yes", "--property=ReadWritePaths=/proc/sys/user",
               "--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK",
               "--property=ExecStartPre=" + str(binary) + " check-runtime", "--setenv=PATH=/usr/bin:/bin",
               "--property=InaccessiblePaths=" + " ".join("-" + p for p in sorted(forbidden)),
               str(binary), "serve", "--config", str(config), "--listen", "127.0.0.1:" + str(port),
               "--socket", str(runtime / "control.sock"), "--client-registry", str(state / "oauth-clients.toml"),
               "--portal-state-dir", str(state / "portal"), "--gateway-socket", str(runtime / "management.sock")]
    try:
        run(command)
        result = checks(base, r)
        status, data = request(base, "/oauth/register", {"client_name": "ssmcp isolated rollout check",
            "redirect_uris": ["https://client.example/rollout"], "token_endpoint_auth_method": "none"})
        require(status == 201, "stage_dcr_registration")
        query = urlencode({"client_id": json.loads(data)["client_id"], "redirect_uri": "https://client.example/rollout",
            "response_type": "code", "resource": r["public_url"] + "/mcp", "scope": "mcp",
            "code_challenge_method": "S256", "code_challenge": base64.urlsafe_b64encode(hashlib.sha256(b"x" * 43).digest()).decode().rstrip("=")})
        require(request(base, "/oauth/authorize?" + query)[0] == 200, "stage_authorization")
        run(["systemctl", "restart", name])
        health(base)
        require(request(base, "/oauth/authorize?" + query)[0] == 200, "stage_dcr_survives_restart")
        result.update(fresh_state=True, production_state_inaccessible=True, commerce_disabled=True, dcr_survives_restart=True,
                      candidate_runtime_preflight=True, matched_runner_sha256=r["runner_sha256"])
        return result
    finally:
        try:
            run(["systemctl", "stop", name], timeout=45)
            stopped = run(["systemctl", "show", name, "-p", "ActiveState", "-p", "MainPID"], timeout=5)
            properties = dict(line.split("=", 1) for line in stopped.splitlines() if "=" in line)
            require(properties.get("ActiveState") in ["inactive", "failed"] and
                    properties.get("MainPID") == "0", "stage_shutdown_unverified")
            shutil.rmtree(state)
        except Exception as error:
            raise StageCleanupDeferred("stage_cleanup_deferred") from error


def save(path, data):
    tmp = path.with_suffix(".tmp")
    with tmp.open("w") as stream:
        json.dump(data, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(tmp, path)


def restore_runtime(folder, before):
    old = Path(before["exe"])
    if os.path.lexists(old):
        require(sha(old) == before["live_sha256"], "original_binary_changed")
        return False
    require(old.parent.is_dir() and (old == Path("/usr/bin/ssmcpd") or
            old.is_relative_to("/usr/local/lib/ssmcp")), "unsupported_restore_path")
    with tarfile.open(folder / "backup.tar", "r:") as archive:
        data = archive.extractfile("runtime/ssmcpd").read()
    require(hashlib.sha256(data).hexdigest() == before["live_sha256"], "rollback_runtime_digest")
    temporary = old.with_name(old.name + ".release-restore")
    with temporary.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.chmod(0o755)
    # Link is exclusive: a concurrent replacement of the old path must win.
    try:
        os.link(temporary, old)
    finally:
        temporary.unlink(missing_ok=True)
    return True


def apply(path):
    r = json.loads(path.read_text())
    folder = path.parent
    stage_only = r.get("stage_only", False)
    require(type(stage_only) is bool, "invalid_stage_only_flag")
    receipt = {"schema": "ssmcp.release-deployment.v1", "status": "running", "commit": r["commit"],
               "target_sha256": r["daemon_sha256"], "expected_live_sha256": r["expected_live_sha256"],
               "assets": r["assets"], "runner_sha256": r["runner_sha256"], "version": r["version"],
               "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "release": r["release"]}
    if not stage_only:
        receipt.update(private_backup_sha256=r["backup_sha256"], rollback_checkpoint_verified=True)
    report = folder / "receipt.json"
    def checkpoint(step):
        receipt["step"] = step
        save(report, receipt)
    lock = open("/run/lock/ssmcp-deployment.lock", "a")
    prefix = Path("/usr/local/lib/ssmcp") / ("release-" + r["commit"][:12] + "-" + r["daemon_sha256"][:12])
    new = prefix / "bin/ssmcpd"
    runner = prefix / "libexec/ssmcp/code-mode.js"
    dropin = MANAGED_DROPIN
    switched, created = False, False
    dropin_temporary = None
    try:
        checkpoint("verify_inputs_and_lease")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(r["before"]["live_sha256"] == r["expected_live_sha256"] and
                r["before"]["exe"] == r["expected_live_exe"] and
                r["before"]["prestart_commands"] in [[], [r["before"]["exe"] + " check-runtime"]] and same_lease(r["before"], snapshot()), "production_lease")
        require(sha(folder / "candidate") == r["daemon_sha256"] and sha(folder / "runner.js") == r["runner_sha256"] and
                (stage_only or sha(folder / "backup.tar") == r["backup_sha256"]), "uploaded_digests")
        require(sha(__file__) == r["helper_sha256"], "deployment_helper_changed")
        require(run([str(folder / "candidate"), "--version"]) == "ssmcpd " + r["version"], "candidate_version")
        receipt["free_bytes_before"] = shutil.disk_usage("/var/lib").free
        require(receipt["free_bytes_before"] > (folder / "candidate").stat().st_size * 3 + 16_777_216, "disk_headroom")
        require(not prefix.is_symlink(), "candidate_prefix_symlink")
        if prefix.exists():
            require(new.is_file() and runner.is_file() and not new.is_symlink() and not runner.is_symlink() and
                    sha(new) == r["daemon_sha256"] and sha(runner) == r["runner_sha256"], "immutable_candidate_conflict")
        else:
            require(prefix.parent.is_dir(), "candidate_parent_missing")
            prefix.mkdir(mode=0o755)
            prefix.chmod(0o755)
            created = True
            for directory in [prefix / "bin", prefix / "libexec", prefix / "libexec/ssmcp"]:
                directory.mkdir(mode=0o755)
                directory.chmod(0o755)
            for source, destination, mode in [(folder / "candidate", new, 0o755), (folder / "runner.js", runner, 0o644)]:
                destination.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
                with destination.open("xb") as dest, source.open("rb") as src:
                    shutil.copyfileobj(src, dest)
                    dest.flush()
                    os.fsync(dest.fileno())
                destination.chmod(mode)
        receipt.update(target_created=created)
        checkpoint("isolated_stage")
        receipt["stage_checks"] = stage(new, r)
        r["assets_frozen"] = True
        checkpoint("recheck_production_lease")
        live = snapshot()
        require(live["pid"] == r["before"]["pid"] and same_lease(r["before"], live),
                "production_changed_after_stage")
        if stage_only:
            if created:
                shutil.rmtree(prefix)
            receipt.update(status="staged", running_sha256=live["live_sha256"],
                           configuration_unchanged=True, original_units_preserved=True, proxy_unchanged=True)
            return 0
        if r.get("maintenance") is not None:
            maintenance_token(r["public_url"], r["maintenance"])
        require(all(Path(p).name < dropin.name for p in r["before"]["units"]
                    if p.endswith(".conf") and p != str(dropin)), "dropin_precedence")
        dropin.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        text = "[Service]\nExecStart=\nExecStart=" + str(new) + " serve\nExecStartPre=\nExecStartPre=" + str(new) + " check-runtime\n"
        tmp = dropin.with_suffix(".tmp")
        dropin_temporary = tmp
        tmp.write_text(text)
        tmp.chmod(0o644)
        switched = True
        os.replace(tmp, dropin)
        checkpoint("switch_and_verify")
        run(["systemctl", "daemon-reload"])
        run(["systemctl", "restart", SERVICE])
        receipt["direct_checks"] = checks("http://127.0.0.1:8080", r)
        receipt["public_checks"] = checks(r["public_url"], r, public=True)
        after = snapshot()
        require(after["live_sha256"] == r["daemon_sha256"] and after["exe"] == str(new), "running_daemon_fingerprint")
        require(after["runtime_files"].get(str(runner)) == r["runner_sha256"] and
                after["prestart_commands"] == [str(new) + " check-runtime"], "running_matched_runtime")
        require(after["sandbox"] == r["before"]["sandbox"], "service_sandbox_changed")
        require(after["proxy_files"] == r["before"]["proxy_files"], "proxy_configuration_changed")
        verify_original_runners(r["before"]["runtime_files"])
        require(after["private_files"] == r["before"]["private_files"], "configuration_changed")
        require(all(after["units"].get(p) == h for p, h in r["before"]["units"].items()
                    if p != str(dropin)), "original_units_changed")
        require(after["units"].get(str(dropin)) == hashlib.sha256(text.encode()).hexdigest(),
                "managed_unit_changed")
        save(prefix / "release-source.json", r["release"])
        receipt.update(status="deployed", running_sha256=after["live_sha256"], configuration_unchanged=True,
                       original_units_preserved=True, proxy_unchanged=True, original_runners_preserved=True)
    except Exception as error:
        receipt.update(status="failed", failure={"step": receipt.get("step"), "type": type(error).__name__, "reason": str(error) if isinstance(error, RuntimeError) else "operation_failed"})
        if isinstance(error, StageCleanupDeferred):
            receipt["cleanup_deferred"] = {"stage_state": True, "candidate": True}
        if switched:
            try:
                rollback_deadline = time.monotonic() + 90
                require(current_sha(deadline=rollback_deadline) in [None, r["daemon_sha256"], r["expected_live_sha256"]], "concurrent_runtime_change")
                receipt["rollback_runtime_restored"] = restore_runtime(folder, r["before"])
                require(all(sha(p) == h for p, h in r["before"]["units"].items() if p != str(dropin)), "original_units_changed")
                restore_dropin(dropin, r["managed_dropin_before"])
                run(["systemctl", "daemon-reload"], deadline=rollback_deadline)
                run(["systemctl", "restart", SERVICE], deadline=rollback_deadline)
                health(r["public_url"], deadline=rollback_deadline, maintenance=r.get("maintenance"))
                require(same_lease(r["before"], snapshot(deadline=rollback_deadline)), "rollback_lease_mismatch")
                status, old_html = request(r["public_url"], "/", timeout=min(10, max(1, rollback_deadline - time.monotonic())),
                                           maintenance=r.get("maintenance"))
                baseline = r["public_baseline"]
                require(status == 200 and Page(old_html).footers == baseline["footer_count"] and
                        hashlib.sha256(proxy_fragment(old_html)).hexdigest() == baseline["proxy_fragment_sha256"], "rollback_public_fingerprint")
                receipt.update(status="rolled_back", rollback_verified=True, rollback_runtime_fingerprint_verified=True)
            except Exception as rollback_error:
                receipt.update(status="rollback_failed", rollback_error=type(rollback_error).__name__)
        if created and receipt["status"] in ["failed", "rolled_back"] and not receipt.get("cleanup_deferred"):
            try:
                if current_sha(deadline=time.monotonic() + 3) != r["daemon_sha256"]:
                    shutil.rmtree(prefix)
            except Exception:
                receipt.setdefault("cleanup_deferred", {})["candidate"] = True
    finally:
        if dropin_temporary is not None:
            dropin_temporary.unlink(missing_ok=True)
        receipt["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        receipt["free_bytes_after"] = shutil.disk_usage("/var/lib").free
        save(report, receipt)
        lock.close()
    return 0 if receipt["status"] == "deployed" else 1


def restore_dropin(path, saved):
    if saved is None:
        path.unlink(missing_ok=True)
        return
    data = base64.b64decode(saved["content"], validate=True)
    require(hashlib.sha256(data).hexdigest() == saved["sha256"], "saved_unit_digest")
    temporary = path.with_suffix(".restore")
    with temporary.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.chmod(saved["mode"])
    os.replace(temporary, path)


def validate_identity(tag, commit, run_id):
    require(re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", tag), "stable_tag_required")
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "full_commit_required")
    require(re.fullmatch(r"[0-9]{1,20}-[0-9]{1,6}", run_id), "github_run_identity_required")


def fetch(url, maximum, destination=None):
    request = urllib.request.Request(url, headers={"User-Agent": "ssmcp-release-deployment", "Accept": "application/json"})
    deadline = time.monotonic() + 180
    data = bytearray()
    with urllib.request.urlopen(request, timeout=30) as response:
        while True:
            require(time.monotonic() < deadline, "download_deadline")
            part = response.read(min(1024 * 1024, maximum + 1 - len(data)))
            if not part:
                break
            data.extend(part)
            require(len(data) <= maximum, "download_size_limit")
    if destination is not None:
        with destination.open("xb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        return len(data)
    return bytes(data)



def release_source_rewrite(tag, commit, manifest, manifest_bytes):
    """Accept only the fixed retained-tree proof; never reinterpret the build."""
    require(isinstance(manifest_bytes, bytes), "release_source_mismatch")
    try:
        decoded = json.loads(manifest_bytes)
        original_json = json.dumps(decoded, sort_keys=True, separators=(",", ":"),
                                   ensure_ascii=True, allow_nan=False)
        supplied_json = json.dumps(manifest, sort_keys=True, separators=(",", ":"),
                                   ensure_ascii=True, allow_nan=False)
    except (TypeError, ValueError, UnicodeError):
        raise RuntimeError("release_source_mismatch") from None
    require(original_json == supplied_json, "release_source_mismatch")
    manifest_sha256 = hashlib.sha256(manifest_bytes).hexdigest()
    table = RELEASE_SOURCE_REWRITES
    try:
        encoded = json.dumps(table, sort_keys=True, separators=(",", ":"),
                             ensure_ascii=True, allow_nan=False).encode("ascii")
    except (TypeError, ValueError):
        raise RuntimeError("release_source_map_invalid") from None
    require(hashlib.sha256(encoded).hexdigest() == RELEASE_SOURCE_REWRITES_SHA256,
            "release_source_map_invalid")
    require(isinstance(table, dict) and set(table) == {"schema", "entries"}
            and type(table["schema"]) is int and table["schema"] == 1
            and isinstance(table["entries"], list), "release_source_map_invalid")

    def digest(value, length):
        return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{" + str(length) + "}", value)

    seen, matches = set(), []
    for row in table["entries"]:
        require(isinstance(row, dict) and set(row) == {
            "repository", "tag", "build_commit", "source_commit", "manifest_sha256", "proof"
        }, "release_source_map_invalid")
        require(row["repository"] == REPOSITORY and isinstance(row["tag"], str)
                and re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", row["tag"])
                and digest(row["build_commit"], 40) and digest(row["source_commit"], 40)
                and row["build_commit"] != row["source_commit"]
                and digest(row["manifest_sha256"], 64), "release_source_map_invalid")
        key = (row["repository"], row["tag"])
        require(key not in seen, "release_source_map_invalid")
        seen.add(key)
        proof = row["proof"]
        require(isinstance(proof, dict) and set(proof) == {
            "kind", "build_tree", "source_tree", "build_retained_sha256",
            "source_retained_sha256", "removed_paths_sha256", "removed_path_count",
            "audit_sha256"
        }, "release_source_map_invalid")
        require(proof["kind"] == "retained-git-tree-v1"
                and all(digest(proof[key], 40) for key in ("build_tree", "source_tree"))
                and all(digest(proof[key], 64) for key in (
                    "build_retained_sha256", "source_retained_sha256",
                    "removed_paths_sha256", "audit_sha256"))
                and proof["build_retained_sha256"] == proof["source_retained_sha256"]
                and type(proof["removed_path_count"]) is int
                and proof["removed_path_count"] >= 0
                and ((proof["removed_path_count"] == 0) == (proof["build_tree"] == proof["source_tree"])),
                "release_source_map_invalid")
        if (row["tag"] == tag and row["source_commit"] == commit
                and row["build_commit"] == manifest.get("commit")
                and row["manifest_sha256"] == manifest_sha256):
            matches.append(row)
    require(len(matches) == 1, "release_source_mismatch")
    proof = matches[0]["proof"]
    return {
        "kind": "generated-artifact-history-removal",
        "allowlist_sha256": RELEASE_SOURCE_REWRITES_SHA256,
        "manifest_sha256": manifest_sha256,
        "proof_kind": proof["kind"],
        "build_tree": proof["build_tree"], "source_tree": proof["source_tree"],
        "retained_tree_sha256": proof["build_retained_sha256"],
        "removed_paths_sha256": proof["removed_paths_sha256"],
        "removed_path_count": proof["removed_path_count"],
        "audit_sha256": proof["audit_sha256"],
        "artifact_rebuilt": False,
    }


def select_release(tag, commit, release, manifest, machine, *, manifest_bytes=None):
    require(release.get("tag_name") == tag and not release.get("draft") and
            not release.get("prerelease") and release.get("published_at"), "published_stable_release_required")
    require(manifest.get("package") == "ssmcp" and manifest.get("version") == tag[1:],
            "release_source_mismatch")
    rewrite = None
    if manifest.get("commit") != commit:
        rewrite = release_source_rewrite(tag, commit, manifest, manifest_bytes)
    target = {"x86_64": "x86_64-unknown-linux-gnu", "aarch64": "aarch64-unknown-linux-gnu"}.get(machine)
    require(target is not None, "unsupported_server_architecture")
    prefix = "ssmcp-" + tag[1:] + "-" + target
    names = {f"ssmcp-{target}.tar.gz", prefix + ".tar.gz"}
    records = [item for item in manifest["artifacts"] if item.get("name") in names]
    assets = [item for item in release["assets"] if item.get("name") in names]
    require(len(records) == len(assets) == 1, "one_server_archive_required")
    record, asset = records[0], assets[0]
    name = record["name"]
    require(asset["name"] == name, "archive_metadata_mismatch")
    require(re.fullmatch(r"[0-9a-f]{64}", record.get("sha256", "")) and
            type(record.get("bytes")) is int and 0 < record["bytes"] <= 134_217_728, "archive_manifest_invalid")
    url = "https://github.com/" + REPOSITORY + "/releases/download/" + tag + "/" + name
    require(asset.get("browser_download_url") == url and asset.get("size") == record["bytes"], "archive_metadata_mismatch")
    if asset.get("digest"):
        require(asset["digest"] == "sha256:" + record["sha256"], "github_archive_digest_mismatch")
    selected = {"repository": REPOSITORY, "tag": tag, "commit": commit, "archive": name,
                "archive_prefix": prefix + "/" if name == prefix + ".tar.gz" else "", "archive_sha256": record["sha256"], "archive_bytes": record["bytes"], "archive_url": url,
                "build_commit": manifest["commit"], "source_commit": commit}
    if rewrite is not None:
        selected["source_history_rewrite"] = rewrite
    return selected


def extract_runtime(archive, folder, record):
    require(archive.stat().st_size == record["archive_bytes"] and sha(archive) == record["archive_sha256"],
            "downloaded_archive_mismatch")
    prefix = record["archive_prefix"]
    wanted = {prefix + "ssmcpd": ("candidate", 0o755, 134_217_728),
              prefix + "code-mode.js": ("runner.js", 0o644, 2_097_152)}
    with tarfile.open(archive, "r:gz") as package:
        entries = package.getmembers()
        require(len(entries) <= 1000 and len({entry.name for entry in entries}) == len(entries), "archive_member_set")
        require(all(not entry.name.startswith("/") and ".." not in Path(entry.name).parts and
                    entry.name.startswith(prefix) and (entry.isfile() or entry.isdir())
                    for entry in entries), "unsafe_archive_member")
        for name, (destination, mode, maximum) in wanted.items():
            member = package.getmember(name)
            require(member.isfile() and 0 < member.size <= maximum, "runtime_archive_member")
            with package.extractfile(member) as source, (folder / destination).open("xb") as output:
                shutil.copyfileobj(source, output)
                output.flush()
                os.fsync(output.fileno())
            (folder / destination).chmod(mode)


def verify_checkpoint(path, expected):
    with tarfile.open(path, "r:") as archive:
        members = archive.getmembers()
        require(len({entry.name for entry in members}) == len(members) and
                all(entry.isfile() and not entry.name.startswith("/") and ".." not in Path(entry.name).parts
                    for entry in members), "checkpoint_member_set")
        manifest = json.load(archive.extractfile("manifest.json"))
        require(set(entry.name for entry in members) == {"manifest.json", *(entry["name"] for entry in manifest["entries"])},
                "checkpoint_manifest_set")
        for entry in manifest["entries"]:
            with archive.extractfile(entry["name"]) as source:
                require(hashlib.file_digest(source, "sha256").hexdigest() == entry["sha256"] and
                        archive.getmember(entry["name"]).size == entry["bytes"], "checkpoint_file_mismatch")
        require(same_lease(manifest["snapshot"], expected), "checkpoint_runtime_mismatch")


def configure_target():
    global PROXY_CONFIG
    settings = tomllib.loads(Path("/etc/ssmcpd/ssmcpd.toml").read_text())
    public_url = settings["server"]["public_url"].rstrip("/")
    require(re.fullmatch(r"https://[A-Za-z0-9.-]+(?::[0-9]+)?", public_url), "configured_public_origin")
    PROXY_CONFIG = "/etc/nginx/conf.d/" + urlsplit(public_url).hostname + ".conf"
    return public_url


def prepare(intent, folder):
    tag, commit = intent["tag"], intent["commit"]
    version = tag[1:]
    public_url = configure_target()
    maintenance = intent.get("maintenance")
    if maintenance is not None:
        maintenance_token(public_url, maintenance)
    release = json.loads(fetch("https://api.github.com/repos/" + REPOSITORY + "/releases/tags/" + tag, 1_048_576))
    manifest_url = "https://github.com/" + REPOSITORY + "/releases/download/" + tag + "/release-manifest.json"
    manifest_assets = [asset for asset in release.get("assets", []) if asset.get("name") == "release-manifest.json"]
    require(len(manifest_assets) == 1 and manifest_assets[0].get("browser_download_url") == manifest_url,
            "published_manifest_required")
    manifest_bytes = fetch(manifest_url, 1_048_576)
    if manifest_assets[0].get("digest"):
        require(manifest_assets[0]["digest"] == "sha256:" + hashlib.sha256(manifest_bytes).hexdigest(), "manifest_digest")
    record = select_release(tag, commit, release, json.loads(manifest_bytes), platform.machine(),
                            manifest_bytes=manifest_bytes)
    archive = folder / "release.tar.gz"
    fetch(record["archive_url"], record["archive_bytes"], archive)
    extract_runtime(archive, folder, record)
    require(run([str(folder / "candidate"), "--version"]) == "ssmcpd " + version, "release_binary_version")
    source_runner = fetch("https://raw.githubusercontent.com/" + REPOSITORY + "/" + commit + "/src/code_mode.js", 2_097_152)
    require(sha(folder / "runner.js") == hashlib.sha256(source_runner).hexdigest(), "release_runner_source_mismatch")
    assets = {}
    for name in sorted(CORE_ASSET_NAMES):
        data = fetch("https://raw.githubusercontent.com/" + REPOSITORY + "/" + commit + "/src/portal/" + name, 2_097_152)
        assets[name] = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "content_types": asset_types(name)}
    before = snapshot()
    current_version = run([before["exe"], "--version"])
    match = re.fullmatch(r"ssmcpd ([0-9]+)\.([0-9]+)\.([0-9]+)", current_version)
    require(match is not None and tuple(map(int, version.split("."))) >= tuple(map(int, match.groups())), "automatic_downgrade_refused")
    require(before["prestart_commands"] in [[], [before["exe"] + " check-runtime"]], "unreviewed_prestart_command")
    request = {"run_id": intent["run_id"], "commit": commit, "version": version, "public_url": public_url,
               "before": before, "expected_live_sha256": before["live_sha256"], "expected_live_exe": before["exe"],
               "daemon_sha256": sha(folder / "candidate"), "runner_sha256": sha(folder / "runner.js"),
               "helper_sha256": sha(__file__), "assets": assets, "release": record}
    if maintenance is not None:
        request["maintenance"] = maintenance
    if intent.get("stage_only"):
        request["stage_only"] = True
    else:
        require(not MANAGED_DROPIN.is_symlink(), "managed_dropin_symlink")
        saved = None
        if MANAGED_DROPIN.exists():
            saved = {"content": base64.b64encode(MANAGED_DROPIN.read_bytes()).decode(),
                     "sha256": sha(MANAGED_DROPIN), "mode": MANAGED_DROPIN.stat().st_mode & 0o777}
        with (folder / "backup.tar").open("xb") as stream:
            backup(before["live_sha256"], stream)
            stream.flush()
            os.fsync(stream.fileno())
        verify_checkpoint(folder / "backup.tar", before)
        request.update(backup_sha256=sha(folder / "backup.tar"),
                       public_baseline=public_baseline(public_url, maintenance=maintenance), managed_dropin_before=saved)
    save(folder / "request.json", request)
    return request


def public_receipt(receipt):
    allowed = {"schema", "status", "completed", "step", "commit", "version", "release", "target_sha256", "running_sha256",
               "expected_live_sha256", "runner_sha256", "assets", "stage_checks", "direct_checks", "public_checks",
               "started_utc", "finished_utc", "configuration_unchanged", "original_units_preserved", "proxy_unchanged",
               "original_runners_preserved", "rollback_checkpoint_verified", "rollback_verified",
               "rollback_runtime_fingerprint_verified", "rollback_runtime_restored", "failure", "rollback_error",
               "cleanup", "cleanup_deferred", "free_bytes_before", "free_bytes_after"}
    result = {key: value for key, value in receipt.items() if key in allowed}
    require(not re.search(r"/home/|/etc/|/usr/|/var/|/run/|ssh://|token=|secret=", json.dumps(result), re.I),
            "receipt_contains_private_data")
    return result


def prune_previous_managed_runtime(before, current_exe):
    """Only remove our reproducible release cache; retain all pre-existing installs."""
    previous = Path(before["exe"]).parent.parent
    if previous.parent != Path("/usr/local/lib/ssmcp") or not re.fullmatch(r"release-[0-9a-f]{12}-[0-9a-f]{12}", previous.name):
        return "not_managed"
    if Path(current_exe).is_relative_to(previous):
        return "still_current"
    source_file = previous / "release-source.json"
    if not source_file.is_file():
        return "untracked"
    source = json.loads(source_file.read_text())
    release = json.loads(fetch("https://api.github.com/repos/" + REPOSITORY + "/releases/tags/" + source["tag"], 1_048_576))
    matches = [asset for asset in release.get("assets", []) if asset.get("name") == source["archive"]]
    require(source["repository"] == REPOSITORY and len(matches) == 1 and
            matches[0].get("digest") == "sha256:" + source["archive_sha256"], "old_archive_not_recoverable")
    expected_files = {"bin/ssmcpd", "libexec/ssmcp/code-mode.js", "release-source.json"}
    require({str(path.relative_to(previous)) for path in previous.rglob("*") if path.is_file()} == expected_files and
            not any(path.is_symlink() for path in previous.rglob("*")), "old_prefix_has_unmanaged_files")
    require(sha(previous / "bin/ssmcpd") == before["live_sha256"], "old_binary_changed_before_cleanup")
    verify_original_runners({path: checksum for path, checksum in before["runtime_files"].items()
                             if Path(path).is_relative_to(previous)})
    for process in Path("/proc").glob("[0-9]*"):
        try:
            require(not os.readlink(process / "exe").startswith(str(previous) + "/") and
                    str(previous).encode() not in (process / "cmdline").read_bytes(), "old_runtime_still_referenced")
        except (FileNotFoundError, ProcessLookupError):
            continue
    for unit in snapshot()["units"]:
        require(str(previous) not in Path(unit).read_text(), "old_runtime_still_configured")
    shutil.rmtree(previous)
    return "removed_recoverable_release_cache"


def worker(intent, folder):
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(RuntimeError("worker_terminated")))
    save(folder / "receipt.json", {"status": "running", "step": "download_verified_release", "commit": intent["commit"], "version": intent["tag"][1:]})
    request_data = None
    try:
        request_data = prepare(intent, folder)
        apply(folder / "request.json")
    except Exception as error:
        existing = json.loads((folder / "receipt.json").read_text())
        if existing.get("status") == "running":
            save(folder / "receipt.json", {"status": "failed", "step": existing.get("step"),
                 "commit": intent["commit"], "version": intent["tag"][1:],
                 "failure": {"type": type(error).__name__, "reason": str(error) if isinstance(error, RuntimeError) else "operation_failed"}})
    receipt = json.loads((folder / "receipt.json").read_text())
    if receipt["status"] in ["staged", "deployed", "failed", "rolled_back"] and not receipt.get("cleanup_deferred"):
        removed = 0
        for name in ["candidate", "runner.js", "release.tar.gz", "backup.tar", "request.json"]:
            path = folder / name
            if path.is_file():
                removed += path.stat().st_size
                path.unlink()
        receipt["cleanup"] = {"temporary_bytes_removed": removed, "stage_stopped_before_removal": True,
                              "other_task_files_touched": False}
        if receipt["status"] == "deployed":
            try:
                receipt["cleanup"]["previous_managed_runtime"] = prune_previous_managed_runtime(request_data["before"], snapshot()["exe"])
            except Exception as error:
                receipt["cleanup"]["previous_managed_runtime"] = "retained_for_review"
                receipt["cleanup"]["reason"] = type(error).__name__
    receipt["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    receipt["completed"] = True
    save(folder / "receipt.json", public_receipt(receipt))
    return 0 if receipt["status"] in ["staged", "deployed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    parser.add_argument("--commit")
    parser.add_argument("--run-id")
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--stage-only", action="store_true", help="validate the candidate in isolation without switching the live service")
    parser.add_argument("--maintenance-token-file", help="root-owned 0600 file with a temporary 64-hex public-probe token")
    args = parser.parse_args()
    os.umask(0o077)
    require(os.geteuid() == 0, "root_required")
    if args.status:
        require(not args.worker and not args.stage_only and not any([args.tag, args.commit, args.run_id, args.maintenance_token_file]), "status_has_no_arguments")
        current = snapshot()
        print(json.dumps({"service_active": True, "version": run([current["exe"], "--version"]), "sha256": current["live_sha256"]}))
        return 0
    validate_identity(args.tag or "", args.commit or "", args.run_id or "")
    BASE.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(not BASE.is_symlink() and BASE.stat().st_uid == 0 and BASE.stat().st_mode & 0o777 == 0o700, "private_deployment_directory")
    folder = BASE / args.run_id
    intent = {"tag": args.tag, "commit": args.commit, "run_id": args.run_id}
    if args.stage_only:
        intent["stage_only"] = True
    if args.maintenance_token_file is not None:
        token_data = read_maintenance_token(args.maintenance_token_file)
        intent["maintenance"] = {"origin": configure_target(), "token_file": args.maintenance_token_file,
                                 "token_sha256": hashlib.sha256(token_data).hexdigest()}
    if args.worker:
        require(folder.is_dir() and not folder.is_symlink() and folder.stat().st_uid == 0 and
                json.loads((folder / "intent.json").read_text()) == intent, "owned_worker_request")
        return worker(intent, folder)
    launch = False
    try:
        folder.mkdir(mode=0o700)
        save(folder / "intent.json", intent)
        launch = True
    except FileExistsError:
        require(not folder.is_symlink() and folder.stat().st_uid == 0 and
                json.loads((folder / "intent.json").read_text()) == intent, "request_identity_conflict")
    if launch:
        unit = "ssmcp-deploy-" + args.run_id
        command = ["systemd-run", "--quiet", "--collect", "--unit=" + unit, "--property=Type=exec",
             "--property=UMask=0077", "--property=RuntimeMaxSec=900s", "--property=TimeoutStopSec=120s",
             "--property=StandardOutput=null", "--property=StandardError=journal", sys.executable, str(Path(__file__).resolve()),
             "--worker", "--tag", args.tag, "--commit", args.commit, "--run-id", args.run_id]
        if args.stage_only:
            command.append("--stage-only")
        if args.maintenance_token_file is not None:
            command.extend(["--maintenance-token-file", args.maintenance_token_file])
        run(command)
    deadline = time.monotonic() + 1020
    while time.monotonic() < deadline:
        report = folder / "receipt.json"
        if report.is_file():
            result = json.loads(report.read_text())
            if result.get("completed") is True:
                print(json.dumps(public_receipt(result), sort_keys=True))
                return 0 if result["status"] == ("staged" if args.stage_only else "deployed") else 1
        time.sleep(1)
    raise RuntimeError("deployment_outcome_pending_reconnect_same_run_id")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({"status": "failed", "failure": {"type": type(error).__name__,
                         "reason": str(error) if isinstance(error, RuntimeError) else "operation_failed"}}))
        raise SystemExit(1)

