# CODAI-SERVER — interface web

Interface administrativa do `rdma-p2p`. **É a interface do produto**: o binário
embute o build de produção deste diretório e o serve na porta do gateway (8080
por padrão), incluindo a tela de configuração inicial. O antigo `web/` foi
removido.

A marca é só da interface. Crate, binário, config (`rdma-p2p.json`) e rotas
continuam `rdma-p2p`.

**React 18 + TypeScript + Vite + Tailwind**, com `react-router`, `framer-motion`,
`@tanstack/react-query`, `react-markdown` + `remark-gfm` + `rehype-highlight`,
`lucide-react` e `recharts`.

## Como o binário serve a interface

`cargo build` **reconstrói a interface** (`build.rs` chama o Vite) e embute o
resultado com `rust-embed`. Portanto, Node e npm são necessários somente na
máquina que compila, inclusive no corte de release. O binário pronto contém os
assets e não precisa de Node, npm ou de uma pasta `dist` ao ser executado.

`dist/` e `dist-release/` são artefatos locais e ignorados pelo Git. Assim cada
release é sempre produzido a partir do fonte atual da interface, sem risco de
embarcar uma tela compilada em outro commit.

### Roteamento

O bundle é montado **por último** no roteador do axum, e o catch-all dele é
estreito de propósito: arquivo real ganha; `/api`, `/v1`, `/admin`, `/peers`,
`/health` e `/generate` nunca caem no app (uma rota de API desconhecida responde
404 em JSON, não uma página com status 200); um caminho com extensão que não
existe é 404; o que sobra recebe o `index.html`, para recarregar `/config/keys/api`
funcionar.

## Desenvolvimento com hot reload

Para iterar na interface sem recompilar o Rust a cada mudança, o dev server do
Vite continua sendo o caminho — ele fala com um nó de verdade por proxy.

```bash
# 1. o backend, na raiz do repo
cargo build --release
./target/release/rdma-p2p --no-discovery

# 2. a interface, apontada para ele
cd apps/frontend
npm install
VITE_API_TARGET=http://127.0.0.1:8099 npm run dev
```

A interface sobe em <http://localhost:5199/>. `VITE_API_TARGET` é para onde o
Vite encaminha `/api`, `/v1`, `/admin`, `/peers`, `/health` e `/generate`
(ver `vite.config.ts`); sem ela o padrão é `http://127.0.0.1:8080`.

Em produção nada disso é necessário: um nó serve API e interface na mesma porta,
então o app é same-origin e todo caminho é relativo.

### `--no-discovery`, e por que ele existe

O broadcast de descoberta da LAN vai para `255.255.255.255:42777` **fixo**,
ignorando `--port`. Um nó subido para experimentar a interface entra sozinho em
qualquer cluster que esteja no ar na mesma rede. `--no-discovery` (adicionado
nesta branch) sobe o nó destacado: não anuncia, não faz broadcast, não procura
bootstrap. É a única forma segura de rodar um nó de teste ao lado de um cluster
que está sendo usado. Há teste disso em `src/network.rs`
(`a_detached_node_neither_announces_nor_looks_for_bootstrap`).

Um nó destacado mostra "Fora da rede" em Configurações › Rede, com um botão
"Voltar para a rede" que chama `/api/ui/rejoin`.

### A tela abriu sem estilo? Reinicie o dev server

Sintoma: a página aparece crua, fontes com serifa, sem layout — e junto,
`/api/*` devolve `<!doctype html>` em vez de JSON, então as telas mostram
"Unexpected token '<'". Os dois ao mesmo tempo, sem nenhum erro no console e com
o servidor respondendo 200 em tudo.

Não é o Tailwind, não é o PostCSS, não é o cache. É o dev server que **perdeu o
contexto do projeto** e está rodando sem config nenhuma. Reiniciar resolve:

```bash
# no diretorio apps/frontend
npm run dev
```

