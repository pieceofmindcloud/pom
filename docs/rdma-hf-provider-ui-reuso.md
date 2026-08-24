# Levantamento de reuso da interface de modelos remotos

Data: 2026-08-24

Este levantamento foi feito antes da implementação da interface de busca,
seleção e download de modelos. A nova tela segue os fluxos já existentes do
POM e não executa comandos ou downloads no navegador.

## Frontend

- Framework: React 18 + TypeScript, empacotado por Vite e estilizado com
  Tailwind. O ponto de entrada é `apps/frontend/src/App.tsx`.
- Rotas: `react-router-dom`; a nova tela será `/modelos/remotos`, com a entrada
  registrada na tabela `components/nav.ts` e no roteador de `App.tsx`.
- Design system: `components/ui.tsx` fornece `PageHeader`, `Panel`,
  `PanelHeader`, `Button`, `Input`, `Select`, `Field`, `Badge`, `Progress`,
  `Empty`, `Skeleton` e `ErrorState`. A tela usará esses componentes e os
  tokens de `index.css`, incluindo os temas claro e escuro.
- Dialog: o padrão de `Modal`/`Confirm` em `components/Confirm.tsx` será
  reutilizado para confirmação do download e para detalhes que precisarem de
  foco modal. Não será criado um segundo primitive.
- Notificações: `ToastProvider`/`useToast` e a busca de `ApiError` são o canal
  de feedback. Não haverá mensagem montada com a linha de comando `hf`.
- Estado e chamadas: React Query em `lib/queries.ts`, `api.ts` como único
  cliente HTTP relativo, `types.ts` como contrato e um hook de query para
  busca/detalhes. O estado dos downloads continua no backend; o componente
  apenas deriva filtros e seleção.
- Tempo real: o backend reutilizará `webui::Job`; a tela reutilizará `useJobs`,
  `requestConditional`/ETag e `jobsPollInterval` de 2 s durante execução e 10 s
  em repouso. Não haverá WebSocket, SSE novo, fetch de GGUF ou downloader no
  browser.
- Internacionalização: todas as strings novas entram nos blocos `en` e
  `pt-BR` de `i18n/locales.ts` e são acessadas por `useI18n().t`.
- Responsividade: a tela segue a coluna scrollável das páginas existentes;
  filtros e detalhes usam a composição flexível atual e continuam utilizáveis
  em desktop, tablet e mobile.

## Backend

- API: um módulo novo de backend registra rotas genéricas de `model-provider`
  e downloads, mesclado no gateway com uma alteração mínima. A UI conhece
  apenas DTOs de modelos e downloads, não nomes de comandos.
- Abstração: handlers recebem `GatewayState` e usam o `DynModelProvider` já
  definido em `models/provider.rs`; a implementação padrão é
  `HuggingFaceProvider`. O parser e a montagem dos argumentos permanecem em
  `models/hf_cli.rs`/`huggingface.rs`.
- Jobs: cada download é um `webui::Job` persistido na memória do nó, com status
  específico (`queued`, `starting`, `downloading`, `verifying`, `completed`,
  `cancelled`, `failed`) e campos opcionais de bytes, velocidade, ETA e erro.
  Isso preserva o armazenamento de estado do backend e a cadência adaptativa já
  usada pelo sino e pelos painéis de fatiamento.
- Cancelamento e fila: cada job recebe um `CancellationToken`; um semáforo
  limita a execução concorrente a duas tarefas e demais tarefas ficam em
  `queued`. Cancelar um token só encerra aquele processo.
- Segurança: `ConnectionAuth` protege busca, detalhes e mutações; os handlers
  validam identificador, revisão e arquivo através do contrato do provider. A
  UI nunca envia token do Hugging Face e nunca recebe saída bruta do processo.
- Espaço e catálogo: o backend revalida tamanho e espaço antes do início,
  escolhe a primeira pasta de modelos configurada, grava somente via provider,
  invalida o cache do catálogo ao concluir e deixa o catálogo local descobrir o
  GGUF. Nenhum arquivo baixado é executado.

## Decisões de fronteira

O download é iniciado por uma requisição JSON estruturada com `repo_id`,
`revision`, `filename` e `quantization` opcional. O destino é decidido no
backend a partir da configuração de pastas do POM, para que o navegador não
possa escolher caminhos arbitrários. A quantização é apenas metadado de
apresentação e de deduplicação; a fonte de verdade do arquivo é o provider.
