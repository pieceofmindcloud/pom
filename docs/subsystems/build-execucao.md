# Build e execução

Como o projeto compila, empacota e roda. Autoritativo é o código; este
documento reflete o estado atual do repositório. O `README.md` da raiz é
apresentação, não fonte de verdade para arquitetura.

## Workspace de crates

`Cargo.toml` na raiz define um workspace com cinco membros e
`default-members = ["crates/node"]`, então `cargo build`/`cargo test` na raiz
compilam e testam o nó. Para todos os crates públicos e o nó, `cargo test
--workspace`. `resolver = "2"`; versão e edition (0.1.0, 2021) vêm de
`[workspace.package]`; dependências compartilhadas são `anyhow`, `serde` e
`serde_json`.

- `node` - o único binário (`pom`), o produto inteiro. Depende só de
  `routing-contract` (path dep). É um binário de raiz única (`src/main.rs`)
  que declara os módulos via `#[path]`; features `cuda`/`metal` repassadas a
  `llama-cpp-sys-2`.
- `routing-contract` - contrato serializável e versionado de roteamento.
- `routing-base` - planejador determinístico aberto (não é chamado por
  ninguém hoje; ver Defeitos).
- `config`, `types` - pontos de extração, hoje scaffolding vazio (lib.rs sem
  conteúdo, nada os linka).

Nota: os `crate::config` citados dentro do `node` são o módulo próprio
`crates/node/src/utils/config.rs`, não o crate `config`.

## Build cargo

```bash
cargo build --release    # binário em target/release/pom
cargo build              # debug
cargo test               # testes do nó
```

- O perfil release em `.cargo/config.toml` usa `opt-level = 3`, `lto = "fat"`
  e `codegen-units = 1` (medido ~30x no laço quente do sampler), `debug = 0`.
  No Linux o linker é clang com `-fuse-ld=mold` para acelerar o link fat LTO;
  **o pacote `mold` é dependência de build no Linux** (`apt-get install mold`).
  `BINDGEN_EXTRA_CLANG_ARGS` aponta o bindgen do llama-cpp-sys para os headers
  do GCC no Ubuntu (o clang do apt não os traz); no macOS é inofensivo.
- **Features**: `cuda`/`metal` (default vazio) repassadas a `llama-cpp-sys-2`,
  que compila o GGML nativo com CUDA ou Metal. Ex.: `cargo build --release
  --features cuda`. São usadas pelo `Dockerfile` e pelo devupdate
  (`POM_BUILD_FEATURES`). Nos builds CUDA, CUDA Graphs ficam desligados por
  padrão para evitar a reserva adicional de VRAM que pode falhar no
  `cudaGraphLaunch` mesmo depois do warmup. Para habilitar esse recurso,
  somente quando houver VRAM livre comprovada, use
  `POM_CUDA_GRAPHS=1 cargo build --release --features cuda`.

## Identidade do produto (`brand.json`)

`brand.json`, na raiz, é a fonte única do nome, do slogan e dos arquivos de
logo. Renomear o produto é editar esse arquivo - e, só para o executável, o
`[[bin]]` de `crates/node/Cargo.toml`, que o cargo exige literal.

- **Binário**: embute o arquivo em tempo de compilação (`utils/brand.rs`,
  `include_str!`), então o nome viaja dentro do executável e não há arquivo para
  entregar ao lado dele. Tudo o que carregava o nome antigo passou a derivar do
  `slug`: `brand::config_filename()` (`pom.json`), `brand::env_var("HOME")`
  (`POM_HOME`), as chaves de identidade (`.pom-node.key`, `.pom.key`),
  `brand::gguf_key("start_layer")` (`pom.start_layer`) e `brand::dns_record()`
  (`_pom`, o TXT do pkarr). Há teste que quebra se algum deles parar de derivar.
