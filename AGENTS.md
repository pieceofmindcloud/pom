# AGENTS.md - raiz

Índice do repositório. **Este arquivo é mapa, não manual**: o detalhe de cada
assunto vive no `AGENTS.md` da pasta e no README de subsistema em
`docs/subsystems/`. Se você precisa de mais de um parágrafo sobre algo, o lugar
dele é lá, não aqui.

`CLAUDE.md` é um symlink para este arquivo.

## Onde está o quê

**Por subsistema** (`docs/subsystems/`) - o "como funciona e por quê":

| Arquivo | Assunto |
|---|---|
| `build-execucao.md` | workspace de crates, `cargo build`/features, Docker, release, variáveis de ambiente, scripts |
| `operacao.md` | subir um cluster, verificar que a inferência é conjunta, pegadinhas de campo, dois peers na mesma máquina, harness, monitor de carga |
| `rede-p2p.md` | descoberta UDP + libp2p, endereços anunciados, rotas dinâmicas, pkarr/DCuTR, identidade |
| `inferencia.md` | prefill, decode, wire de estado oculto, anel vs resposta direta, especulativa/MTP, limite de prompt |
| `paralelismo.md` | pipeline parallelism, tensor parallelism, escolha por topologia e telemetria |
| `fatiamento.md` | split/merge de GGUF, tabela de tipos ggml, planejamento por tamanho |
| `implantacao.md` | pôr fatias em serviço, fusão, ordem do pipeline, ciclo de vida da fatia |
| `freetoken-moe.md` | medições do goal FreeToken, orçamento de KV e caminho MoE próprio |
| `transferencia.md` | puxar/oferecer arquivos entre máquinas, retomada, MD5, concessões |
| `api-http.md` | todas as rotas, autenticação, `x_pom.source`, `gateway::testkit` |
| `web-ui.md` | como a UI sobe, menu, modais, Painel, chat de teste, telas de fatiar/implantar |

**Por pasta** (`AGENTS.md`) - o que cada arquivo faz, função a função, e as
ligações com outras pastas: `crates/`, `crates/node/`, e dentro de
`crates/node/src/`: `models/`, `network/`, `backend/`, `inference/`, `routing/`,
`utils/`; `crates/routing-contract/`, `crates/routing-base/`, `crates/types/`;
`apps/frontend/` e `src/{components,lib,routes,i18n}/`; `docs/`; `scripts/`.

`docs/AGENTS.md` explica specs e planos (`docs/superpowers/`);
`docs/architecture/routing-plugin-contract.md` é a fronteira open-core.

**`brand.json` (raiz)** é a identidade do produto - nome, slogan e logo - e a
fonte única de todo nome derivado: arquivo de config, variáveis de ambiente,
chaves de identidade, metadados GGUF e o registro pkarr. O binário o embute em
tempo de compilação (`utils/brand.rs`) e a interface o copia para o bundle
(`apps/frontend/scripts/build-brand.mjs`). As imagens ficam em `assets/brand/`.

**Não é fonte de verdade para arquitetura:** o `README.md` da raiz (é
apresentação) e `old/` (Python legado, fora do runtime).

## Obrigação de documentação

**Quem mexer numa pasta atualiza o `AGENTS.md` dela e os READMEs de subsistema
afetados no mesmo commit.** A tarefa não está pronta enquanto a documentação não
refletir o código entregue. Doc desatualizada é bug: um agente futuro lê a doc e
entende o sistema errado.

Regras de escrita: pt-BR; nunca travessão, use hífen "-"; documente o que o
código faz hoje, verificado; onde código e doc divergem, o código vence e a doc
é corrigida; defeito real se anota em `## Defeitos anotados` e se reporta, não se
conserta de passagem. Defeito corrigido sai da anotação **no mesmo commit** -
um defeito corrigido que continua anotado é pior que um não anotado.

A pasta `crates/node/models/` está no `.gitignore` (padrão `models*`), então o
`AGENTS.md` dela vive em `crates/node/src/inference/AGENTS.md`.

A cópia em `apps/frontend/src/docs/` é **gerada** por
`apps/frontend/scripts/build-docs.mjs` (roda no `npm run build`/`dev`/`test`).
Nunca edite lá.

## Comandos

```bash
cargo build --release                 # ./target/release/pom
cargo build --release --features metal   # Apple Silicon
cargo build --release --features cuda    # NVIDIA (toolkit >= 12.8 para sm_120)
cargo test                            # crates/node
cargo test --workspace                # todos os crates públicos
RUST_LOG=debug ./target/release/pom ...

cd apps/frontend && npm run build     # OBRIGATÓRIO após editar a UI
```

A interface é **lida do disco** em `apps/frontend/dist` em tempo de execução
(`config::ui_dist_dir()`, `$POM_UI_DIST` sobrescreve), não embutida no
binário: `cargo build` não reconstrói a tela e não precisa de Node.

```bash
./target/release/pom --list-models
./target/release/pom                       # sem args: usa pom.json
./target/release/pom --model split_0_15.gguf --port 42777 --inference-port 8181
```

Todo nó serve a UI e a API na `--gateway-port` (8080): abra
`http://<peer>:8080/`. Ver `docs/subsystems/operacao.md` para a subida completa
e `build-execucao.md` para os scripts e variáveis.

## Arquitetura em um parágrafo

