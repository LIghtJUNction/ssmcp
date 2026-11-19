#!/usr/bin/env python3
"""Verify user waits and real Linux notification calls on an owned private bus.

Requires dbus-daemon, notify-send, Python dbus/gi, and already-built client,
library router fixture and VT decoder. No Cargo, model, installed account,
host desktop notification or host input is used.
"""
import argparse
import html
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from e2e_user_wait import UserWait, file_sha


def notification_server(address, events, ready):
    import dbus
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib

    DBusGMainLoop(set_as_default=True)
    bus = dbus.bus.BusConnection(address)
    name = dbus.service.BusName("org.freedesktop.Notifications", bus)

    class Service(dbus.service.Object):
        @dbus.service.method("org.freedesktop.Notifications",
                             in_signature="", out_signature="as")
        def GetCapabilities(self):
            return ["body", "body-markup"]

        @dbus.service.method("org.freedesktop.Notifications",
                             in_signature="", out_signature="ssss")
        def GetServerInformation(self):
            return ("ssmcp private fixture", "local test", "1", "1.2")

        @dbus.service.method("org.freedesktop.Notifications",
                             in_signature="susssasa{sv}i", out_signature="u")
        def Notify(self, app, replaces, icon, title, body, actions, hints, expires):
            target = Path(events)
            if target.exists() and target.stat().st_size > 65536:
                raise dbus.DBusException("private fixture notification budget exceeded")
            record = {"app": str(app), "title": str(title), "body": str(body),
                      "actions": list(map(str, actions)), "expires_ms": int(expires)}
            with target.open("a", encoding="utf-8") as out:
                out.write(json.dumps(record, ensure_ascii=False) + "\n")
            return 41

        @dbus.service.method("org.freedesktop.Notifications",
                             in_signature="u", out_signature="")
        def CloseNotification(self, notification_id):
            pass

    service = Service(bus, "/org/freedesktop/Notifications")
    Path(ready).write_text(json.dumps({"ready": True, "pid": os.getpid()}) + "\n")
    GLib.MainLoop().run()
    # Keep these references alive until the loop exits.
    return name, service


def stop_owned(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)


class PrivateBus:
    def __init__(self, directory):
        self.directory = directory
        self.bus = None
        self.server = None
        self.log = (directory / "private-dbus.log").open("w+")
        self.events = directory / "notifications.jsonl"
        self.ready = directory / "notifications-ready.json"
        self.events.touch(mode=0o600)
        config = directory / "private-bus.conf"
        endpoint = "unix:path=" + str(directory / "notification.sock")
        config.write_text(
            "<busconfig><type>session</type><listen>" + html.escape(endpoint)
            + "</listen><auth>EXTERNAL</auth>"
            '<policy context="default"><allow send_destination="*"/>'
            '<allow receive_sender="*"/><allow own="*"/></policy>'
            "</busconfig>"
        )
        try:
            self.bus = subprocess.Popen(
                ["dbus-daemon", "--nofork", "--config-file=" + str(config),
                 "--print-address=1"], stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=self.log, start_new_session=True,
            )
            with selectors.DefaultSelector() as selector:
                selector.register(self.bus.stdout, selectors.EVENT_READ)
                if not selector.select(timeout=3):
                    raise AssertionError("the owned private bus did not start")
                self.address = self.bus.stdout.readline().decode().strip()
            if not self.address.startswith(endpoint):
                raise AssertionError("the notification bus escaped the private socket")
            self.server = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "--server",
                 self.address, str(self.events), str(self.ready)],
                stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
                start_new_session=True,
            )
            until = time.monotonic() + 5
            while not self.ready.exists():
                if self.server.poll() is not None or time.monotonic() >= until:
                    raise AssertionError("private notification service failed to become ready")
                time.sleep(0.02)
        except BaseException:
            self.close()
            raise

    def records(self):
        if self.events.stat().st_size > 65536:
            raise AssertionError("private notification transcript exceeded 64 KiB")
        return [json.loads(line) for line in self.events.read_text().splitlines()]

    def close(self):
        stop_owned(self.server)
        stop_owned(self.bus)
        if self.bus and self.bus.stdout:
            self.bus.stdout.close()
        if not self.log.closed:
            self.log.close()


