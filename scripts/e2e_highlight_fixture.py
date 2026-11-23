#!/usr/bin/env python3
"""Render the ignored production highlighter fixture using already-built binaries.

No Cargo, client connection, model, or private session is started. The input is
synthetic regression data; PNGs are rendered from actual production ANSI frames.
The source revision is an assertion supplied by the build owner, and executable
hashes are measured here. NO_COLOR checks the effective Ui.color=false renderer
mode; this fixture does not test CLI environment-variable parsing.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

TEST = "client::terminal::events::highlight_tests::raw_json_visual_fixture"


def describe(path):
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest()}


def run(command, **kwargs):
    return subprocess.run(command, capture_output=True, check=False, timeout=60, **kwargs)


def truecolor(sgr):
    values = sgr.split(";")
    start = values.index("38")
    if values[start + 1] != "2":
        raise AssertionError(f"expected a truecolor fixture palette: {sgr}")
    return [int(value) for value in values[start + 2:start + 5]]


def check_frame(snapshot, spec, source):
    if snapshot["size"] != [spec["rows"], spec["cols"]]:
        raise AssertionError("decoder geometry differs from the exported frame")
    if len(snapshot["cells"]) != spec["rows"]:
        raise AssertionError("decoder returned an incomplete grid")
    compact, cells = [], []
    for row in snapshot["cells"]:
        if len(row) != spec["cols"]:
            raise AssertionError("decoder returned an incomplete row")
        for cell in row:
            if cell.get("continuation"):
                continue
            for character in cell["text"]:
                if not character.isspace():
                    compact.append(character)
                    cells.append(cell)
    text = "".join(compact)
    for line in source.splitlines():
        expected = "".join(line.split())
        if expected and expected not in text:
            raise AssertionError(f"logical input line is missing or clipped: {line!r}")

    checks = []

    def tone(context, token, semantic, bold=None):
        context = "".join(context.split())
        token = "".join(token.split())
        start = text.index(context) + context.index(token)
        color = truecolor(spec["tones"][semantic])
        actual = cells[start:start + len(token)]
        if not actual or any(cell["foreground"] != color for cell in actual):
            raise AssertionError(f"{token!r} does not retain {semantic} color")
        if bold is not None and any(cell["bold"] != bold for cell in actual):
            raise AssertionError(f"{token!r} has an unexpected bold attribute")
        checks.append({"context": context, "token": token, "tone": semantic})

    tone('{"file":"src/portal/guide/zh.html"', '"file"', "heading")
    tone('"src/portal/guide/zh.html"', '"src/portal/guide/zh.html"', "success")
    tone(r"<article><h3>\u9080\u8bf7</h3>", r"\u9080\u8bf7", "success")
    tone(r'<p>\"workers\"</p>\n71:', r'\"workers\"', "success")
    tone(r'["\u4f60\u597d","</code>",true,42,null]',
         r'"\u4f60\u597d"', "success")
    tone(r'["\u4f60\u597d","</code>",true,42,null]', '"</code>"', "success")
    tone('"ready":true', "true", "accent", bold=True)
    tone('"count":42', "42", "warning")
    tone(r"opened C:\work\ssmcp src/main.rs", r"C:\work\ssmcp", "blue")
    tone(r"opened C:\work\ssmcp src/main.rs", "src/main.rs", "blue")
    tone("[WARN] wait for the current claim", "wait", "warning")
    tone('{"file":"src/lib.rs"} trailing-output', '"file"', "blue")
    tone('~~~sh {"file":"src/lib.rs"}', '"file"', "success")
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--font", type=Path,
                        default=Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be the complete build-owner commit")
    binary, decoder = args.fixture_binary.resolve(), args.decoder.resolve()
    for path in (binary, decoder, args.font):
        if not path.is_file():
            parser.error(f"required file is absent: {path}")
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    report = {
        "schema": "ssmcp.highlight-acceptance.v1",
        "status": "running",
        "synthetic_input": True,
        "source_revision_supplied_by_build_owner": args.source_revision,
        "fixture_binary": describe(binary),
        "decoder": describe(decoder),
        "runner": describe(Path(__file__).resolve()),
        "font": describe(args.font.resolve()),
        "frames": [],
    }
    try:
        env = os.environ.copy()
        env["SSMCP_HIGHLIGHT_SNAPSHOT_DIR"] = str(output)
        command = [str(binary), "--exact", TEST, "--ignored", "--nocapture", "--test-threads=1"]
        result = run(command, env=env)
        (output / "fixture.stdout.txt").write_bytes(result.stdout)
        (output / "fixture.stderr.txt").write_bytes(result.stderr)
        report["fixture"] = {"command": command, "exit_code": result.returncode}
        if result.returncode != 0:
            raise AssertionError(f"production fixture exited {result.returncode}")
        fixture = json.loads((output / "fixture.json").read_text())
        expected_pairs = {("dark", 48), ("dark", 120), ("light", 48), ("light", 120)}
        if len(fixture["frames"]) != 4 or {(frame["theme"], frame["cols"]) for frame in fixture["frames"]} != expected_pairs:
            raise AssertionError("fixture did not export the required four configurations")
        source_bytes = (output / "input.txt").read_bytes()
        input_source = Path(__file__).resolve().parents[1] / "src/client/terminal/events/highlight_fixture.txt"
        report["fixture_input_source"] = describe(input_source)
        if source_bytes != input_source.read_bytes():
            raise AssertionError("the built fixture input differs from this acceptance source")
        no_color = (output / "no-color.txt").read_bytes()
        if source_bytes != no_color or b"\x1b" in no_color:
            raise AssertionError("NO_COLOR renderer output changed the source bytes")
        report["no_color"] = {"mode": fixture["no_color_mode"], "byte_equal": True,
                              "input": describe(output / "input.txt"),
                              "output": describe(output / "no-color.txt")}
        renderer = Path(__file__).resolve().with_name("render_ui_snapshot.py")
        report["png_renderer"] = describe(renderer)
        for spec in fixture["frames"]:
            stem = spec["stem"]
            ansi = output / (stem + ".ansi")
            requests = [
                {"op": "resize", "rows": spec["rows"], "cols": spec["cols"]},
                {"op": "feed", "base64": base64.b64encode(ansi.read_bytes()).decode("ascii")},
                {"op": "snapshot", "cells": True},
                {"op": "quit"},
            ]
            decoded = run([str(decoder)], input="".join(json.dumps(row) + "\n" for row in requests),
                          text=True)
            replies = [json.loads(line) for line in decoded.stdout.splitlines()]
            if decoded.returncode != 0 or len(replies) != 4 or not all(r.get("ok") for r in replies):
                raise AssertionError(f"VT decoder failed: exit={decoded.returncode}; {decoded.stderr}")
            snapshot = replies[2]
            cell_path = output / (stem + ".cells.json")
            cell_path.write_text(json.dumps(snapshot, ensure_ascii=False) + "\n")
            checks = check_frame(snapshot, spec, source_bytes.decode("utf-8"))
            png = output / (stem + ".png")
            rendered = run([sys.executable, str(renderer), str(cell_path), str(png),
                            "--font", str(args.font)])
            if rendered.returncode != 0:
                raise AssertionError(f"PNG renderer failed: {rendered.stderr.decode(errors='replace')}")
            report["frames"].append({
                **spec, "decoder_exit_code": decoded.returncode,
                "renderer_exit_code": rendered.returncode,
                "logical_lines_visible": True, "color_checks": checks,
                "ansi": describe(ansi), "cells": describe(cell_path), "png": describe(png),
            })
        for key, path in (("fixture_binary", binary), ("decoder", decoder)):
            if describe(path) != report[key]:
                raise AssertionError(f"{key} changed while acceptance was running")
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        raise
    finally:
        (output / "report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps({"status": report["status"], "report": str(output / "report.json"),
                      "pngs": [frame["png"]["path"] for frame in report["frames"]]}))


if __name__ == "__main__":
    main()
