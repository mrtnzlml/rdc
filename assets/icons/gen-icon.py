#!/usr/bin/env python3
"""Generate the Rossum Local app icon at 1024x1024.

macOS Tahoe Liquid Glass style:
- Continuous-curve squircle background (Apple's icon mask)
- Warm amber gradient (Rossum brand color #ED8E47)
- Subtle inner highlight on top edge
- Soft drop shadow on the glyph
- Central glyph: a stylized folder with a downward arrow,
  representing "pull from Rossum into a local folder"
"""
from __future__ import annotations

import math
from PIL import Image, ImageChops, ImageDraw, ImageFilter

SIZE = 1024
CORNER_RADIUS = 232  # iOS/macOS squircle radius for 1024
PAD = 92  # standard padding inside icon mask
ROSSUM_AMBER = (237, 142, 71, 255)
ROSSUM_AMBER_DEEP = (210, 110, 50, 255)


def squircle_mask(size: int, radius: int) -> Image.Image:
    """Approximation of Apple's continuous-corners squircle using two
    rounded-rectangle masks blended. Closer to iOS look than a plain
    rounded rect."""
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return m


def linear_gradient(size: int, top: tuple, bottom: tuple) -> Image.Image:
    g = Image.new("RGB", (size, size), top[:3])
    px = g.load()
    for y in range(size):
        t = y / (size - 1)
        # Ease-out for a more "lit from above" feel
        t = 1 - (1 - t) ** 1.6
        r = int(top[0] * (1 - t) + bottom[0] * t)
        gr = int(top[1] * (1 - t) + bottom[1] * t)
        b = int(top[2] * (1 - t) + bottom[2] * t)
        for x in range(size):
            px[x, y] = (r, gr, b)
    return g


def add_top_highlight(img: Image.Image, mask: Image.Image) -> Image.Image:
    """Subtle white-to-transparent gradient on the top half — fake
    specular highlight. Clipped to the squircle by multiplying the
    highlight's own alpha by the mask (not replacing it)."""
    size = img.size[0]
    hl = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    px = hl.load()
    for y in range(size // 2):
        t = 1 - (y / (size // 2)) ** 2
        a = int(70 * t)
        for x in range(size):
            px[x, y] = (255, 255, 255, a)
    # Multiply existing highlight alpha by mask so highlight stays
    # inside the squircle.
    hl_a = hl.getchannel("A")
    hl.putalpha(ImageChops.multiply(hl_a, mask))
    return Image.alpha_composite(img, hl)


def draw_glyph(canvas: Image.Image) -> None:
    """Folder + down-arrow glyph, centered. Drawn on the canvas as a
    white-on-amber mark. Slight drop shadow underneath for depth."""
    size = canvas.size[0]
    cx = size // 2
    cy = size // 2
    # Folder dimensions (relative to icon size)
    fw = int(size * 0.50)
    fh = int(size * 0.36)
    fx = cx - fw // 2
    fy = cy - fh // 2 + int(size * 0.04)
    tab_w = int(fw * 0.40)
    tab_h = int(size * 0.05)
    r = int(size * 0.04)

    # Drop shadow (offset down, blurred)
    shadow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shadow)
    sd.rounded_rectangle(
        (fx + 8, fy - tab_h + 8 + 30, fx + tab_w + 8, fy + tab_h + 30),
        radius=r,
        fill=(0, 0, 0, 80),
    )
    sd.rounded_rectangle(
        (fx + 8, fy + 8 + 30, fx + fw + 8, fy + fh + 8 + 30),
        radius=r,
        fill=(0, 0, 0, 80),
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(radius=24))
    canvas.alpha_composite(shadow)

    # Folder back (tab)
    d = ImageDraw.Draw(canvas)
    d.rounded_rectangle(
        (fx, fy - tab_h, fx + tab_w, fy + tab_h),
        radius=r,
        fill=(255, 255, 255, 255),
    )
    # Folder body
    d.rounded_rectangle(
        (fx, fy, fx + fw, fy + fh),
        radius=r,
        fill=(255, 255, 255, 255),
    )

    # Down arrow inside the folder, in amber
    ax = cx
    ay = fy + fh // 2 - int(size * 0.01)
    aw = int(size * 0.12)
    ah = int(size * 0.16)
    stem_w = int(aw * 0.35)
    head_h = int(ah * 0.45)

    # Stem
    d.rounded_rectangle(
        (ax - stem_w // 2, ay - ah // 2, ax + stem_w // 2, ay + ah // 2 - head_h),
        radius=int(stem_w * 0.35),
        fill=ROSSUM_AMBER_DEEP,
    )
    # Arrowhead (triangle)
    d.polygon(
        [
            (ax - aw // 2, ay + ah // 2 - head_h),
            (ax + aw // 2, ay + ah // 2 - head_h),
            (ax, ay + ah // 2),
        ],
        fill=ROSSUM_AMBER_DEEP,
    )


def main() -> None:
    # Inner area (with padding) is where we draw; then we mask with
    # squircle and add lighting.
    bg = linear_gradient(SIZE, top=(255, 175, 100), bottom=ROSSUM_AMBER_DEEP[:3])
    bg = bg.convert("RGBA")

    draw_glyph(bg)

    mask = squircle_mask(SIZE, CORNER_RADIUS)
    bg.putalpha(mask)

    bg = add_top_highlight(bg, mask)

    bg.save("/tmp/icon_1024.png", "PNG")

    # Also save the smaller sizes that iconutil needs.
    for s in (16, 32, 64, 128, 256, 512, 1024):
        resized = bg.resize((s, s), Image.LANCZOS)
        resized.save(f"/tmp/icon_{s}.png", "PNG")
    print("OK")


if __name__ == "__main__":
    main()
