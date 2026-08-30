# Precisão configurável do estado oculto Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tornar a precisão do estado oculto por fronteira de peer configurável, negociada e mensurável, mantendo f16 como padrão.

**Architecture:** `WireDtype::F8` implementa FP8 E4M3FN no protocolo batch existente e reconverte para f32 antes do motor. `RuntimeParams.hidden_state_dtype` percorre o mesmo caminho de implantação dos demais parâmetros globais; o planejador valida a capacidade f8 já anunciada no vetor `inference_frame_capabilities` antes de qualquer envio. `RequestTrace` mede bytes de tensor por hop e o frontend expõe o ajuste e a medição.

**Tech Stack:** Rust, Tokio, Axum, serde, protocolo `frame/v1-batch`, React, TypeScript, Vitest, documentação Markdown em pt-BR.

**Spec:** `docs/superpowers/specs/2026-08-26-precisao-estado-oculto-design.md`

## Global Constraints

- O default permanece `f16`; nenhuma implantação sem configuração nova muda de comportamento.
- Apenas `f16` e `f8` aparecem como escolhas na interface; `f8` só é aceito quando todos os hops o anunciam.
- O f8 é FP8 E4M3FN, um byte por valor, com conversão para f32 antes da inferência e recusa explícita de valores não representáveis.
- A negociação usa `inference_frame_capabilities` e `inference/frame-v1-dtype-f8`; não criar mecanismo paralelo.
- A telemetria separa `hidden_state_bytes_sent`, `hidden_state_bytes_received` e `hidden_state_dtype` do tamanho total do frame.
- Ajustes Linux de TCP são documentação opcional para operador, exigem administrador e não são dependência do POM.
- Não editar `CHANGELOG.md` nem `apps/frontend/src/docs/`, que é gerado.
- Todo texto novo fica em pt-BR e usa hífen simples, nunca travessão.
- Validar frontend com `rtk npm test` e `rtk npm run build`; registrar que Rust continua bloqueado pelo erro basal de `llama-cpp-sys-2` se o vendor permanecer ausente.

---

### Task 1: Conversão FP8 e contrato de wire

**Files:**
- Modify: `crates/node/src/inference/wire.rs`
- Test: `crates/node/src/inference/wire.rs` unit tests

**Interfaces:**
- Produces `WireDtype::{F32,F16,F8}`, `FrameEncoding::F8`, `WireDtype::frame_encoding()`, `WireDtype::parse("f8")`, `tensor_payload_len` for f8, and f8 round-trip errors.

- [ ] **Step 1: Write the failing tests**

Add tests named `f8_round_trip_stays_within_activation_tolerance`,
`f8_uses_one_byte_per_value`, `f8_rejects_non_finite_or_out_of_range_values`,
and `f8_decode_rejects_reserved_encoding`. They must assert the decoded
metadata says `f8`, the shape survives, the tensor is finite, the payload
shrinks by one byte per value relative to f16, and invalid values return an
error containing `f8`.

- [ ] **Step 2: Run the tests to verify the feature failure**

Run `rtk cargo test -p node wire::tests::f8_round_trip_stays_within_activation_tolerance`.
Expected: the command is unable to reach the test because the known baseline
build script reports the missing `vendor/llama-cpp-sys-2/llama.cpp`; once that
environmental failure is bypassed, the new symbol/variant assertions must be
the first feature failure.

- [ ] **Step 3: Implement the minimal wire support**

Derive serde for `WireDtype`, add f8 parsing and byte width, map it to
`FrameEncoding`, reuse `encode_tensor_payload` from the single-item encoder,
and implement finite FP8 E4M3FN conversion. Reject non-finite input, values
above 448, and reserved exponent-15/mantissa-7 on decode. Decode all accepted
wire types to f32.

- [ ] **Step 4: Run the focused wire tests**

Run `rtk cargo test -p node wire::tests::f8_` and then
`rtk cargo test -p node wire::tests::`. Expected: no new failure beyond the
pre-existing missing llama.cpp vendor if the workspace cannot compile.