- **Interface**: `apps/frontend/scripts/build-brand.mjs` roda antes de
  dev/build/test (pre-hooks do npm), copia o manifesto para `src/brand/` e as
  imagens para `public/brand/` - as duas pastas são geradas e ignoradas pelo
  git. `src/lib/brand.ts` expõe `brand` e `storageKey()`, que é o que dá o
  prefixo das chaves de `localStorage` (`pom.theme`, `pom.locale`, ...).
  O `index.html` não pode importar nada, então o plugin `brandHtml` do
  `vite.config.ts` substitui `%BRAND_NAME%`, `%BRAND_SLUG%` e os ícones no
  build - é o que impede a aba do navegador de anunciar um nome velho.
- **Imagens**: `assets/brand/` guarda o logo completo (`pom-logo.png`, o
  lockup com letreiro e slogan), a arte do ícone (`pom-icon-src.png`, a origem),
  a marca quadrada em 1024 (`pom-mark.png`), os PNGs de 16 a 512 e o
  `favicon.ico`. As cópias servidas pela interface levam **hash de conteúdo no
  nome** (`pom-icon-512.f95e6ea3.png`), porque `webapp.rs` serve tudo que não é
  `index.html` como `immutable` por um ano - com nome fixo, trocar a logo não
  chegaria a quem já abriu a tela.
- **O lockup tem duas artes, uma por tema**: `logo.light` e `logo.dark` no
  `brand.json`, e por isso ele não precisa de placa atrás - cada tema recebe a
  arte desenhada para o fundo dele. A escura é **derivada** da clara por
  `scripts/derive-dark-logo.py`: o letreiro (azul-petróleo chapado, feito para
  fundo claro) tem a luminância espelhada, e a ilustração do cérebro não é
  tocada, porque recolori-la achatava o volume. Quando existir uma arte escura
  desenhada de verdade, sobrescrever `assets/brand/pom-logo-dark.png` é a
  mudança inteira - e o script pode ser apagado.

- **Bandeiras do seletor de idioma**: `assets/brand/flags/{us,br}.svg`,
  mapeadas por locale em `brand.json` (`flags`). São SVG porque o alvo é um
  ícone de 24x16 na barra lateral e um PNG pequeno esticado fica sujo; trocar
  por outra arte é substituir o arquivo e rodar o build da interface.

### Ícone do aplicativo, por plataforma

Um executável Unix não carrega ícone; cada sistema resolve isso de um jeito, e
os três saem do mesmo `brand.json` (`app_icons`):

| Sistema | Arquivo | Como entra |
|---|---|---|
| Windows | `assets/brand/pom.ico` | recurso **dentro do `.exe`**, gravado por `crates/node/build.rs` (`embed_windows_icon`, via `winresource`). O `cfg(windows)` ali é do host: o release compila o alvo Windows num runner Windows. Cruzar de Linux sai sem ícone, em silêncio, em vez de quebrar o build. |
| macOS | `assets/brand/pom.icns` | quem mostra o ícone é o bundle, não o binário: `scripts/make-macos-app.sh` monta `POM.app` em volta do executável já compilado (Info.plist, `LSUIElement` para não abrir no Dock, ícone em `Resources/`). O release roda o script e leva o `.app` no pacote do macOS. |
| Linux | `assets/brand/linux/hicolor/**` + `pom.desktop` | árvore `hicolor` de 16 a 512 e a entrada `.desktop`, empacotadas em `share/` no tarball. Instalar é copiar para `~/.local/share/{icons,applications}`. |

**O `.app` tem que achar a interface.** Dentro do bundle o binário fica em
`Contents/MacOS/`, e dali `project_root()` cairia no cwd de quem abriu (o
Finder abre em `/`), com a tela respondendo 500. `config::bundled_ui_dist()`
resolve `Contents/Resources/apps/frontend/dist` antes de tentar o project root;
o script de release copia o `dist` para lá.

Renomear o produto de novo: edite `brand.json`, troque o `[[bin]]`, gere as
imagens novas em `assets/brand/` e rode `scripts/migrar-para-pom.sh` (ou o
equivalente) em cada máquina, porque config e chaves já gravadas em disco
continuam com o nome anterior.

