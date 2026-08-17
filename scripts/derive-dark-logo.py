#!/usr/bin/env python3
"""Deriva a versão do lockup para fundo escuro a partir da arte clara.

Não é arte nova: é a arte de `brand.json` -> `logo.light` recolorida. O letreiro
("POM" e "PIECE OF MIND") é azul-petróleo chapado, desenhado para fundo claro, e
some num painel escuro; este script espelha a luminância dele (escuro vira
claro) preservando matiz e saturação. A ilustração do cérebro **não** é tocada:
ela já tem contraste próprio no escuro, e recolori-la achatava o volume.

    python3 scripts/derive-dark-logo.py

Escreve `assets/brand/pom-logo-dark.png`. Quando existir uma arte escura
desenhada de verdade, é só sobrescrever esse arquivo e apagar este script.

Requer Pillow e numpy.
"""
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

RAIZ = Path(__file__).resolve().parent.parent
ENTRADA = RAIZ / "assets/brand/pom-logo-light.png"
SAIDA = RAIZ / "assets/brand/pom-logo-dark.png"

# Retângulo da ilustração no arquivo de origem (680x411). Medido a partir do
# brilho dourado, que só existe no cérebro.
ILUSTRACAO = (398, 2, 676, 250)


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
    def canal(n):
        k = (n + h * 12) % 12
        a = s * np.minimum(l, 1 - l)
        return l - a * np.maximum(-1, np.minimum(np.minimum(k - 3, 9 - k), 1))

    return np.stack([canal(0), canal(8), canal(4)], -1)


def main() -> int:
    if not ENTRADA.exists():
        print(f"não achei {ENTRADA}", file=sys.stderr)
        return 1
    src = Image.open(ENTRADA).convert("RGBA")
    w, h_img = src.size
    a = np.asarray(src).astype(np.float32) / 255.0
    rgb, alpha = a[..., :3], a[..., 3:4]

    h, l, s = to_hls(rgb)
    claro = from_hls(h, np.clip(1 - l * 0.92, 0, 1), np.clip(s * 1.05, 0, 1))

    # Máscara do que fica intacto, com borda suave para não deixar costura.
    m = Image.new("L", (w, h_img), 0)
    ImageDraw.Draw(m).rounded_rectangle(list(ILUSTRACAO), radius=40, fill=255)
    m = m.filter(ImageFilter.GaussianBlur(14))
    manter = (np.asarray(m).astype(np.float32) / 255.0)[..., None]

    saida = rgb * manter + claro * (1 - manter)
    Image.fromarray(
        (np.concatenate([saida, alpha], -1) * 255).astype(np.uint8), "RGBA"
    ).save(SAIDA)
    print(f"✓ {SAIDA.relative_to(RAIZ)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
