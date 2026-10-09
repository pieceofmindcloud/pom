#!/usr/bin/env python3
"""Derives the macOS status bar icon from `pom-icon-src.png`.

The menu bar does not accept the product icon as it is. Two reasons, and both
hurt if ignored:

**Template image.** An `NSStatusItem` marked as a template uses **only the
alpha channel** of the image and paints the result with the bar's color - black
on the light theme, white on the dark one. The POM art is a fully opaque blue
square, that is, alpha 255 everywhere: used directly, it becomes a solid black
rectangle in the bar. What is needed is a cutout of the art, and that is what
this script extracts - the white stroke becomes alpha, the blue background
becomes transparency.

**Size.** The glyph lives in an 18-point square. The "POM" wordmark that
accompanies the head takes up a third of the art's height and, reduced to that
size, is not legible - it becomes dirt. The script keeps only the head, which is
the part that is recognizable when small, and discards the text.

The separation between stroke and background is clean because the stroke is
white and everything else is blue: `min(R, G, B)` is above 240 in the art and
below 96 in the background and the midtone, with little in between. That
intermediate band is precisely the antialiasing, and it is what gives the soft
edge once normalized.

Output at 1x, 2x and 3x. `macos_status.m` loads the three as representations of
the same 18-point `NSImage`, so AppKit picks the one that matches the screen.

Usage: `python3 scripts/derive-macos-status-icon.py`
"""

from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "assets" / "brand"
SOURCE = BRAND / "pom-icon-src.png"
DESTINATION = BRAND / "macos"

# Points of the status bar item's square, and the height the glyph takes up
# inside it. The vertical margin keeps the stroke from touching the bar's edge.
BOX_POINTS = 18
GLYPH_POINTS = 16
SCALES = (1, 2, 3)

# Bounds of `min(R, G, B)`: below LOW is background, above HIGH is full stroke,
# and the middle is antialiasing, which becomes proportional alpha.
LOW = 110
HIGH = 235

# The wordmark starts after an empty band. Any row with less stroke than this
# counts as a separation, not as part of the drawing.
BLANK_ROW_THRESHOLD = 8


def artwork_alpha(image: Image.Image) -> np.ndarray:
    """Alpha 0..255 of the white stroke, measured by the darkest channel of each pixel."""
    channels = np.asarray(image.convert("RGB")).astype(np.float32)
    whiteness = channels.min(axis=2)
    alpha = (whiteness - LOW) / (HIGH - LOW)
    return (np.clip(alpha, 0.0, 1.0) * 255.0).round().astype(np.uint8)


def head_only(alpha: np.ndarray) -> np.ndarray:
    """Crops the head, cutting at the horizontal gap that separates it from the wordmark."""
    filled_per_row = (alpha > 127).sum(axis=1)
    drawn = np.flatnonzero(filled_per_row > BLANK_ROW_THRESHOLD)
    if drawn.size == 0:
        raise SystemExit(f"nenhum traço encontrado em {SOURCE}")

    first = int(drawn[0])
    gaps = np.flatnonzero(np.diff(drawn) > 1)
    last = int(drawn[gaps[0]]) if gaps.size else int(drawn[-1])

    head = alpha[first : last + 1]
    columns = np.flatnonzero((head > 127).any(axis=0))
    return head[:, int(columns[0]) : int(columns[-1]) + 1]


def template_image(glyph: np.ndarray, scale: int) -> Image.Image:
    """Composes the glyph, black and translucent, centered in the item square."""
    box = BOX_POINTS * scale
    height = GLYPH_POINTS * scale
    width = max(1, round(glyph.shape[1] * height / glyph.shape[0]))

    resized = np.asarray(
        Image.fromarray(glyph, mode="L").resize(
            (width, height), Image.Resampling.LANCZOS
        )
    ).astype(np.float32)

    # At 1x the stroke is too thin to survive the reduction: the alpha peak
    # stops at 195 and the whole glyph turns grey, with no black at all.
    # Restoring the peak to 255 gives the stroke its weight back without
    # changing the shape; at 2x and 3x the peak is already 255 and this does
    # nothing.
    peak = resized.max()
    if peak > 0:
        resized = np.clip(resized * (255.0 / peak), 0.0, 255.0)
    mask = Image.fromarray(resized.round().astype(np.uint8), mode="L")

    canvas = Image.new("RGBA", (box, box), (0, 0, 0, 0))
    black = Image.new("RGBA", mask.size, (0, 0, 0, 255))
    canvas.paste(black, ((box - width) // 2, (box - height) // 2), mask)
    return canvas


def main() -> None:
    if not SOURCE.exists():
        raise SystemExit(f"arte de origem não encontrada: {SOURCE}")

    glyph = head_only(artwork_alpha(Image.open(SOURCE)))
    DESTINATION.mkdir(parents=True, exist_ok=True)

    for scale in SCALES:
        suffix = "" if scale == 1 else f"@{scale}x"
        path = DESTINATION / f"pom-status-template{suffix}.png"
        template_image(glyph, scale).save(path, format="PNG", optimize=True)
        print(f"{path.relative_to(ROOT)}  {BOX_POINTS * scale}x{BOX_POINTS * scale}")


if __name__ == "__main__":
    main()