Como se confirma em dez segundos, sem abrir o navegador:

```bash
curl -s http://localhost:5199/src/index.css | grep -c '@tailwind'   # sadio: 0
curl -s http://localhost:5199/api/ui/node | head -c 20              # sadio: {"node_id"...
```

`@tailwind` cru na saída significa que o PostCSS não rodou de todo — diferente
de `content` mal configurado, que geraria o preflight e comeria a diretiva.

**Como se chega nesse estado sem editar nada:** um `git rebase`, um `git
checkout` de outra branch, qualquer coisa que reescreva `vite.config.ts` com o
dev server no ar. O Vite se reinicia sozinho ao ver o arquivo mudar, e o
reinício pode cair no instante em que a árvore está no meio da reescrita; ele
sobe sem achar o `package.json` e, a partir daí, sem config: sem PostCSS e sem
`server.proxy`. Nada falha alto, o servidor continua servindo.

A regra prática: **mexeu no git com o dev server rodando, reinicie o dev server.**

### A porta e a configuração usadas na validação

`rdma-p2p.json` não é versionado. O que subiu durante esta entrega:

```json
{
  "role": "principal",
  "connection_key": "codai-dev-key",
  "gateway_port": 8099,
  "p2p_port": 42799,
  "inference_port": 8199,
  "bootstrap_nodes": [],
  "shard_dir": "/Volumes/sdcard/models_sliced"
}
```

Portas fora das usuais (8080/42777/8181) de propósito, para não colidir com um
cluster já rodando na máquina.

---

## O que está ligado ao backend de verdade

Tudo abaixo lê ou escreve num nó real. Não há dado de exemplo em nenhuma tela:
a versão anterior tinha um `mock-server.mjs` e ele foi removido.

| Tela | Rotas |
|---|---|
| Painel (fluxo e detalhes) | `GET /api/ui/node`, `/api/ui/cluster`, `/peers` |
| Chat de teste | `POST /v1/chat/completions` (SSE, token a token) |
| Modelos inteiros | `GET /api/ui/models` |
| Fatiar modelos | `POST /api/ui/slice/plan`, `POST /api/ui/slice` |
| Fatias / Fatiamentos | `GET /api/ui/cluster/models`, `/api/ui/jobs`, `/api/ui/jobs/:id` |
| Transferências | `GET /api/ui/jobs`, rotas de `src/transfer.rs` |
| Enviar um arquivo | transferência por concessão (`grant`) |
| Logs por token e por nó | `GET /api/ui/requests` |
| Logs ao vivo | `GET /api/ui/requests` em polling |
| Rede (bloquear nó) | `GET /admin/nodes`, `POST /admin/nodes/:id/block`/`unblock` |
| API keys | `GET`/`POST /admin/api-keys`, `DELETE /admin/api-keys/:key` |
| Chaves de conexão | `GET /api/ui/node`, `POST /admin/regenerate-key` |
| Pastas de modelos | `GET`/`POST /api/ui/paths` |
| Configuração inicial | `GET /api/setup/key`, `POST /api/setup/save` |
| Implantar modelo fatiado | `POST /api/ui/deploy/plan`, `POST /api/ui/deploy`, `GET /api/ui/jobs` |
| Liberar fatia | `GET /api/ui/release/candidate`, `POST /api/ui/release` |

### Nós bloqueados e disponibilidade para inferência

Bloquear um `node_id` remove imediatamente esse peer remoto da visão operacional
do cluster: ele não aparece no Painel, não participa do catálogo e do plano de
implantação, e não pode receber tráfego de inferência. Anúncios UDP que já
estejam chegando não desfazem esse bloqueio. A tela de Rede ainda o lista como
bloqueado, para que o operador possa desbloqueá-lo ou removê-lo.

