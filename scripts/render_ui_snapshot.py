#!/usr/bin/env python3
"""Render a vt_snapshot JSON cell grid as a PNG for visual review.

The input is captured from the production compositor, not a UI mockup.
Requires Pillow and a monospace TrueType font; neither is used by the client.
"""

import argparse
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


ANSI = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
]


def rgb(value, default):
    if value is None:
        return default
    if isinstance(value, list):
        return tuple(value)
    if value < 16:
        text = ANSI[value].lstrip("#")
        return tuple(int(text[i:i + 2], 16) for i in (0, 2, 4))
    if value < 232:
        value -= 16
        levels = (0, 95, 135, 175, 215, 255)
        return tuple(levels[i] for i in (value // 36, value // 6 % 6, value % 6))
    return (8 + 10 * (value - 232),) * 3


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--font", type=Path,
                        default=Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    parser.add_argument("--size", type=int, default=18)
    args = parser.parse_args()
    snapshot = json.loads(args.snapshot.read_text())
    rows, cols = snapshot["size"]
    if not 1 <= rows <= 1000 or not 1 <= cols <= 1000:
        raise ValueError("snapshot size is outside the VT decoder bounds")
    font = ImageFont.truetype(str(args.font), args.size)
    bold_path = args.font.with_name(args.font.stem + "-Bold.ttf")
    bold = ImageFont.truetype(str(bold_path), args.size) if bold_path.is_file() else font
    cell_width = round(font.getlength("M"))
    cell_height = round(args.size * 1.55)
    padding = 24
    default_bg, default_fg = (16, 18, 20), (232, 234, 237)
    canvas = Image.new("RGB", (cols * cell_width + padding * 2,
                               rows * cell_height + padding * 2),
                       rgb(snapshot["cells"][0][0]["background"], default_bg))
    draw = ImageDraw.Draw(canvas)
    glyphs = []
    for row_index, row in enumerate(snapshot["cells"]):
        for col_index, cell in enumerate(row):
            fg = rgb(cell.get("foreground"), default_fg)
            bg = rgb(cell.get("background"), default_bg)
            if cell.get("inverse"):
                fg, bg = bg, fg
            if cell.get("dim"):
                fg = tuple(round((a + b) / 2) for a, b in zip(fg, bg))
            x, y = padding + col_index * cell_width, padding + row_index * cell_height
            draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=bg)
            if cell["text"] and not cell.get("continuation"):
                glyphs.append((x, y, cell["text"], fg, bold if cell.get("bold") else font))
    # A wide glyph spans its continuation cell, whose background must be drawn first.
    for x, y, value, fg, glyph_font in glyphs:
        draw.text((x, y + 3), value, fill=fg, font=glyph_font)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(args.output, optimize=True)
    print(f"{args.output}: {canvas.width}×{canvas.height}, {cols}×{rows} VT cells")


if __name__ == "__main__":
    main()
