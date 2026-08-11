# Atualização remota de um nó pela interface (só em desenvolvimento)

Data: 2026-08-10

## O problema

Um cluster é várias máquinas, e nem todas são acessíveis por quem opera. Em
2026-08-10 um peer respondia `residual capture did not fire` — um erro já
corrigido no commit `78f1105`. A máquina (`tiago-System-Product-Name`) rodava um
binário anterior à correção, e o operador não tinha shell nela. Um peer
desatualizado é indistinguível de um peer com defeito novo: mesma faixa de
camadas, `/health` saudável, e um erro vindo de um caminho já consertado.

O commit `1d353cd` resolveu o *diagnóstico* — `/api/ui/node` agora devolve
`build_commit`. Falta o *remédio*: pedir à máquina que se atualize.

## O que se constrói

Um botão no card de cada peer que, na máquina daquele peer:

1. confere que dá para atualizar com segurança;
2. `git pull --ff-only origin main`;
3. `cargo build --release` com as features daquela máquina;
4. reinicia **todos** os nós daquela máquina, cada um preservando o próprio
   ambiente, e volta servindo a mesma fatia.

Se qualquer passo falhar, ninguém reinicia.

## O portão

`RDMA_P2P_DEV_UPDATE=1` no ambiente do processo. Sem ela, as rotas respondem
403 e a interface não desenha o botão.

Por que ambiente e não config: a variável é por processo — o que casa com
vários peers na mesma máquina, cada um com seu env — e **não é ligável pela
rede**, diferente do `rdma-p2p.json`, que a própria interface reescreve. Vale em
debug e em release; limitar a `debug_assertions` foi considerado e descartado,
porque todas as máquinas do cluster rodam `--release` e o recurso nunca
existiria.

A autenticação continua a existente (chave de conexão como Bearer, igual a
`apply`/`launch`). O portão é uma segunda tranca, não a única: quem tem a chave
já podia mandar o nó carregar modelos.

`GET /api/ui/node` passa a devolver `dev_update: bool`, para a tela nunca
oferecer o que vai dar 403.

## As duas rotas

**`POST /api/ui/dev/update`** — atualiza a máquina. Roda no nó do card.

**`POST /api/ui/dev/restart`** — reinicia **a si mesmo**, e só. É o que resolve
as configs distintas: nenhum nó consegue respawnar o irmão, porque não conhece o
`RDMA_P2P_CONFIG` nem o `RDMA_P2P_NODE_KEY` dele. Cada nó se reinicia herdando o
próprio ambiente.

Os irmãos são os peers com o mesmo `hostname` — `PeerInfo` já carrega `hostname`
e `gateway_port`, então "os nós daquela máquina" é uma consulta que a rede já
responde.

## A sequência

Um lock em arquivo no `project_root` serializa a atualização, para que dois
cards da mesma máquina não disputem o `target/`.

1. **HEAD no `main`.** Outro branch, ou HEAD destacado, recusa dizendo onde
   está. Sem isso, `git pull --ff-only origin main` num branch de trabalho o
   avançaria para o `main` em silêncio — nenhum commit se perde, mas o ponteiro
   do branch de outra pessoa se move, e isso é intromissão.
2. **Árvore limpa.** `git status --porcelain` (arquivos rastreados) não vazio
   recusa com a lista na tela. Um build de árvore suja não corresponde a commit
   nenhum, e o carimbo `build_commit` viraria mentira.
3. `git pull --ff-only origin main`.
4. `cargo build --release`, com as features lidas de `RDMA_P2P_BUILD_FEATURES`
   (vazio = nenhuma). Sem isso a ksia perderia CUDA no primeiro clique — o
   mesmo engano já cometido à mão em 2026-08-10.
5. **Só se tudo passou:** `dev/restart` em cada irmão, e por último em si mesmo.

Progresso e log pelo sistema de jobs que já existe (`/api/ui/jobs`), como o
fatiamento: fechar a aba não interrompe nada.

## Garantias sobre o trabalho de quem usa a máquina

A lista de comandos é fechada: `git status`, `git rev-parse`, `git pull
--ff-only`, `cargo build`. **Não** existe `reset --hard`, `checkout .`, `clean`
nem `stash`.

- **Modificações não commitadas:** a atualização para no passo 2, antes de tocar
  em qualquer coisa.
- **Commits locais não enviados:** `--ff-only` aborta em vez de mesclar ou
  rebasear.
- **Arquivos não rastreados:** o pull não os toca; se um deles colidir com um
  arquivo que vem do remoto, o próprio git aborta sem sobrescrever.
- **Worktrees:** cada um é um diretório com checkout próprio. O pull roda no
  `project_root` daquele nó. O banco de objetos compartilhado só recebe
  acréscimos.
- **Branch de outra pessoa:** o passo 1 recusa.

Numa máquina onde alguém está desenvolvendo, o botão **falha e explica**, em vez
de arrumar a casa alheia.

O que sobra e não é perda de dado: um `cargo build` humano no mesmo diretório se
serializa no lock do próprio cargo, e o binário em `target/` é substituído para
os dois. Isso é inerente a compartilhar o diretório; a tela avisa.

## Voltar servindo a mesma fatia

A atribuição de modelo vive só em memória, então reiniciar esvazia o nó.

Antes de reiniciar, cada nó grava o `NodeConfig` que estava servindo num arquivo
derivado do caminho da sua config — `<config>.last-slice.json` — para que dois
nós na mesma máquina não escrevam no mesmo lugar. Ao subir, se o arquivo
existir, o nó reaplica a fatia e **apaga o arquivo**. Assim um arquivo órfão de
um restart que deu errado não ressuscita fatia velha para sempre.

## Correção necessária no caminho

O respawn de hoje (`webui::save_setup`) perde os argumentos de linha de comando:
repassa só `--gateway-port`. Um nó subido com `--advertise-addr` ou `--refresh
3600` voltaria diferente do que era. O `argv` original passa a ser capturado no
arranque e reexecutado igual, e os dois caminhos de restart usam isso.

## Erros

Build quebrado, árvore suja, branch errado, pull sem fast-forward: o job termina
em `failed`, o log completo vai para a tela, e ninguém reinicia. O pior caso é a
máquina continuar exatamente como estava, servindo o binário que já está em
memória — que não depende do arquivo em disco.

## Testes

Sem subir cluster:

- portão fechado responde 403 e `dev_update: false`; ligado, o inverso;
- árvore suja recusa, com os arquivos na mensagem;
- branch diferente de `main` recusa, e HEAD destacado também;
- o `argv` capturado no arranque é reexecutado igual — a regressão que existe
  hoje no `save_setup`;
- o arquivo de fatia é gravado, reaplicado e apagado, e o arquivo de um nó não é
  lido pelo vizinho da mesma máquina.

`git pull` e `cargo build` de verdade são integração, cobertos pelo uso.

## O que não se constrói

- Escolher branch ou commit pela tela. Só `main`.
- Atualizar máquina que não tem o portão ligado.
- Reverter para o binário anterior. O remédio de um build ruim é outro commit.
