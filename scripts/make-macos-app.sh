#!/usr/bin/env bash
#
# Monta um bundle POM.app em volta do binário já compilado.
#
# Um executável Unix não tem ícone: no macOS quem carrega o ícone é o bundle
# `.app`, não o arquivo. Sem isto, o Finder mostra o binário com o ícone
# genérico de terminal e não há onde pendurar a identidade do produto.
#
#     ./scripts/make-macos-app.sh [binario] [destino]
#
# Padrões: `target/release/pom` e `dist-release/POM.app`.
#
# O bundle é um invólucro fino: o binário continua sendo o mesmo programa de
# linha de comando, e `POM.app/Contents/MacOS/pom` pode ser chamado direto com
# os mesmos argumentos. `LSUIElement` mantém o nó fora do Dock; a interface
# principal continua web, com acesso rápido pelo item nativo da barra de status.
set -euo pipefail

raiz="$(cd "$(dirname "$0")/.." && pwd)"
bin="${1:-$raiz/target/release/pom}"
destino="${2:-$raiz/dist-release/POM.app}"

nome=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["name"])' "$raiz/brand.json")
slug=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["slug"])' "$raiz/brand.json")
icns="$raiz/$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["app_icons"]["macos"])' "$raiz/brand.json")"
# A arte do item de barra e as traduções da tela nativa moram juntas.
recursos_macos="$raiz/assets/brand/macos"
status_icon="$recursos_macos/pom-status-template.png"

[ -x "$bin" ] || { echo "binário não encontrado: $bin" >&2; exit 1; }
# O pom só inicia plugins pelo `pom-plugin-host` ao lado do próprio executável
# (`plugin_host::host_executable`); sem ele nenhum plugin instala.
host="$(dirname "$bin")/pom-plugin-host"
[ -x "$host" ] || {
    echo "pom-plugin-host não encontrado: $host" >&2
    echo "compile com: cargo build --release -p node -p pom-plugin-host" >&2
    exit 1
}
[ -f "$icns" ] || { echo "ícone não encontrado: $icns" >&2; exit 1; }
# Um item de barra sem arte cai para um rótulo de texto, o que é uma entrega
# pior e silenciosa. `scripts/derive-macos-status-icon.py` gera esta arte.
[ -f "$status_icon" ] || {
    echo "ícone da barra não encontrado: $status_icon" >&2
    echo "gere-o com: python3 scripts/derive-macos-status-icon.py" >&2
    exit 1
}

rm -rf "$destino"
mkdir -p "$destino/Contents/MacOS" "$destino/Contents/Resources"

cp "$bin" "$destino/Contents/MacOS/$slug"
cp "$host" "$destino/Contents/MacOS/pom-plugin-host"
cp "$icns" "$destino/Contents/Resources/$slug.icns"
# As três resoluções entram com o nome original: o lado nativo procura as
# companheiras `@2x` e `@3x` ao lado da arte 1x que o Rust lhe indica.
cp "$recursos_macos"/pom-status-template*.png "$destino/Contents/Resources/"
find "$recursos_macos" -mindepth 1 -maxdepth 1 -type d -name '*.lproj' \
    -exec cp -R {} "$destino/Contents/Resources/" \;

versao=$(grep -m1 '^version' "$raiz/Cargo.toml" | sed 's/[^0-9.]//g')
versao=${versao:-0.1.0}

cat > "$destino/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$nome</string>
    <key>CFBundleDisplayName</key><string>$nome</string>
    <key>CFBundleIdentifier</key><string>app.$slug.node</string>
    <key>CFBundleVersion</key><string>$versao</string>
    <key>CFBundleShortVersionString</key><string>$versao</string>
    <key>CFBundleExecutable</key><string>$slug</string>
    <key>CFBundleIconFile</key><string>$slug</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# O Finder guarda em cache o ícone por caminho; sem isto um bundle recriado no
# mesmo lugar continua mostrando o anterior.
touch "$destino"

echo "✓ $destino"
echo "  executável: $destino/Contents/MacOS/$slug"
