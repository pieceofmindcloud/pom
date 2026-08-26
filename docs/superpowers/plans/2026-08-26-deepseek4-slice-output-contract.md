# Contrato de saída para fatiamento DeepSeek4 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:verification-before-completion before claiming completion.

**Goal:** impedir que o fatiador escreva fatias que o carregador llama.cpp não aceita por faltar a cabeça específica da arquitetura, e manter uma fatia final DeepSeek4 estruturalmente completa quando os tensores existirem na origem.

**Architecture:** o contrato de tensores não-bloco obrigatórios de `deepseek4` fica explícito no fatiador. A validação acontece antes de criar o arquivo de destino: fatias não finais são recusadas porque o loader exige `output.weight`, `output_norm.weight`, `output_hc_fn.weight`, `output_hc_base.weight` e `output_hc_scale.weight` sem fallback ou flag opcional; a fatia final copia esses tensores. O planejador e a execução local usam o mesmo preflight, e o cálculo de tamanho usa a mesma seleção.

**Tech Stack:** Rust, GGUF, llama.cpp, Tokio/Axum, testes unitários e documentação Markdown em pt-BR.

**Evidence:** `llama.cpp/src/models/deepseek4.cpp` cria os cinco tensores de saída com flags obrigatórias e usa a cabeça HC antes de `output_norm` e `output`; `gemma3.cpp` e `gemma4.cpp` marcam `output` como opcional e fazem fallback explícito para `token_embd`. `llama-model-loader.cpp` ignora metadados POM e recusa qualquer tensor obrigatório ausente.

## Reparo dos artefatos já gerados

Não há reparo seguro apenas por metadata: o loader não usa `pom.*` para tornar
opcionais os tensores obrigatórios. Também não há reparo barato por anexar os
tensores ao fim do arquivo: os descritores GGUF ficam antes do payload, então
adicionar descritores desloca a seção de dados e exige reescrever os cerca de
40 GB da fatia. `output.weight` ainda é uma projeção real de vocabulário e não
pode ser trocada por uma identidade sintética correta. As fatias inválidas
devem ser preservadas até a decisão de escopo e depois refeitas a partir da
origem se DeepSeek4 vier a ser suportado em pipeline.

## Invariantes

- A decisão é baseada em `general.architecture` e na tabela de tensores, nunca no nome do modelo.
- `deepseek4` não ganha tensores sintéticos para a cabeça: `output_hc_fn`, `output_hc_base`, `output_hc_scale` e `output.weight` têm semântica real e são grandes ou arquiteturalmente específicos.
- Uma faixa não final de `deepseek4` falha antes de abrir o arquivo de saída, com uma mensagem que lista os tensores obrigatórios e explica que o loader não conhece fatias parciais.
- Uma faixa final de `deepseek4` só é escrita se todos os tensores obrigatórios estiverem na origem; eles entram no arquivo e no orçamento previsto.
- A compilação Rust deve ser comparada à linha de base conhecida dos bindings `llama-cpp-sys-2`; nenhum erro preexistente é tratado como parte desta tarefa.
- Não editar `CHANGELOG.md` nem `apps/frontend/src/docs/`, que é gerado.

## Tasks

### Task 1: contrato e testes regressivos

- [x] Adicionar testes para recusar fatia intermediária DeepSeek4 sem criar destino.
- [x] Adicionar teste para copiar todos os cinco tensores na fatia final.
- [x] Adicionar teste para a mensagem de tensor obrigatório ausente na origem.
- [x] Rodar os testes focados; a compilação foi impedida pelos erros preexistentes dos bindings `llama-cpp-sys-2`.

### Task 2: implementar seleção e preflight

- [x] Implementar a lista de saída obrigatória e a validação estrutural no `splitter`.
- [x] Fazer `want_tensor` incluir a cabeça completa apenas no final e manter a recusa para fatias intermediárias.
- [x] Espelhar a seleção no `gguf::read_layout` e em `slice_bytes`.
- [x] Fazer o planner e os endpoints de prévia e execução recusarem antes de iniciar o corte.
- [x] Ajustar o merge e os mapas de classificação para não descartar tensores HC.

### Task 3: documentação e entrega

- [x] Atualizar os mapas de `crates/node/src/models/` e `crates/node/src/backend/`, além de `docs/subsystems/fatiamento.md`, `implantacao.md` e `web-ui.md`.
- [x] Rodar formatação, testes focados, build e `git diff --check`, comparando os erros com a linha de base.
- [x] Commitar somente os arquivos nomeados, rebater em `origin/feat/implantar`, empurrar sem force e registrar o SHA.