Antes de montar um pipeline, cada requisição também consulta `GET /health` de
cada endpoint de inferência candidato, em paralelo, com limite de dez segundos.
Só peers que respondem com sucesso entram no plano daquela requisição. Assim um
anúncio UDP antigo ou um servidor de inferência parado não fecha artificialmente
a cobertura de camadas nem faz a requisição esperar por um hop indisponível.

**API token por nó está ligado de verdade**, não é maquete: os tokens são
emitidos, listados e revogados em `/admin/api-keys`, ficam em `api_keys` no
`rdma-p2p.json`, valem como credencial nas rotas `/v1` (`ApiKeyAuth`, `src/auth.rs`),
e cada requisição atendida é atribuída ao token que a pagou — é isso que a tela
de Logs mostra. O registro vive **na memória do processo** (`src/reqlog.rs`):
reiniciar o nó zera o histórico, e só as mais recentes ficam.

As notificações (sino) sobrevivem a um recarregamento porque o progresso vem do
nó, via `/api/ui/jobs`, e não de estado do navegador.

## O que não está ligado

- **Histórico de conversas do chat** — sessões ficam em `localStorage`
  (`src/lib/chatStore.ts`). Some ao limpar o navegador e não acompanha o
  usuário entre máquinas. Foi a escolha aceita para esta entrega.
- **`GET /admin/transfer-offers`** devolve lista vazia fixa no backend
  (`src/admin.rs`). A interface consome a rota; o backend ainda não guarda as
  ofertas recebidas para respondê-la.
- **Geração de texto** depende de haver modelo carregado no nó. O nó validado
  subiu sem modelo (ver abaixo), então o Chat conecta, chama `/v1/chat/completions`
  e mostra o erro que o nó devolve. O caminho SSE em si está exercitado.

### Por que o nó validado subiu sem modelo

A máquina de validação é a de trabalho do captain: 16 GB no total, com ~5 GB
recuperáveis no momento da medição (`vm_stat`, páginas free + inactive +
speculative + purgeable). A menor combinação de fatias que cobre um modelo
inteiro entre as disponíveis é `qwen3-8b_0_31` (3,9 GB) + `qwen3-8b_32_35`
(1,3 GB) = 5,2 GB em dois processos — acima da folga, ou seja, swap. Nenhum
modelo pequeno o bastante estava no disco. Por cálculo, então, e não por
tentativa: o nó subiu em modo gateway, com todas as rotas no ar e sem pesos na
memória.

Para exercitar a geração, suba o nó com uma fatia e `--inference-port`, ou
aponte `VITE_API_TARGET` para um nó que já tenha modelo carregado.

---

## Rotas novas criadas no backend

Todas em `src/webui.rs`, no estilo do arquivo, mais um módulo novo.

| Rota | O que faz |
|---|---|
| `GET /api/ui/jobs` | Lista os jobs ativos e os terminados há menos de dez minutos. Já havia `/api/ui/jobs/:id`, que só serve para acompanhar um job cujo id você já tem — sem a listagem, o sino não teria como se reconstruir depois de um recarregamento. |
| `POST /api/ui/unload` | Descarrega da memória o modelo do nó. |
| `POST /api/ui/leave` | Sai da rede: para de anunciar e esquece os peers. |
| `POST /api/ui/rejoin` | Volta para a rede. |
| `GET /api/ui/requests` | O registro de requisições, por token e por nó, que alimenta as duas telas de Logs. |
| `POST /api/ui/release` | O nó devolve a memória da fatia que serve e continua na rede; se houver peer ocioso com a fatia no disco, ela é passada adiante. Nunca é recusada. |
| `GET /api/ui/release/candidate` | Quem assumiria a fatia, para a confirmação na tela avisar de verdade quando a faixa vai ficar descoberta. |
| `POST /api/ui/deploy/plan` | Só leitura: quem serviria qual faixa de um conjunto de fatias pronto, e o que teria de ser copiado antes. |
| `POST /api/ui/deploy` | Executa. Refaz o plano no servidor, manda cada alvo puxar o que falta pelo transporte existente, espera, e então chama o `launch` com `skip_split`. É um job, então o sino acompanha e recarregar não perde nada. |

