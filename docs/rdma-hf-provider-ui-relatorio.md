# Relatório de entrega: interface de modelos remotos

Data: 2026-08-24  
Branch: `fm/rdma-hf-provider`  
Especificação: `/Users/jairpereira/Sites/firstmate/data/rdma-hf-provider/spec-ui.md`

## 1. Arquitetura implementada

A UI fala com a camada agnóstica `model_provider_backend`, que recebe um
`DynModelProvider` no `GatewayState`. O provider padrão continua sendo
`HuggingFaceProvider`; nenhum componente conhece `hf`, shell, stdout, stderr ou
credenciais.

O backend expõe DTOs de busca, detalhes, arquivos GGUF, armazenamento e
downloads. Cada download é um `webui::Job` com status próprio, token de
cancelamento e limite de dois processos concorrentes. O frontend compõe a
tela usando os primitives existentes, React Query, o cliente `api.ts`, o
endpoint condicional de jobs com ETag e o polling adaptativo já usado pelo
POM.

## 2. Arquivos criados

- `crates/node/src/backend/model_provider.rs`
- `apps/frontend/src/routes/RemoteModels.tsx`
- `apps/frontend/src/components/RemoteModelCard.tsx`
- `apps/frontend/src/components/RemoteModelDetails.tsx`
- `apps/frontend/src/components/DownloadModelDialog.tsx`
- `apps/frontend/src/components/ModelDownloadsPanel.tsx`
- `apps/frontend/src/lib/modelProvider.ts`
- `apps/frontend/src/lib/modelProvider.test.ts`
- `apps/frontend/src/lib/modelProvider.api.test.ts`
- `apps/frontend/src/components/RemoteModelCard.test.tsx`
- `apps/frontend/src/components/ModelDownloadsPanel.test.tsx`
- `docs/rdma-hf-provider-ui-reuso.md`
- `docs/rdma-hf-provider-ui-relatorio.md`
- `docs/superpowers/plans/2026-08-24-rdma-hf-provider-ui.md`

## 3. Arquivos alterados

- Backend: `crates/node/src/main.rs`, `backend/gateway.rs`,
  `backend/webui.rs`, `models/provider.rs` e `models/hf_cli.rs`.
- Frontend: `App.tsx`, `components/nav.ts`, `components/nav.test.ts`,
  `lib/api.ts`, `lib/queries.ts`, `lib/types.ts` e `i18n/locales.ts`.
- Documentação de manutenção: `apps/frontend/AGENTS.md`,
  `apps/frontend/src/components/AGENTS.md`, `src/routes/AGENTS.md`,
  `src/lib/AGENTS.md`, `src/i18n/AGENTS.md`,
  `crates/node/src/backend/AGENTS.md` e `docs/AGENTS.md`.

## 4. Componentes

`RemoteModels` oferece busca com debounce de 400 ms, Enter imediato, filtro
GGUF, ordenação por relevância/downloads/atualização, paginação preservada na
URL e cards comparáveis. Cada card mostra repo, autor, descrição, tags,
downloads, curtidas, atualização, GGUF, gated/private e os estados baixado ou
em download quando o backend os conhece.

`RemoteModelDetails` carrega arquivos sob demanda em `Modal`, destaca GGUF e
expõe quantização estruturada, nome, tamanho, revisão, licença, tags e estado
local. `DownloadModelDialog` confirma a operação, mostra destino lógico,
espaço conhecido e bloqueia falta de espaço ou destino indisponível.

`ModelDownloadsPanel` reutiliza o `Job` do backend para mostrar fila, status,
percentual somente quando há total confiável, bytes, velocidade, ETA,
cancelamento, retry e mensagens seguras.

## 5. Endpoints e comandos internos

Rotas autenticadas com `ConnectionAuth`:

- `GET /api/ui/model-provider/search`
- `GET /api/ui/model-provider/details`
- `GET /api/ui/model-provider/storage`
- `POST /api/ui/model-provider/downloads`
- `POST /api/ui/model-provider/downloads/:id/cancel`
- `POST /api/ui/model-provider/downloads/:id/retry`

O serviço chama apenas métodos de `ModelProvider`: `search_models`,
`get_model_info`, `list_files` e `download_file`. A implementação Hugging Face
monta argumentos separados e executa o processo externo pelo runner já
existente. A UI envia JSON estruturado, nunca uma linha de comando.

## 6. Fluxo de início

