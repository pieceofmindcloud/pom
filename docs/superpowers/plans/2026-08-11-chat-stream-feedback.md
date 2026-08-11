# Chat Stream Feedback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mostrar velocidade final de geração, pensamento separado e preservar a rolagem manual do histórico durante streaming.

**Architecture:** O backend anexa uma extensão `x_rdma_p2p` somente ao chunk SSE final e mantém os chunks de conteúdo no formato OpenAI. O frontend acumula conteúdo, extrai blocos `<think>` incrementalmente para `reasoning` e usa um controlador de acompanhamento do fim independente da atualização de mensagens.

**Tech Stack:** Rust, Axum SSE, React, TypeScript, Vitest.

## Global Constraints

- Não alterar o caminho de inferência ou o algoritmo de roteamento.
- Preservar `choices[0].delta.content` para clientes compatíveis com OpenAI.
- Mostrar métricas somente quando o chunk final trouxer `x_rdma_p2p`.
- Não escrever em CHANGELOG.md.

---

### Task 1: Métricas finais no contrato SSE

**Files:**
- Modify: `crates/node/src/backend/openai.rs`
- Test: `crates/node/src/backend/openai.rs`

**Interfaces:**
- Consumes: `GenEvent::Done(GenSummary)`.
- Produces: último chunk SSE com `x_rdma_p2p.tokens_per_sec`, `tokens_generated`, `duration_ms`, `source` e `partial`.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn final_stream_chunk_carries_generation_summary() {
    let payload = final_chunk_for_test(GenSummary {
        source: "pipeline:a→b".into(),
        tokens_generated: 12,
        tokens_per_sec: 4.5,
        duration_ms: 2666,
        partial: false,
    });
    assert_eq!(payload["x_rdma_p2p"]["tokens_per_sec"], 4.5);
    assert_eq!(payload["x_rdma_p2p"]["tokens_generated"], 12);
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node final_stream_chunk_carries_generation_summary`

Expected: FAIL because the final SSE chunk has no `x_rdma_p2p` object.

- [x] **Step 3: Write minimal implementation**

Extend `chunk_json` with an optional `summary: Option<&GenSummary>`, serialize
`x_rdma_p2p` only when `summary` is present, and pass the summary from the
`GenEvent::Done` branch.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node final_stream_chunk_carries_generation_summary`

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/backend/openai.rs
git commit -m "feat: include generation metrics in chat stream"
```

### Task 2: Estado de mensagem e separação incremental de pensamento

**Files:**
- Create: `apps/frontend/src/lib/chatStream.ts`
- Create: `apps/frontend/src/lib/chatStream.test.ts`
- Modify: `apps/frontend/src/lib/chatStore.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`

**Interfaces:**
- Produces: `consumeChatDelta(state, chunk): ChatStreamState`, com `content`,
  `reasoning`, `tokensPerSecond`, `tokensGenerated` e `durationMs`.
- Consumes: chunks OpenAI com `choices[0].delta.content` e o resumo
  opcional `x_rdma_p2p`.

- [x] **Step 1: Write the failing test**

```ts
it("separates a think block split across streaming chunks", () => {
  let state = emptyChatStream();
  state = consumeChatDelta(state, "<think>cal");
  state = consumeChatDelta(state, "culo</think>Resposta");
  expect(state.reasoning).toBe("calculo");
  expect(state.content).toBe("Resposta");
});
```

- [x] **Step 2: Run test to verify it fails**

Run: `npm --prefix apps/frontend run test -- --run src/lib/chatStream.test.ts`

Expected: FAIL because `chatStream.ts` does not exist.

- [x] **Step 3: Write minimal implementation**

Implement an incremental parser that retains incomplete delimiters, writes
thinking text to `reasoning`, writes ordinary text to `content`, and stores the
final metrics from `x_rdma_p2p`. Extend `ChatMessage` with optional persisted
fields and render reasoning in a `details` element plus a final tok/s footer.

- [x] **Step 4: Run test to verify it passes**

Run: `npm --prefix apps/frontend run test -- --run src/lib/chatStream.test.ts`

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add apps/frontend/src/lib/chatStream.ts apps/frontend/src/lib/chatStream.test.ts apps/frontend/src/lib/chatStore.ts apps/frontend/src/routes/Chat.tsx
git commit -m "feat: show chat reasoning and generation speed"
```

### Task 3: Acompanhamento de rolagem respeitando leitura manual

**Files:**
- Create: `apps/frontend/src/lib/chatScroll.ts`
- Create: `apps/frontend/src/lib/chatScroll.test.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`

**Interfaces:**
- Produces: `isNearBottom({ scrollTop, clientHeight, scrollHeight }, threshold)`.
- Consumes: o elemento rolável da conversa e uma margem de 32 px.

- [x] **Step 1: Write the failing test**

```ts
it("does not follow a stream when the reader is above the bottom", () => {
  expect(isNearBottom({ scrollTop: 40, clientHeight: 300, scrollHeight: 800 }, 32)).toBe(false);
});
```

- [x] **Step 2: Run test to verify it fails**

Run: `npm --prefix apps/frontend run test -- --run src/lib/chatScroll.test.ts`

Expected: FAIL because `chatScroll.ts` does not exist.

- [x] **Step 3: Write minimal implementation**

Replace global `bottom.scrollIntoView` with a ref to the scroll container. Keep
an `autoFollowRef` updated on `onScroll`; after a token render, set
`scrollTop = scrollHeight` only when that ref is true. Set it true immediately
when the user submits a new prompt.

- [x] **Step 4: Run test to verify it passes**

Run: `npm --prefix apps/frontend run test -- --run src/lib/chatScroll.test.ts`

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add apps/frontend/src/lib/chatScroll.ts apps/frontend/src/lib/chatScroll.test.ts apps/frontend/src/routes/Chat.tsx
git commit -m "fix: preserve chat scroll while streaming"
```

### Task 4: Integração e verificação

**Files:**
- Modify: `docs/superpowers/plans/2026-08-11-chat-stream-feedback.md`

- [x] **Step 1: Run focused verification**

Run: `cargo test -p node final_stream_chunk_carries_generation_summary && npm --prefix apps/frontend run test -- --run src/lib/chatStream.test.ts src/lib/chatScroll.test.ts && npm --prefix apps/frontend run build`

Expected: PASS.

- [x] **Step 2: Run release verification**

Run: `cargo build --release --bin rdma-p2p`

Expected: PASS.

- [x] **Step 3: Manual browser verification**

The local UI loaded at `http://127.0.0.1:8099/chat`. Its isolated node has no
model assigned, so a live inference response could not be produced there; the
stream parser, metrics contract, and scroll policy are covered by focused tests.

- [x] **Step 4: Commit the completed plan marker**

```bash
git add docs/superpowers/plans/2026-08-11-chat-stream-feedback.md
git commit -m "docs: record chat stream verification"
```
