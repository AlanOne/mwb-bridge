#!/usr/bin/env python3
"""Draws the companion's icons and writes them next to this script.

The same two-headed arrow as the Mac bridge's menu bar icon
(daemon/macos/src/tray.rs, bridge_icon), in the same proportions:
- tray-dark.ico and tray-light.ico: the bare arrow, white for a dark
  taskbar and black for a light one, picked by the app at runtime the way
  macOS recolors its template image;
- icon.ico: the app's own icon, a white arrow on a blue tile.
Plain geometry, supersampled for antialiasing, so it needs nothing beyond
the Python standard library. Re-run after changing the drawing:
python3 windows/companion/make-icon.py (add --preview to also write
icon-preview.png for a look at every size).
"""
import struct
import sys
import zlib
from pathlib import Path

APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
TRAY_SIZES = [16, 20, 24, 32, 40, 48]

TILE_TOP = (0x3B, 0x82, 0xF6)
TILE_BOTTOM = (0x1D, 0x4E, 0xD8)
WHITE = (0xFF, 0xFF, 0xFF)
BLACK = (0x00, 0x00, 0x00)

# The Mac draws on a 36-pixel grid: 3 pixels of margin, arrowheads 10 long
# and 13 tall, a shaft 5 tall.
MAC = 36.0
MARGIN = 3 / MAC
HEAD_LEN = 10 / MAC
HEAD_HALF = 6.5 / MAC
SHAFT_HALF = 2.5 / MAC


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


def arrow_poly(size, x0, x1, scale):
    """The two-headed arrow between x0 and x1, centered vertically.

    Small sizes put the shaft's edges and the tips on whole pixels so they
    stay crisp: an odd shaft is centered on a pixel, an even one between two.
    """
    if size > 32:
        cy, half = 0.5, SHAFT_HALF * scale
    else:
        shaft_px = max(1, round(2 * SHAFT_HALF * scale * size))
        mid = size // 2 if shaft_px % 2 == 0 else size // 2 + 0.5
        cy, half = mid / size, shaft_px / 2 / size
        x0, x1 = round(x0 * size) / size, round(x1 * size) / size
    head_len, head_half = HEAD_LEN * scale, HEAD_HALF * scale
    return [
        (x0, cy),
        (x0 + head_len, cy - head_half),
        (x0 + head_len, cy - half),
        (x1 - head_len, cy - half),
        (x1 - head_len, cy - head_half),
        (x1, cy),
        (x1 - head_len, cy + head_half),
        (x1 - head_len, cy + half),
        (x0 + head_len, cy + half),
        (x0 + head_len, cy + head_half),
    ]


def tray_shapes(color):
    def shapes(size):
        arr = arrow_poly(size, MARGIN, 1 - MARGIN, 1.0)
        return [(color, lambda x, y: inside_poly(x, y, arr))]
    return shapes


def app_shapes(size):
    # The arrow fills less of the tile at big sizes, where there's room for
    # a margin, and nearly all of it at small ones, where every pixel counts.
    scale = 0.86 if size <= 24 else 0.74
    width = (1 - 2 * MARGIN) * scale
    arr = arrow_poly(size, 0.5 - width / 2, 0.5 + width / 2, scale)
    return [
        ("tile", lambda x, y: inside_round_rect(x, y, 0.02, 0.02, 0.98, 0.98, 0.22)),
        (WHITE, lambda x, y: inside_poly(x, y, arr)),
    ]


def render(size, shapes_for):
    ss = 8 if size <= 64 else 4
    shapes = shapes_for(size)
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
                        color = tuple(a + (b - a) * y for a, b in zip(TILE_TOP, TILE_BOTTOM))
                    for i in range(3):
                        acc[i] += color[i]
                    acc[3] += 1
            a = acc[3] / (ss * ss)
            rgb = [round(acc[i] / acc[3]) for i in range(3)] if acc[3] else [0, 0, 0]
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


def preview(strips, path, zoom=1):
    """Each strip's icons side by side on its background, zoomed by `zoom`."""
    gap = 8
    width = max(sum(s * zoom for s, _ in imgs) + gap * (len(imgs) + 1) for _, imgs in strips)
    height = max(s * zoom for _, imgs in strips for s, _ in imgs) + 2 * gap
    rows = []
    for bg, imgs in strips:
        canvas = [[bg + (255,) for _ in range(width)] for _ in range(height)]
        x = gap
        for s, img in imgs:
            top = height - gap - s * zoom
            for yy in range(s * zoom):
                for xx in range(s * zoom):
                    r, g, b, a = img[yy // zoom][xx // zoom]
                    f = a / 255
                    canvas[top + yy][x + xx] = tuple(round(v * f + w * (1 - f)) for v, w in zip((r, g, b), bg)) + (255,)
            x += s * zoom + gap
        rows += canvas
    path.write_bytes(png(rows))


def main():
    here = Path(__file__).resolve().parent
    app = [(s, render(s, app_shapes)) for s in APP_SIZES]
    light = [(s, render(s, tray_shapes(BLACK))) for s in TRAY_SIZES]
    dark = [(s, render(s, tray_shapes(WHITE))) for s in TRAY_SIZES]
    (here / "icon.ico").write_bytes(ico(app))
    (here / "tray-light.ico").write_bytes(ico(light))
    (here / "tray-dark.ico").write_bytes(ico(dark))
    if "--preview" in sys.argv:
        light_bg, dark_bg = (0xF3, 0xF3, 0xF3), (0x20, 0x20, 0x20)
        preview([(light_bg, app), (dark_bg, app), (light_bg, light), (dark_bg, dark)], here / "icon-preview.png")
        small = lambda imgs: [(s, i) for s, i in imgs if s <= 32]
        preview([(light_bg, small(app)), (light_bg, small(light)), (dark_bg, small(dark))], here / "icon-preview-zoomed.png", zoom=8)


if __name__ == "__main__":
    main()
