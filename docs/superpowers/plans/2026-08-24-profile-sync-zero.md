# Perfil de sincronização e precisão da telemetria - Plano de implementação

> **Para agentes:** use a sub-habilidade `superpowers:executing-plans` para executar este plano tarefa por tarefa. Os passos usam caixas de seleção para acompanhar o trabalho.

**Objetivo:** preservar na telemetria os intervalos sub-milisegundo medidos no nó para que uma conclusão real menor que 1 ms não vire zero no wire.

**Arquitetura:** os contadores de diagnóstico do nó continuam acumulando microssegundos internamente. A serialização para JSON passa a emitir milissegundos fracionários (`f64`) e o acumulador de `PhaseTrace` preserva esses valores ao atravessar o relay. Os cronômetros externos de transporte continuam com sua unidade inteira atual, e a publicação do estado explícito de `POM_PROFILE_SYNC` por peer fica fora deste ramo, conforme a tarefa `rdma-chat-telemetria`.

**Tecnologias:** Rust, `serde_json`, testes unitários do crate `node`, React/TypeScript apenas como consumidor do número já existente.

**Especificação:** `docs/superpowers/specs/2026-08-12-request-telemetry-design.md`

## Restrições globais

- Não alterar `apps/frontend/src/routes/Chat.tsx` nem duplicar a publicação do estado real de `POM_PROFILE_SYNC`.
- Manter os contadores internos em microssegundos e os nomes públicos atuais em milissegundos.
- Converter todos os campos de `NodeTimings`, não apenas `backend_completion_us`, porque a perda é sistemática dentro do bloco de diagnóstico do nó.
- Não reiniciar, publicar, fazer deploy ou escrever na ksia.
- Atualizar a documentação da pasta de inferência e o README de subsistema afetado no mesmo commit.

---

### Tarefa 1: Teste de regressão para serialização sub-milisegundo

**Arquivos:**
- Modificar: `crates/node/src/inference/infer_server.rs` no módulo `tests`.

**Interfaces:**
- Consome: `NodeTimings::json()`.
- Produz: uma garantia de que `999 us` chega ao consumidor como `0.999 ms`, em particular no campo `backend_completion_ms` que a tela usa para a decisão de rótulo.

- [x] **Passo 1: escrever o teste que deve falhar**

Adicionar ao módulo `tests`:

```rust
    #[test]
    fn serializacao_de_timing_preserva_submilissegundos() {
        let timings = NodeTimings {
            frame_decode_us: 999,
            backend_completion_us: 999,
            residual_us: 1_234,
            sampling_us: 1,
            ..Default::default()
        };

        let json = timings.json();

        assert_eq!(json["frame_decode_ms"].as_f64(), Some(0.999));
        assert_eq!(json["backend_completion_ms"].as_f64(), Some(0.999));
        assert_eq!(json["residual_ms"].as_f64(), Some(1.234));
        assert_eq!(json["sampling_ms"].as_f64(), Some(0.001));
        assert_ne!(json["backend_completion_ms"], serde_json::json!(0));
    }
```

- [x] **Passo 2: executar o teste para confirmar a falha**

Executar: `rtk cargo test -p node serializacao_de_timing_preserva_submilissegundos`

Resultado esperado antes da implementação: falha porque a divisão inteira serializa `999 us` como `0 ms`.

### Tarefa 2: Emitir milissegundos fracionários em todo o bloco de telemetria

**Arquivos:**
- Modificar: `crates/node/src/inference/infer_server.rs:87-112`.
- Modificar: `crates/node/src/inference/prefill.rs:77-230`.
- Atualizar: `crates/node/src/inference/llm.rs` no comentário do teste de
  perfil, para refletir que a flag fica ligada por padrão.
- Modificar: `crates/node/src/inference/AGENTS.md` na descrição de `NodeTimings`/`profile_backend_sync`.
- Modificar: `docs/subsystems/inferencia.md` na seção de telemetria do servidor de inferência.

**Interfaces:**
- Consome: os contadores `*_us` produzidos por `infer_server.rs` e `llm.rs`.
- Produz: os mesmos campos JSON de diagnóstico `*_ms`, agora como números em milissegundos com fração quando necessário, e `PhaseTrace` mantém a fração até o Chat.

- [x] **Passo 1: implementar a conversão mínima**

Trocar a conversão inteira por uma conversão fracionária usada por todos os campos:

```rust
        let ms = |us: u64| us as f64 / 1_000.0;
```

Atualizar os comentários para dizer que o wire usa milissegundos fracionários, que o acumulador não converte esses números de volta para inteiros, e que `backend_completion_ms == 0` não é, sozinho, um indicador do perfil.

- [x] **Passo 2: executar o teste para confirmar a passagem**

Executar: `rtk cargo test -p node serializacao_de_timing_preserva_submilissegundos`

Resultado esperado: PASS, com `backend_completion_ms` diferente de zero para `999 us`.

- [x] **Passo 3: atualizar a documentação factual**

Registrar em `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md` que os contadores são mantidos em microssegundos e os campos públicos `*_ms` preservam frações. Registrar também que o estado de `POM_PROFILE_SYNC` é uma informação separada, publicada por peer, e não deve ser deduzida de uma medição zero.

### Tarefa 3: Validação completa e entrega local

**Arquivos:**
- Verificar o diff de todos os arquivos alterados, sem modificar arquivos gerados.

- [x] **Passo 1: executar os testes Rust do crate**

Executar: `rtk cargo test -p node`

Resultado esperado: saída com código `0`; se a compilação nativa de `llama-cpp-sys-2` falhar, registrar a mensagem exata e o escopo não coberto.

- [x] **Passo 2: executar o build da interface**

Executar: `rtk npm run build` em `apps/frontend` para verificar que os campos numéricos fracionários continuam compatíveis com TypeScript e Vite.

- [x] **Passo 3: verificar estado e commit**

Executar `rtk git diff --check`, `rtk git status --short` e `rtk git diff --stat`, revisar que não há mudanças fora do escopo, e então:

```bash
rtk git add crates/node/src/inference/infer_server.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md docs/superpowers/plans/2026-08-24-profile-sync-zero.md
rtk git commit -m "fix: preserve sub-millisecond inference telemetry"
```

- [x] **Passo 4: confirmar ramo limpo e pronto**

Executar `rtk git status --short --branch` e confirmar o commit em `fm/rdma-profile-sync-zero` antes de escrever o status final.
