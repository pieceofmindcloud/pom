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
  `diagram`, `stats` (números de destaque), `checklist`, `faq` (perguntas que
  abrem sob demanda, `<details>`) e `deep`. Guias hoje, nesta ordem:
  `visao-geral`, `primeiros-passos`, `perfis`, `requisicao`,
  `fatiar-implantar`, `usar-api`, `rede-internet`, `plugins`, `operar`,
  `acesso`, followed by the deployment inspector's category guides from
  `deploySettings.ts`. Os `id` são o endereço `?guia=` e não mudam com o idioma.
- `deploySettings.ts` - the guides behind the help button of each deployment
  inspector category: `config-padrao` (Default settings), `config-pom` (POM
  settings) and `config-beta` (the Beta group: MoE experts in RAM, the GPU
  expert cache and the automatic prefill chunk, and how to combine them). Each
  describes every property of its category and how to change it, from the
  field hints and `docs/subsystems/deployment.md`. They open in
  `components/GuideDialog.tsx` and also list in Documentation.
- `Guide.tsx` - `GuideView`, o renderizador. Cada tipo de bloco tem **um** visual,
  decidido aqui e em nenhum outro lugar: é por isso que os guias são dados. Quem
  escreve uma página nova escolhe entre os blocos existentes e ganha espaçamento,
  cor e tema escuro de graça, sem poder inventar um quarto tipo de aviso. Os
  blocos `deep` são separados do corpo e agrupados no fim, sob `docs.deeper`.
- `diagrams.tsx` - SVG inline (`topology`, `request`, `slicing`, `lifecycle`,
  `setup`, `profiles`, `network`, `api`, `plugins`). Cor pelos mesmos tokens do
  painel (`fill-iris`, `stroke-line`), então trocar de tema redesenha sem
  trabalho e nenhuma captura de tela envelhece. O movimento (pacotes que
  atravessam um salto, tracejado que corre, anéis de descoberta) é CSS puro,
  classes `guide-*` em `src/index.css`: a regra global de
  `prefers-reduced-motion` o congela, e todo pacote animado fica sobre uma
  seta estática, então o desenho continua legível parado.

## Duas regras que mantêm isto honesto

1. **Nenhum número ou comportamento pode aparecer aqui sem estar no markdown.**
   Quando os dois divergirem, o markdown (que acompanha o código) vence e este
   conteúdo é corrigido. Todo guia termina em links `deep` para a referência,
   em vez de duplicar o detalhe.
   A tabela do guia de fatiar e implantar acompanha `docs/subsystems/deployment.md`:
   faixas válidas podem compartilhar uma máquina, mas gap global e sobreposição
   continuam recusas.
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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
