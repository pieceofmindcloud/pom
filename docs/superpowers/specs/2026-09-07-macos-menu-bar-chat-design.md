# Chat do POM na barra de menus do macOS

## Contexto

O `POM.app` atual é um bundle fino em volta do binário `pom`. O executável
continua sendo o processo que serve o gateway e a interface é lida do bundle
da UI. A nova capacidade precisa preservar esse contrato para Linux, Windows e
para quem chama o binário diretamente, mas dar ao bundle macOS uma superfície
nativa de acesso rápido ao chat.

## Decisão

Usar o próprio binário Rust com uma ponte Objective-C mínima compilada apenas
quando o alvo for macOS. A ponte usa `NSStatusItem` para a barra de menus,
`NSPopover` para o painel compacto e `WKWebView` para renderizar a rota real do
frontend. Não haverá Electron, runtime JavaScript adicional ou segundo
processo. O serviço Rust roda em uma thread Tokio enquanto o loop principal de
Cocoa fica na thread principal do processo macOS.

O painel terá 390 x 844 points, uma dimensão de smartphone próxima ao viewport
de um iPhone moderno. O botão de abrir em janela maior pertence ao próprio
chat, passa por `window.webkit.messageHandlers` e é exibido apenas quando o
WebView está dentro do app nativo. A janela maior usará 1080 x 760 points e
carregará a mesma URL.

## URL, porta e credencial

O Rust calculará `http://127.0.0.1:<gateway_port>/admin-ui/chat`. A porta vem
do `pom.json` carregado, com `Args::gateway_port` como fallback, cujo padrão é
8080. A rota `admin-ui/chat` cai no shell da SPA e o `App` a trata como o chat
compacto, sem o chrome administrativo que quebraria a largura do painel.

Cada WebView usará `WKWebsiteDataStore.defaultDataStore`, e o frontend seguirá
seu fluxo existente: `useNode()` lê `connection_key` de `/api/ui/node`, grava
`pom.connectionKey` e as requisições do chat usam essa chave. Não haverá header,
arquivo ou API key criado pelo código nativo.

## Estados de erro

Falha de navegação inicial não deixará um WebView branco. O delegate nativo
mostrará uma página de indisponibilidade com retry e explicará que o gateway
local ainda não respondeu. Os textos nativos serão localizados em
`assets/brand/macos/*.lproj/Localizable.strings`, e o bundle copiará esses
recursos. O retry volta a tentar a URL real, inclusive depois de uma subida
tardia do gateway.

## Marca e empacotamento

O ícone de status reutilizará `assets/brand/pom-mark.png`, marcado como
`template` para o AppKit aplicar o contraste correto nos temas claro e escuro.
O script de bundle copiará o mark e as localizações para `Contents/Resources`.
O `.icns` existente continua sendo o ícone do Finder.

## Testes e verificação

- Teste de frontend garante que `/admin-ui/chat` renderiza o chat e que o botão
  nativo só aparece quando o bridge `webkit` existe.
- Testes Rust cobrem construção da URL, dimensão do painel e seleção da porta
  persistida antes do fallback de CLI.
- `cargo test --workspace`, `npm test` e `npm run build` serão executados.
- No macOS, o bundle será construído e aberto com captura do ícone, do painel
  compacto e da janela maior. A tela será inspecionada em ambos os temas e
  também com o gateway indisponível.

## Fora de escopo

- Não substituir o frontend existente por uma implementação nativa.
- Não alterar o protocolo de inferência ou o ciclo de vida das chaves.
- Não adicionar integração equivalente para Linux ou Windows.
- Não transformar a janela compacta em um gerenciador geral do nó.
