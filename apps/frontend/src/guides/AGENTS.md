# src/guides - a documentação escrita para humanos

O markdown de `src/docs/` (gerado por `scripts/build-docs.mjs` a partir dos
`AGENTS.md` e dos `docs/subsystems/*`) é escrito **para agentes**: exaustivo,
por arquivo e por função, fiel ao código até a constante. Esta pasta é a outra
metade: um conjunto pequeno e ordenado de páginas explicadas, com diagramas,
para quem abriu o painel e quer entender o que o cluster faz.

A tela `routes/Documentation.tsx` serve as duas na mesma rota, em abas
(`?tab=guias|referencia`). Guias é a aba padrão - um humano não pode aterrissar
em `AGENTS.md - crates/routing-base/`.

## Arquivos

- `content.ts` - **os textos**, nos dois idiomas (`guidesFor(locale)`). Um guia
  é `{ id, label, summary, title, blocks }` e um bloco é dado, não JSX:
  `lead`, `p`, `h`, `steps`, `cards`, `note`, `warn`, `table`, `code`,
  `diagram`, `deep`. Guias hoje: `visao-geral`, `requisicao`,
  `fatiar-implantar`, `operar`, `acesso`.
  Os blocos `deep` de `requisicao` e `fatiar-implantar` também apontam para
  `docs/subsystems/paralelismo.md`, que explica quando preferir pipeline ou TP.
- `Guide.tsx` - `GuideView`, o renderizador. Cada tipo de bloco tem **um** visual,
  decidido aqui e em nenhum outro lugar: é por isso que os guias são dados. Quem
  escreve uma página nova escolhe entre os blocos existentes e ganha espaçamento,
  cor e tema escuro de graça, sem poder inventar um quarto tipo de aviso. Os
  blocos `deep` são separados do corpo e agrupados no fim, sob `docs.deeper`.
- `diagrams.tsx` - SVG inline (`topology`, `request`, `slicing`, `lifecycle`).
  Cor pelos mesmos tokens do painel (`fill-iris`, `stroke-line`), então trocar
  de tema redesenha sem trabalho e nenhuma captura de tela envelhece.

## Duas regras que mantêm isto honesto

1. **Nenhum número ou comportamento pode aparecer aqui sem estar no markdown.**
   Quando os dois divergirem, o markdown (que acompanha o código) vence e este
   conteúdo é corrigido. Todo guia termina em links `deep` para a referência,
   em vez de duplicar o detalhe.
2. **A letra dentro de um diagrama é neutra de idioma** - letras de máquina,
   faixas de camada, `prompt`, `token`, contagem de bytes. A frase que explica a
   figura é a legenda, que é conteúdo localizado. É isso que faz um desenho só
   servir os dois idiomas em vez de duas cópias que divergem.

## Ligações

- Renderizado por `routes/Documentation.tsx`; o botão `deep` chama `onOpenDoc`,
  que troca para a aba de referência com `?doc=`.
- Usa `components/ui` (`ScrollX`) e `i18n/provider` (`docs.deeper`).
- Os rótulos das abas e da árvore vivem em `i18n/locales.ts` (`docs.*`); o texto
  dos guias **não** - ele é grande demais para a tabela de mensagens e vive em
  `content.ts`, por locale.

## Defeitos anotados

Nenhum em aberto nesta pasta.
