"""Owned high-range Xvfb fixtures sharing the product's per-UID allocator lock."""
from __future__ import annotations
import contextlib
import fcntl
import os
from pathlib import Path
import selectors
import secrets
import shutil
import socket
import stat
import struct
import subprocess
import time

def trusted_parent(metadata):
    assert stat.S_ISDIR(metadata.st_mode) and metadata.st_uid in (0, os.geteuid())
    assert not metadata.st_mode & 0o022 or metadata.st_mode & stat.S_ISVTX


@contextlib.contextmanager
def display_allocation_lock():
    """Use exactly the stable UID lock used by the Rust private desktop owner."""
    with contextlib.ExitStack() as stack:
        def open_fd(path, flags, mode=0o600, **kwargs):
            fd = os.open(path, flags | os.O_NOFOLLOW | os.O_CLOEXEC, mode, **kwargs)
            stack.callback(os.close, fd)
            return fd

        parent = open_fd("/tmp", os.O_RDONLY | os.O_DIRECTORY)
        trusted_parent(os.fstat(parent))
        name = "ssmcp-x11-" + str(os.geteuid())
        try:
            os.mkdir(name, mode=0o700, dir_fd=parent)
        except FileExistsError:
            pass
        directory = open_fd(name, os.O_RDONLY | os.O_DIRECTORY, dir_fd=parent)
        metadata = os.fstat(directory)
        assert metadata.st_uid == os.geteuid() and stat.S_IMODE(metadata.st_mode) == 0o700
        lock = open_fd("allocate.lock", os.O_RDWR | os.O_CREAT | os.O_NONBLOCK, dir_fd=directory)
        owned = os.fstat(lock)
        assert stat.S_ISREG(owned.st_mode) and owned.st_uid == os.geteuid()
        assert stat.S_IMODE(owned.st_mode) == 0o600 and owned.st_nlink == 1
        deadline = time.monotonic() + 5
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                assert time.monotonic() < deadline, "private display allocation lock timeout"
                time.sleep(0.02)
        for expected, found in (
            (metadata, os.stat(name, dir_fd=parent, follow_symlinks=False)),
            (owned, os.stat("allocate.lock", dir_fd=directory, follow_symlinks=False)),
        ):
            assert (expected.st_dev, expected.st_ino) == (found.st_dev, found.st_ino)
        yield


def reserved_path(path):
    try:
        os.lstat(path)
        return True
    except FileNotFoundError:
        return False


def unix_endpoints():
    with open("/proc/net/unix", "rb") as stream:
        data = stream.read(4 * 1024 * 1024 + 1)
    assert len(data) <= 4 * 1024 * 1024
    return {parts[7] for line in data.decode(errors="replace").splitlines()[1:]
            if len(parts := line.split()) >= 8}


def high_display_number():
    try:
        trusted_parent(os.lstat("/tmp/.X11-unix"))
    except FileNotFoundError:
        pass
    endpoints = unix_endpoints()
    try:
        host = int(os.environ.get("DISPLAY", "").rsplit(":", 1)[-1].split(".", 1)[0].split("_", 1)[0])
    except ValueError:
        host = None
    start = secrets.randbelow(20_000)
    for offset in range(64):
        number = 30_000 + (start + offset) % 20_000
        canonical = f"/tmp/.X11-unix/X{number}"
        names = (canonical, canonical + "_")
        if number != host and not any(reserved_path(path) for path in (*names, f"/tmp/.X{number}-lock", f"/tmp/.X{number}_-lock")):
            if not any(path in endpoints or "@" + path in endpoints for path in names):
                return number
    raise AssertionError("no free high display among 64 candidates")


def ready_line(stream, timeout=10):
    fd = stream.fileno()
    blocking = os.get_blocking(fd)
    os.set_blocking(fd, False)
    deadline, data = time.monotonic() + timeout, bytearray()
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(fd, selectors.EVENT_READ)
            while b"\n" not in data:
                remaining = deadline - time.monotonic()
                assert remaining > 0 and selector.select(remaining), "private display startup timeout"
                chunk = os.read(fd, 65 - len(data))
                assert chunk, "private Xvfb exited without readiness"
                data.extend(chunk)
                assert len(data) <= 64, "oversized private display readiness"
        return bytes(data)
    finally:
        os.set_blocking(fd, blocking)


def peer_credentials(address):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(1)
        connection.connect(address)
        return struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")))


class PrivateXvfb:
    def __init__(self, directory, width=640, height=480):
        self.directory = Path(directory)
        self.width, self.height = width, height
        self.process = self.number = self.pin = None
        self.socket_path = None

    @property
    def environment(self):
        assert self.number is not None
        env = dict(os.environ, DISPLAY=f":{self.number}",
                   XAUTHORITY=str(self.directory / "Xauthority"),
                   ZUSE_BACKEND="x11", XDG_SESSION_TYPE="x11")
        for variable in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE"):
            env.pop(variable, None)
        return env

    def start(self):
        assert self.process is None
        self.directory.mkdir(mode=0o700)
        authority = self.directory / "Xauthority"
        with display_allocation_lock():
            self.number = number = high_display_number()
            fields = (b"", str(number).encode(), b"MIT-MAGIC-COOKIE-1", secrets.token_bytes(16))
            record = b"\xff\xff" + b"".join(struct.pack("!H", len(field)) + field for field in fields)
            fd = os.open(authority, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            with os.fdopen(fd, "wb") as stream:
                stream.write(record)
            process = self.process = subprocess.Popen([
                shutil.which("Xvfb"), f":{number}", "-displayfd", "1", "-nopn",
                "-screen", "0", f"{self.width}x{self.height}x24", "-nolisten", "tcp", "-noreset", "-auth", str(authority),
            ], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            pin = None
            try:
                assert ready_line(process.stdout) == f"{number}\n".encode(), "Xvfb must announce exactly the requested high display"
                assert process.poll() is None
                trusted_parent(os.lstat("/tmp/.X11-unix"))
                path = Path(f"/tmp/.X11-unix/X{number}")
                pin = os.open(path, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC)
                metadata = os.fstat(pin)
                assert stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == os.geteuid()
                peer = peer_credentials(str(path))
                assert peer[:2] == (process.pid, os.geteuid()), "filesystem socket must belong to this exact Xvfb child"
                named = os.lstat(path)
                assert (metadata.st_dev, metadata.st_ino) == (named.st_dev, named.st_ino)
                assert process.poll() is None
                self.socket_path, self.pin = path, pin
                pin = None
                endpoints = unix_endpoints()
                if "@" + str(path) in endpoints:
                    assert peer_credentials("\0" + str(path)) == peer, "both X11 endpoints must belong to one child"
                assert not any(reserved_path(value) for value in (str(path) + "_", f"/tmp/.X{number}-lock", f"/tmp/.X{number}_-lock"))
                assert str(path) + "_" not in endpoints and "@" + str(path) + "_" not in endpoints
                named = os.lstat(path)
                assert (metadata.st_dev, metadata.st_ino) == (named.st_dev, named.st_ino)
                assert process.poll() is None
            except BaseException:
                self.close()
                raise
            finally:
                if pin is not None:
                    os.close(pin)
        return self

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.pin is not None:
            assert self.process.poll() is not None, "never remove state of a live display"
            try:
                try:
                    current, owned = os.lstat(self.socket_path), os.fstat(self.pin)
                    if stat.S_ISSOCK(current.st_mode) and (current.st_dev, current.st_ino) == (owned.st_dev, owned.st_ino):
                        self.socket_path.unlink()
                except FileNotFoundError:
                    pass
            finally:
                os.close(self.pin)
                self.pin = None
