# Windows llama.cpp Autoclone Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o autoclone do `llama-cpp-sys-2` materializar o checkout em `vendor/llama-cpp-sys-2/llama.cpp` no Windows, evitando que o `WalkDir` abra o symlink versionado e falhe com `ERROR_ACCESS_DENIED`.

**Architecture:** O build continuará usando o clone raso do fork configurado por `POM_LLAMA_CPP_REPOSITORY` e `POM_LLAMA_CPP_REF`. Em hosts Windows, o destino de instalação será sempre o caminho do crate e o symlink ou arquivo-placeholder será removido antes do `rename` atômico; em hosts Unix, o destino relativo do symlink continuará preservado. O teste isolará a decisão de destino para garantir que uma mudança acidental volte a seguir o symlink no Windows.

**Tech Stack:** Rust build script, `std::fs`, `walkdir`, Cargo tests, GitHub Actions `windows-2022`.

**Spec:** `docs/subsystems/build-execucao.md`, seção `Release pelo GitHub Actions`, e `vendor/llama-cpp-sys-2/AGENTS.md`.

## Global Constraints

- O checkout do fork `llama.cpp` não pode ser alterado nem versionado neste repositório.
- O fork continua sendo obtido da branch `pom-tensor-parallel` por clone raso, salvo overrides já suportados pelas variáveis existentes.
- O `WalkDir` deve receber um diretório real e acessível em Windows.
- A documentação deve permanecer em pt-BR e usar hífen simples.
- O branch de entrega é `fm/rdma-windows-build-llamacpp-autoclone`; não abrir PR.

---

### Task 1: Fixar a regressão de destino no Windows

**Files:**
- Modify: `vendor/llama-cpp-sys-2/build.rs`, instalação do autoclone
- Create: `vendor/llama-cpp-sys-2/src/build_support.rs`, helper de destino e teste unitário
- Modify: `.github/workflows/release-windows.yml`, smoke test do helper no runner Windows

**Interfaces:**
- Consumes: `manifest_dir`, `source`, `symlink_target` e `cfg!(windows)` dentro de `ensure_llama_source`.
- Produces: `clone_destination(manifest_dir, source, symlink_target, is_windows) -> PathBuf` em `src/build_support.rs`, que retorna `source` quando `is_windows` é verdadeiro e preserva o destino do symlink em outros hosts.

- [x] **Step 1: Confirmar a reprodução e a hipótese**

  Evidência já coletada: a execução `34630179303` falhou no workflow `Release · Windows`, no `WalkDir` da linha 650, com `PermissionDenied` no caminho do symlink. A revisão anterior do workflow clonava diretamente sobre esse caminho e teve execução Windows bem-sucedida. A hipótese é que seguir o symlink versionado no Windows deixa o `WalkDir` dependente da representação de reparse point criada pelo checkout; a instalação local elimina essa dependência.

- [x] **Step 2: Escrever o teste falhando**

  Adicionar em `src/build_support.rs` testes que chamem `clone_destination` com `is_windows = true` e um symlink relativo `../../llama.cpp`, esperando o caminho do crate, e que verifiquem que um placeholder existente pode ser substituído somente quando o destino coincide com a entrada do crate no Windows:

  ```rust
  #[test]
  fn windows_installs_clone_at_source_instead_of_following_symlink() {
      let manifest_dir = Path::new(r"D:\a\rdma-p2p\rdma-p2p\vendor\llama-cpp-sys-2");
      let source = manifest_dir.join("llama.cpp");

      assert_eq!(
          clone_destination(manifest_dir, &source, Some(Path::new(r"..\..\llama.cpp")), true),
          source
      );
  }
  ```

- [x] **Step 3: Rodar o teste para confirmar RED**

  Run: `rtk rustc --test vendor/llama-cpp-sys-2/src/build_support.rs -o target/build-support-test`

  Resultado: falha de compilação por `no clone_destination in the root`, confirmando que o teste capturava o helper ausente. O teste isolado usa somente `std`, porque Cargo não executa automaticamente `#[cfg(test)]` de um `build.rs`.

- [x] **Step 4: Implementar o menor fix**

  Alterar `clone_destination` para receber `is_windows: bool`, retornar `source` nesse caso, e chamar a função com `cfg!(windows)` em `ensure_llama_source`. Usar `source_can_be_used_directly` para não reutilizar um symlink Windows válido como raiz do `WalkDir`, e `destination_can_be_replaced` para permitir a troca somente quando `destination == source` no Windows. Antes do `rename`, quando `destination == source`, remover com `symlink_metadata` o symlink ou arquivo-placeholder existente. Não alterar o fork nem adicionar retry ou sleep.