## Carimbo de commit (crates/node/build.rs)

O binário carimba o commit de que foi construído, exposto em `/api/ui/node`
(`build_commit`) - é como se descobre um peer com binário velho sem shell na
máquina. Precedência:

1. Variável `POM_BUILD_COMMIT` (é como o commit entra num container: o
   `.dockerignore` corta `.git/` do contexto e a imagem builder não tem `git`,
   então ler dentro dela falha e carimbaria "desconhecido").
2. `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
   mostrar mudanças.
3. `desconhecido` se não der para ler nada.

`rerun-if-env-changed` na variável e `rerun-if-changed` em `.git/HEAD` e
`.git/index` impedem o carimbo de congelar no primeiro build.

## Atualizar um nó pela própria interface (só desenvolvimento)

`backend/devupdate.rs`. O card de cada peer ganha "Atualizar e reiniciar" quando
**aquele processo** subiu com `POM_DEV_UPDATE=1` - é variável de ambiente,
por processo, porque a config é reescrita pela própria interface e um campo lá
deixaria o portão ligável pela rede. `POM_BUILD_FEATURES=cuda` (ou `metal`)
diz com que features a máquina compila; sem isso uma máquina com GPU perde o
offload no primeiro clique.

- **Agir sobre outro peer é repasse**: o painel só fala com o próprio nó
  (caminhos relativos em `apps/frontend/src/lib/api.ts`), então
  `POST /api/ui/dev/update` com `{"node_id": "..."}` faz o nó local encaminhar.
  O portão é verificado **no destino**, nunca em quem repassa.
- **Nenhum comando destrutivo**: a sequência recusa branch fora do `main`, HEAD
  destacado e árvore suja, e só então faz `git pull --ff-only` e
  `cargo build --release`. Não há `reset`, `checkout`, `clean` nem `stash`, então
  numa máquina onde alguém está codificando o botão falha e explica. Falhou em
  qualquer passo, ninguém reinicia.
- **Cada nó reinicia a si mesmo** (`/api/ui/dev/restart`): nenhum consegue
  respawnar o irmão, porque não conhece o `POM_CONFIG` nem o
  `POM_NODE_KEY` dele. Irmão é peer com o mesmo hostname **e** o mesmo host
  de rede (`same_machine`) - hostname sozinho casaria duas VMs chamadas `ubuntu`;
  peer com `addr` `0.0.0.0`/`::` é descartado, porque esse endereço não reescrito
  aponta para o próprio nó.
- `devupdate` **não** escreve a anotação de última fatia: esse ciclo de vida é do
  `runtime.rs` (ver `implantacao.md`), e uma escrita aqui só poderia mentir.

## Docker

- **`Dockerfile`**: dois estágios. Builder
  `nvidia/cuda:13.0.3-devel-ubuntu24.04` instala rustup + node, faz
  `npm run build` em `apps/frontend` e `cargo build --release --features cuda`
  com cache mounts (registry/git/target). Runtime é a base CUDA 13.0.3 com
  `LD_LIBRARY_PATH` incluindo `compat/` e `POM_HOME=/app`. O `ARG
  POM_BUILD_COMMIT` recebe o commit do host.
- **`scripts/build-docker.sh`**: leia o commit **no host** (`git rev-parse
  --short HEAD`, sufixo `-sujo` se a árvore tem mudanças), exporta
  `POM_BUILD_COMMIT`, roda `npm run build` e `docker compose build`.
  Construa a imagem por este script; `docker compose build` na mão carimba
  "desconhecido" em todo container, de propósito.
- **`scripts/run-docker.sh`**: `docker compose down` + `docker compose up -d`
  (não recria a imagem).
- **`docker-compose.yml`**: cluster local de 4 serviços (`node-gpu`,
  `node-gpu-2`, `node-cpu-1`, `node-cpu-2`) com `network_mode: host`; estado
  em `tests/fixtures/<nome>`, modelos montados de `./models`, GPU só via
  `deploy.resources` (exige NVIDIA Container Toolkit). Cada serviço define
  `POM_HOME=/data` e `POM_UI_DIST=/app/apps/frontend/dist` (a UI
  não ficaria sob o diretório de dados montado). As portas de cada nó vêm da
  config, não do CLI - daí o gerador abaixo. Os nomes de fixture do compose
  (`node-gpu`, `node-cpu-1`...) e os do gerador (`node-<N>`) são de ambientes
  diferentes de propósito e não precisam se encontrar - ver `scripts/AGENTS.md`.
- **`scripts/gen-node-configs.py`**: gera um `pom.json` por nó em
  `tests/fixtures/node-<N>` (nó 1 = principal, guarda `connection_key` e
  `api_keys`; demais = node com bootstrap para `127.0.0.1:42777`). Recusa
  gerar sem `connection_key` (herda da config base `--base`, nunca gera a
  chave), `--force` sobrescreve e `--advertise-addr` atualiza apenas o
  endereço anunciado mesmo em configs existentes. As portas seguem a série:
  gateway 8080+, p2p 42777+, inferência 8181+ (nó 1 herda a histórica 8092).
- `.dockerignore` corta `target/`, `.git/`, `venv/`, `models/`, `*.gguf`,
  `*.key`, `pom.json` e `docs/` do contexto de build - por isso o commit
  chega por variável e a config não entra na imagem.

## Python legado e venv

O runtime suportado é **Rust** (`crates/node/src/inference/infer_server.rs` é
o servidor de inferência; `crates/node/src/models/splitter.rs` o fatiador).
Os scripts `inference_server.py` e `split_model.py` vivem em
`old/python-inference/` e **não são invocados pelo binário**. Um venv em
`./venv` (python 3.9+ do macOS ou o do Ubuntu) é útil só para o operador que
quiser rodar o legado via `old/scripts/run-inf.sh`; a tabela de tipos ggml de
`gguf.rs` é conferida contra a `GGML_TRUTH` no próprio arquivo (teste de
build), não contra o venv. O único script que o build Rust consulta
externamente é o `gen-node-configs.py` (não é do build, é de operação).

## Scripts de cluster

Os lançadores (`run-p1.sh`, `run-p2.sh`, `request.sh`) **não estão na raiz**:
vivem em `old/scripts/` e são referência de operador (o binário não os usa).
O procedimento de subida está em `operacao.md`.

- `old/scripts/run-p1.sh` - peer 1 (MacBook, Metal): camadas 0-15, `--port
  42777`, gateway em 8080, `--inference-port 8181`, `--n-gpu-layers 99`,
  `--no-thinking`. Aceita `MODEL`, `N_CTX`, `N_BATCH`, `MAX_NEW_TOKENS` por
  variável de ambiente.
- `old/scripts/run-p2.sh` - peer 2 (ksia): camadas 16-31, bootstrap para o
  peer 1, `--inference-port 8182`. Faz `cd "$(dirname "$0")"` e assume o
  binário relativo ao cwd.
- `old/scripts/request.sh` - `POST /api/generate` para o gateway com o prompt
  como primeiro argumento (`GATEWAY`/`PORT` sobrescrevem o host/porta).

Não existe `pom-ksia.sh` em lugar nenhum do repositório (nem em histórico de
git); a referência a ele no plano da etapa não corresponde a artefato
versionado. O papel de "subir a ksia" hoje é do `old/scripts/run-p2.sh`.

## Variáveis de ambiente importantes

| Variável | Efeito |
|---|---|
| `POM_HOME` | Resolução de `project_root()` em primeiro lugar: config, `models/`, `venv/`, UI. Sem ela: cwd que parece o projeto, depois subir do diretório do executável. Dois nós na mesma máquina precisam de casas distintas (identidade `node_id` também mora no project root). |
| `POM_CONFIG` | Caminho do `pom.json` (padrão `<project_root>/pom.json`). |
| `POM_UI_DIST` | Onde `webapp.rs` lê o bundle da UI (padrão `<project_root>/apps/frontend/dist`). A imagem Docker a seta explicitamente. |
| `POM_DEV_AUTO_ACCEPT_TRANSFERS` | `=1` faz o nó **receptor** aceitar toda oferta de transferência sem clique. Só desenvolvimento; lida uma vez no arranque. O remetente não pode pedi-la. |
| `POM_DEV_UPDATE` | `=1` liga o portão de "Atualizar e reiniciar" no card do peer (`devupdate.rs`). Por processo, nunca na config. |
| `POM_BUILD_FEATURES` | Features com que a máquina compila no devupdate (`cuda`/`metal`); sem ela a ksia perde a GPU no primeiro clique. |
| `POM_CUDA_GRAPHS` | `=1`, `true`, `yes` ou `on` habilita CUDA Graphs no build CUDA. O padrão é desligado para reduzir o risco de OOM no lançamento do grafo. É uma opção de compilação, então exige reconstruir o binário. |
| `POM_BUILD_COMMIT` | Commit carimbado pelo `build.rs` (usado pelo build-docker.sh; `.dockerignore` corta `.git/`). |
| `VITE_API_TARGET` | Alvo do proxy do dev server da UI (padrão `http://127.0.0.1:8080`). |