### Task 2: Negotiation and propagation through inference

**Files:**
- Modify: `crates/node/src/routing/pipeline.rs`
- Modify: `crates/node/src/inference/prefill.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/relay.rs`
- Modify: `crates/node/src/inference/direct.rs`
- Test: `crates/node/src/routing/pipeline.rs` unit tests

**Interfaces:**
- Produces `CAP_FRAME_DTYPE_F8`, `PipelinePlan::validate_wire_dtype(WireDtype)`,
dtype-preserving relay calls, and `SentFrame` hidden-state byte fields.

- [ ] **Step 1: Write incompatible negotiation tests**

Add `f8_is_refused_when_any_pipeline_step_lacks_the_capability` and
`f8_is_accepted_when_every_pipeline_step_advertises_it`. Assert the refusal
names the missing peer and f8, and that f16 remains accepted with an empty
legacy capability vector.

- [ ] **Step 2: Run the negotiation tests and observe the failure**

Run `rtk cargo test -p node pipeline::tests::f8_is_refused_when_any_pipeline_step_lacks_the_capability`.
Expected: the same known Rust vendor failure precedes compilation; the test
must not be made vacuous by weakening its assertions.

- [ ] **Step 3: Implement preflight and dtype-preserving forwarding**

Require the f8 capability in the plan before `prefill::run` or `prefill::step`
can send a frame. Replace hardcoded sender/relay f16 encodings with the
selected dtype. Parse the dtype from received batch metadata in both prefill
and step handlers, and use it for every outgoing residual and final response.
Keep control-only acknowledgements f16. A missing or unknown dtype returns a
visible protocol error.

- [ ] **Step 4: Add transport telemetry data**

Record outbound and inbound tensor payload lengths using the selected dtype in
`SentFrame`, direct hop entries, and `RequestTrace::record_hop_from_json`.
Propagate the observed dtype into each `PhaseTrace`, preserving `None` if a
phase genuinely contains mixed formats.

- [ ] **Step 5: Run inference and negotiation tests**

Run `rtk cargo test -p node pipeline::tests::` and
`rtk cargo test -p node relay::tests::`; compare the output with the baseline
and confirm no new compile/test error is introduced.

### Task 3: Deployment configuration and capability blocking

**Files:**
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/backend/capabilities.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Test: `crates/node/src/backend/webui.rs` unit tests
- Test: `crates/node/src/backend/deploy.rs` unit tests

**Interfaces:**
- Produces `RuntimeParams.hidden_state_dtype`, `NodeConfig.hidden_state_dtype`,
`DeploymentCapabilities.hidden_state_f8`, and f8 plan/deploy refusal before
peer shutdown or frame transmission.

- [ ] **Step 1: Write persistence and capability tests**

Extend runtime parameter tests to assert `{}` defaults to f16 and explicit
`{"hidden_state_dtype":"f8"}` round-trips. Add a deployment capability test
that marks `hidden_state_f8` unsupported and asserts the capability block
explains that a selected peer does not announce f8.

- [ ] **Step 2: Run the focused tests and observe the baseline failure**

Run `rtk cargo test -p node webui::tests::runtime_params` and
`rtk cargo test -p node deploy::tests::hidden_state_f8`. The repository's
known missing llama.cpp vendor failure may happen before both test filters;
retain the tests and their exact assertions.

- [ ] **Step 3: Add the field to the existing deployment path**

Add serde-defaulted f16 fields to `RuntimeParams` and `NodeConfig`, copy them
in both conversion directions, and let existing apply/launch/redeploy payloads
carry the field without a parallel endpoint or persistence mechanism.

- [ ] **Step 4: Advertise and block using existing capabilities**

Add `inference/frame-v1-dtype-f8` to `/api/ui/node`, health, network announce,
and local peer capability lists. Compute `hidden_state_f8` for the selected
deployment and reject f8 when support is unsupported or unknown. Ensure
redeploy performs the same check before stopping a running peer.