def process_identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "state": fields[0], "start_ticks": fields[19]}
    except FileNotFoundError:
        return None


def is_live(identity):
    current = process_identity(identity["pid"])
    return bool(current and current["start_ticks"] == identity["start_ticks"]
                and current["state"] not in ("Z", "X"))


class NotificationWait(UserWait):
    def __init__(self, *arguments, bus, notify_send):
        super().__init__(*arguments)
        self.bus = bus
        self.notify_send = notify_send
        self.helper_mode = "native"
        self.helper_record = self.directory / "blocked-helper.json"
        self.owned_helpers = []
        self.cases = []

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        for key in list(environment):
            if key.startswith("DBUS_") or key in ("DISPLAY", "WAYLAND_DISPLAY",
                                                  "SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE"):
                environment.pop(key, None)
        environment["DBUS_SESSION_BUS_ADDRESS"] = self.bus.address
        helper = Path(environment["PATH"]) / "notify-send"
        if self.helper_mode == "native":
            helper.symlink_to(self.notify_send)
        elif self.helper_mode == "blocked":
            helper.write_text(
                f"#!{sys.executable}\n"
                "import json, os, subprocess, sys, time\n"
                "from pathlib import Path\n"
                "def identity(pid):\n"
                "    fields=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()\n"
                "    return {'pid':pid,'start_ticks':fields[19]}\n"
                "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\n"
                f"Path({str(self.helper_record)!r}).write_text(json.dumps([identity(os.getpid()),identity(child.pid)]))\n"
                "time.sleep(30)\n"
            )
            helper.chmod(0o700)
        return environment

    def wait_notification(self, front, wait_id):
        until = time.monotonic() + 6
        while time.monotonic() < until:
            front.pump(0.03)
            entry = next((item for item in self.state()["active"]
                          if item["wait_id"] == wait_id), None)
            if entry and entry.get("notification") is not None:
                return entry
        raise AssertionError("the exact native wait did not publish its notification receipt")

    def cancel_and_receipt(self, front, future, entry):
        self.native_finish(entry["wait_id"], "cancelled")
        return self.completed(front, future, "cancelled", wait_id=entry["wait_id"])[1]

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        front = self.start_front("notice-native")
        front.resize(28, 120)
        reason = "Private notice <fixture> & quoted 'text' 界"
        future = self.begin(reason=reason, notify=True)
        entry = self.active(front, 1)["active"][0]
        entry = self.wait_notification(front, entry["wait_id"])
        notice = entry["notification"]
        self.check(notice["status"] == "sent" and notice["reason"] == "submitted"
                   and not notice["deduplicated"] and notice["user_seen"] is None,
                   "real notify-send returns a submission receipt without claiming user viewing")
        records = self.bus.records()
        self.check(len(records) == 1 and records[0]["app"].lower() == "ssmcp",
                   "exactly one real Notify reaches only the owned private DBus service")
        self.check("&lt;fixture&gt;" in records[0]["body"]
                   and "&amp;" in records[0]["body"] and "界" in records[0]["body"]
                   and "<fixture>" not in records[0]["body"],
                   "the real notification keeps Unicode and escapes markup in literal arguments")
        self.save_shot("notification-submitted", front, cells=True)
        receipt = self.cancel_and_receipt(front, future, entry)
        self.check(receipt["notification"]["status"] == "sent"
                   and receipt["notification"]["user_seen"] is None,
                   "the authenticated outer receipt keeps native submission separate from viewing")
        self.cases.append("real_private_dbus_submission")

        again = self.begin(reason=reason, notify=True)
        repeated = self.active(front, 1)["active"][0]
        repeated = self.wait_notification(front, repeated["wait_id"])
        self.check(repeated["episode_id"] == entry["episode_id"]
                   and repeated["notification"]["status"] == "sent"
                   and repeated["notification"]["deduplicated"],
                   "a repeated wait in the same authenticated episode reports deduplication")
        self.cancel_and_receipt(front, again, repeated)
        self.check(len(self.bus.records()) == 1,
                   "repeated waits do not issue a second desktop notification")
        self.cases.append("same_episode_deduplication")
        front.close()

        self.helper_mode = "missing"
        front = self.start_front("notice-unavailable")
        future = self.begin(reason="Missing private helper remains cancellable", notify=True)
        entry = self.active(front, 1)["active"][0]
        entry = self.wait_notification(front, entry["wait_id"])
        self.check(entry["notification"]["status"] == "unavailable"
                   and entry["notification"]["reason"] == "helper_unavailable",
                   "missing notification support has an explicit bounded unavailable receipt")
        self.cancel_and_receipt(front, future, entry)
        self.check(len(self.bus.records()) == 1,
                   "an unavailable helper does not contact the host or private notification service")
        self.cases.append("helper_unavailable_keeps_wait_cancellable")
        front.close()

        self.helper_mode = "blocked"
        front = self.start_front("notice-cancel-helper")
        future = self.begin(reason="Cancel only the owned blocked helper", notify=True)
        entry = self.active(front, 1)["active"][0]
        until = time.monotonic() + 2
        while not self.helper_record.exists():
            front.pump(0.01)
            if time.monotonic() >= until:
                raise AssertionError("the owned blocking helper did not record its exact process tree")
        self.owned_helpers = json.loads(self.helper_record.read_text())
        self.check(len(self.owned_helpers) == 2 and all(map(is_live, self.owned_helpers)),
                   "the cancellation fixture observes its exact live helper and descendant")
        receipt = self.cancel_and_receipt(front, future, entry)
        until = time.monotonic() + 3
        while any(map(is_live, self.owned_helpers)) and time.monotonic() < until:
            front.pump(0.02)
        self.check(not any(map(is_live, self.owned_helpers)),
                   "cancelling the real native wait terminates its owned helper process tree")
        self.check(receipt["notification"] is None
                   or receipt["notification"]["status"] != "sent",
                   "cancellation before OS submission never manufactures a sent receipt")
        self.cases.append("cancelled_native_wait_cleans_helper_tree")
        front.close()

    def cleanup_helpers(self):
        # Exact PID plus start ticks, from our own fixture only. Never kill by name.
        for identity in self.owned_helpers:
            if is_live(identity):
                os.kill(identity["pid"], signal.SIGKILL)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    notify_send = shutil.which("notify-send")
    if not notify_send or not shutil.which("dbus-daemon"):
        parser.error("requires existing Linux dbus-daemon and notify-send")
    report = {
        "status": "failed", "scope": "owned loopback account router, PTY and private DBus",
        "host_desktop_contacted": False, "models_invoked": False,
        "binary_sha256": file_sha(args.bin_dir / "ssmcp"),
        "fixture_sha256": file_sha(os.environ["SSMCP_E2E_FIXTURE_BINARY"]),
        "decoder_sha256": file_sha(args.decoder), "script_sha256": file_sha(__file__),
    }
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-wait-notify-") as temporary:
        directory = Path(temporary)
        bus_directory = directory / "bus"
        bus_directory.mkdir(mode=0o700)
        bus = PrivateBus(bus_directory)
        harness = NotificationWait(directory, args.bin_dir, args.decoder, args.out,
                                   bus=bus, notify_send=Path(notify_send).resolve())
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            message = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[redacted]")
            report["error"] = message
            if harness.last_frontend and not harness.last_frontend.closed:
                harness.save_shot("notification-failure", harness.last_frontend, cells=True)
            harness.diagnostics()
        finally:
            try:
                harness.close_waits()
                harness.close_frontends()
                harness.cleanup()
            finally:
                harness.cleanup_helpers()
                report["notification_calls"] = bus.records()
                bus.close()
            report.update(checks=harness.checks, cases=harness.cases,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "cases": len(report["cases"]),
                      "checks": len(report["checks"]), "error": report.get("error")}))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--server"]:
        notification_server(*sys.argv[2:])
    else:
        main()
