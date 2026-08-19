#!/usr/bin/env python3
"""Regenera os derivados de ícone a partir de `pom-icon-src.png`.

Os três arquivos de arte são fontes manuais. Este script deriva somente os
formatos e tamanhos consumidos pelos empacotadores web, Linux, Windows e
macOS, mantendo a mesma arte em todas as plataformas.
"""

from pathlib import Path
from io import BytesIO
import struct

from PIL import Image


ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "assets" / "brand"
SOURCE = BRAND / "pom-icon-src.png"
PNG_SIZES = (16, 24, 32, 48, 64, 128, 180, 256, 512)
LINUX_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)


def resized(source: Image.Image, size: int) -> Image.Image:
    return source.resize((size, size), Image.Resampling.LANCZOS)


def write_png(source: Image.Image, destination: Path, size: int) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    resized(source, size).save(destination, format="PNG", optimize=True)


def write_ico(source: Image.Image, destination: Path, sizes: tuple[int, ...]) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    largest = resized(source, max(sizes))
    largest.save(destination, format="ICO", sizes=[(size, size) for size in sizes])


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

    # O mark usado pelo manifest é o mesmo ícone quadrado, sem uma segunda
    # fonte que possa divergir.
    write_png(source, BRAND / "pom-mark.png", 512)

    for size in LINUX_SIZES:
        write_png(
            source,
            BRAND / "linux" / "hicolor" / f"{size}x{size}" / "apps" / "pom.png",
            size,
        )

    write_ico(source, BRAND / "favicon.ico", (16, 32, 48))
    write_ico(source, BRAND / "pom.ico", ICO_SIZES)
    write_icns(source, BRAND / "pom.icns")
    print("generate-brand-assets: derivados regenerados a partir de pom-icon-src.png")


if __name__ == "__main__":
    main()
