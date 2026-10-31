#!/usr/bin/env python3
"""Regenerates the icon derivatives from `pom-icon-src.png`.

The three art files are manual sources. This script derives only the formats
and sizes consumed by the web, Linux, Windows and macOS packagers, keeping the
same art on every platform. Only the macOS icon changes shape: the art goes in a
rounded tile, the outline that system expects.
"""

from pathlib import Path
from io import BytesIO
import math
import struct

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "assets" / "brand"
SOURCE = BRAND / "pom-icon-src.png"
PNG_SIZES = (16, 24, 32, 48, 64, 128, 180, 256, 512)
LINUX_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
# Apple's icon grid: an 824 tile centered on a 1024 canvas.
MACOS_CANVAS = 1024
MACOS_TILE = 824
# The rounded outline of a macOS tile, drawn as a superellipse.
MACOS_TILE_EXPONENT = 5
MASK_SUPERSAMPLING = 4


def resized(source: Image.Image, size: int) -> Image.Image:
    return source.resize((size, size), Image.Resampling.LANCZOS)


def write_png(source: Image.Image, destination: Path, size: int) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    resized(source, size).save(destination, format="PNG", optimize=True)


def write_ico(source: Image.Image, destination: Path, sizes: tuple[int, ...]) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    largest = resized(source, max(sizes))
    largest.save(destination, format="ICO", sizes=[(size, size) for size in sizes])


def tile_mask(size: int) -> Image.Image:
    large = size * MASK_SUPERSAMPLING
    half = large / 2
    points = []
    for step in range(3600):
        angle = 2 * math.pi * step / 3600
        cos, sin = math.cos(angle), math.sin(angle)
        points.append((
            half + half * math.copysign(abs(cos) ** (2 / MACOS_TILE_EXPONENT), cos),
            half + half * math.copysign(abs(sin) ** (2 / MACOS_TILE_EXPONENT), sin),
        ))
    mask = Image.new("L", (large, large), 0)
    ImageDraw.Draw(mask).polygon(points, fill=255)
    return mask.resize((size, size), Image.Resampling.LANCZOS)


def macos_icon(source: Image.Image) -> Image.Image:
    # macOS does not round an app icon: the outline has to be in the art. The
    # tile is opaque because the square art has transparent seams and corners,
    # and macOS 26 boxes an icon in gray when it does not read as a solid tile.
    tile = Image.new("RGBA", (MACOS_TILE, MACOS_TILE), "white")
    tile.alpha_composite(resized(source, MACOS_TILE))
    tile.putalpha(tile_mask(MACOS_TILE))
    canvas = Image.new("RGBA", (MACOS_CANVAS, MACOS_CANVAS), (255, 255, 255, 0))
    margin = (MACOS_CANVAS - MACOS_TILE) // 2
    canvas.paste(tile, (margin, margin))
    return canvas


def write_icns(source: Image.Image, destination: Path) -> None:
    # PNG-backed ICNS resources. The type codes are the public Apple
    # representations for standard and Retina icon sizes. This avoids relying
    # on iconutil accepting a temporary iconset on every Xcode/macOS version.
    representations = (
        (b"icp4", 16),
        (b"icp5", 32),
        (b"icp6", 48),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic09", 512),
        (b"ic10", 1024),
        (b"ic11", 32),
        (b"ic12", 64),
        (b"ic13", 256),
        (b"ic14", 512),
    )
    body = bytearray()
    for kind, size in representations:
        buffer = BytesIO()
        resized(source, size).save(buffer, format="PNG", optimize=True)
        data = buffer.getvalue()
        body.extend(kind)
        body.extend(struct.pack(">I", len(data) + 8))
        body.extend(data)
    destination.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main() -> None:
    source = Image.open(SOURCE).convert("RGBA")
    for size in PNG_SIZES:
        write_png(source, BRAND / f"pom-icon-{size}.png", size)

    # The mark used by the manifest is the same square icon, with no second
    # source that could diverge.
    write_png(source, BRAND / "pom-mark.png", 512)

    for size in LINUX_SIZES:
        write_png(
            source,
            BRAND / "linux" / "hicolor" / f"{size}x{size}" / "apps" / "pom.png",
            size,
        )

    write_ico(source, BRAND / "favicon.ico", (16, 32, 48))
    write_ico(source, BRAND / "pom.ico", ICO_SIZES)
    write_icns(macos_icon(source), BRAND / "pom.icns")
    print("generate-brand-assets: derivados regenerados a partir de pom-icon-src.png")


if __name__ == "__main__":
    main()