## Release pelo GitHub Actions

Três workflows, um por sistema, todos só manuais (`workflow_dispatch`):

| Arquivo | Alvo | Roda em | Entradas |
|---|---|---|---|
| `.github/workflows/release-linux.yml` | Linux x86_64 (CPU, e CUDA opcional) | runner `ksia` | `version`, `cuda`, `prerelease` |
| `.github/workflows/release-macos.yml` | macOS arm64, Metal | runner `macbook` | `version`, `prerelease` |
| `.github/workflows/release-windows.yml` | Windows x86_64, CPU | hospedado (`windows-2022`) | `version`, `prerelease` |

**São separados para poder rodar sozinhos.** Publicar só o macOS não deve
custar um build de Windows, e um alvo que quebra não leva os outros junto. Cada
workflow publica o próprio pacote **na mesma tag**: o primeiro a rodar cria a
release, os seguintes anexam o arquivo deles. Rodar de novo só o de Linux não
apaga o pacote de macOS que já estava lá.

### O fork do llama.cpp é o passo que não pode faltar

`vendor/llama-cpp-sys-2/llama.cpp` é um **symlink versionado** para a raiz do
repositório, e a pasta que ele aponta (`/llama.cpp/`) está no `.gitignore`. Num
checkout limpo - que é exatamente o que a CI faz - o symlink aponta para o
vazio, e o build morre no `build.rs` do `llama-cpp-sys-2` com
`Failed to obtain entry ... NotFound`, antes de compilar qualquer coisa do
projeto. Por isso cada workflow tem um passo que clona o fork
(`pereirajair/llama.cpp`, branch `pom-tensor-parallel`, público) **por cima do
symlink**, no caminho que o `build.rs` de fato lê (`<manifest_dir>/llama.cpp`).

