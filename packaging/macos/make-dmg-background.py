#!/usr/bin/env python3
"""Draws the background of the POM installer window.

The window is 660x400 points. The POM logo sits centred at the top, and an
arrow runs from the POM.app icon (left) to the Applications shortcut (right).
Icon positions must match the `--icon` and `--app-drop-link` coordinates in
`make-dmg.sh`.

    python3 packaging/macos/make-dmg-background.py

Writes dmg-background.png (1x) and dmg-background@2x.png (2x) next to this file.
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
LOGO = HERE.parents[1] / "assets" / "brand" / "pom-logo-light.png"

WIDTH, HEIGHT = 660, 400
APP_X, DROP_X, ICON_Y = 165, 495, 215  # keep in sync with make-dmg.sh
ARROW_COLOR = (11, 95, 208)  # POM blue from the logo gradient


def draw(scale: int) -> Image.Image:
    w, h = WIDTH * scale, HEIGHT * scale
    canvas = Image.new("RGB", (w, h), (255, 255, 255))

    logo = Image.open(LOGO).convert("RGBA")
    logo_w = 300 * scale
    logo_h = round(logo.height * logo_w / logo.width)
    logo = logo.resize((logo_w, logo_h), Image.LANCZOS)
    canvas.paste(logo, ((w - logo_w) // 2, 28 * scale), logo)

    # Arrow between the two icons, with a little space from each icon's edge.
    # Drawn at 4x and reduced, so the edges stay smooth.
    ss = 4
    arrow = Image.new("RGBA", (w * ss, h * ss), (0, 0, 0, 0))
    d = ImageDraw.Draw(arrow)
    x0 = (APP_X + 78) * scale * ss
    x1 = (DROP_X - 78) * scale * ss
    y = ICON_Y * scale * ss
    shaft = 7 * scale * ss
    head_len = 26 * scale * ss
    head_half = 17 * scale * ss
    d.rectangle([x0, y - shaft, x1 - head_len, y + shaft], fill=ARROW_COLOR + (255,))
    d.polygon(
        [
            (x1 - head_len, y - head_half),
            (x1, y),
            (x1 - head_len, y + head_half),
        ],
        fill=ARROW_COLOR + (255,),
    )
    arrow = arrow.resize((w, h), Image.LANCZOS)
    canvas.paste(arrow, (0, 0), arrow)
    return canvas


def main() -> None:
    draw(1).save(HERE / "dmg-background.png")
    draw(2).save(HERE / "dmg-background@2x.png")
    print("wrote", HERE / "dmg-background.png", "and @2x")


if __name__ == "__main__":
    main()
