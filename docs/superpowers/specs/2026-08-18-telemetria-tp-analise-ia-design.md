# Telemetria de TP, análise automática e identificação do modelo

Data: 2026-08-18

## Objetivo

Tornar uma execução distribuída explicável quando estiver usando pipeline,
tensor parallelism (TP) ou os dois ao mesmo tempo. A interface deve mostrar
onde o tempo foi gasto, permitir que o usuário envie a telemetria para o
modelo de chat analisar e registrar qual modelo estava selecionado antes da
primeira mensagem da conversa.

## Contexto observado

No teste entre Salem e Ksia, o hop `L0-L31` levou 25,1 s em 14 chamadas de
decode. O campo atual `backend: enfileirar` inclui as esperas síncronas das
reduções TP, mas não informa quantas reduções ocorreram, quanto cada uma
esperou, quais bytes foram trocados ou se a conexão direta caiu para o túnel.
Assim, a tabela atual atribui a espera da rede ao backend e oculta o custo
real do TP.

O sistema não possui um endpoint separado de agente de análise. O botão usará
o mesmo modelo atualmente selecionado na conversa, por meio do fluxo já
existente de `/v1/chat/completions`, com um prompt estruturado contendo as duas
tabelas de telemetria.

## Escopo

- Adicionar telemetria opcional e retrocompatível para coletivas TP.
- Preservar o significado e a renderização do caminho de pipeline.
- Exibir uma leitura automática da execução, funcionando com pipeline, TP ou
  ambos.
- Adicionar o botão `Analisar com IA` na tabela de telemetria.
- Enviar ao modelo as duas tabelas: a tabela de fases/hops e a tabela de
  coletivas TP, sem enviar prompt, estado oculto ou pesos.
- Mostrar o nome do modelo usado no início da conversa, antes da primeira
  mensagem do usuário.
- Adicionar testes unitários e de interface para os novos caminhos.
- Atualizar a documentação de inferência e dos módulos alterados.

Não fazem parte desta mudança: persistência externa de métricas, um novo
serviço de observabilidade, alteração do roteador, mudança do protocolo de
pesos ou criação de um agente separado do modelo de chat.

## Contrato de telemetria

Os campos existentes de `RequestTrace`, fases e hops permanecem válidos. O
campo `node_decode_ms` continua sendo o tempo total observado no decode. O
campo atualmente chamado `backend_enqueue_ms` será apresentado com texto que
deixe claro que ele inclui a chamada de decode e suas esperas internas; não
será tratado como tempo puro de kernel.

Cada fase pode receber uma seção opcional `tp`. Ela não aparece ou fica nula
quando a fase não usa TP:

```json
{
  "enabled": true,
  "rank": 0,
  "size": 2,
  "expected_reduces": 64,
  "reduces": 64,
  "wait_ms": 25100,
  "reduce_p50_ms": 27.9,
  "reduce_p95_ms": 31.4,
  "request_bytes": 1048576,
  "response_bytes": 1048576,
  "direct_calls": 63,
  "tunnel_calls": 1,
  "fallback_calls": 1,
  "broadcast_calls": 14,
  "broadcast_bytes": 229376
}
```

Os nomes finais podem ser ajustados durante a implementação para seguir os
tipos existentes, mas devem manter estas propriedades:

- contagem esperada e efetiva de reduções;
- espera TP total e percentis das reduções;
- bytes de requisição e resposta das coletivas;
- chamadas diretas, chamadas pelo túnel e fallbacks;
- broadcasts separados das reduções;
- identificação de rank e tamanho da coletiva.

O transporte deve informar a origem da resposta (`direct` ou `tunnel`) e se
houve fallback, sem alterar o corpo da resposta nem quebrar os chamadores
existentes. A telemetria de pipeline continua contabilizando apenas os frames
do pipeline. Os novos bytes TP ficam em campos próprios e não são somados
silenciosamente aos bytes antigos.

No modo pipeline sem TP, a tabela existente continua mostrando hops e fases
com os mesmos campos e valores. No modo TP, o hop de pipeline continua
aparecendo, acompanhado da tabela TP. Em um cenário híbrido, as duas tabelas
mostram seus custos separadamente.

## Leitura automática da execução

Será criada uma função pura no frontend que recebe o trace e retorna uma
leitura estruturada, sem fazer requisição e sem decidir rota. A leitura terá
severidade, título, explicação, evidências numéricas e próximo passo sugerido.