Módulo novo `src/reqlog.rs`: registro em memória das requisições atendidas, com
o `Caller` (operador, token com dica do valor, ou anônimo) e os hops percorridos.
`src/gateway.rs` e `src/openai.rs` passaram a alimentá-lo, e `src/auth.rs` ganhou
`ApiKeyAuth`, que resolve a credencial da requisição em `Caller`.

Flag nova de CLI: `--no-discovery` (`src/main.rs`, `src/network.rs`).

`cargo build --release` passa e `cargo test` fica verde (145 testes).

---

## Implantar um modelo já fatiado

`src/deploy.rs` + `src/routes/DeployModel.tsx`. As peças já existiam — o catálogo
sabe quem tem qual fatia, o `transfer` move arquivo entre máquinas, o `launch`
amarra uma faixa a um peer. O que faltava era a decisão do meio: **qual máquina
serve qual fatia**, e o que precisa ser copiado para isso ser possível. O módulo
é só essa decisão mais a sequência; não inventa um segundo mecanismo de
transferência nem de launch.

### O plano automático continua sendo o caminho de poucos cliques

Sem `selection` no corpo, as rotas se comportam como antes: o nó decide e a tela
mostra o que ele decidiu. As regras, na ordem em que importam:

1. A varredura começa na camada 0 e sempre pega a faixa **mais longa** que
   continua de onde a anterior parou. Isso não é estética: menos fatias é menos
   máquinas necessárias, e pegar só o que continua a corrente torna sobreposição
   impossível por construção — o que passou a importar quando o mesmo modelo
   pode estar fatiado duas vezes com cortes diferentes.
2. Um nó serve uma faixa por vez, então o conjunto precisa de pelo menos tantas
   máquinas quantas fatias. Menos que isso é recusa, não implantação pela metade.
3. Máquina que já tem a fatia é preferida para ela — cada cópia evitada são ~17
   minutos a menos no enlace real.
4. A camada 0 vai para a máquina com mais memória livre, porque quem fica com
   ela processa o prompt inteiro de toda requisição.

Depois das cópias, o caminho de cada arquivo é perguntado à máquina que o
recebeu em vez de deduzido — só ela sabe onde o próprio `recebidos/` fica.

### O que o operador escolhe por cima do plano

O plano é sugestão. `selection` no corpo das duas rotas carrega a resposta do
próprio operador, e o servidor **valida** essa resposta em vez de recalcular a
sua e ignorá-la — `POST /api/ui/deploy` refaz a varredura do cluster (uma
máquina pode ter saído entre olhar e decidir) mas não refaz a escolha.

- **Quais camadas entram.** A tela lista *toda* faixa que existe no cluster, de
  todos os fatiamentos, cada uma com uma caixa de marcar. Misturar cortes
  diferentes é legítimo e a fita de cobertura mostra na hora o que ficou
  coberto, descoberto ou dobrado.
- **Qual máquina fica com cada faixa.** Um `select` por linha. Escolher uma
  máquina que já está com outra faixa **troca** as duas, que é o que o operador
  quis dizer.
- **A ordem do pipeline.** Os botões ↑ ↓ trocam a máquina com a linha vizinha.
  Não existe um segundo botão de "ordem" porque não existe uma segunda ordem: o
  `pipeline.rs` varre da camada 0 para cima, então quem tem a camada 0 recebe
  todo prompt, e reordenar o pipeline **é** reatribuir as faixas. A fita no topo
  mostra a ordem resultante, com "processa o prompt" no primeiro.

Viabilidade, e o que é recusa e o que é aviso:

| Situação | Resposta |
|---|---|
| Faixas escolhidas se sobrepõem | **Recusa.** As camadas rodariam duas vezes e a resposta sairia errada sem nada falhar. |
| Duas faixas na mesma máquina | **Recusa** por padrão — a não ser que "juntar fatias vizinhas" esteja ligado: faixas **adjacentes** da mesma máquina são fundidas num arquivo só (`splitter::merge`) antes de subir, e a máquina passa a servir a união. Faixas não adjacentes são recusa mesmo com a junta ligada. |
| Faixa sem máquina, ou máquina que saiu do cluster | **Recusa.** |
| Faixa que ninguém do cluster tem | **Recusa.** |
| Disco insuficiente no destino de uma cópia | **Recusa** — o `transfer.rs` recusaria de qualquer jeito, melhor antes de mover byte. |
| Memória apertada no destino | **Aviso**, com o número. É estimativa (VRAM livre onde há placa, RAM disponível no resto), e um bloqueio errado seria pior que uma linha em amarelo. |
| Buraco na cobertura | **Aviso** + confirmação por diálogo. Subir meio modelo é um passo legítimo; ele só não vai atender até fechar. |

### Ajustes avançados (execução)

Os treze knobs de `RuntimeParams` (`src/webui.rs`) voltaram para a tela, com os
mesmos rótulos e padrões da interface antiga, e agora vão de verdade para o
`launch` de cada fatia: contexto (`n_ctx`), lote (`n_batch`), máx. tokens novos,
requisições simultâneas, tokens por pedaço, temperatura, `top_p`, `top_k`,
penalidade de repetição e sua janela, desligar o bloco de raciocínio, e forçar
sem embeddings.

São **do cluster**, não de cada peer, porque o pipeline precisa que sejam: o
contexto e o lote decidem o orçamento de prompt que o gateway cobra de toda
requisição, e a amostragem só roda no último peer. A exceção é **camadas na
GPU**, que é propriedade da máquina — uma caixa com 5060 Ti e uma sem placa
querem números diferentes. Ela é editada na tabela de máquinas, viaja em
`LaunchTarget::n_gpu_layers` e sobrescreve só aquele peer (`webui::params_for`).

O cabeçalho do painel cita o número que decide se um harness cabe — "cada
requisição aceita até N tokens de prompt, em pedaços de M" — calculado no
servidor por `gateway::prompt_budget_of` / `prompt_chunk_of`, as mesmas funções
que o gateway usa para recusar um prompt grande demais. Medido na tela:
`n_ctx 32768`, 2 sessões, 256 novos → 16.128 tokens; pedaço `min(4096, 3072)` =
3.072.

`no_embeddings` fica no painel por paridade com a interface antiga e **com o
aviso na tela**: normalmente é automático (só o último peer dispensa), e ligar
impede o primeiro de extrair o estado oculto.

### O que está ligado ao backend de verdade nesta tela

Tudo. Não há dado de exemplo: o conjunto de fatias vem de
`GET /api/ui/cluster/models`, o plano e a validação de `POST /api/ui/deploy/plan`
(que pergunta a cada peer o seu `/api/ui/models` e o seu `/api/ui/node`), a
execução de `POST /api/ui/deploy`, e o progresso de `GET /api/ui/jobs` — o mesmo
job que aparece no sino, agora também com barra na própria tela. As cópias
continuam saindo por `POST /api/transfer/pull` em cada destino: nenhum mecanismo
paralelo foi criado.

O que a tela **não** calcula sozinha: nada da validação. Cada edição volta ao nó
(com 260 ms de espera para não varrer o cluster a cada tecla) e a coluna "o que
acontece" diz "conferindo…" enquanto a resposta ainda descreve outra escolha.

### Rotas alteradas