Ao confirmar, o backend valida `repo_id`, `revision`, `filename` e
`quantization`, verifica duplicidade, relê a lista remota de arquivos, revalida
espaço livre, escolhe a primeira pasta de modelos configurada, cria a pasta e
registra um job único. O retorno contém apenas `download_id`. A tarefa começa
em `queued` e aguarda o semáforo de dois processos.

## 7. Progresso

O provider envia `DownloadProgress`; o backend o converte para
`queued`, `starting`, `downloading`, `verifying`, `completed`, `cancelled` ou
`failed` no mesmo `webui::Job` consultado pelo restante da aplicação. O
frontend reutiliza `GET /api/ui/jobs`, ETag e a cadência existente de 2 s com
trabalho ativo e 10 s em repouso. Não há WebSocket, SSE novo ou downloader no
navegador.

## 8. Cancelamento

Cada job guarda seu próprio `CancellationToken`. Cancelar valida que o job
ainda está ativo, cancela somente aquele token e o provider encerra o processo
correspondente. O semáforo libera a próxima tarefa da fila. Um cancelamento
terminal é preservado contra uma atualização posterior anônima.

## 9. Persistência

O estado do download fica no mapa de jobs do nó, não no componente React. O
endpoint de jobs sobrevive a refresh e navegação enquanto o processo continua;
jobs terminais permanecem pelo período de retenção já usado pelo webui. A
identidade de deduplicação é `repo_id`, revisão e filename. O POM não inclui
`local_path` em JSON de job, evitando expor caminho local ao navegador.

## 10. Catálogo local

Após sucesso, o backend grava um registro lateral em `.pom-hf/` com source,
repo, revisão, filename, quantização, tamanho, SHA-256, timestamp e os arquivos
do `ModelManifest` retornado pelo provider. Depois chama a invalidação do
catálogo local. A busca lê esses registros para marcar o repo como baixado, e
os detalhes verificam arquivo físico mais registro lateral antes de marcar uma
quantização como instalada. Nenhum modelo é carregado na GPU ou executado.

## 11. Testes adicionados

- Rust: identidade estruturada rejeita traversal e extensões não GGUF,
  mensagens não vazam token/path, jobs remotos não se confundem com jobs de
  fatia e tamanho de página é limitado. O parser existente também recebeu
  cobertura para descrição e atualização.
- Frontend: serialização de URL e `AbortSignal`, endpoints estruturados de
  início/cancelamento/retry, filtragem de jobs, polling, percentual sem total,
  cards de resultado, metadados GGUF e painel de progresso/cancelamento/retry.
- `rtk npm test -- --run`: 48 arquivos, 287 testes, todos passaram.
- `rtk npm run build`: TypeScript e Vite passaram.
- `rtk cargo fmt --all -- --check` e `rtk git diff --check`: passaram.

## 12. Limitações conhecidas

- Os testes e o build do crate `node` não puderam compilar neste worktree: o
  symlink `vendor/llama-cpp-sys-2/llama.cpp` aponta para um checkout ausente.
  O erro exato reproduzido foi `Failed to obtain entry ... Os { code: 2,
  kind: NotFound, message: "No such file or directory" }` no `build.rs` de
  `llama-cpp-sys-2`. Portanto não há afirmação de suíte Rust verde.
- A tela oferece apenas filtros que o endpoint atual sustenta com segurança:
  GGUF, além da ordenação por relevância, downloads e atualização. Filtros por
  tamanho, licença ou quantização exigem metadados de arquivos por resultado e
  ficam para uma extensão do contrato.
- O catálogo local existente continua sendo o inventário de arquivos GGUF; o
  registro lateral é a identidade de origem do provider e não distribui pesos
  nem altera o fluxo de fatiamento automaticamente.
- O comportamento de retomada de bytes continua delegado ao cache/provider do
  Hugging Face. O POM não implementa um segundo mecanismo de resume.

## 13. Próximos passos

1. Restaurar o checkout nativo de `llama.cpp` e executar os testes do crate
   `node`, a suíte workspace e o build release.
2. Usar o `ModelManifest` lateral no fluxo de fatiamento/implantação quando a
   integração de origem remota precisar distinguir arquivos por fonte.
3. Se necessário, evoluir o contrato de busca para filtros remotos de tamanho,
   licença e quantização sem fazer N chamadas ocultas por resultado.
4. Adicionar fixtures de resposta para versões futuras do CLI e uma
   implementação HTTP alternativa do mesmo `ModelProvider`.