- [ ] **Step 5: Run configuration tests**

Run `rtk cargo test -p node webui::tests::` and
`rtk cargo test -p node deploy::tests::`; compare failures to the baseline.

### Task 4: Frontend control and telemetry

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/chatStream.ts`
- Modify: `apps/frontend/src/components/RuntimeParamsForm.tsx`
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/components/RuntimeParamsForm.capabilities.test.tsx`

**Interfaces:**
- Produces a `hidden_state_dtype` deployment control with f16 default and f8
capability feedback, and displays hidden-state bytes plus effective dtype in
the execution telemetry table.

- [ ] **Step 1: Write frontend tests**

Extend the shared form tests to render f16 by default, select f8 when the
deployment capability is supported, and disable f8 with an explanatory
message when capability is unsupported. Add a telemetry fixture with f8 and
assert the bytes/format label is rendered.

- [ ] **Step 2: Run frontend tests to verify the missing behavior**

Run `rtk npm test -- src/components/RuntimeParamsForm.capabilities.test.tsx`.
Expected: the new queries fail before the form and telemetry changes.

- [ ] **Step 3: Implement types and shared controls**

Add `WireDtype`, optional legacy-safe runtime fields, capability metadata, and
the shared select beside the existing context/batch/chunk/cache controls. Use
f16 when an old saved object has no field. Keep f8 visible only for a supported
plan, or disabled with the server negotiation reason.

- [ ] **Step 4: Render telemetry**

Add hidden-state fields to `PhaseTrace` and render the bytes sent per traversal
and effective dtype in each phase row, while treating missing fields as zero
and f16 for old local traces.

- [ ] **Step 5: Run the complete frontend validation**

Run `rtk npm test` and `rtk npm run build`. Fix all test, typecheck, lint, and
build failures before proceeding.

### Task 5: Product documentation and folder maps

**Files:**
- Modify: `docs/subsystems/inferencia.md`
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/operacao.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `crates/node/src/routing/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`

- [ ] **Step 1: Document the implementation**

Describe f16/f8, default behavior, negotiation refusal, telemetry fields, and
the shared deployment setting in the relevant subsystem docs. In operation
docs add the optional Linux commands
`net.ipv4.tcp_congestion_control=bbr` and
`net.ipv4.tcp_slow_start_after_idle=0`, explain bursty prefill, administrator
privileges, and that POM does not depend on them. Record that the observed
1.3-to-4.1 MB/s result on a 214 KB file was download-only and cannot isolate a
factor or promise a result.

- [ ] **Step 2: Update folder maps**

Add concise pointers to the touched source responsibilities in every touched
folder's `AGENTS.md`, preserving its `## Maintaining this file` section.

- [ ] **Step 3: Check generated and prohibited files**

Run `rtk git status --short` and confirm no `CHANGELOG.md` or
`apps/frontend/src/docs/` file is modified.

### Task 6: Final verification and shared-branch delivery

**Files:**
- Modify: only the explicitly listed files above

- [ ] **Step 1: Re-run the baseline Rust command**

Run `rtk cargo build -p node` and record that the error remains the missing
`vendor/llama-cpp-sys-2/llama.cpp`, with no new project-level Rust error.

- [ ] **Step 2: Re-run frontend validation**

Run `rtk npm test` and `rtk npm run build` from `apps/frontend`.

- [ ] **Step 3: Inspect the diff**

Run `rtk git diff --check`, `rtk git status --short`, and targeted `rtk git diff`
for each changed source/doc file. Confirm no em dash, generated docs, or
unrelated worker changes are included.

- [ ] **Step 4: Commit named files only**

Run `rtk git add <each changed file listed by git status>` with every path named
explicitly, then `rtk git commit -m "feat: tornar precisão do estado oculto configurável"`.

- [ ] **Step 5: Rebase and push the shared branch**

Run `rtk git pull --rebase origin feat/implantar`, resolve only trivial changes,
then `rtk git push origin feat/implantar`. Never use force options.