| Rota | Mudança |
|---|---|
| `POST /api/ui/deploy/plan` | Aceita `selection` (faixa + máquina + `n_gpu_layers` opcional) e `params`. Devolve, além do plano: `available` (toda faixa do cluster, com quem a tem), `machines` (memória e disco livres, o que cada uma tem e o que assume), `missing`, `overlap`, `warnings`, `params`, `prompt_tokens`, `chunk_tokens`, e por passo `position`, `primary`, `options`, `problem`, `note`. |
| `POST /api/ui/deploy` | Aceita e valida a mesma `selection`/`params` em vez de replanejar por conta própria. |
| `POST /api/ui/launch` | `LaunchTarget` ganhou `n_gpu_layers: Option<u32>`. |

Nada foi removido do corpo antigo dessas rotas: um cliente que só manda `group`
continua recebendo exatamente o plano automático de antes.

### Como isto foi revisado, e o que ficou de fora da revisão

**Nada foi implantado no cluster do captain.** A ordem era clara: planejar,
validar e revisar, sem apertar "Implantar" contra as máquinas dele, e sem tocar
na ksia. Então:

- **Backend real, uma máquina.** Um nó isolado (`--no-discovery`, com
  `RDMA_P2P_HOME` num diretório temporário) com quatro fatias GGUF de cabeçalho
  só, sobrepostas de propósito (0-4 / 5-31 e 0-10 / 11-31). É a origem dos
  estados de recusa: "são 2 fatias para 1 máquina", faixa sem máquina, e a
  escolha automática pegando 0-10 + 11-31 entre os quatro cortes.
- **Três máquinas, por dublê.** Um servidor local de ~250 linhas
  (scratchpad, não versionado) serve as mesmas rotas com três máquinas, para
  revisar o que uma máquina só não desenha: `select` de destino, a troca, linha
  de cópia, aviso de memória, coluna de GPU por máquina. É um **port** do
  `plan()` do Rust, não a fonte da verdade — a lógica é a do Rust, coberta pelos
  testes.
- O `POST /api/ui/deploy` foi exercitado **só contra o dublê** (que devolve 400
  de propósito), para conferir o toast de erro e o diálogo de confirmação.

Corrigido na revisão por pixel: caixa de marcar nativa (o estado desmarcado do
sistema chega como quadrado cinza claro e lê como *desabilitado*), alinhamento
da faixa entre os cartões da fita, largura do `select` de destino (uma classe de
largura nele perde para o `w-full` do componente — a largura vive na célula),
rodapé prometendo "1 cópia" para uma faixa sem máquina, e a ordem das fatias na
tabela de máquinas, que vinha na ordem do diretório.

Testes: `cargo test` (145 + 2 verdes), com o planejador e a validação cobertos —
cobertura completa, buraco, sobreposição, duas faixas numa máquina, faixa sem
máquina, máquina fora do cluster, faixa que ninguém tem, disco insuficiente,
memória apertada, leitura de VRAM vs RAM, eco dos parâmetros e orçamento, e GPU
por máquina.

## O que ficou faltando

- Persistir o histórico do chat fora do navegador.
- Guardar as ofertas de transferência no backend, para `/admin/transfer-offers`
  deixar de ser lista vazia.
- Registro de requisições em disco, para sobreviver ao reinício do nó.
- A logo. `src/components/Logo.tsx` é um placeholder com dimensões fixas,
  isolado de propósito: trocar por um SVG mexe só nesse arquivo.

## Detalhes que economizam tempo depois

- **Editar `apps/frontend/` exige `cargo build`** para valer no binário, porque o bundle
  é embutido em tempo de compilação. Durante o desenvolvimento use o dev server,
  que tem hot reload.
- **`Footnote`** (`src/components/ui.tsx`) existe porque a marcação óbvia é uma
  armadilha: um `<p className="flex">` com um `<span>` no meio da frase faz do
  span um item de flex próprio e a frase se parte em colunas. Use o componente,
  não repita o markup.
- O tema escuro é o padrão; o claro é definido no `:root` nu, e o escuro
  redefine tokens sob `[data-theme="dark"]` (`src/index.css`).
- Nada rola na horizontal: conteúdo largo rola dentro do próprio painel
  (`ScrollX`), nunca a página.