Clonar nesse caminho, e não na raiz, é o que faz o Windows funcionar: lá o
checkout não cria symlink nenhum - materializa um arquivo de texto com o caminho
dentro -, então repor a pasta na raiz não adiantaria.

### A interface tem que ir no pacote

A UI é lida do disco em tempo de execução (`config::ui_dist_dir`), não embutida
no binário: sem `apps/frontend/dist` no pacote, o nó sobe e responde 500 na
tela.

Nos dois workflows self-hosted ela é compilada no próprio job, porque lá não há
multiplicador de minuto e separá-la só acrescentaria uma ida e volta de artefato
entre dois jobs que rodariam na mesma máquina de qualquer jeito - o runner
atende um job por vez. No de Windows, que é hospedado, o job `ui` continua em
`ubuntu-latest`: ali o minuto de Windows conta 2x e compilar a mesma interface
lá custaria o dobro pelo mesmo `dist`.

Os dois workflows self-hosted fixam o Node com `actions/setup-node` em vez de
usar o do sistema. O runner grava um **retrato do PATH de quem o configurou**, e
esse retrato envelhece: a ksia traz Node 18, e no macOS o Node vem do nvm, cujo
caminho muda quando a versão padrão muda.

### Runners self-hosted

Linux e macOS rodam em máquinas próprias, então **não consomem cota**:

| Runner | Máquina | Labels | Serviço |
|---|---|---|---|
| `ksia` | Ubuntu 24.04 x86_64, 8 núcleos, 46 GB | `self-hosted, Linux, X64, ksia, cuda` | systemd, usuário `github-runner` |
| `macbook` | macOS arm64 | `self-hosted, macOS, ARM64, macbook, metal` | LaunchAgent, no login do usuário |

**O runner da ksia roda como `github-runner`, sem sudo, e isso é deliberado.**
Um runner self-hosted executa o que estiver no workflow de qualquer branch do
repositório; como root, quem conseguisse abrir um branch aqui teria root na
máquina. Sem sudo, o estrago fica limitado ao usuário do runner. Pelo mesmo
motivo as dependências são provisionadas **uma vez na máquina** e o workflow só
confere se estão lá, em vez de instalar a cada execução.

No macOS o runner roda na conta do próprio usuário - inerente a self-hosted numa
máquina pessoal: um workflow tem o mesmo acesso que o usuário logado. Vale
enquanto quem pode empurrar branch neste repositório é confiável.

Comandos de manutenção:

```bash
# ksia
ssh root@ksia '/home/github-runner/actions-runner/svc.sh status'
ssh root@ksia '/home/github-runner/actions-runner/svc.sh stop'
# macbook
~/actions-runner/svc.sh status
```

Se a máquina estiver desligada, o job **fica na fila** em vez de falhar.

Para remover um runner: `./svc.sh uninstall` e depois `./config.sh remove
--token <token de remoção>` (`gh api --method POST
/repos/pereirajair/rdma-p2p/actions/runners/remove-token`).

### Cota (só o workflow de Windows)

Actions é gratuito e ilimitado em repositório **público**. Neste, que é privado,
vale a cota mensal do plano (2.000 minutos no Free) com multiplicador por
sistema: Linux 1x, Windows 2x, macOS 10x. Depois que Linux e macOS foram para
runners próprios, só o de Windows consome cota - e é o único que ainda vale a
pena vigiar.

Um build limpo do zero - incluindo o llama.cpp nativo - foi medido em **3m33s**
num Apple Silicon; um `target/` deste projeto mede **~9 GB**, o que importa na
ksia, onde o disco é o recurso apertado. O workflow de Linux avisa (sem falhar)
quando sobram menos de 15 GB.

### O pacote

Binário na raiz do pacote com `apps/` ao lado, para `project_root()` achar a UI
a partir do diretório do executável - a pasta descompactada funciona em qualquer
lugar, sem `POM_HOME`. Além disso, por sistema: macOS leva o bundle `POM.app`
(com uma segunda cópia do `dist` dentro, para o `.app` sobreviver a ser
arrastado para `/Applications`), Linux leva `share/hicolor` e o `.desktop`, e o
Windows não leva nada porque o ícone já está dentro do `.exe`. Unix empacota com
`tar czf`, Windows com `7z`.

**Windows é o alvo menos exercitado**: nunca foi compilado neste repositório e a
inferência é só em CPU. Ele falha sozinho, sem afetar os outros dois.

## Defeitos anotados

- DEFEITO ANOTADO: `routing-base::plan` (sua única lógica real) não é chamado
  por ninguém - o planejador do nó
  (`crates/node/src/routing/pipeline.rs::plan_pipeline`) faz o próprio plano
  greedy. `routing-base` é código morto mantido vivo só pelos próprios testes.
  Adiado até a decisão de open-core (ver `TODO.md`).
