# CODAI-SERVER — interface web

Interface administrativa do `rdma-p2p`, reescrita em React. Vive em `web-ui/` e
**não substitui** `web/`: o binário continua embutindo `web/` com `include_str!`
e servindo-o na porta do gateway. Este app é um cliente separado, servido pelo
dev server do Vite, que fala com um nó de verdade pela mesma API HTTP.

A marca é só da interface. Crate, binário, config (`rdma-p2p.json`) e rotas
continuam `rdma-p2p`.

**React 18 + TypeScript + Vite + Tailwind**, com `react-router`, `framer-motion`,
`@tanstack/react-query`, `react-markdown` + `remark-gfm` + `rehype-highlight`,
`lucide-react` e `recharts`.

---

## Como subir

Duas peças: o nó (backend Rust) e o dev server.

```bash
# 1. o backend, na raiz do repo
cargo build --release
./target/release/rdma-p2p --no-discovery

# 2. a interface, apontada para ele
cd web-ui
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

Módulo novo `src/reqlog.rs`: registro em memória das requisições atendidas, com
o `Caller` (operador, token com dica do valor, ou anônimo) e os hops percorridos.
`src/gateway.rs` e `src/openai.rs` passaram a alimentá-lo, e `src/auth.rs` ganhou
`ApiKeyAuth`, que resolve a credencial da requisição em `Caller`.

Flag nova de CLI: `--no-discovery` (`src/main.rs`, `src/network.rs`).

`cargo build --release` passa e `cargo test` fica verde (118 testes).

---

## O que ficou faltando

- Persistir o histórico do chat fora do navegador.
- Guardar as ofertas de transferência no backend, para `/admin/transfer-offers`
  deixar de ser lista vazia.
- Registro de requisições em disco, para sobreviver ao reinício do nó.
- Substituir `web/` por este app — é passo separado, decisão do captain. Enquanto
  isso os dois coexistem e o binário serve o antigo.
- A logo. `src/components/Logo.tsx` é um placeholder com dimensões fixas,
  isolado de propósito: trocar por um SVG mexe só nesse arquivo.

## Detalhes que economizam tempo depois

- **Editar `web/` exige `cargo build`** — aqueles arquivos são embutidos com
  `include_str!`. `web-ui/` não: tem hot reload.
- **`Footnote`** (`src/components/ui.tsx`) existe porque a marcação óbvia é uma
  armadilha: um `<p className="flex">` com um `<span>` no meio da frase faz do
  span um item de flex próprio e a frase se parte em colunas. Use o componente,
  não repita o markup.
- O tema escuro é o padrão; o claro é definido no `:root` nu, e o escuro
  redefine tokens sob `[data-theme="dark"]` (`src/index.css`).
- Nada rola na horizontal: conteúdo largo rola dentro do próprio painel
  (`ScrollX`), nunca a página.
