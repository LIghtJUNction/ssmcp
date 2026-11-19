#!/usr/bin/env python3
"""Render a vt_snapshot JSON cell grid as a PNG for visual review.

The input is captured from the production compositor, not a UI mockup.
Requires Pillow and a monospace TrueType font; neither is used by the client.
"""

import argparse
import json
from functools import lru_cache
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


@lru_cache(maxsize=4096)
def glyph_mask(font, character):
    mask = font.getmask(character, mode="L")
    return mask.size, bytes(mask)


def font_for_text(primary, fallback, text):
    if fallback is not None:
        # A Unicode noncharacter gives this font's missing-glyph template.
        missing = glyph_mask(primary, "\U0010ffff")
        if any(glyph_mask(primary, char) == missing for char in text):
            fallback_missing = glyph_mask(fallback, "\U0010ffff")
            if all(glyph_mask(fallback, char) != fallback_missing for char in text):
                return fallback
    return primary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--font", type=Path,
                        default=Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    parser.add_argument("--font-index", type=int, default=0,
                        help="font face within a TrueType/OpenType collection")
    parser.add_argument("--fallback-font", type=Path,
                        help="font for characters missing from the primary font")
    parser.add_argument("--fallback-font-index", type=int, default=0,
                        help="fallback font face within a TrueType/OpenType collection")
    parser.add_argument("--size", type=int, default=18)
    args = parser.parse_args()
    snapshot = json.loads(args.snapshot.read_text())
    rows, cols = snapshot["size"]
    if not 1 <= rows <= 1000 or not 1 <= cols <= 1000:
        raise ValueError("snapshot size is outside the VT decoder bounds")
    font = ImageFont.truetype(str(args.font), args.size, index=args.font_index)
    bold_path = args.font.with_name(args.font.stem + "-Bold.ttf")
    bold = ImageFont.truetype(str(bold_path), args.size) if bold_path.is_file() else font
    fallback = (ImageFont.truetype(str(args.fallback_font), args.size,
                                   index=args.fallback_font_index)
                if args.fallback_font is not None else None)
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
            # vt100 stores default attributes in a wide character's continuation.
            # Both halves use the leading cell's style, including inverse colors.
            style = cell
            if cell.get("continuation") and col_index > 0 and row[col_index - 1].get("wide"):
                style = row[col_index - 1]
            fg = rgb(style.get("foreground"), default_fg)
            bg = rgb(style.get("background"), default_bg)
            if style.get("inverse"):
                fg, bg = bg, fg
            if style.get("dim"):
                fg = tuple(round((a + b) / 2) for a, b in zip(fg, bg))
            x, y = padding + col_index * cell_width, padding + row_index * cell_height
            draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=bg)
            if cell["text"] and not cell.get("continuation"):
                primary = bold if cell.get("bold") else font
                glyph_font = font_for_text(primary, fallback, cell["text"])
                baseline = y + 3 + primary.getmetrics()[0]
                glyphs.append((x, baseline, cell["text"], fg, glyph_font))
    # A wide glyph spans its continuation cell, whose background must be drawn first.
    for x, y, value, fg, glyph_font in glyphs:
        draw.text((x, y), value, fill=fg, font=glyph_font, anchor="ls")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(args.output, optimize=True)
    print(f"{args.output}: {canvas.width}×{canvas.height}, {cols}×{rows} VT cells")


if __name__ == "__main__":
    main()