Binário Rust único (`crates/node`, pacote `node`) que coordena camadas de LLM
entre máquinas. Cada nó descobre peers por UDP/libp2p, anuncia a faixa de
camadas que serve, e qualquer nó pode ser o gateway: `POST /api/generate` e as
rotas `/v1` rodam **somente** o pipeline distribuído - sem plano cobrindo
`0..total_layers` a requisição falha com mensagem, não há fallback para Ollama.
O servidor de inferência de cada peer (`/v1/prefill`, `/v1/step`) carrega o
modelo em-processo via `llama-cpp-sys-2` e troca estado oculto em f16.
Concorrência: Tokio, estado compartilhado em `Arc<RwLock<>>`.

Módulos em `crates/node/src/`: `main.rs` (CLI e orquestração), `models/`
(catálogo, GGUF, perfis, splitter), `network/` (descoberta, identidade,
transporte, túnel), `routing/` (planejador de pipeline), `backend/` (gateway,
APIs, admin, transferência, deploy, UI), `inference/` (servidor embutido, llm,
wire, relay, direct, especulativa), `utils/` (config, TUI, sysmon).

## Invariantes que valem em todo lugar

Cada uma é detalhada no subsistema indicado; estão aqui porque quebrá-las
quebra o produto inteiro.

- **Project root.** `models/`, `pom.json`, chaves e a UI saem de
  `config::project_root()` (`$POM_HOME` → cwd que pareça o projeto →
  diretório do executável). Lançar o binário de outro cwd lê outra config.
  Dois nós na mesma máquina precisam de `POM_HOME` distintos, porque a
  identidade (`.pom-node.key`) mora ali. → `operacao.md`
- **`node_id` é o nome da máquina no cluster** e a chave de `blocked_nodes`;
  apagar `.pom-node.key` cria uma máquina nova. Não confundir com
  `.pom.key`, do pkarr. → `rede-p2p.md`
- **A chave de conexão é a mesma no cluster inteiro** e é a credencial de
  operador: autentica `/admin/*`, `/api/ui/{plan,apply,launch}` e as rotas
  `/v1`. Sem `pom.json` o nó sobe em **modo setup**, que serve só a tela de
  escolha Principal/Nó e gera essa chave. → `api-http.md`, `web-ui.md`
- **Política de primeira versão.** O produto ainda não foi lançado. Portanto,
  não manter no runtime código de compatibilidade, adapters ou fallbacks para
  versões antigas de protocolos - especialmente formatos de inferência v1/v2.
  O `v1` de `frame/v1-batch` identifica o primeiro contrato oficial atual, não
  uma obrigação de suportar um protocolo legado; mudanças futuras devem
  evoluir esse contrato de forma explícita. → `inferencia.md`
- **A geração nunca acontece na sequência de KV do prompt.** Cada sessão de um
  peer tem duas sequências: o prefill escreve na do prompt, a geração numa cópia
  dela, e o fim do turno apaga a cópia inteira. Só remoção **total** de
  sequência é usada, porque a parcial é recusada por motores recorrentes e
  híbridos - e o cache de prefixo entre turnos é funcionalidade de produto, que
  vale para todo modelo servido, não critério de elegibilidade. Retenção é ato
  local do peer no EOS; `/v1/session/rewind` apenas confere. → `inferencia.md`
- **A ordem do pipeline é a ordem das camadas**, e um nó pode servir várias
  faixas. `merge` apenas junta faixas adjacentes no mesmo nó. Sobreposição e
  buraco global são recusa. Quem fica com a camada 0 processa o prompt inteiro
  de toda requisição. → `implantacao.md`
- **Quantas camadas o modelo tem é `Option`, não número.** Fatia sem metadata
  `pom.*` não sabe o total; inventar zero foi um bug real. → `fatiamento.md`
- **`<config>.last-slice.json` só vira `serving` pelo watcher do `/health`.**
  Anotar no `apply` o que só o `/health` sabe é laço infinito de reinício. →
  `implantacao.md`
- **Nenhum peso trafega na rede por causa do fatiamento**: cada peer extrai a
  própria faixa da própria cópia. Transferência de arquivo é ato explícito do
  operador. → `fatiamento.md`, `transferencia.md`
- **O nome do produto não é literal em lugar nenhum.** Config, env, chaves,
  metadados GGUF e registro pkarr derivam do `slug` de `brand.json`
  (`brand::config_filename()`, `brand::env_var()`, `brand::gguf_key()`,
  `brand::dns_record()`). A única exceção que o cargo obriga é o `[[bin]]` em
  `crates/node/Cargo.toml`. → `build-execucao.md`
- **O gossip nunca é fonte de verdade da flag de TLS.** O campo `tls` do
  Announce é gatilho; a política só é gravada depois de busca autenticada no
  principal, e busca que falha preserva o valor local. O UDP é em claro e não
  autenticado: obedecer a ele seria oferecer o rebaixamento que o TLS existe
  para impedir. O certificado de cada nó é derivado da `.pom-node.key` e
  verificado por `SHA256(pubkey) == node_id`. → `rede-p2p.md`
- **Testes de rota usam `gateway::testkit`**, nunca UDP de verdade - a suíte
  transmite na LAN e pode injetar mensagens num cluster vivo. → `api-http.md`

## Manutenção deste arquivo

Ele deve caber numa leitura. Conhecimento novo entra no `AGENTS.md` da pasta ou
no subsistema correspondente; aqui só entra o que muda o mapa (uma pasta nova,
um subsistema novo, uma invariante que atravessa tudo). Prefira reescrever e
podar a acrescentar.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
