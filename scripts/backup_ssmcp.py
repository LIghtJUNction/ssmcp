#!/usr/bin/env python3
"""Plan, capture and verify the reviewed strict-eight SSMCP backup (Linux/Python 3.11+).

No extraction, service control, deployment, or write-fence implementation lives here.
See docs/service-backup.md before authorizing a capture.
"""

import argparse
import base64
import contextlib
import datetime as dt
import gzip
import hashlib
import json
import os
from pathlib import Path
import posixpath
import re
import selectors
import shlex
import stat
import subprocess
import sys
import tarfile
import time


ROOTS = (
    "etc/ssmcpd",
    "var/lib/ssmcpd",
    "etc/systemd/system/ssmcpd.service.d",
    "usr/lib/systemd/system/ssmcpd.service",
    "etc/nginx/conf.d/ssmcp.lmm.best.conf",
    "usr/local/lib/ssmcp/release-1d6b55320fff-ad3af5835f8a",
    "usr/local/libexec/ssmcp/deploy-release",
    "usr/local/libexec/ssmcp/deploy-entry",
)
SCHEMA = "ssmcp-strict-eight-backup-v1"
BLOCK = 1024 * 1024
RESERVE = 256 * 1024 * 1024
MAX_MANIFEST = 64 * 1024 * 1024
MAX_STDERR = 1024 * 1024
PAX_META = "SSMCP.metadata"
LIMITS = {
    "xattrs": "All attributes visible to Linux listxattr/getxattr, raw base64 in SSMCP.metadata PAX; read errors fail.",
    "acl": "Linux POSIX ACL xattrs are preserved as raw bytes when exposed by the filesystem; no semantic/portable restore validation.",
    "restore": "No extraction or restoration. Standard tar does not apply the custom xattr field. A separate reviewed restore is required.",
    "links": "Internal hardlink relations retained; names outside scope sharing an inode are not inventoried. Symlink targets are never read.",
    "consistency": "Before/after inventories detect observed drift; external controlled write exclusion is required and is not proved by this tool.",
    "authenticity": "Hashes detect mismatches, not a party replacing both archive and receipts. Keep receipts in a separately trusted location.",
}


class BackupError(Exception):
    pass


def require(condition, code):
    if not condition:
        raise BackupError(code)


