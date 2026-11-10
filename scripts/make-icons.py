#!/usr/bin/env python3
"""Иконки Site Restorer по знаку с сайта lastcopy.ru.

Знак: тёмный квадрат #2D3748 со скруглением, три белых квадрата 2×2 и четвёртый
пунктирный — «копия, которую можно восстановить». Геометрия взята из разметки
шапки сайта (viewBox 0 0 32 32):

    rect 3,3 11×11 rx1.2   rect 16.5,3 11×11 rx1.2
    rect 3,16.5 11×11 rx1.2   rect 17.2,17.2 9.6×9.6 пунктир #9FB0C2 1.5 (2.2/2)

Запуск: python scripts/make-icons.py
Пишет: packaging/icons/*.png, site-restorer.svg, site-restorer.ico, site-restorer-64.rgba
"""

import io
import os
import struct

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "packaging", "icons")
SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
BG = (0x2D, 0x37, 0x48, 255)
FG = (255, 255, 255, 255)
DASH = (0x9F, 0xB0, 0xC2, 255)
SS = 4  # суперсэмплинг


def draw_icon(size):
    """Рисует знак на прозрачном поле size×size в цвете BG."""
    s = size * SS
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    k = s / 32.0  # масштаб координат сайта

    radius = 0.18 * s
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=radius, fill=BG)

    def sq(x, y, w, h, r):
        d.rounded_rectangle(
            [x * k, y * k, (x + w) * k - 1, (y + h) * k - 1], radius=r * k, fill=FG
        )

    sq(3, 3, 11, 11, 1.2)
    sq(16.5, 3, 11, 11, 1.2)
    sq(3, 16.5, 11, 11, 1.2)

    # четвёртый квадрат — пунктирный
    x, y, w, h = 17.2, 17.2, 9.6, 9.6
    width = max(1, int(round(1.5 * k)))
    dash, gap = 2.2 * k, 2.0 * k
    box = [x * k, y * k, (x + w) * k, (y + h) * k]
    r = 1.0 * k

    points = []
    # верх и низ
    for (x0, y0, x1, y1) in (
        (box[0] + r, box[1], box[2] - r, box[1]),
        (box[0] + r, box[3], box[2] - r, box[3]),
        (box[0], box[1] + r, box[0], box[3] - r),
        (box[2], box[1] + r, box[2], box[3] - r),
    ):
        length = ((x1 - x0) ** 2 + (y1 - y0) ** 2) ** 0.5
        if length <= 0:
            continue
        ux, uy = (x1 - x0) / length, (y1 - y0) / length
        t = 0.0
        while t < length:
            end = min(t + dash, length)
            points.append((x0 + ux * t, y0 + uy * t, x0 + ux * end, y0 + uy * end))
            t = end + gap
    for (x0, y0, x1, y1) in points:
        d.line([x0, y0, x1, y1], fill=DASH, width=width)

    return img.resize((size, size), Image.LANCZOS)


def main():
    os.makedirs(OUT, exist_ok=True)
    images = {}
    for size in SIZES:
        img = draw_icon(size)
        images[size] = img
        path = os.path.join(OUT, f"site-restorer-{size}.png")
        img.save(path, "PNG", optimize=True)
        print(f"{path}: {os.path.getsize(path)} байт")

    ico = os.path.join(OUT, "site-restorer.ico")
    images[256].save(ico, sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    print(f"{ico}: {os.path.getsize(ico)} байт")

    raw = images[64].tobytes()
    rgba = os.path.join(OUT, "site-restorer-64.rgba")
    with open(rgba, "wb") as fh:
        fh.write(raw)
    print(f"{rgba}: {os.path.getsize(rgba)} байт (64×64×4)")

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="512" height="512" role="img" aria-label="Site Restorer">
  <title>Site Restorer</title>
  <rect width="512" height="512" rx="92" fill="#2D3748"/>
  <rect x="48" y="48" width="176" height="176" rx="19" fill="#FFFFFF"/>
  <rect x="264" y="48" width="176" height="176" rx="19" fill="#FFFFFF"/>
  <rect x="48" y="264" width="176" height="176" rx="19" fill="#FFFFFF"/>
  <rect x="275" y="275" width="154" height="154" rx="16" fill="none" stroke="#9FB0C2" stroke-width="24" stroke-dasharray="35 32"/>
</svg>
"""
    path = os.path.join(OUT, "site-restorer.svg")
    with io.open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(svg)
    print(f"{path}: {os.path.getsize(path)} байт")


if __name__ == "__main__":
    main()
