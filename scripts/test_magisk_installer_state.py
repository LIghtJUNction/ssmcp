"""Exercise the installed Magisk lifecycle hooks with a disposable Android filesystem."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from urllib.parse import urlsplit


MODULE = Path(__file__).resolve().parents[1] / "packaging/magisk/src/ssmcp"
INSTALLER = """
set_perm() { :; }
abort() { printf '%s\\n' "$*" >&2; exit 1; }
ui_print() { :; }
. "$MODPATH/customize.sh" || exit 1
printf '%s\\n' "$TMPDIR" "$HOME" "$PATH" > "$FIXTURE/parent-env"
case "$TMPDIR" in
    "$FIXTURE/installer/tmp"|"$FIXTURE/data/adb/ssmcp") /bin/rm -rf -- "$TMPDIR" ;;
    *) exit 2 ;;
esac
"""


def executable(path, source):
    path.write_text("#!/bin/sh\n" + source, encoding="utf-8")
    path.chmod(0o755)


class InstallerStateTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.staged = self.root / "data/adb/modules_update/ssmcp"
        self.active = self.root / "data/adb/modules/ssmcp"
        self.state = self.root / "data/adb/ssmcp"
        self.owner = self.state / "console.url"
        self.opened = self.root / "opened"
        self.keys_read = self.root / "key-reads"
        self.events = self.root / "fixture-events"
        self.starts = self.root / "starts"
        self.scratch = self.root / "installer/tmp"
        system_bin = self.root / "system/bin"
        installer_bin = self.root / "installer/bin"
        home = self.root / "installer/home"
        for path in (self.staged / "bin", self.active.parent, system_bin, installer_bin,
                     self.root / "data/local/tmp", home):
            path.mkdir(parents=True)
        self.opened.touch()
        self.keys_read.touch()
        self.events.touch()
        self.starts.touch()

        # Only device absolute paths change in these copies of the real scripts.
        for name in ("customize.sh", "launch.sh", "action.sh", "service.sh", "open.sh", "onboarding/open.sh"):
            script = (MODULE / name).read_text(encoding="utf-8")
            for prefix in ("/data/adb/", "/data/local/", "/system/", "/proc/"):
                script = script.replace(prefix, str(self.root) + prefix)
            target = self.staged / name
            target.parent.mkdir(exist_ok=True)
            target.write_text(script, encoding="utf-8")
            target.chmod(0o755)
        for path in (system_bin, installer_bin):
            for tool in ("mkdir", "chmod", "cat", "cp", "mv", "ln", "mktemp", "mkfifo", "printf", "rm", "rmdir"):
                (path / tool).symlink_to(shutil.which(tool))
            executable(path / "sleep", "exec /bin/sleep 0.05\n")
        (system_bin / "sh").symlink_to("/bin/sh")
        executable(system_bin / "getevent", """
[ "$1" = -ql ] || exit 1
echo read >> "$FIXTURE/key-reads"
/bin/cat "$FIXTURE/fixture-events"
exec /bin/sleep 10
""")
        timeout = system_bin / "timeout"
        timeout.write_text(f"""#!{sys.executable}
import subprocess, sys
assert sys.argv[1] == "20"
try:
    sys.exit(subprocess.run(sys.argv[2:], timeout=0.3).returncode)
except subprocess.TimeoutExpired:
    sys.exit(124)
""")
        timeout.chmod(0o755)

        self.template = "ssmcp test@example.invalid\n"
        (self.staged / "ssmcp.rc").write_text(self.template)
        executable(self.staged / "bin/ssmcp", """
[ "$1" = __magisk ] && [ "$SSMCP_SESSION_HOME" = "$2/sessions" ] || exit 1
printf '%s\\n' "$$" >> "$FIXTURE/starts"
mkdir -p "$FIXTURE/proc/$$" || exit 1
ln "$0" "$FIXTURE/proc/$$/exe" || exit 1
printf '%s\\nhttp://127.0.0.1:%s/#%s\\n' "$$" "$3" "$FIXTURE_FRAGMENT" > "$2/staged.url"
mv "$2/staged.url" "$2/console.url"
""")
        executable(system_bin / "am", """
