# Capacidades reais nas configurações de implantação - Implementation Plan

> **For agentic workers:** use the execution workflow from
> `superpowers:executing-plans` and verify each checkpoint before continuing.

**Goal:** Expor na tela de implantação somente opções cuja ausência no modelo
foi comprovada, separando isso das limitações de runtime e mantendo
desconhecidas visíveis, acionáveis e explicadas.

**Architecture:** O leitor GGUF produz evidências de template, vocabulário,
camadas de previsão e tensores. O catálogo transporta essa evidência para o
survey de implantação. O planner mantém a fonte do modelo selecionado separada
das capacidades runtime dos peers e da topologia. Limitação runtime vira
`warning`, nunca ausência do modelo. O web UI valida novamente antes do launch e
retorna o diagnóstico no plano. A UI não infere capacidades.

**Tech Stack:** Rust, Axum, serde, GGUF header/tensor reader, React, TypeScript,
Vitest, cargo test e Vite.

**Spec:** `docs/superpowers/specs/2026-08-25-deployment-capabilities-design.md`

**Global Constraints:** Não usar nome de arquivo, allowlist de modelos ou
heurística de arquitetura como prova. Não alterar regra de negócio fora da
validação aprovada. Não tocar cluster, deploy real ou `data/ksia-target`. Não
publicar.

---

## 1. Evidência GGUF e contrato do catálogo

- [ ] Escrever testes vermelhos em `crates/node/src/models/gguf.rs` para template
  e vocabulário de thinking, `nextn` metadata/tensores e ausência/ambiguidade.
- [ ] Adicionar ao resultado GGUF apenas evidências derivadas dos KV e da tabela
  de tensores, preservando `unknown` quando a leitura não prova o caso.
- [ ] Escrever teste vermelho do JSON de `DiscoveredModel` contendo as
  evidências para o survey remoto.
- [ ] Propagar os campos em `catalog.rs` e cobrir serialização compatível.

## 2. Capacidades tri-state no planner

- [ ] Escrever testes vermelhos para agregação de capacidades em modelo
  suportado, não suportado e indeterminado.
- [ ] Transportar capacidades runtime de `/api/ui/node` para `Machine` e usar
  as capacidades dos peers efetivamente selecionados.
- [ ] Implementar a estrutura serializável tri-state com `source` e `reason`.
- [ ] Calcular thinking, especulativa, MTP, fit memory e embeddings depois que
  o planner escolher os alvos, sem allowlist ou nome de arquivo.
- [ ] Ocultar somente ausência comprovada na fonte correta; manter runtime
  limitado e `unknown` acionáveis com aviso visível.

## 3. `no_embeddings` seguro por peer

- [ ] Escrever testes vermelhos para peer único seguro, intermediário que
  exporta residual e topologia indeterminada.
- [ ] Implementar validação pura da topologia que distingue seguro, inseguro e
  desconhecido, nomeando node/range no diagnóstico.
- [ ] Aplicar decisão por alvo no launch e rejeitar a requisição inteira antes
  de parar processos quando qualquer alvo solicitado não é seguro.
- [ ] Revalidar o contrato em `ApplyRequest` para impedir chamadas diretas de
  contornar a validação do planner.
- [ ] Preservar o comportamento automático: sem override, somente o peer final
  desliga embeddings.

## 4. Frontend

- [ ] Adicionar os tipos de capacidades ao contrato `DeployPlan`, incluindo
  aviso separado para limitações de runtime.
- [ ] Escrever testes de componente para suportado, não suportado e
  indeterminado, verificando ocultação, switch visível acionável e alerta.
- [ ] Integrar os gates em `DeployModel.tsx` e no formulário compartilhado de
  parâmetros, mantendo `no_embeddings` visível e exibindo bloqueios por peer.
- [ ] Manter subopções escondidas quando o controle pai é comprovadamente
  incompatível e permitir envio de `unknown` com o alerta de risco.

## 5. Documentação e verificação

- [ ] Atualizar os `AGENTS.md` e os READMEs de subsistema das pastas tocadas.
- [ ] Rodar testes Rust direcionados e suite relevante.
- [ ] Rodar `npm test` e `npm run build` em `apps/frontend`.
- [ ] Reproduzir a tela no navegador em `http://localhost:5299` e conferir o
  alerta de capacidade, sem tocar em cluster.
- [ ] Inspecionar diff, manter o arquivo temporário existente intacto, commitar
  localmente em `fm/rdma-frontend-lab` e registrar `done` no status.
