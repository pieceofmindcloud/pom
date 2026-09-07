#!/usr/bin/env python3
"""Deriva o ícone da barra de status do macOS a partir de `pom-icon-src.png`.

A barra de menus não aceita o ícone do produto como ele é. Duas razões, e as
duas doem se forem ignoradas:

**Template image.** Um `NSStatusItem` marcado como template usa **somente o
canal alfa** da imagem e pinta o resultado com a cor da barra - preto no tema
claro, branco no escuro. A arte do POM é um quadrado azul totalmente opaco, ou
seja, alfa 255 em toda parte: usada diretamente, ela vira um retângulo preto
sólido na barra. O que é preciso é o recorte da arte, e é isso que este script
extrai - o traço branco vira alfa, o fundo azul vira transparência.

**Tamanho.** O glifo mora num quadrado de 18 pontos. O wordmark "POM" que
acompanha a cabeça ocupa um terço da altura da arte e, reduzido a essa medida,
não é legível - vira sujeira. O script fica só com a cabeça, que é a parte que
se reconhece pequena, e descarta o texto.

A separação entre traço e fundo é limpa porque o traço é branco e tudo o mais
é azul: `min(R, G, B)` fica acima de 240 na arte e abaixo de 96 no fundo e no
meio-tom, com pouca coisa entre os dois. Essa faixa intermediária é justamente
a antisserrilha, e é ela que dá a borda suave depois de normalizada.

Saída em 1x, 2x e 3x. `macos_status.m` carrega as três como representações da
mesma `NSImage` de 18 pontos, para o AppKit escolher a que casa com a tela.

Uso: `python3 scripts/derive-macos-status-icon.py`
"""

from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "assets" / "brand"
SOURCE = BRAND / "pom-icon-src.png"
DESTINATION = BRAND / "macos"

# Pontos do quadrado do item de barra, e a altura que o glifo ocupa dentro
# dele. A folga vertical evita que o traço encoste na borda da barra.
BOX_POINTS = 18
GLYPH_POINTS = 16
SCALES = (1, 2, 3)

# Limites de `min(R, G, B)`: abaixo de LOW é fundo, acima de HIGH é traço
# cheio, e o meio é antisserrilha, que vira alfa proporcional.
LOW = 110
HIGH = 235

# O wordmark começa depois de uma faixa vazia. Qualquer linha com menos traço
# que isto conta como separação, não como parte do desenho.
BLANK_ROW_THRESHOLD = 8


def artwork_alpha(image: Image.Image) -> np.ndarray:
    """Alfa 0..255 do traço branco, medido pelo canal mais escuro de cada pixel."""
    channels = np.asarray(image.convert("RGB")).astype(np.float32)
    whiteness = channels.min(axis=2)
    alpha = (whiteness - LOW) / (HIGH - LOW)
    return (np.clip(alpha, 0.0, 1.0) * 255.0).round().astype(np.uint8)


def head_only(alpha: np.ndarray) -> np.ndarray:
    """Recorta a cabeça, cortando no vão horizontal que a separa do wordmark."""
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
    """Compõe o glifo, preto e translúcido, centrado no quadrado do item."""
    box = BOX_POINTS * scale
    height = GLYPH_POINTS * scale
    width = max(1, round(glyph.shape[1] * height / glyph.shape[0]))

    resized = np.asarray(
        Image.fromarray(glyph, mode="L").resize(
            (width, height), Image.Resampling.LANCZOS
        )
    ).astype(np.float32)

    # Em 1x o traço é fino demais para sobreviver à redução: o pico do alfa
    # para nos 195 e o glifo inteiro fica cinzento, sem preto nenhum. Repor o
    # pico em 255 devolve o peso do traço sem mexer na forma; em 2x e 3x o
    # pico já é 255 e isto não faz nada.
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