- [x] **Step 5: Rodar o teste para confirmar GREEN**

  Run: `rtk rustc --test vendor/llama-cpp-sys-2/src/build_support.rs -o target/build-support-test` e `rtk target/build-support-test`

  Resultado: PASS, com quatro casos cobrindo materialização Windows, placeholder Windows, symlink Windows já resolvido e preservação do destino Unix.

### Task 2: Documentar o comportamento Windows

**Files:**
- Modify: `vendor/llama-cpp-sys-2/AGENTS.md`, seção `Preparo automático do llama.cpp`
- Modify: `docs/subsystems/build-execucao.md`, seção `Release pelo GitHub Actions`

**Interfaces:**
- Consumes: comportamento implementado em `ensure_llama_source`.
- Produces: documentação verificável de que Windows materializa o clone no caminho do crate e Unix pode preservar o symlink.

- [x] **Step 1: Atualizar a documentação do crate**

  Explicar que o Windows não segue o symlink versionado durante o autoclone: o build remove o placeholder e instala um diretório real em `vendor/llama-cpp-sys-2/llama.cpp`; o fork permanece fora do histórico.

- [x] **Step 2: Atualizar o subsistema de build**

  Registrar a causa da falha no `WalkDir` e a regra de instalação específica do Windows, mantendo a instrução de `LLAMA_SOURCE` para checkouts locais.

### Task 3: Verificar, integrar e entregar

**Files:**
- Verify: `vendor/llama-cpp-sys-2/build.rs`, documentação e workflow Windows

**Interfaces:**
- Consumes: teste de regressão e regra documentada de materialização.
- Produces: branch limpa e commitável, com o workflow Windows validado quando a revisão estiver disponível no ref remoto.

- [x] **Step 1: Rodar testes e formatação**

  Run: `rtk rustfmt --check vendor/llama-cpp-sys-2/build.rs vendor/llama-cpp-sys-2/src/build_support.rs`

  Resultado: PASS. `rtk cargo fmt --all -- --check` também foi executado e encontrou somente divergências preexistentes em `crates/node/src/backend/gateway/tests.rs`, `crates/node/src/backend/reqlog.rs` e `crates/node/src/network/network.rs`, fora do escopo deste fix.

  O teste unitário do helper é executado diretamente porque Cargo não trata o build script vendorizado como pacote do workspace:

  ```bash
  rtk rustc --test vendor/llama-cpp-sys-2/src/build_support.rs -o target/build-support-test
  rtk target/build-support-test
  ```

  `rtk cargo check -p node --message-format=short` e `rtk cargo test --workspace --no-run --message-format=short` passaram. `rtk cargo test --workspace --quiet` executou 1.330 testes com 3 falhas nos testes de autenticação/UI de `crates/node`, fora dos arquivos deste fix; elas não foram incorporadas à alteração.

- [x] **Step 2: Verificar o diff e a árvore**

  Run: `rtk git diff --check`

  Run: `rtk git status --short`

- [x] **Step 3: Atualizar a base antes da entrega**

  Run: `rtk git fetch origin`

  Run: `rtk git rebase origin/feat/implantar3`

- [x] **Step 4: Commitar somente o fix e a documentação**

  Run: `rtk git add .github/workflows/release-windows.yml vendor/llama-cpp-sys-2/build.rs vendor/llama-cpp-sys-2/src/build_support.rs vendor/llama-cpp-sys-2/AGENTS.md docs/subsystems/build-execucao.md docs/superpowers/plans/2026-09-11-windows-llamacpp-autoclone.md`

  Run: `rtk git commit -m "fix(build): materialize llama.cpp on Windows"`

- [ ] **Step 5: Validar o workflow Windows na revisão entregue**

  O job inclui o smoke test de `build_support.rs` antes do build nativo. Quando autorizado pelo fluxo de entrega, executar `rtk ~/.nvm/versions/node/v22.15.0/bin/gh-axi workflow run release-windows.yml --ref feat/implantar3 --field version=v0.0.0-windows-autoclone-test --field prerelease=true` e acompanhar com `rtk ~/.nvm/versions/node/v22.15.0/bin/gh-axi run list --workflow release-windows.yml --limit 5` e `rtk ~/.nvm/versions/node/v22.15.0/bin/gh-axi run view <id> --log-failed`. O critério é o job `Windows x86_64 (CPU)` passar pelo smoke test e pela etapa `llama-cpp-sys-2`; se o build passar, o pacote Windows também deve ser produzido.