A classificação segue esta ordem de evidência:

1. espera TP dominante, quando `wait_ms` representa a maior parte do decode ou
   os percentis de redução são altos;
2. transporte de pipeline dominante, quando residual ou transporte consome a
   maior parte do tempo fora do backend;
3. fila/concorrência do motor, quando a espera do mutex excede o trabalho de
   decode;
4. backend dominante, quando o tempo de decode permanece alto sem espera de
   rede ou fila relevante;
5. execução sem gargalo identificado, quando os dados são pequenos,
   incompletos ou equilibrados.

A função deve distinguir fato medido de interpretação. Por exemplo, deve
informar que `wait_ms / reduces` é uma média aproximada e não latência pura de
pacote. Quando o trace não contém TP, não deve inferir que TP está desligado
globalmente, apenas que não foi registrado naquela fase.

Na interface, a leitura aparece antes das tabelas em um bloco compacto com
status visual, evidência e ação recomendada. A tabela detalhada permanece
disponível para conferência.

## Análise com IA

Cada bloco de telemetria terá um botão `Analisar com IA`. Ele ficará
desabilitado enquanto a resposta estiver sendo transmitida ou quando não
houver modelo selecionado.

Ao clicar:

1. o frontend serializa a tabela de fases/hops completa;
2. serializa a tabela TP completa, incluindo a indicação explícita de que está
   ausente quando a execução não registrou TP;
3. cria uma mensagem de usuário visível e curta na conversa;
4. envia ao modelo atual um conteúdo de requisição separado, contendo as duas
   tabelas, instruções para responder em português quando a interface estiver
   em pt-BR, citar os números observados e separar fatos de hipóteses;
5. mostra a resposta como uma resposta normal do assistente, com streaming e
   métricas habituais.

O conteúdo estruturado da requisição não será duplicado na bolha visual do
   usuário. A mensagem persistida terá um campo interno para o conteúdo de
   transporte, preservando a conversa e evitando uma bolha com JSON extenso.
O prompt de análise não incluirá prompt original, tokens ocultos, pesos,
segredos ou chaves.

## Modelo no início da conversa

`ChatSession` receberá `modelAtStart`, preenchido com o modelo selecionado no
momento em que a sessão é criada ou na primeira requisição, caso a sessão
antiga ainda não tenha esse campo.

Antes da primeira mensagem visual da conversa, o Chat renderizará uma marca de
evento informando o modelo usado no início. Essa marca é somente de interface
e não entra em `turnsOnly`, portanto nunca é enviada ao modelo como histórico.

Sessões antigas serão compatíveis: se `modelAtStart` não existir, o frontend
tentará usar o modelo salvo na sessão ou o modelo do primeiro trace disponível.
Trocas de modelo no meio de uma conversa continuarão gerando os eventos
existentes de alteração de modelo.

## Compatibilidade e internacionalização

Todos os campos novos de Rust serão opcionais com valores padrão. O frontend
aceitará traces antigos sem seção TP. Os textos do analisador, botão, marca de
modelo e mensagens de ausência serão adicionados em inglês e pt-BR.

## Verificação

- Testes Rust cobrirão a coleta de contagem, duração, bytes e transporte das
  operações TP sem alterar o fluxo pipeline.
- Testes frontend cobrirão a leitura automática para pipeline, TP, híbrido,
  fila e dados insuficientes.
- Testes frontend verificarão que o botão produz uma solicitação contendo as
  duas tabelas e que fica desabilitado durante streaming ou sem modelo.
- Testes frontend verificarão a marca de modelo antes da primeira mensagem,
  a compatibilidade de sessões antigas e a exclusão da marca do histórico
  enviado.
- Serão executados `cargo test`, testes do frontend, `npm run build` no
  frontend e `cargo fmt --check`.

## Arquivos e documentação esperados

- Telemetria de inferência e transporte em `crates/node/src/inference/`.
- Tipos, análise e serialização do trace em `apps/frontend/src/lib/`.
- Tabela, botão, marca inicial e fluxo de análise em
  `apps/frontend/src/routes/Chat.tsx`.
- Traduções em `apps/frontend/src/i18n/locales.ts`.
- Testes próximos aos módulos alterados.
- Atualização dos `AGENTS.md` dos diretórios alterados e de
  `docs/subsystems/inferencia.md`, sem editar arquivos gerados.
