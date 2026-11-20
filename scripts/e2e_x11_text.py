#!/usr/bin/env python3
"""Real SSMCP worker / GTK text regressions on an owned X11 display.

Uses the repository's private_xvfb.py; compiles only a tiny GTK receiver, never
Cargo or the application. Run once per baseline/fixed ELF into separate evidence
directories. Direct worker IPC proves its own cleanup, not a persistent Computer
release tracker; run_mcp_recovery() below supplies that case for the existing
authenticated MCP harness using the same receiver and private environment.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


# Adapted from zuse@d9307690/tests/native_x11_utf8_fixture.c. GTK performs the
# actual text editing; the pipe barrier observes a real changed callback.
RECEIVER_C = r'''
#include <gtk/gtk.h>
#include <gdk/gdkx.h>
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/stat.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static GtkWidget *window, *entry, *other, *other_entry;
static Display *display;
static int changes, blocked, changed_fd, resume_fd;
static void changed(GtkEditable *editable, gpointer data) {
    (void)editable; (void)data; changes++;
    if (blocked) {
        blocked = 0;
        if (write(changed_fd, "1", 1) != 1) _exit(4);
        char byte; ssize_t count;
        do { count = read(resume_fd, &byte, 1); } while (count < 0 && errno == EINTR);
        if (count != 1) _exit(5);
    }
}
static void state(void) {
    const char *value = gtk_entry_get_text(GTK_ENTRY(entry));
    const char *untouched = gtk_entry_get_text(GTK_ENTRY(other_entry));
    gchar *a = g_base64_encode((const guchar *)value, strlen(value));
    gchar *b = g_base64_encode((const guchar *)untouched, strlen(untouched));
    int start = -1, end = -1;
    gtk_editable_get_selection_bounds(GTK_EDITABLE(entry), &start, &end);
    printf("STATE %s %s %d %d %d %d\n", *a ? a : "-", *b ? b : "-",
        gtk_editable_get_position(GTK_EDITABLE(entry)), start, end, changes);
    fflush(stdout); g_free(a); g_free(b);
}
static void context(void) {
    const char *names[] = {"DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS", "AT_SPI_DISPLAY"};
    fputs("CONTEXT", stdout);
    for (unsigned int i = 0; i < 4; i++) {
        const char *value = getenv(names[i]);
        gchar *encoded = g_base64_encode((const guchar *)(value ? value : ""), value ? strlen(value) : 0);
        printf(" %s", *encoded ? encoded : "-"); g_free(encoded);
    }
    Atom actual; int format; unsigned long length, after; unsigned char *bytes = NULL;
    int result = XGetWindowProperty(display, DefaultRootWindow(display),
        XInternAtom(display, "AT_SPI_BUS", False), 0, 2048, False, AnyPropertyType,
        &actual, &format, &length, &after, &bytes);
    gchar *encoded = g_base64_encode(bytes, result == Success && format == 8 && !after ? length : 0);
    printf(" %s", *encoded ? encoded : "-"); g_free(encoded); if (bytes) XFree(bytes);
    puts(""); fflush(stdout);
}
static int control_fifo(const char *directory, const char *name, int flags) {
    char path[PATH_MAX]; struct stat before, opened;
    if (snprintf(path, sizeof path, "%s/%s", directory, name) >= (int)sizeof path) return -1;
    if (lstat(path, &before) || !S_ISFIFO(before.st_mode) || before.st_uid != getuid() ||
        (before.st_mode & 0777) != 0600) return -1;
    int fd = open(path, flags | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    if (fstat(fd, &opened) || opened.st_dev != before.st_dev || opened.st_ino != before.st_ino) {
        close(fd); return -1;
    }
    return fd;
}
static gboolean command(GIOChannel *channel, GIOCondition condition, gpointer data) {
    (void)data;
    if (condition & G_IO_HUP) { gtk_main_quit(); return FALSE; }
    gchar *line = NULL;
    if (g_io_channel_read_line(channel, &line, NULL, NULL, NULL) != G_IO_STATUS_NORMAL) return TRUE;
    g_strchomp(line);
    if (!strcmp(line, "STATE")) { state(); g_free(line); return TRUE; }
    if (!strcmp(line, "CONTEXT")) { context(); g_free(line); return TRUE; }
    if (!strncmp(line, "RESET ", 6)) {
        gsize length; gchar *raw = (gchar *)g_base64_decode(line + 6, &length);
        gchar *value = g_strndup(raw, length); g_free(raw); blocked = 0;
        gtk_entry_set_text(GTK_ENTRY(entry), value); g_free(value);
        gtk_entry_set_text(GTK_ENTRY(other_entry), "other window untouched");
        gtk_editable_set_position(GTK_EDITABLE(entry), -1);
        gtk_widget_grab_focus(entry);
        gdk_window_focus(gtk_widget_get_window(window), GDK_CURRENT_TIME);
        changes = 0;
    } else if (!strncmp(line, "SELECT ", 7)) {
        int a, b;
        if (sscanf(line + 7, "%d %d", &a, &b) == 2)
            gtk_editable_select_region(GTK_EDITABLE(entry), a, b);
    } else if (!strcmp(line, "BLOCK")) {
        if (changed_fd < 0 || resume_fd < 0) { fputs("BLOCK requires inherited test pipes\n", stderr); exit(3); }
        blocked = 1;
    } else if (!strncmp(line, "PID ", 4)) {
        unsigned long pid = strtoul(line + 4, NULL, 10);
        XChangeProperty(display, GDK_WINDOW_XID(gtk_widget_get_window(window)),
            XInternAtom(display, "_NET_WM_PID", False), XA_CARDINAL, 32,
            PropModeReplace, (const unsigned char *)&pid, 1);
    } else if (!strncmp(line, "MACHINE ", 8)) {
        char hostname[256] = {0};
        if (!strcmp(line + 8, "local")) gethostname(hostname, sizeof hostname - 1);
        else snprintf(hostname, sizeof hostname, "%s", line + 8);
        XChangeProperty(display, GDK_WINDOW_XID(gtk_widget_get_window(window)),
            XInternAtom(display, "WM_CLIENT_MACHINE", False), XA_STRING, 8,
            PropModeReplace, (const unsigned char *)hostname, strlen(hostname));
    } else { fprintf(stderr, "unknown command: %s\n", line); exit(3); }
    XSync(display, False);
    while (gtk_events_pending()) gtk_main_iteration();
    puts("OK"); fflush(stdout); g_free(line); return TRUE;
}
int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "--control-dir")) {
        int input = control_fifo(argv[2], "commands", O_RDONLY);
        int output = control_fifo(argv[2], "replies", O_WRONLY);
        if (input < 0 || output < 0 || dup2(input, 0) < 0 || dup2(output, 1) < 0) return 2;
        close(input); close(output); changed_fd = resume_fd = -1;
        argc = 1; argv[1] = NULL;
    } else {
        const char *a = getenv("SSMCP_FIXTURE_CHANGED_FD"), *b = getenv("SSMCP_FIXTURE_RESUME_FD");
        if (!a || !b) return 2;
        changed_fd = atoi(a); resume_fd = atoi(b);
    }
    gtk_init(&argc, &argv);
    display = gdk_x11_display_get_xdisplay(gdk_display_get_default());
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "ssmcp GTK UTF-8 receiver");
    gtk_window_set_default_size(GTK_WINDOW(window), 480, 150);
    gtk_window_move(GTK_WINDOW(window), 20, 30);
    entry = gtk_entry_new(); gtk_container_add(GTK_CONTAINER(window), entry);
    atk_object_set_name(gtk_widget_get_accessible(entry), "SSMCP UTF-8 editable");
    g_signal_connect(entry, "changed", G_CALLBACK(changed), NULL);
    other = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(other), "ssmcp GTK untouched window");
    gtk_window_set_default_size(GTK_WINDOW(other), 350, 120);
    gtk_window_move(GTK_WINDOW(other), 540, 30);
    other_entry = gtk_entry_new(); gtk_container_add(GTK_CONTAINER(other), other_entry);
    gtk_widget_get_accessible(other_entry);
    gtk_entry_set_text(GTK_ENTRY(other_entry), "other window untouched");
    gtk_widget_show_all(other); gtk_widget_show_all(window);
    gtk_widget_grab_focus(entry); gdk_window_focus(gtk_widget_get_window(window), GDK_CURRENT_TIME);
    GIOChannel *input = g_io_channel_unix_new(0);
    g_io_add_watch(input, G_IO_IN | G_IO_HUP, command, NULL);
    printf("READY %lu %lu %d\n", GDK_WINDOW_XID(gtk_widget_get_window(window)),
        GDK_WINDOW_XID(gtk_widget_get_window(other)), getpid()); fflush(stdout);
    gtk_main(); g_io_channel_unref(input); return 0;
}
'''


def require(condition, description):
    if not condition:
        raise AssertionError(description)


def file_sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def build_receiver(root, evidence):
    source, executable = root / "receiver.c", root / "receiver"
    source.write_text(RECEIVER_C, encoding="utf-8")
    flags = subprocess.check_output(["pkg-config", "--cflags", "--libs", "gtk+-3.0", "x11"], text=True)
    command = [shutil.which("cc"), "-std=c11", "-D_DEFAULT_SOURCE", "-O0", str(source), "-o", str(executable), *shlex.split(flags)]
    built = subprocess.run(command, capture_output=True, text=True, timeout=30)
    (evidence / "receiver-build.log").write_text(built.stdout + built.stderr, encoding="utf-8")
    require(built.returncode == 0, "the real GTK receiver did not compile; see receiver-build.log")
    return executable


def wait_for(predicate, description, timeout=5):
    deadline = time.monotonic() + timeout
    while True:
        result = predicate()
        if result:
            return result
        require(time.monotonic() < deadline, "timed out: " + description)
        time.sleep(0.005)


class Lines:
    def __init__(self, stream):
        self.stream = stream
        self.pending = bytearray()

    def read(self, timeout=8, limit=768 * 1024):
        deadline = time.monotonic() + timeout
        with selectors.DefaultSelector() as selector:
            selector.register(self.stream, selectors.EVENT_READ)
            while b"\n" not in self.pending:
                remaining = deadline - time.monotonic()
                require(remaining > 0 and selector.select(remaining), "native reply timeout")
                chunk = os.read(self.stream.fileno(), min(8192, limit + 1 - len(self.pending)))
                require(bool(chunk), "native process closed its output before a complete reply")
                self.pending.extend(chunk)
                require(len(self.pending) <= limit, "native reply exceeded its bound")
        line, _, tail = self.pending.partition(b"\n")
        self.pending = bytearray(tail)
        return bytes(line)


class Worker:
    def __init__(self, fixture, operation, arguments):
        self.process = fixture.spawn(
            [str(fixture.binary), "--internal-zuse-worker"], cwd=fixture.root,
            env=fixture.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, bufsize=0,
        )
        self.lines = Lines(self.process.stdout)
        require(self.lines.read(timeout=5) == b"ssmcp-zuse-worker-v1", "wrong real-worker greeting")
        self.process.stdin.write(json.dumps({"operation": operation, "arguments": arguments},
                                           ensure_ascii=False).encode() + b"\n")
        self.process.stdin.flush()

    def cancel(self):
        if self.process.stdin is not None:
            self.process.stdin.close()
            self.process.stdin = None

    def finish(self):
        reply = json.loads(self.lines.read(timeout=12))
        self.cancel()
        tail, error = self.process.communicate(timeout=5)
        require(not self.lines.pending and not tail, "worker produced more than one reply")
        return {"reply": reply, "exit_code": self.process.returncode,
                "stderr": error.decode(errors="replace")[:4096]}

    def cancellation_reader(self):
        # This is the owned Linux worker's named stdin-monitor thread. In the
        # audited worker protocol, setting cancellation is its final action.
        tasks = Path(f"/proc/{self.process.pid}/task")
        matches = []
        for task in tasks.iterdir():
            try:
                name = (task / "comm").read_text().strip()
            except FileNotFoundError:
                continue
            if name.startswith("ssmcp-zuse-inpu"):
                matches.append(task)
        require(len(matches) == 1, "cannot identify the worker's cancellation-monitor thread")
        return matches[0]


class ReceiverControl:
    def command(self, value):
        self.command_input.write(value.encode() + b"\n")
        self.command_input.flush()
        return self.receiver_lines.read().decode()

    def reset(self, text=""):
        require(self.command("RESET " + (base64.b64encode(text.encode()).decode() or "-")) == "OK", "GTK reset")

    def state(self):
        values = self.command("STATE").split()
        require(len(values) == 7 and values[0] == "STATE", "invalid GTK state reply")
        decode = lambda value: base64.b64decode(value).decode() if value != "-" else ""
        return {"text": decode(values[1]), "other": decode(values[2]), "caret": int(values[3]),
                "selection": [int(values[4]), int(values[5])], "changes": int(values[6])}

    def context(self):
        values = self.command("CONTEXT").split()
        require(len(values) == 6 and values[0] == "CONTEXT", "invalid GTK environment reply")
        names = ("display", "session_bus", "bus_override", "display_override", "root_bus")
        return dict(zip(names, (base64.b64decode(value).decode().rstrip("\0") if value != "-" else "" for value in values[1:])))


class Fixture(ReceiverControl):
    def __init__(self, root, binary, evidence, helpers):
        self.root, self.binary = root, binary.resolve()
        self.evidence, self.helpers = evidence, helpers
        self.processes, self.logs, self.cases = [], [], []
        self.server = self.receiver = None
        self.changed_read = self.resume_write = None
        self.env = None

    def spawn(self, argv, **kwargs):
        child = subprocess.Popen(argv, **kwargs)
        self.processes.append(child)
        return child

    def log(self, name):
        stream = (self.evidence / name).open("wb")
        self.logs.append(stream)
        return stream

    def start(self):
        module_path = self.helpers / "private_xvfb.py"
        spec = importlib.util.spec_from_file_location("ssmcp_owned_private_xvfb", module_path)
        require(spec is not None and spec.loader is not None, "private_xvfb.py is unavailable")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.server = module.PrivateXvfb(self.root / "display", width=960, height=360)
        self.server.start()
        self.env = self.server.environment
        self.env.update(GDK_BACKEND="x11", NO_AT_BRIDGE="0", GIO_USE_VFS="local")
        subprocess.run(["xdpyinfo"], env=self.env, stdout=subprocess.DEVNULL,
                       stderr=subprocess.PIPE, check=True, timeout=5)
        config = Path("/usr/share/defaults/at-spi2/accessibility.conf")
        require(config.is_file(), "at-spi2 accessibility bus configuration is missing")
        bus = self.spawn(["dbus-daemon", "--config-file=" + str(config), "--nofork", "--print-address=1"],
                         env=self.env, stdout=subprocess.PIPE, stderr=self.log("accessibility-bus.log"), bufsize=0)
        address = Lines(bus.stdout).read(timeout=5).decode()
        require(address.startswith("unix:"), "accessibility bus must be private and local")
        self.env["AT_SPI_BUS_ADDRESS"] = address
        registry = next((p for p in [Path("/usr/lib/at-spi2-registryd"), Path("/usr/libexec/at-spi2-registryd")]
                         if p.is_file()), None)
        require(registry is not None, "at-spi2-registryd is missing")
        self.spawn([str(registry)], env=self.env, stdout=self.log("registry.stdout"),
                   stderr=self.log("registry.stderr"))

        def bus_call(destination, path, method, *arguments):
            return subprocess.run(["gdbus", "call", "--address", address, "--dest", destination,
                                   "--object-path", path, "--method", method, *arguments],
                                  env=self.env, capture_output=True, text=True, timeout=2)

        wait_for(lambda: "true" in bus_call("org.freedesktop.DBus", "/org/freedesktop/DBus",
                 "org.freedesktop.DBus.NameHasOwner", "org.a11y.atspi.Registry").stdout,
                 "private AT-SPI registry")
        executable = build_receiver(self.root, self.evidence)
        self.changed_read, changed_write = os.pipe()
        resume_read, self.resume_write = os.pipe()
        receiver_env = dict(self.env, SSMCP_FIXTURE_CHANGED_FD=str(changed_write),
                            SSMCP_FIXTURE_RESUME_FD=str(resume_read))
        try:
            self.receiver = self.spawn([str(executable)], env=receiver_env, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=self.log("receiver.stderr"), bufsize=0,
                                       pass_fds=(changed_write, resume_read))
        finally:
            os.close(changed_write)
            os.close(resume_read)
        self.receiver_lines = Lines(self.receiver.stdout)
        self.command_input = self.receiver.stdin
        ready = self.receiver_lines.read().decode().split()
        require(len(ready) == 4 and ready[0] == "READY", "invalid GTK readiness")
        self.window, self.other_window, self.receiver_pid = ready[1:]
        wait_for(lambda: "/org/a11y/atspi/accessible/root" in bus_call(
                 "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root",
                 "org.a11y.atspi.Accessible.GetChildren").stdout, "GTK accessibility registration")
        self.reset()

    def call(self, operation, arguments):
        return Worker(self, operation, arguments).finish()

    def native_state(self):
        # Reuse the existing fixture's read-only, process-local X11 probe.
        raw = subprocess.check_output([
            sys.executable, str(self.helpers / "e2e_zuse_worker.py"), "--x11-state-probe",
        ], env=self.env, timeout=3)
        require(len(raw) < 16384, "X11 state probe exceeded its bound")
        return json.loads(raw)

    def focus(self):
        outcome = self.call("focus", {"window_id": self.window})
        require(outcome["exit_code"] == 0 and outcome["reply"]["error"] is None,
                "real worker could not focus the owned GTK receiver")

    def record(self, name, outcome, expected, *, error=None, changes=None, caret=None):
        state = self.state()
        row = {"name": name, "worker": outcome, "expected": expected, "observed": state}
        self.cases.append(row)
        reply = outcome["reply"]
        correct = state["text"] == expected and state["other"] == "other window untouched"
        correct = correct and outcome["exit_code"] == 0 and reply["cleanup_ok"] is True
        if error is None:
            correct = correct and reply["error"] is None and isinstance(reply["value"], dict) and reply["value"].get("backend") == "x11"
        else:
            correct = correct and isinstance(reply["error"], str) and all(part in reply["error"] for part in error)
        if changes is not None:
            correct = correct and state["changes"] == changes
        if caret is not None:
            correct = correct and state["caret"] == caret
        row["status"] = "passed" if correct else "failed"
        require(correct, name + ": " + json.dumps(row, ensure_ascii=False))
        return row

    def rejection(self, field):
        self.reset()
        self.focus()
        wrong = "MACHINE not-this-private-host.invalid" if field == "machine" else "PID 4294967294"
        restore = "MACHINE local" if field == "machine" else "PID " + self.receiver_pid
        require(self.command(wrong) == "OK", "change receiver identity")
        try:
            outcome = self.call("type", {"text": "ASCII prefix🙂"})
            self.record("whole-string rejection for wrong " + field, outcome, "",
                        error=["Unsupported X11 Unicode text", "no input was sent"], changes=0)
        finally:
            require(self.command(restore) == "OK", "restore receiver identity")
        value = "恢复🙂" if field == "pid" else "Z 你好こんにちは مرحبا नमस्ते 🙂"
        self.record("fresh worker after corrected " + field,
                    self.call("type", {"text": value}), value, changes=1)

    def cancelled_after_dispatch(self):
        self.reset()
        self.focus()
        before = self.native_state()
        require(self.command("BLOCK") == "OK", "arm actual GTK callback barrier")
        value = "取消🙂e\u0301"
        worker = Worker(self, "type", {"text": value})
        released = False
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self.changed_read, selectors.EVENT_READ)
                require(bool(selector.select(5)), "GTK never observed insertion dispatch")
                require(os.read(self.changed_read, 1) == b"1", "invalid GTK insertion marker")
            reader = worker.cancellation_reader()
            worker.cancel()
            # No fixed sleep: the audited monitor sets cancellation then exits.
            # Release GTK only after EOF was observed while insertion was blocked.
            wait_for(lambda: not reader.exists(), "worker observed cancellation EOF", timeout=0.7)
            require(worker.process.poll() is None, "worker returned before the blocked insertion completed")
            os.write(self.resume_write, b"1")
            released = True
            outcome = worker.finish()
            row = self.record("cancel after native dispatch does not resend or roll back", outcome, value,
                              error=["cancelled after X11 text dispatch", "text may have changed", "do not retry"],
                              changes=1, caret=len(value))
            row["barrier"] = {"gtk_changed_callback": True, "worker_stdin_monitor_exited": True,
                              "sender_exited": worker.process.poll() is not None}
            after = self.native_state()
            row.update(native_before=before, native_after=after)
            unchanged = before == after and not after["mask"] & 0x1f00
            row["status"] = "passed" if unchanged else "failed"
            require(unchanged, "cancelled whole-text insertion changed pointer/focus/keymap or left a pointer button held")
        finally:
            if not released:
                try:
                    os.write(self.resume_write, b"1")
                except OSError:
                    pass
            worker.cancel()

    def run(self):
        self.rejection("machine")
        self.rejection("pid")
        self.reset("左旧右")
        self.focus()
        require(self.command("SELECT 1 2") == "OK", "select one Unicode character")
        value = "🙂नमe\u0301"
        self.record("whole UTF-8 replaces a character selection", self.call("type", {"text": value}),
                    "左" + value + "右", changes=2, caret=1 + len(value))
        self.cancelled_after_dispatch()

    def close(self):
        errors = []
        if self.resume_write is not None:
            try:
                os.write(self.resume_write, b"1")
            except OSError:
                pass
        for child in reversed(self.processes):
            try:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired) as error:
                errors.append(str(error))
        if self.server is not None:
            try:
                self.server.close()
            except Exception as error:
                errors.append(str(error))
        for descriptor in (self.changed_read, self.resume_write):
            if descriptor is not None:
                os.close(descriptor)
        for stream in self.logs:
            stream.close()
        require(not errors, "owned fixture cleanup failed: " + "; ".join(errors))


class OwnedGtkReceiver(ReceiverControl):
    """Launch through the existing owned desktop, without host authorization.

    The caller keeps its existing ComputerUse client and desktop_start result.
    A private FIFO pair supplies test control only; all input under test crosses
    MCP. The app and worker receive production desktop environments. Neither
    this class nor run_mcp_recovery passes an accessibility address to a worker.
    """
    def __init__(self, harness, directory, evidence, expected_display):
        self.harness, self.directory, self.evidence = harness, Path(directory).resolve(), Path(evidence).resolve()
        self.expected_display = expected_display
        self.command_input = self.response_stream = None
        self.observed_context = None
        self.cases = []

    def start(self):
        self.directory.mkdir(mode=0o700)
        self.evidence.mkdir(parents=True, exist_ok=True)
        executable = build_receiver(self.directory, self.evidence)
        for name in ("commands", "replies"):
            os.mkfifo(self.directory / name, 0o600)
        # Opening both ends in the controller prevents either side's startup
        # from blocking before app_launch returns. This is not an input API.
        self.command_input = os.fdopen(os.open(self.directory / "commands", os.O_RDWR | os.O_CLOEXEC), "wb", buffering=0)
        self.response_stream = os.fdopen(os.open(self.directory / "replies", os.O_RDWR | os.O_CLOEXEC), "rb", buffering=0)
        self.receiver_lines = Lines(self.response_stream)
        self.launch_result = self.harness.call("ssmcp.computer.app_launch", {
            "command": [str(executable), "--control-dir", str(self.directory)],
        })
        ready = self.receiver_lines.read(timeout=10).decode().split()
        require(len(ready) == 4 and ready[0] == "READY", "owned GTK app did not become ready")
        self.window, self.other_window, self.receiver_pid = ready[1:]

        def private_context():
            result = self.context()
            require(result["display"] == self.expected_display, "GTK did not inherit the owned display")
            require(result["session_bus"].startswith("unix:"), "GTK has no private app session bus")
            require(not result["bus_override"] and not result["display_override"],
                    "owned app inherited an AT-SPI override from the outer environment")
            return result if result["root_bus"] else None

        self.observed_context = wait_for(private_context, "GTK publishes its private X11 accessibility address")
        address = self.observed_context["root_bus"]
        require(address.startswith("unix:"), "private accessibility address is not local")

        def registered():
            response = subprocess.run([
                "gdbus", "call", "--address", address, "--dest", "org.a11y.atspi.Registry",
                "--object-path", "/org/a11y/atspi/accessible/root",
                "--method", "org.a11y.atspi.Accessible.GetChildren",
            ], capture_output=True, text=True, timeout=2)
            return response.returncode == 0 and "/org/a11y/atspi/accessible/root" in response.stdout

        wait_for(registered, "owned GTK is registered on the observed private accessibility bus")
        self.reset()
        return self

    def close(self):
        for stream in (self.command_input, self.response_stream):
            if stream is not None:
                stream.close()
        # EOF makes the fixture quit. Its launch/cleanup remains owned by the
        # product's existing desktop manager, never by an unrelated PID kill.

    def __enter__(self):
        try:
            return self.start()
        except Exception:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()


def run_mcp_recovery(harness, fixture):
    """Use ONE authenticated Computer and its running owned isolated desktop.

    Use OwnedGtkReceiver, not Fixture: omission of a desktop selector here
    deliberately uses the already-running product-owned desktop. No host
    approval, direct worker, external model or replacement Computer is used.
    """
    require(isinstance(fixture, OwnedGtkReceiver), "MCP regression requires a product-owned receiver")
    client = harness.client_id
    rows = fixture.cases
    for field in ("machine", "pid"):
        fixture.reset()
        harness.call("ssmcp.computer.focus", {"window_id": fixture.window})
        wrong = "MACHINE not-this-private-host.invalid" if field == "machine" else "PID 4294967294"
        restore = "MACHINE local" if field == "machine" else "PID " + fixture.receiver_pid
        require(fixture.command(wrong) == "OK", "set MCP receiver identity")
        try:
            reply = harness.code("return await call(%s,%s,%s);" % (
                json.dumps(client), json.dumps("ssmcp.computer.type"),
                json.dumps({"text": "ASCII prefix🙂"})))
            state = fixture.state()
            row = {"name": "same Computer rejects then recovers from " + field,
                   "client_id": client, "rejection": reply, "rejected_state": state}
            rows.append(row)
            require(reply.get("isError") is True, "MCP must return a real native rejection")
            require(state["text"] == "" and state["changes"] == 0, "MCP rejection sent an ASCII prefix")
        finally:
            require(fixture.command(restore) == "OK", "restore MCP receiver identity")
        value = "同一实例🙂"
        recovered = harness.call("ssmcp.computer.type", {"text": value})
        state = fixture.state()
        row.update(recovery=recovered, recovered_state=state)
        require(harness.client_id == client, "test must retain the same authenticated Computer")
        require(state["text"] == value and state["changes"] == 1 and
                state["other"] == "other window untouched", "persistent Computer did not recover cleanly")
        row["status"] = "passed"
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--helpers-dir", type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--inside-session", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    require(sys.platform == "linux", "this acceptance fixture is Linux-specific")
    for helper in ("Xvfb", "xdpyinfo", "dbus-run-session", "dbus-daemon", "gdbus", "pkg-config", "cc"):
        require(shutil.which(helper) is not None, "missing fixture helper: " + helper)
    args.binary = args.binary.resolve()
    args.helpers_dir = args.helpers_dir.resolve()
    args.evidence_dir = args.evidence_dir.resolve()
    args.report = args.report.resolve()
    require(args.binary.is_file(), "SSMCP binary is missing")
    require((args.helpers_dir / "private_xvfb.py").is_file(), "use the repository's scripts directory as --helpers-dir")
    require((args.helpers_dir / "e2e_zuse_worker.py").is_file(), "the existing X11 state probe is required")
    if not args.inside_session:
        command = ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()),
                   "--inside-session", "--binary", str(args.binary), "--helpers-dir", str(args.helpers_dir),
                   "--evidence-dir", str(args.evidence_dir), "--report", str(args.report)]
        return subprocess.call(command)
    args.evidence_dir.mkdir(parents=True, exist_ok=True)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {"schema": "ssmcp.x11.whole-text.v1", "status": "running", "cases": [],
              "scope": "real internal worker and real GTK; persistent Computer recovery is a separate MCP case",
              "binary_sha256": file_sha256(args.binary),
              "script_sha256": file_sha256(__file__),
              "receiver_source_sha256": hashlib.sha256(RECEIVER_C.encode()).hexdigest(),
              "private_xvfb_sha256": file_sha256(args.helpers_dir / "private_xvfb.py"),
              "state_probe_script_sha256": file_sha256(args.helpers_dir / "e2e_zuse_worker.py")}
    error = None
    with tempfile.TemporaryDirectory(prefix="ssmcp-x11-text-", dir=args.evidence_dir) as temporary:
        fixture = Fixture(Path(temporary), args.binary, args.evidence_dir, args.helpers_dir)
        try:
            fixture.start()
            report["display"] = {"number": fixture.env["DISPLAY"], "private_auth": True,
                                 "private_session_and_accessibility_buses": True}
            fixture.run()
            report["status"] = "passed"
        except Exception as failure:
            error = failure
            report["status"] = "failed"
            report["error"] = str(failure)
        finally:
            report["cases"] = fixture.cases
            try:
                fixture.close()
                report["owned_process_cleanup"] = "passed"
            except Exception as failure:
                error = error or failure
                report["status"] = "failed"
                report["cleanup_error"] = str(failure)
    report["duration_seconds"] = round(time.monotonic() - started, 3)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if error is not None:
        print(str(error), file=sys.stderr)
        return 1
    print("Passed", len(report["cases"]), "real X11 text worker cases")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