def encoded(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")).encode("ascii")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(stream):
    h = hashlib.sha256()
    while data := stream.read(BLOCK):
        h.update(data)
    return h.hexdigest()


def scoped(name):
    return any(name == root or name.startswith(root + "/") for root in ROOTS)


def valid_name(name):
    require(isinstance(name, str) and name and "\0" not in name
            and not name.startswith("/") and all(p not in ("", ".", "..") for p in name.split("/"))
            and scoped(name), "invalid_or_out_of_scope_member")


def valid_symlink(name, target):
    require(isinstance(target, str) and target and "\0" not in target, "invalid_symlink")
    resolved = posixpath.normpath(target if target.startswith("/") else "/" + posixpath.join(posixpath.dirname(name), target))
    require(resolved.startswith("/") and scoped(resolved[1:]), "symlink_target_outside_scope")


@contextlib.contextmanager
def directory_fd(path):
    """Open every absolute-path component without following symlinks."""
    absolute = os.path.abspath(path)
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in absolute.split("/")[1:]:
            if not part:
                continue
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


@contextlib.contextmanager
def parent_fd(root_fd, name):
    valid_name(name)
    fd = os.dup(root_fd)
    try:
        for part in name.split("/")[:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd, name.rsplit("/", 1)[-1]
    finally:
        os.close(fd)


def fingerprint(s):
    return (s.st_dev, s.st_ino, s.st_mode, s.st_uid, s.st_gid, s.st_size,
            s.st_mtime_ns, s.st_ctime_ns, s.st_nlink)


def attributes(fd_or_path, *, symlink=False):
    options = {"follow_symlinks": False} if symlink else {}
    values = {}
    for key in sorted(os.listxattr(fd_or_path, **options)):
        values[key] = base64.b64encode(os.getxattr(fd_or_path, key, **options)).decode("ascii")
    return values


def service_identity():
    """Fresh read-only binding; these identity paths are not archive members."""
    def main_pid():
        value = subprocess.run(["systemctl", "show", "ssmcpd.service", "--property=MainPID", "--value"],
                               capture_output=True, text=True, check=True, timeout=10).stdout.strip()
        require(value.isdigit() and int(value) > 0, "service_pid_unavailable")
        return int(value)
    pid = main_pid()
    proc = Path("/proc") / str(pid)
    def start_ticks():
        return int((proc / "stat").read_text().rpartition(")")[2].split()[19])
    start = start_ticks()
    executable = os.readlink(proc / "exe")
    require(executable.startswith("/") and not executable.endswith(" (deleted)")
            and scoped(executable[1:]), "running_executable_outside_scope")
    with (proc / "exe").open("rb") as stream:
        exe_sha = file_digest(stream)
    result = {"machine_id_sha256": digest(Path("/etc/machine-id").read_bytes().strip()),
              "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
              "pid": pid, "start_ticks": start, "exe_path": executable, "exe_sha256": exe_sha}
    require(main_pid() == pid and start_ticks() == start and os.readlink(proc / "exe") == executable,
            "service_identity_changed_during_read")
    return result


def validate_identity(value):
    require(isinstance(value, dict), "source_identity_required")
    for field in ("machine_id_sha256", "exe_sha256"):
        require(re.fullmatch(r"[0-9a-f]{64}", value.get(field, "")) is not None, "invalid_source_identity_digest")
    require(re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", value.get("boot_id", "")) is not None,
            "invalid_source_boot_id")
    for field in ("pid", "start_ticks"):
        require(type(value.get(field)) is int and value[field] > 0, "invalid_source_process_identity")
    path = value.get("exe_path", "")
    require(isinstance(path, str) and path.startswith("/"), "invalid_source_executable")
    valid_name(path[1:])


def inventory(base="/"):
    source_identity = service_identity() if os.fspath(base) == "/" else None
    entries = []
    with directory_fd(base) as root_fd:
        def visit(name):
            with parent_fd(root_fd, name) as (parent, leaf):
                before = os.stat(leaf, dir_fd=parent, follow_symlinks=False)
                item = {"name": name, "uid": before.st_uid, "gid": before.st_gid,
                        "mode": stat.S_IMODE(before.st_mode), "size": before.st_size,
                        "mtime_ns": before.st_mtime_ns, "ctime_ns": before.st_ctime_ns,
                        "device": before.st_dev, "inode": before.st_ino, "nlink": before.st_nlink}
                if stat.S_ISLNK(before.st_mode):
                    require(before.st_nlink == 1, "hardlinked_symlink_unsupported")
                    item.update(kind="symlink", target=os.readlink(leaf, dir_fd=parent))
                    valid_symlink(name, item["target"])
                    # The pinned directory fd prevents intermediate-path substitution;
                    # lgetxattr does not follow the final symlink.
                    item["xattrs"] = attributes(f"/proc/self/fd/{parent}/{leaf}", symlink=True)
                elif stat.S_ISDIR(before.st_mode) or stat.S_ISREG(before.st_mode):
                    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
                    if stat.S_ISDIR(before.st_mode):
                        flags |= os.O_DIRECTORY
                    fd = os.open(leaf, flags, dir_fd=parent)
                    try:
                        require(fingerprint(os.fstat(fd)) == fingerprint(before), "source_changed_before_read")
                        item["xattrs"] = attributes(fd)
                        if stat.S_ISDIR(before.st_mode):
                            item["kind"] = "directory"
                            children = sorted(os.listdir(fd))
                            entries.append(item)
                            for child in children:
                                visit(name + "/" + child)
                            require(children == sorted(os.listdir(fd)), "directory_changed_during_read")
                        else:
                            item["kind"] = "file"
                            with os.fdopen(os.dup(fd), "rb") as stream:
                                item["sha256"] = file_digest(stream)
                        require(fingerprint(os.fstat(fd)) == fingerprint(before)
                                and attributes(fd) == item["xattrs"], "source_changed_during_read")
                    finally:
                        os.close(fd)
                else:
                    raise BackupError("unsupported_source_type_including_socket")
                require(fingerprint(os.stat(leaf, dir_fd=parent, follow_symlinks=False)) == fingerprint(before),
                        "source_changed_after_read")
                if item["kind"] != "directory":
                    entries.append(item)
        for root in ROOTS:
            visit(root)
    entries.sort(key=lambda item: item["name"])
    inodes = {}
    for item in entries:
        if item["kind"] == "file":
            key = (item["device"], item["inode"])
            if key in inodes:
                item["hardlink"] = inodes[key]
            else:
                inodes[key] = item["name"]
    if source_identity is not None:
        require(source_identity == service_identity(), "service_identity_changed_during_inventory")
    result = {"schema": SCHEMA, "roots": list(ROOTS), "entries": entries, "limits": LIMITS,
              "source_identity": source_identity}
    validate_inventory(result)
    return result


def validate_inventory(value):
    require(value.get("schema") == SCHEMA and value.get("roots") == list(ROOTS)
            and value.get("limits") == LIMITS, "inventory_schema_or_scope_mismatch")
    seen = {}
    for item in value["entries"]:
        name = item["name"]
        valid_name(name)
        require(name not in seen, "duplicate_inventory_member")
        require(item["kind"] in ("directory", "file", "symlink"), "invalid_inventory_type")
        for field in ("uid", "gid", "mode", "size", "mtime_ns", "ctime_ns", "device", "inode", "nlink"):
            require(type(item[field]) is int and (field in ("mtime_ns", "ctime_ns") or item[field] >= 0), "invalid_inventory_metadata")
        require(item["mode"] <= 0o7777, "invalid_inventory_mode")
        require(isinstance(item["xattrs"], dict), "invalid_inventory_xattrs")
        for key, val in item["xattrs"].items():
            require(isinstance(key, str) and key and "\0" not in key, "invalid_xattr_name")
            base64.b64decode(val, validate=True)
        if item["kind"] == "symlink":
            valid_symlink(name, item["target"])
        if item["kind"] == "file":
            require(re.fullmatch(r"[0-9a-f]{64}", item["sha256"]) is not None, "invalid_content_digest")
        if "hardlink" in item:
            target = seen.get(item["hardlink"])
            require(item["kind"] == "file" and target is not None and target["kind"] == "file"
                    and "hardlink" not in target, "invalid_hardlink_target")
            require(all(item[k] == target[k] for k in ("uid", "gid", "mode", "size", "mtime_ns", "ctime_ns", "device", "inode", "nlink", "sha256", "xattrs")),
                    "hardlink_metadata_mismatch")
        seen[name] = item
    require(set(ROOTS) <= seen.keys(), "missing_scope_root")
    for name in seen:
        if name not in ROOTS:
            require(posixpath.dirname(name) in seen and seen[posixpath.dirname(name)]["kind"] == "directory", "invalid_member_parent")
    if value.get("source_identity") is not None:
        identity = value["source_identity"]
        validate_identity(identity)
        executable = seen.get(identity["exe_path"][1:])
        require(executable is not None and executable["kind"] == "file"
                and executable["sha256"] == identity["exe_sha256"], "running_executable_inventory_mismatch")
    return seen


def tar_info(item):
    info = tarfile.TarInfo(item["name"])
    info.uid, info.gid, info.mode = item["uid"], item["gid"], item["mode"]
    seconds, nanos = divmod(abs(item["mtime_ns"]), 1_000_000_000)
    info.mtime = item["mtime_ns"] // 1_000_000_000
    sign = "-" if item["mtime_ns"] < 0 else ""
    info.pax_headers = {"mtime": f"{sign}{seconds}.{nanos:09d}",
                        PAX_META: encoded({"schema": SCHEMA, "xattr_encoding": "base64", "entry": item}).decode("ascii")}
    if item["kind"] == "directory":
        info.type = tarfile.DIRTYPE
    elif item["kind"] == "symlink":
        info.type, info.linkname = tarfile.SYMTYPE, item["target"]
    elif "hardlink" in item:
        info.type, info.linkname = tarfile.LNKTYPE, item["hardlink"]
    else:
        info.type, info.size = tarfile.REGTYPE, item["size"]
    return info


def archive_bound(value):
    raw = 10240
    for item in value["entries"]:
        info = tar_info(item)
        raw += len(info.tobuf(format=tarfile.PAX_FORMAT, encoding="utf-8", errors="surrogateescape"))
        raw += ((info.size + 511) // 512) * 512
    # Larger than deflate's incompressible overhead, plus gzip header/footer.
    return raw + raw // 1000 + 65536


def capture_stream(output, expected, base="/"):
    before = inventory(base)
    require(digest(encoded(before)) == expected, "source_changed_since_inventory")
    with directory_fd(base) as root_fd:
        with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                for item in before["entries"]:
                    info = tar_info(item)
                    if info.isreg():
                        with parent_fd(root_fd, item["name"]) as (parent, leaf):
                            fd = os.open(leaf, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
                            with os.fdopen(fd, "rb") as stream:
                                current = os.fstat(stream.fileno())
                                require(stat.S_ISREG(current.st_mode) and current.st_dev == item["device"]
                                        and current.st_ino == item["inode"] and current.st_size == item["size"], "archive_source_identity_changed")
                                archive.addfile(info, stream)
                    else:
                        archive.addfile(info)
    require(before == inventory(base), "source_changed_during_archive")


def verify_archive(path, value, expected_sha256):
    expected = validate_inventory(value)
    bound = archive_bound(value)
    require(re.fullmatch(r"[0-9a-f]{64}", expected_sha256) is not None, "invalid_archive_digest")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as raw:
        initial = os.fstat(raw.fileno())
        require(stat.S_ISREG(initial.st_mode), "archive_not_regular")
        require(file_digest(raw) == expected_sha256, "archive_digest_mismatch")
        raw.seek(0)
        require(os.fstat(raw.fileno()).st_size <= bound, "archive_exceeds_bound")
        # Bound decompression before tarfile can allocate an oversized PAX body.
        with gzip.GzipFile(fileobj=raw, mode="rb") as checked:
            size = 0
            while data := checked.read(BLOCK):
                size += len(data)
                require(size <= bound, "uncompressed_archive_exceeds_bound")
        raw.seek(0)
        seen = set()
        with gzip.GzipFile(fileobj=raw, mode="rb") as compressed:
            # Seekable mode lets us inspect every byte after the last tar member,
            # including both zero end blocks and the gzip checksum/footer.
            with tarfile.open(fileobj=compressed, mode="r:") as archive:
                for member in archive:
                    valid_name(member.name)
                    require(member.name not in seen and member.name in expected, "extra_or_duplicate_archive_member")
                    item = expected[member.name]
                    info = tar_info(item)
                    require(member.type == info.type and member.uid == info.uid and member.gid == info.gid
                            and member.uname == info.uname and member.gname == info.gname
                            and member.mode == info.mode and member.size == info.size
                            and member.linkname == info.linkname, "archive_metadata_mismatch")
                    require(member.pax_headers.get(PAX_META) == info.pax_headers[PAX_META]
                            and member.pax_headers.get("mtime") == info.pax_headers["mtime"], "archive_pax_metadata_mismatch")
                    require(set(member.pax_headers) <= {PAX_META, "mtime", "path", "linkpath", "uid", "gid", "size", "hdrcharset"},
                            "unsupported_archive_pax_field")
                    if member.isreg():
                        with archive.extractfile(member) as stream:
                            require(file_digest(stream) == item["sha256"], "archive_content_mismatch")
                    seen.add(member.name)
                end = archive.offset
            require(seen == expected.keys(), "missing_archive_members")
            compressed.seek(end)
            tail = 0
            while data := compressed.read(BLOCK):
                tail += len(data)
                require(not any(data) and tail <= 10240, "nonzero_or_excessive_tar_tail")
            require(tail >= 1024 and tail % 512 == 0, "missing_tar_end_blocks")
        require(fingerprint(os.fstat(raw.fileno())) == fingerprint(initial), "archive_changed_during_verification")
    return {"members": len(seen), "archive_sha256": expected_sha256,
            "inventory_sha256": digest(encoded(value)), "archive_consistency": "verified_against_inventory",
            "archive_content_verified": True, "metadata_recorded": True, "metadata_restore_verified": False,
            "write_exclusion": "operator_attestation_only", "service_switch_authorized": False}


def private_file(directory, name):
    fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=directory)
    os.fchmod(fd, 0o600)
    return os.fdopen(fd, "wb")


def save_private(directory, name, value):
    with private_file(directory, name) as stream:
        stream.write(encoded(value) + b"\n")
        stream.flush()
        os.fsync(stream.fileno())


def private_read(path, maximum=MAX_MANIFEST):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as stream:
        meta = os.fstat(stream.fileno())
        require(stat.S_ISREG(meta.st_mode) and stat.S_IMODE(meta.st_mode) == 0o600
                and meta.st_uid == os.geteuid() and meta.st_size <= maximum, "private_input_permissions_or_size")
        value = stream.read(maximum + 1)
        require(len(value) <= maximum, "private_input_too_large")
        return json.loads(value)


def validate_window(value, host):
    require(value.get("schema") == "ssmcp-controlled-write-window-v1" and value.get("host") == host
            and value.get("roots") == list(ROOTS), "window_scope_mismatch")
    require(value.get("accounts_orders_authorization_writes_blocked") is True
            and value.get("archive_execution_authorized") is True
            and value.get("service_switch_authorized") is False, "controlled_write_window_required")
    for field in ("approval_reference", "write_fence_evidence"):
        require(isinstance(value.get(field), str) and value[field].strip(), "window_evidence_required")
    require(re.fullmatch(r"[0-9a-f]{64}", value.get("preflight_sha256", "")) is not None, "fresh_preflight_required")
    validate_identity(value.get("source_identity"))
    now = dt.datetime.now(dt.timezone.utc)
    times = {key: dt.datetime.fromisoformat(value[key].replace("Z", "+00:00"))
             for key in ("started_at_utc", "expires_at_utc", "preflight_at_utc")}
    require(all(t.utcoffset() is not None for t in times.values()), "window_timezone_required")
    require(times["started_at_utc"] <= now < times["expires_at_utc"]
            and 0 <= (now - times["preflight_at_utc"]).total_seconds() <= 1800
            and 0 < (times["expires_at_utc"] - times["started_at_utc"]).total_seconds() <= 1800,
            "window_or_preflight_expired")


def ssh_command(host, known_hosts, mode, expected=None):
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", host) is not None, "invalid_ssh_alias")
    source = Path(__file__).read_text()
    remote = ["sudo", "-n", "python3", "-B", "-c", source, mode]
    if expected:
        remote.append(expected)
    return ["ssh", "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no", "-o", "ConnectTimeout=6", "-o", "ConnectionAttempts=1",
            "-o", "ControlMaster=no", "-o", "ControlPath=none", "-o", f"UserKnownHostsFile={known_hosts}",
            host, shlex.join(remote)]


def run_stream(command, output, errors, maximum, directory, timeout):
    """No shell and no retries. A nonzero/unknown producer never creates success."""
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    totals = {"stdout": 0, "stderr": 0}
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ, "stdout")
            selector.register(process.stderr, selectors.EVENT_READ, "stderr")
            while selector.get_map():
                remaining = deadline - time.monotonic()
                require(remaining > 0, "producer_timeout_outcome_unknown_do_not_replay")
                for key, _ in selector.select(min(remaining, 1)):
                    data = os.read(key.fd, BLOCK)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    channel = key.data
                    totals[channel] += len(data)
                    limit = maximum if channel == "stdout" else MAX_STDERR
                    require(totals[channel] <= limit, channel + "_size_limit")
                    free = os.fstatvfs(directory)
                    require(free.f_bavail * free.f_frsize >= RESERVE + len(data), "output_free_space_limit")
                    (output if channel == "stdout" else errors).write(data)
        require(process.wait(timeout=max(0.1, deadline - time.monotonic())) == 0, "producer_nonzero_exit")
        for stream in (output, errors):
            stream.flush()
            os.fsync(stream.fileno())
        return totals["stdout"]
    finally:
        process.stdout.close()
        process.stderr.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def capture(args):
    window = private_read(args.window, 1024 * 1024)
    validate_window(window, args.host)
    require(Path(args.known_hosts).is_file(), "known_hosts_required")
    output = Path(os.path.abspath(args.output))
    with directory_fd(output.parent) as parent:
        meta = os.fstat(parent)
        require(stat.S_IMODE(meta.st_mode) == 0o700 and meta.st_uid == os.geteuid(), "private_parent_required")
        os.mkdir(output.name, 0o700, dir_fd=parent)  # Exclusive; an existing destination is never reused.
        os.fsync(parent)
        directory = os.open(output.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    try:
        os.fchmod(directory, 0o700)
        save_private(directory, "write-window.json", window)
        with private_file(directory, "inventory.partial.json") as out, private_file(directory, "inventory.stderr") as err:
            run_stream(ssh_command(args.host, args.known_hosts, "_inventory"), out, err, MAX_MANIFEST, directory, args.timeout)
        value = private_read(f"/proc/self/fd/{directory}/inventory.partial.json")
        validate_inventory(value)
        require(value.get("source_identity") == window["source_identity"], "source_identity_changed_since_preflight")
        free = os.fstatvfs(directory)
        bound = archive_bound(value)
        require(free.f_bavail * free.f_frsize >= bound + 2 * len(encoded(value)) + 2 * MAX_STDERR + RESERVE, "insufficient_free_space")
        validate_window(window, args.host)
        with private_file(directory, "archive.partial.tar.gz") as out, private_file(directory, "capture.stderr") as err:
            run_stream(ssh_command(args.host, args.known_hosts, "_archive", digest(encoded(value))), out, err, bound, directory, args.timeout)
        validate_window(window, args.host)
        archive_path = f"/proc/self/fd/{directory}/archive.partial.tar.gz"
        with open(archive_path, "rb") as stream:
            archive_sha = file_digest(stream)
        receipt = verify_archive(archive_path, value, archive_sha)
        receipt.update(schema=SCHEMA, source_host=args.host, write_window_sha256=digest(encoded(window)),
                       archive_bytes=os.stat("archive.partial.tar.gz", dir_fd=directory).st_size,
                       producer_exit=0, verified_at_utc=dt.datetime.now(dt.timezone.utc).isoformat(), limits=LIMITS,
                       publication_durability="data_and_ready_receipt_synced; final_marker_name_not_directory_synced")
        save_private(directory, "inventory.json", value)
        # Link-then-unlink is exclusive even if an unexpected destination appears.
        os.link("archive.partial.tar.gz", "archive.tar.gz", src_dir_fd=directory, dst_dir_fd=directory, follow_symlinks=False)
        os.unlink("archive.partial.tar.gz", dir_fd=directory)
        save_private(directory, "verification.ready.json", receipt)
        os.fsync(directory)
        # Commit point: all fallible writes/syncs precede this exclusive publish.
        # Keep the ready inode. No later mutation can turn success into failure.
        # A power loss may lose this last name (a conservative false negative),
        # so we explicitly do not promise durability of the success marker name.
        os.link("verification.ready.json", "verified.json", src_dir_fd=directory, dst_dir_fd=directory, follow_symlinks=False)
        return receipt
    except Exception:
        # Retain private partials; never advertise an incomplete bundle as verified.
        raise
    finally:
        # This read-only descriptor has no pending data; its cleanup is not a
        # second commit step and must not mask a committed outcome or an error.
        with contextlib.suppress(OSError):
            os.close(directory)


def verify_bundle(path):
    with directory_fd(path) as directory:
        meta = os.fstat(directory)
        require(stat.S_IMODE(meta.st_mode) == 0o700 and meta.st_uid == os.geteuid(), "private_bundle_required")
        prefix = f"/proc/self/fd/{directory}"
        value = private_read(prefix + "/inventory.json")
        receipt = private_read(prefix + "/verified.json")
        window = private_read(prefix + "/write-window.json", 1024 * 1024)
        require(receipt["schema"] == SCHEMA and receipt["producer_exit"] == 0
                and receipt["service_switch_authorized"] is False
                and receipt["metadata_restore_verified"] is False
                and value.get("source_identity") == window["source_identity"]
                and receipt["inventory_sha256"] == digest(encoded(value))
                and receipt["write_window_sha256"] == digest(encoded(window)), "receipt_mismatch")
        s = os.stat("archive.tar.gz", dir_fd=directory, follow_symlinks=False)
        require(stat.S_ISREG(s.st_mode) and stat.S_IMODE(s.st_mode) == 0o600
                and s.st_uid == os.geteuid() and s.st_size == receipt["archive_bytes"], "archive_permissions_or_size")
        result = verify_archive(prefix + "/archive.tar.gz", value, receipt["archive_sha256"])
        result["publication_durability"] = receipt["publication_durability"]
        return result


def plan(args):
    return {"mode": "plan", "network_calls": 0, "writes": 0, "roots": ["/" + r for r in ROOTS],
            "source_bytes": "unknown", "source_identity": "unknown", "source_stability": "unknown", "free_bytes": "unknown",
            "excluded": ["/run and sockets", "symlink targets outside strict scope", "other services"],
            "future_commands": ["ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no <host> sudo -n python3 <reviewed-source> _inventory",
                                "ssh <same-options> <host> sudo -n python3 <reviewed-source> _archive <inventory-sha256>",
                                "python3 scripts/backup_ssmcp.py verify <private-bundle>"],
            "unmet_conditions": ["Explicit capture --execute with a new --output and private --window record",
                                 "Fresh preflight and approved controlled exclusion of account/order/authorization writes throughout capture",
                                 "Existing strict known_hosts and SSH agent; working sudo -n and Linux Python 3.11+",
                                 "All eight roots present; readable xattrs; no unsupported file types or escaping links",
                                 "Private 0700 destination parent, sufficient measured free space, external custody of receipt"],
            "archive_consistency": "not_evaluated", "service_switch_authorized": False, "limits": LIMITS}


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] in ("_inventory", "_archive"):
        require(os.geteuid() == 0, "remote_root_required")
        if argv[0] == "_inventory":
            sys.stdout.buffer.write(encoded(inventory()) + b"\n")
        else:
            require(len(argv) == 2, "expected_inventory_digest_required")
            capture_stream(sys.stdout.buffer, argv[1])
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", nargs="?", choices=("plan", "capture", "verify"), default="plan")
    parser.add_argument("bundle", nargs="?")
    parser.add_argument("--execute", action="store_true")
    parser.add_argument("--output")
    parser.add_argument("--window")
    parser.add_argument("--host", default="archczy")
    parser.add_argument("--known-hosts", default=str(Path.home() / ".ssh/known_hosts_archczy"))
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args(argv)
    if args.mode == "verify":
        require(args.bundle is not None, "bundle_required")
        result = verify_bundle(args.bundle)
    elif args.mode == "capture" and args.execute:
        require(args.output and args.window and 1 <= args.timeout <= 1800, "capture_arguments_required")
        os.umask(0o077)
        result = capture(args)
    else:
        result = plan(args)
    print(json.dumps(result, sort_keys=True, ensure_ascii=True, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        # Paths, SSH output, config contents and xattrs stay out of console logs.
        code = str(error) if isinstance(error, BackupError) else type(error).__name__
        print(json.dumps({"status": "failed", "code": code, "service_switch_authorized": False}), file=sys.stderr)
        raise SystemExit(1)
