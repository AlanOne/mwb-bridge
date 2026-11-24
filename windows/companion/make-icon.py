#!/usr/bin/env python3
"""Draws the companion's icon and writes icon.ico next to this script.

A white document on a blue tile, with an orange two-way arrow across it:
files crossing between machines. Plain geometry, supersampled for
antialiasing, so it needs nothing beyond the Python standard library.
Re-run after changing the drawing: python3 windows/companion/make-icon.py
(add --preview to also write icon-preview.png for a look at every size).
"""
import struct
import sys
import zlib
from pathlib import Path

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]

TILE_TOP = (0x3B, 0x82, 0xF6)
TILE_BOTTOM = (0x1D, 0x4E, 0xD8)
PAPER = (0xFF, 0xFF, 0xFF)
FOLD = (0xBF, 0xDB, 0xFE)
LINES = (0xC7, 0xD2, 0xE0)
ARROW = (0xF9, 0x73, 0x16)


def inside_poly(x, y, pts):
    hit = False
    j = len(pts) - 1
    for i in range(len(pts)):
        xi, yi = pts[i]
        xj, yj = pts[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            hit = not hit
        j = i
    return hit


def inside_round_rect(x, y, x0, y0, x1, y1, r):
    if not (x0 <= x <= x1 and y0 <= y <= y1):
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def arrow(cy, x0, x1, shaft, head_len, head_half):
    """A two-headed horizontal arrow as one polygon."""
    return [
        (x0, cy),
        (x0 + head_len, cy - head_half),
        (x0 + head_len, cy - shaft),
        (x1 - head_len, cy - shaft),
        (x1 - head_len, cy - head_half),
        (x1, cy),
        (x1 - head_len, cy + head_half),
        (x1 - head_len, cy + shaft),
        (x0 + head_len, cy + shaft),
        (x0 + head_len, cy + head_half),
    ]


ARROW_Y = 0.665


def layers(size):
    """The shapes, back to front, as (color, test(x, y)) in unit coordinates."""
    small = size <= 24
    doc = (0.27, 0.14, 0.73, 0.84) if not small else (0.24, 0.12, 0.76, 0.86)
    fold = 0.15 if not small else 0.17
    # Small sizes put the edges on whole pixels so they stay crisp.
    snap = (lambda v: round(v * size) / size) if size <= 32 else (lambda v: v)
    x0, y0, x1, y1 = (snap(v) for v in doc)
    fold = snap(fold)
    doc_pts = [(x0, y0), (x1 - fold, y0), (x1, y0 + fold), (x1, y1), (x0, y1)]
    fold_pts = [(x1 - fold, y0), (x1 - fold, y0 + fold), (x1, y0 + fold)]

    if small:
        ax0, ax1, shaft, head_len, head_half, gap = 0.06, 0.94, 0.06, 0.19, 0.19, 0.0
    else:
        ax0, ax1, shaft, head_len, head_half, gap = 0.10, 0.90, 0.048, 0.15, 0.13, 0.04
    ay, shaft, ax0, ax1 = snap(ARROW_Y), max(snap(shaft), 1 / size), snap(ax0), snap(ax1)
    arr = arrow(ay, ax0, ax1, shaft, head_len, head_half)
    # The same arrow a little bigger: a tile-colored gap around it on the paper.
    halo = arrow(ay, ax0 - 1.6 * gap, ax1 + 1.6 * gap, shaft + gap, head_len + 0.5 * gap, head_half + 1.9 * gap)

    out = [
        ("tile", lambda x, y: inside_round_rect(x, y, 0.02, 0.02, 0.98, 0.98, 0.22)),
        (PAPER, lambda x, y: inside_poly(x, y, doc_pts)),
        (FOLD, lambda x, y: inside_poly(x, y, fold_pts)),
    ]
    if not small:
        for ly, lx1 in ((0.33, x1 - fold - 0.04), (0.43, x1 - 0.08), (0.53, x1 - 0.14)):
            half = 0.02 if size > 32 else 0.5 / size
            ly = ly if size > 32 else (int(ly * size) + 0.5) / size
            out.append((LINES, lambda x, y, ly=ly, lx1=snap(lx1), h=half: snap(x0 + 0.07) <= x <= lx1 and ly - h <= y <= ly + h))
    if gap:
        out.append(("tile", lambda x, y: inside_poly(x, y, halo) and inside_poly(x, y, doc_pts)))
    out.append((ARROW, lambda x, y: inside_poly(x, y, arr)))
    return out


def render(size):
    ss = 8 if size <= 64 else 4
    shapes = layers(size)
    px = []
    for py in range(size):
        row = []
        for pxl in range(size):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(ss):
                for sx in range(ss):
                    x = (pxl + (sx + 0.5) / ss) / size
                    y = (py + (sy + 0.5) / ss) / size
                    color = None
                    for c, test in shapes:
                        if test(x, y):
                            color = c
                    if color is None:
                        continue
                    if color == "tile":
                        t = y
                        color = tuple(a + (b - a) * t for a, b in zip(TILE_TOP, TILE_BOTTOM))
                    for i in range(3):
                        acc[i] += color[i]
                    acc[3] += 1
            n = ss * ss
            a = acc[3] / n
            if acc[3]:
                rgb = [round(acc[i] / acc[3]) for i in range(3)]
            else:
                rgb = [0, 0, 0]
            row.append((rgb[0], rgb[1], rgb[2], round(a * 255)))
        px.append(row)
    return px


def png(px):
    h = len(px)
    w = len(px[0])
    raw = b"".join(b"\0" + bytes(c for p in row for c in p) for row in px)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def dib(px):
    """A 32-bit icon bitmap: BGRA rows bottom-up, then an empty AND mask."""
    size = len(px)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = b"".join(bytes((p[2], p[1], p[0], p[3])) for row in reversed(px) for p in row)
    mask_row = ((size + 31) // 32) * 4
    return header + pixels + b"\0" * (mask_row * size)


def ico(images):
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    blobs = []
    for size, px in images:
        # Classic bitmaps below 256 (every reader takes those), PNG at 256.
        data = png(px) if size >= 256 else dib(px)
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blobs.append(data)
    return out + b"".join(blobs)


def main():
    here = Path(__file__).resolve().parent
    images = [(s, render(s)) for s in SIZES]
    (here / "icon.ico").write_bytes(ico(images))
    if "--preview" in sys.argv:
        # Every size side by side, on light and dark strips, at 1:1.
        gap = 8
        width = sum(s for s, _ in images) + gap * (len(images) + 1)
        height = 256 + 2 * gap
        rows = []
        for bg in ((0xF3, 0xF3, 0xF3), (0x20, 0x20, 0x20)):
            canvas = [[bg + (255,) for _ in range(width)] for _ in range(height)]
            x = gap
            for s, img in images:
                top = height - gap - s
                for yy in range(s):
                    for xx in range(s):
                        r, g, b, a = img[yy][xx]
                        br, bg_, bb, _ = canvas[top + yy][x + xx]
                        f = a / 255
                        canvas[top + yy][x + xx] = (
                            round(r * f + br * (1 - f)),
                            round(g * f + bg_ * (1 - f)),
                            round(b * f + bb * (1 - f)),
                            255,
                        )
                x += s + gap
            rows += canvas
        (here / "icon-preview.png").write_bytes(png(rows))


if __name__ == "__main__":
    main()
