"""Exercise Magisk's sourced installer and its TMPDIR cleanup without Android or root."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from urllib.parse import urlsplit


MODULE = Path(__file__).resolve().parents[1] / "packaging/magisk/src/ssmcp"


def executable(path, source):
    path.write_text("#!/bin/sh\n" + source, encoding="utf-8")
    path.chmod(0o755)


class InstallerStateTest(unittest.TestCase):
    def test_first_install_cleanup_and_daily_action(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            module = root / "data/adb/modules_update/ssmcp"
            state = root / "data/adb/ssmcp"
            system_bin = root / "system/bin"
            installer_bin = root / "installer/bin"
            home = root / "installer/home"
            scratch = root / "installer/tmp"
            for path in (module / "bin", system_bin, installer_bin, home, scratch):
                path.mkdir(parents=True)

            # Translate only device absolute paths in the staged copies of the real hooks.
            for filename in ("customize.sh", "launch.sh", "action.sh"):
                script = (MODULE / filename).read_text(encoding="utf-8")
                for prefix in ("/data/adb/", "/system/", "/proc/"):
                    script = script.replace(prefix, str(root) + prefix)
                (module / filename).write_text(script, encoding="utf-8")

            for path in (system_bin, installer_bin):
                for tool in ("mkdir", "chmod", "cp", "mv", "ln"):
                    (path / tool).symlink_to(shutil.which(tool))
                executable(path / "sleep", 'exec /bin/sleep 0.02\n')
            (system_bin / "sh").symlink_to("/bin/sh")

            template = "ssmcp test@example.invalid\n"
            (module / "ssmcp.rc").write_text(template, encoding="utf-8")
            executable(module / "bin/ssmcp", """
[ "$1" = __magisk ] || exit 1
mkdir -p "$FIXTURE/proc/$$" || exit 1
ln -s "$0" "$FIXTURE/proc/$$/exe" || exit 1
printf '%s\\nhttp://127.0.0.1:%s/#%s\\n' "$$" "$3" "$FIXTURE_FRAGMENT" > "$2/staged.url"
mv "$2/staged.url" "$2/console.url"
""")
            executable(system_bin / "am", """
[ "$1" = start ] && [ "$2" = -a ] && [ "$3" = android.intent.action.VIEW ] && [ "$4" = -d ] || exit 1
printf '%s\\n' "$5" >> "$FIXTURE/opened"
""")
            environment = {
                "ARCH": "arm64", "API": "36", "BOOTMODE": "true",
                "FIXTURE": str(root), "FIXTURE_FRAGMENT": "a" * 64,
                "MODPATH": str(module), "TMPDIR": str(scratch),
                "HOME": str(home), "PATH": str(installer_bin),
            }
            # Simulate Magisk sourcing customize.sh in its own shell, then cleaning TMPDIR.
            # Permit deletion of just the two possible paths inside this disposable fixture.
            installer = """
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
            subprocess.run(["/bin/sh", "-c", installer], env=environment, check=True, timeout=8)
            with self.subTest("installer environment"):
                self.assertEqual(
                    (root / "parent-env").read_text().splitlines(),
                    [str(scratch), str(home), str(installer_bin)],
                )
            with self.subTest("state after installer cleanup"):
                self.assertFalse(scratch.exists())
                self.assertEqual((state / "ssmcp.rc").read_text(), template)
                self.assertTrue((state / "console.url").is_file())

            subprocess.run(["/bin/sh", str(module / "action.sh")], env=environment, check=True, timeout=8)
            urls = [urlsplit(url) for url in (root / "opened").read_text().splitlines()]
            self.assertEqual([url.path for url in urls], ["/setup", "/"])
            for url in urls:
                self.assertEqual((url.scheme, url.netloc, url.fragment),
                                 ("http", "127.0.0.1:42387", environment["FIXTURE_FRAGMENT"]))


if __name__ == "__main__":
    unittest.main()
