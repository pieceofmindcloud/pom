# Seletor de motor de inferência - design

## Objetivo

Permitir que o operador escolha o motor de inferência de uma implantação MoE
na tela de implantação, levando a escolha pelos três formatos de execução:
modelo inteiro, pipeline de fatias e Tensor Parallelism. A tela só mostra o
seletor quando o GGUF real declara metadata de experts; nunca usa o nome do
arquivo para classificar o modelo.

## Decisões

- O campo `inference_mode` aceita `auto`, `dense` e `moe`.
- `auto` é o padrão e preserva a classificação automática existente.
- `dense` é apresentado como llama.cpp.
- `moe` é o executor MoE próprio.
- A escolha é um parâmetro global de uma implantação e é copiada para cada
  `RuntimeParams` de nó, inclusive nos alvos de Tensor Parallelism.
- Tensor Parallelism com modelo MoE permanece explicitamente não suportado pelo
  contrato atual de fatiamento. O plano informa a razão e a tela não apresenta
  esse modo como executável; o seletor continua visível para não mascarar a
  limitação.
- `selected_path` e `executed_engine` são fatos diferentes. A UI usa os campos
  de telemetria existentes e exibe divergência quando eles não coincidem. O
  código desta entrega não altera `executor.rs`, `InferencePath` ou o cálculo
  de `engine_mismatch` de `feat/ftk@55f8e89`.

## Contrato

O catálogo e o plano expõem `is_moe`, calculado pelo cabeçalho GGUF a partir de
`expert_count > 1`. O `DeployPlan` também expõe as capacidades do modo de
execução solicitado, para que a interface não tenha de inferir suporte a
partir de arquitetura ou nome.

`RuntimeParams.inference_mode` é serializado no pedido de plano, no pedido de
deploy e no registro persistido da implantação. Campo ausente em registros
anteriores lê como `auto`.

## Fluxo

1. O plano consulta o catálogo real e determina `is_moe`.
2. Para modelo denso, `inference_mode` continua em `auto` e o controle não é
   renderizado.
3. Para modelo MoE, o controle aparece no inspector global. Alterar o valor
   invalida o plano e o mesmo valor é enviado em `deploy`, `launch` e nos
   registros de cada nó.
4. O backend preserva o valor na configuração que o executor recebe. Quando o
   caminho selecionado e o motor executado divergirem, a telemetria e o aviso
   da UI continuam mostrando a divergência, independentemente do valor do
   seletor.

## Testes

- GGUF e catálogo: `expert_count` real classifica MoE e arquitetura sem experts
  continua densa.
- Plano: whole, shards e tensor parallelism carregam o mesmo modo; TP MoE
  retorna suporte explícito como indisponível.
- UI: o seletor aparece somente para planos MoE, permanece oculto para dense,
  e mostra o aviso de TP sem fingir que a escolha será aplicada.
- UI de telemetria: caminho MoE mais motor llama.cpp mostra divergência mesmo
  com o seletor MoE.