[ "$1" = start ] && [ "$2" = -a ] && [ "$3" = android.intent.action.VIEW ] && [ "$4" = -d ] || exit 1
printf '%s\\n' "$5" >> "$FIXTURE/opened"
""")
        self.environment = {
            "ARCH": "arm64", "API": "36", "BOOTMODE": "true",
            "FIXTURE": str(self.root), "FIXTURE_FRAGMENT": "a" * 64,
            "MODPATH": str(self.staged), "TMPDIR": str(self.scratch),
            "HOME": str(home), "PATH": str(installer_bin),
        }

    def shell(self, *args):
        # Background services redirect stdout. Their fixture copies retain this pipe on
        # fd 9 until they finish publishing the owner, so assertions can run immediately.
        result = subprocess.run(["/bin/sh", "-c", 'exec 9>&1; exec /bin/sh "$@"', "hook", *args],
                                env=self.environment, stdout=subprocess.PIPE, check=True, timeout=4)
        return result.stdout.decode()

    def wait_for_console(self):
        # Some Android shells close extra inherited descriptors before the fixture child exits.
        deadline = time.monotonic() + 3
        while not self.owner.is_file() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(self.owner.is_file(), "hook did not publish the console owner")

    def install(self):
        self.scratch.mkdir()
        self.shell("-c", INSTALLER)
        self.assertEqual((self.root / "parent-env").read_text().splitlines(),
                         [self.environment[key] for key in ("TMPDIR", "HOME", "PATH")])
        self.assertFalse(self.scratch.exists())
        self.assertTrue((self.state / "ssmcp.rc").is_file(), "cleanup removed the module configuration")

    def test_first_install_cleanup_and_explicit_onboarding(self):
        self.install()
        self.assertEqual((self.state / "ssmcp.rc").read_text(), self.template)
        self.assertTrue(self.owner.is_file())
        self.assertEqual(len(self.opened.read_text().splitlines()), 1)
        self.staged.rename(self.active)
        self.shell(self.active / "onboarding/open.sh")
        urls = [urlsplit(url) for url in self.opened.read_text().splitlines()]
        self.assertEqual([url.path for url in urls], ["/setup", "/setup"])
        for url in urls:
            self.assertEqual((url.scheme, url.netloc, url.fragment),
                             ("http", "127.0.0.1:42387", self.environment["FIXTURE_FRAGMENT"]))
        self.assertEqual(len(self.starts.read_text().splitlines()), 1)
        self.assertEqual(self.keys_read.read_text(), "")

    def test_webui_launch_selects_gui_only_for_confirmed_target(self):
        self.staged.rename(self.active)
        self.shell(self.active / "open.sh")
        self.assertEqual(urlsplit(self.opened.read_text().strip()).path, "/setup")
        (self.state / "root-consent-v1").write_text("test@example.invalid")
        self.shell(self.active / "open.sh")
        url = urlsplit(self.opened.read_text().splitlines()[-1])
        self.assertEqual((url.scheme, url.netloc, url.path, url.query, url.fragment),
                         ("http", "127.0.0.1:42387", "/", "", self.environment["FIXTURE_FRAGMENT"]))
        self.assertEqual((self.state / "sessions").stat().st_mode & 0o777, 0o700)
        (self.state / "ssmcp.rc").write_text("ssmcp other@example.invalid", encoding="utf-8")
        self.shell(self.active / "open.sh")
        self.assertEqual(urlsplit(self.opened.read_text().splitlines()[-1]).path, "/setup")
        self.assertEqual(len(self.starts.read_text().splitlines()), 1)

    def test_disabled_or_removed_module_does_not_launch(self):
        self.staged.rename(self.active)
        for name in ("disable", "remove"):
            flag = self.active / name
            flag.touch()
            with self.assertRaises(subprocess.CalledProcessError):
                self.shell(self.active / "open.sh")
            flag.unlink()
        self.assertEqual(self.starts.read_text(), "")
        self.assertEqual(self.opened.read_text(), "")

    def test_rejects_unowned_or_malformed_pairing_url(self):
        self.install()
        pid = self.owner.read_text().splitlines()[0]
        self.staged.rename(self.active)
        for owner, url in [(pid, "http://127.0.0.1:42387/#short"),
                           (pid, "http://127.0.0.1:42387/#x" + "a" * 63),
                           (pid, "http://example.invalid/#" + "a" * 64),
                           ("999999999", "http://127.0.0.1:42387/#" + "a" * 64)]:
            self.owner.write_text(f"{owner}\n{url}\n")
            with self.assertRaises(subprocess.CalledProcessError):
                self.shell("-c", 'MODDIR=$1; . "$MODDIR/launch.sh"; read_console_url', "check", self.active)

    def test_action_default_opens_workbench_or_setup(self):
        self.staged.rename(self.active)
        self.events.write_text("/dev/input/event1: EV_KEY KEY_VOLUMEDOWN DOWN\n")
        self.shell(self.active / "action.sh")
        self.assertEqual(urlsplit(self.opened.read_text().strip()).path, "/setup")

    def test_boot_and_legacy_action_start_without_menu_or_browser(self):
        self.staged.rename(self.active)
        for hook, args in (("service.sh", []), ("action.sh", ["setup"])):
            with self.subTest(hook=hook, args=args):
                self.owner.unlink(missing_ok=True)
                before = len(self.starts.read_text().splitlines())
                self.shell(self.active / hook, *args)
                self.wait_for_console()
                running = self.owner.read_bytes()
                self.shell(self.active / hook, *args)
                self.assertEqual(self.owner.read_bytes(), running)
                self.assertEqual(len(self.starts.read_text().splitlines()), before + 1)
                self.assertEqual(self.opened.read_text(), "")
                self.assertEqual(self.keys_read.read_text(), "")

    def test_module_action_opens_immediately_without_reading_volume_keys(self):
        self.staged.rename(self.active)
        self.shell(self.active / "action.sh")
        self.shell(self.active / "action.sh")
        self.assertEqual(len(self.opened.read_text().splitlines()), 2)
        self.assertEqual(len(self.starts.read_text().splitlines()), 1)
        self.assertEqual(self.keys_read.read_text(), "")

    def test_initialized_update_preserves_active_service(self):
        self.install()
        self.staged.rename(self.active)
        shutil.copytree(self.active, self.staged)
        target = "my-account@private.example.invalid"
        files = {
            self.state / "root-consent-v1": target,
            self.state / "ssmcp.rc": f"ssmcp {target}\n",
            self.state / "connected-0": "session in progress",
        }
        for path, text in files.items():
            path.write_text(text)
        running = self.owner.read_bytes()
        starts = self.starts.read_bytes()
        self.opened.write_text("")
        self.install()
        self.assertEqual(self.opened.read_text(), "")
        self.assertEqual(self.owner.read_bytes(), running)
        self.assertEqual(self.starts.read_bytes(), starts)
        self.assertEqual(self.keys_read.read_text(), "")
        for path, text in files.items():
            self.assertEqual(path.read_text(), text)


if __name__ == "__main__":
    unittest.main()
