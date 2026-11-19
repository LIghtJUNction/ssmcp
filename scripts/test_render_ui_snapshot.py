#!/usr/bin/env python3
"""Check wide-character backgrounds using real ANSI, vt_snapshot and rendered pixels."""

import argparse
import base64
import json
from pathlib import Path
import subprocess
import sys
import tempfile

from PIL import Image


def check(decoder, renderer, font, output):
    light = (250, 250, 250)
    dark = (16, 18, 20)
    blue = (25, 85, 155)
    fixtures = [
        ("light", "\x1b[38;2;32;33;36;48;2;250;250;250m账号\x1b[0m", [light] * 4),
        ("dark", "\x1b[38;2;232;234;237;48;2;16;18;20m账号\x1b[0m", [dark] * 4),
        ("mixed", "\x1b[38;2;32;33;36;48;2;250;250;250m账\x1b[48;2;25;85;155m号\x1b[0m",
         [light, light, blue, blue]),
        ("inverse", "\x1b[38;2;25;85;155;48;2;250;250;250;7m账号\x1b[0m", [blue] * 4),
    ]
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for name, ansi, expected in fixtures:
        requests = [
            {"op": "resize", "rows": 1, "cols": 4},
            {"op": "feed", "base64": base64.b64encode(ansi.encode()).decode()},
            {"op": "snapshot", "cells": True},
        ]
        decoded = subprocess.run(
            [str(decoder)], input="\n".join(json.dumps(item) for item in requests) + "\n",
            text=True, capture_output=True, check=True,
        )
        snapshot = json.loads(decoded.stdout.splitlines()[-1])
        assert snapshot["cells"][0][0]["wide"]
        assert snapshot["cells"][0][1]["continuation"]
        path = output / f"{name}.json"
        path.write_text(json.dumps(snapshot, ensure_ascii=False), encoding="utf-8")
        picture = output / f"{name}.png"
        subprocess.run(
            [sys.executable, str(renderer), str(path), str(picture), "--font", str(font)],
            check=True, capture_output=True, text=True,
        )
        with Image.open(picture) as image:
            cell_width = (image.width - 48) // 4
            # The top pixel in each cell is above glyph ink, including CJK descenders.
            actual = [image.getpixel((24 + col * cell_width + cell_width // 2, 24))
                      for col in range(4)]
        assert actual == expected, f"{name}: expected {expected}, got {actual}"
        results.append({"case": name, "pixels": actual, "cells": snapshot["cells"][0]})
    (output / "results.json").write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"PASS: {len(results)} ANSI-to-pixel wide-character background cases")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--renderer", type=Path, default=Path(__file__).with_name("render_ui_snapshot.py"))
    parser.add_argument("--font", type=Path, default=Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if args.out:
        check(args.decoder, args.renderer, args.font, args.out)
    else:
        with tempfile.TemporaryDirectory(prefix="ssmcp-preview-test-") as directory:
            check(args.decoder, args.renderer, args.font, Path(directory))


if __name__ == "__main__":
    main()
