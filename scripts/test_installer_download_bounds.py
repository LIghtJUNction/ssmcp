#!/usr/bin/env python3
"""Exercise the installer download helper with real HTTPS chunked responses.

No installer is run. A tiny curl shim removes --max-filesize to reproduce the
unknown-size behavior of curl before 8.4.0; the real curl process still performs
the transfer. Every output and test certificate stays in an owned temp folder.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(os.name == "posix", "install.sh runs on Linux and macOS")
class DownloadBoundsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.real_curl = shutil.which("curl")
        openssl = shutil.which("openssl")
        if not cls.real_curl or not openssl:
            raise unittest.SkipTest("curl and openssl are needed for the HTTPS fixture")
        cls.temporary = tempfile.TemporaryDirectory(prefix="ssmcp download bounds ")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.directory = Path(cls.temporary.name)
        certificate = cls.directory / "localhost.pem"
        key = cls.directory / "localhost-key.pem"
        configuration = cls.directory / "openssl.cnf"
        configuration.write_text(
            "[req]\n"
            "distinguished_name = subject\n"
            "x509_extensions = certificate\n"
            "prompt = no\n"
            "[subject]\n"
            "CN = localhost\n"
            "[certificate]\n"
            "subjectAltName = DNS:localhost,IP:127.0.0.1\n"
            "basicConstraints = critical,CA:TRUE\n"
            "keyUsage = critical,digitalSignature,keyEncipherment,keyCertSign\n"
            "extendedKeyUsage = serverAuth\n",
            encoding="ascii",
        )
        subprocess.run(
            [openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(key), "-out", str(certificate), "-days", "1",
             "-config", str(configuration)],
            check=True, capture_output=True, timeout=20,
        )
        payloads = {
            "/oversize": b"x" * (256 * 1024),
            "/small": b"s" * (32 * 1024),
            "/large": b"l" * (128 * 1024),
        }

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self):
                payload = payloads[self.path]
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    for offset in range(0, len(payload), 4096):
                        chunk = payload[offset:offset + 4096]
                        self.wfile.write(("%x\r\n" % len(chunk)).encode("ascii"))
                        self.wfile.write(chunk + b"\r\n")
                    self.wfile.write(b"0\r\n\r\n")
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
                    pass
                self.close_connection = True

            def log_message(self, *_arguments):
                pass

        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(certificate, key)
        cls.server.socket = context.wrap_socket(cls.server.socket, server_side=True)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.addClassCleanup(cls.server.server_close)
        cls.addClassCleanup(cls.thread.join, 5)
        cls.addClassCleanup(cls.server.shutdown)
        cls.url = "https://localhost:%s" % cls.server.server_port

        shim = cls.directory / "legacy_curl.py"
        shim.write_text(
            "import os, sys\n"
            "arguments = sys.argv[1:]\n"
            "assert arguments[0] == '--disable'\n"
            "assert arguments.count('--max-filesize') == 1\n"
            "index = arguments.index('--max-filesize')\n"
            "del arguments[index:index + 2]\n"
            "command = [os.environ['SSMCP_TEST_REAL_CURL'], arguments[0],\n"
            "           '--cacert', os.environ['SSMCP_TEST_CERTIFICATE'],\n"
            "           '--noproxy', '*'] + arguments[1:]\n"
            "os.execv(command[0], command)\n",
            encoding="utf-8",
        )
        bin_directory = cls.directory / "bin"
        bin_directory.mkdir()
        wrapper = bin_directory / "curl"
        wrapper.write_text(
            '#!/bin/sh\nexec "$SSMCP_TEST_PYTHON" "$SSMCP_TEST_CURL_SHIM" "$@"\n',
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        cls.environment = os.environ.copy()
        cls.environment.update({
            "PATH": str(bin_directory) + os.pathsep + cls.environment["PATH"],
            "SSMCP_TEST_REAL_CURL": cls.real_curl,
            "SSMCP_TEST_CERTIFICATE": str(certificate),
            "SSMCP_TEST_PYTHON": sys.executable,
            "SSMCP_TEST_CURL_SHIM": str(shim),
        })
        cls.environment.pop("BASH_ENV", None)
        cls.environment.pop("ENV", None)
        source = (ROOT / "scripts/install.sh").read_text(encoding="utf-8")
        begin = source.index("download_url() {")
        end = source.index("\ndigest_file() {", begin)
        cls.helper = source[begin:end]
        cls.shells = ["/bin/sh"]
        bash = shutil.which("bash")
        if bash:
            # Explicit bash invocation has different default ulimit units.
            cls.shells.append(bash)

    def run_helper(self, shell, arguments, commands):
        return subprocess.run(
            [shell, "-c", self.helper + "\n" + commands, "ssmcp-download-test", *arguments],
            cwd=self.directory, env=self.environment,
            capture_output=True, text=True, timeout=15,
        )

    def test_chunked_overflow_is_stopped_at_the_kernel_limit(self):
        for index, shell in enumerate(self.shells):
            with self.subTest(shell=shell):
                destination = self.directory / ("overflow-%s" % index)
                cap = 64 * 1024
                result = self.run_helper(
                    shell, [self.url + "/oversize", str(destination), str(cap)],
                    'download_url "$1" "$2" "$3"',
                )
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertEqual(destination.stat().st_size, cap)
                self.assertFalse(list(self.directory.glob("core*")))

    def test_exact_limits_succeed_and_do_not_leak_between_downloads(self):
        for index, shell in enumerate(self.shells):
            with self.subTest(shell=shell):
                small = self.directory / ("small-%s" % index)
                large = self.directory / ("large-%s" % index)
                result = self.run_helper(
                    shell,
                    [self.url + "/small", str(small), str(32 * 1024),
                     self.url + "/large", str(large), str(128 * 1024)],
                    'before=$(ulimit -f)\n'
                    'download_url "$1" "$2" "$3" || exit 1\n'
                    'download_url "$4" "$5" "$6" || exit 1\n'
                    '[ "$(ulimit -f)" = "$before" ]',
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(small.read_bytes(), b"s" * (32 * 1024))
                self.assertEqual(large.read_bytes(), b"l" * (128 * 1024))

    def test_unavailable_resource_limit_fails_before_download(self):
        # Root can raise hard limits; the ordinary installer user cannot.
        if os.geteuid() == 0:
            self.skipTest("root can raise an inherited hard resource limit")
        destination = self.directory / "must-not-start"
        result = self.run_helper(
            "/bin/sh", [self.url + "/small", str(destination), str(32 * 1024)],
            'ulimit -f 1\ndownload_url "$1" "$2" "$3"',
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot enforce", result.stderr)
        self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
