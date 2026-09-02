# Banco local único do POM

## Contexto

O runtime mantém auditoria em `audit.sqlite3`, implantações em
`deployments/*.json` e chaves na configuração. Isso fragmenta estado que deve
ser consultado junto e torna impossível manter a memória de artefatos depois
que um arquivo GGUF é removido. A base atual do produto é 0.0.1, então o banco
novo nasce limpo: o processo não carrega nem conhece formatos legados.

## Decisões

`pom.db` será aberto no arranque e terá o schema criado pelo próprio binário.
`POM_DB_DIR` aponta o diretório para um volume Docker; sem a variável, o
diretório é `config::project_root()`. O diretório pai é criado quando falta.
Cada conexão usa `busy_timeout` de 5 segundos, `foreign_keys=ON`,
`journal_mode=WAL` e `synchronous=NORMAL`. WAL permite que leitores e o
escritor concorram sem bloquear indefinidamente; o timeout cobre apenas
contenção curta e qualquer erro continua sendo reportado ao chamador. O
arquivo e os sidecars `-wal`/`-shm` ficam com modo 0600 no Unix, inclusive
quando o escritor de auditoria os cria depois do arranque.

O módulo `backend/db.rs` será o dono da persistência local. Ele expõe operações
para implantações, chaves e histórico de modelos, abrindo conexões curtas e
transacionais para que diferentes tasks e processos compartilhem o mesmo
arquivo. Auditoria passa a receber o caminho do `pom.db` e preserva o escritor
dedicado já existente.

O schema contém:

- `deployments`, com o JSON validado do registro, metadados de consulta e um
  estado ativo. Desimplantação é um tombstone persistente, não exclusão física,
  para que o arquivo antigo mantido como backup não ressuscite o registro.
- `api_keys`, que mantém o segredo localmente protegido pelo modo 0600 do
  arquivo, e `api_key_revocations`, append-only, com chave, identidade do
  revogador e timestamp. `local_settings` guarda a política térmica da GPU;
  `local_secrets` guarda o token do Hugging Face. O token nunca entra em
  logs, erros ou respostas HTTP.
- `model_history` e `model_slices`, com identidade do modelo, rótulo, origem,
  caminho conhecido, tamanho, total de camadas, hash da parte, faixa, rank TP,
  primeira/última observação e presença atual. A ausência posterior do arquivo
  apenas marca `present=0`; não remove a história.
- `chat_conversations` e `chat_messages`, com título, modelo inicial, ordem,
  conteúdo, eventos, erros, telemetria e trace de cada conversa. O histórico é
  lido e alterado por rotas autenticadas do dashboard, e excluir uma conversa
  não apaga as auditorias de pedidos.
- as tabelas atuais de auditoria e FTS5, agora no mesmo arquivo.

API keys, a política térmica e o token do Hugging Face são carregados do banco
para o estado em memória no arranque. Geração e revogação atualizam o banco e
o espelho; a revogação só desativa a chave e registra o ator, então uma chave
revogada não volta a ser aceita após reinício. Os campos equivalentes de
`pom.json` continuam serializáveis como cópia antiga, mas não são usados como
fallback pelo runtime.

O catálogo grava o histórico após o enriquecimento em background. Para modelos
inteiros sem hash fornecido pelo catálogo, calcula SHA-256 do arquivo; para
fatias, usa `pom_id` e o hash já declarado no manifesto. O registro ocorre em
transação e nunca depende da permanência do arquivo.

## Migração opcional fora do runtime

Não há migração automática, tabela de migração legada ou leitura das fontes
antigas no arranque. A preservação opcional fica em uma única ferramenta
descartável, `scripts/migrate_pom_db.py`, que o operador executa explicitamente
depois de iniciar uma base nova. Ela copia auditoria, implantações, chaves e
as preferências/credencial antigas sem alterar `audit.sqlite3`, `deployments/`
ou `pom.json`, e aborta diante de conflitos ou registros ilegíveis.

## Fora de escopo

O banco não substitui o armazenamento de pesos, não apaga fontes antigas e não
implementa sincronização entre máquinas. O histórico é local ao projeto que o
abriu; transferência de modelos continua sendo uma ação explícita do operador.
