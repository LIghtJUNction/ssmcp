#!/usr/bin/env python3
"""Local guide routes and real 120x30 sidebar QR, using an owned Xvfb/ztty."""
import argparse, contextlib, json, os, shutil, signal, subprocess, tempfile, time
from pathlib import Path
from urllib.parse import urlsplit
from e2e_native_images import Window
from e2e_input_history import InputHistory
from e2e_dashboard import sha256
class Guide(InputHistory):
    def config(self, name, **options):
        path = super().config(name, **options)
        path.write_text(path.read_text().replace("sidebar=false", "sidebar=true\nsidebar_width=37"))
        return path

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--ztty", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary_dir, ztty, output = args.bin_dir.resolve(), args.ztty.resolve(), args.out.resolve()
    decoder = binary_dir / "vt_snapshot"
    if not decoder.is_file():
        decoder = binary_dir / "examples/vt_snapshot"
    for path in (binary_dir / "ssmcp", binary_dir / "ssmcpd", decoder, ztty):
        if not path.is_file():
            parser.error("missing already-built binary: " + str(path))
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "failed", "binaries": {p.name: sha256(p) for p in (binary_dir / "ssmcp", binary_dir / "ssmcpd", ztty)}}
    start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-guide-acceptance-") as directory:
        temp = Path(directory)
        h = Guide(temp, binary_dir, decoder, output)
        window = None
        try:
            h.prepare()
            routes = []
            for path, title, language in [
                ("/guide/", "Guide · ssmcp", "en"),
                ("/guide/en", "Guide · ssmcp", "en"),
                ("/guide/zh", "使用指南 · ssmcp", "zh-CN"),
                ("/guide/style.css", None, None),
            ]:
                status, body, headers = h.request("GET", path=path, secret="", headers={"Accept": "text/html, text/css"})
                lower = {k.lower(): v for k, v in headers.items()}
                content_type = lower.get("content-type", "")
                h.check(status == 200, "unauthenticated candidate GET " + path + " returns 200")
                if title:
                    h.check("text/html" in content_type and "<title>" + title + "</title>" in body
                            and '<html lang="' + language + '">' in body, path + " returns the correct localized guide")
                    h.check("{{" not in body and "https://ssmcp.example/mcp" in body
                            and "ssmcp.fs.read_image" in body, path + " renders real gateway examples")
                    file = "guide-zh.html" if language == "zh-CN" else "guide-en.html"
                else:
                    h.check("text/css" in content_type and body.strip(), "shared guide CSS is served as CSS")
                    file = "style.css"
                (output / file).write_text(body)
                routes.append({"path": path, "status": status, "content_type": content_type, "title": title})
            report["routes"] = routes
            chrome = shutil.which("chromium") or shutil.which("chromium-browser")
            if chrome:
                for label, route, size in [("english-desktop", "/guide/", "1365,900"), ("chinese-mobile", "/guide/zh", "390,844")]:
                    r = subprocess.run([chrome, "--headless=new", "--no-sandbox", "--disable-gpu",
                        "--disable-dev-shm-usage", "--hide-scrollbars",
                        "--user-data-dir=" + str(temp / ("chrome-" + label)),
                        "--window-size=" + size, "--virtual-time-budget=1200",
                        "--screenshot=" + str(output / (label + ".png")),
                        "http://127.0.0.1:" + str(h.http_port) + route],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25)
                    h.check(r.returncode == 0 and (output / (label + ".png")).is_file(), label + " renders from the candidate HTTP route")
            else:
                raise RuntimeError("Chromium is required for actual route screenshots")
            h.workspace = temp / "workspace"
            h.workspace.mkdir()
            subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=h.workspace,
                env={**os.environ, **h.safe_git_env, "HOME": str(h.home)}, check=True, timeout=5,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            window = Window(h, ztty, temp)
            credentials = window.connection()
            h.client_id = credentials["client_id"]
            h.sensitive.append(credentials["login_code"])
            h.oauth_login(credentials)
            window.wait(lambda: h.ui("/display/rows") == 32, "candidate UI")
            window.resize(30, 120)
            window.wait(lambda: h.ui("/display/rows") == 30 and h.ui("/display/cols") == 120, "120x30 terminal")
            endpoint = urlsplit(credentials["url"])
            assert endpoint.username is None and endpoint.password is None
            expected = endpoint.scheme + "://" + endpoint.netloc + "/guide/"
            zbar = shutil.which("zbarimg")
            if not zbar:
                raise RuntimeError("zbarimg is required for a real QR decode")
            shot = output / "sidebar-120x30.png"
            def decoded():
                window.image().save(shot)
                r = subprocess.run([zbar, "--quiet", "--raw", str(shot)], capture_output=True, text=True, timeout=4)
                return r.stdout.strip() if r.returncode == 0 else None
            value = window.wait(decoded, "QR decoder reads the sidebar", 10)
            h.check(value == expected, "actual 120x30 sidebar QR decodes exactly to the credential-free gateway guide URL")
            (output / "qr-decoded.txt").write_text(value + "\n")
            report["qr_url"] = value
            report["cell_pixels"] = [window.cw, window.ch]
            window.resize(16, 60)
            window.wait(lambda: h.ui("/display/rows") == 16 and h.ui("/display/cols") == 60, "small terminal")
            time.sleep(0.15)
            narrow = output / "sidebar-60x16.png"
            window.image().save(narrow)
            r = subprocess.run([zbar, "--quiet", "--raw", str(narrow)], capture_output=True, text=True, timeout=4)
            h.check(r.returncode != 0 and not r.stdout.strip(), "a small terminal switches away from a clipped QR")
            h.check(h.ui("/display/conversation/draft/text") == "", "graphics replies and resizing leave the guide session draft empty")
            report["status"] = "passed"
        except Exception as error:
            detail = type(error).__name__ + ": " + str(error)
            for secret in [h.secret, *h.sensitive]:
                if secret: detail = detail.replace(secret, "[redacted]")
            report["error"] = detail
            if window:
                with contextlib.suppress(Exception):
                    window.image().save(output / "failure.png")
            print(detail, flush=True)
        finally:
            if window: window.close()
            h.close_frontends()
            h.cleanup()
            for name in ("ztty.stderr", "xvfb.stderr"):
                if (temp / name).exists(): shutil.copyfile(temp / name, output / name)
            report.update(checks=h.checks, metrics=h.metrics, duration_seconds=round(time.monotonic()-start,3))
            (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]), "report": str(output / "report.json")}), flush=True)
    return 0 if report["status"] == "passed" else 1

if __name__ == "__main__":
    def interrupted(*_): raise TimeoutError("guide validation interrupted")
    signal.signal(signal.SIGTERM, interrupted)
    raise SystemExit(main())
