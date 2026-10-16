#!/usr/bin/env python3
"""Derives the dark-background version of the lockup from the light art.

It is not new art: it is the `brand.json` -> `logo.light` art recolored. The
lettering ("POM" and "PIECE OF MIND") is flat petrol blue, designed for a light
background, and vanishes on a dark panel; this script mirrors its luminance
(dark becomes light) while preserving hue and saturation. The brain
illustration is **not** touched: it already has its own contrast on dark, and
recoloring it flattened the volume.

    python3 scripts/derive-dark-logo.py

Writes `assets/brand/pom-logo-dark.png`. When a truly designed dark art exists,
just overwrite that file and delete this script.

Requires Pillow and numpy.
"""
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
INPUT = ROOT / "assets/brand/pom-logo-light.png"
OUTPUT = ROOT / "assets/brand/pom-logo-dark.png"

# Rectangle of the illustration in the source file (680x411). Measured from the
# golden glow, which only exists in the brain.
ILLUSTRATION = (398, 2, 676, 250)


def to_hls(rgb):
    mx, mn = rgb.max(-1), rgb.min(-1)
    l, d = (mx + mn) / 2, mx - mn
    s = np.where(d == 0, 0, d / np.where(l < 0.5, mx + mn + 1e-9, 2 - mx - mn + 1e-9))
    r, g, b = rgb[..., 0], rgb[..., 1], rgb[..., 2]
    dd = np.where(d == 0, 1, d)
    rc, gc, bc = (mx - r) / dd, (mx - g) / dd, (mx - b) / dd
    h = np.where(mx == r, bc - gc, np.where(mx == g, 2 + rc - bc, 4 + gc - rc)) / 6 % 1.0
    return h, l, s


def from_hls(h, l, s):
    def channel(n):
        k = (n + h * 12) % 12
        a = s * np.minimum(l, 1 - l)
        return l - a * np.maximum(-1, np.minimum(np.minimum(k - 3, 9 - k), 1))

    return np.stack([channel(0), channel(8), channel(4)], -1)


def main() -> int:
    if not INPUT.exists():
        print(f"não achei {INPUT}", file=sys.stderr)
        return 1
    src = Image.open(INPUT).convert("RGBA")
    w, h_img = src.size
    a = np.asarray(src).astype(np.float32) / 255.0
    rgb, alpha = a[..., :3], a[..., 3:4]

    h, l, s = to_hls(rgb)
    light = from_hls(h, np.clip(1 - l * 0.92, 0, 1), np.clip(s * 1.05, 0, 1))

    # Mask of what stays intact, with a soft edge so as not to leave a seam.
    m = Image.new("L", (w, h_img), 0)
    ImageDraw.Draw(m).rounded_rectangle(list(ILLUSTRATION), radius=40, fill=255)
    m = m.filter(ImageFilter.GaussianBlur(14))
    keep = (np.asarray(m).astype(np.float32) / 255.0)[..., None]

    output = rgb * keep + light * (1 - keep)
    Image.fromarray(
        (np.concatenate([output, alpha], -1) * 255).astype(np.uint8), "RGBA"
    ).save(OUTPUT)
    print(f"✓ {SAIDA.relative_to(RAIZ)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
