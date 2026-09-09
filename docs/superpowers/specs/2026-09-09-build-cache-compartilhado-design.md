# Cache de compilacao compartilhado

## Contexto

Cada worktree recebe seu proprio `target/` e o `build.rs` do
`llama-cpp-sys-2` coloca o build CMake dentro do `OUT_DIR` gerenciado pelo
Cargo. Isso duplica artefatos nativos e faz worktrees recompilarem os mesmos
objetos. O projeto ja possui pontos de integracao para `CMAKE_*`, `GGML_*` e
`CCACHE`; a mudanca deve aproveitar esses pontos sem tocar no fork
vendorizado de `llama.cpp`.

## Decisoes

1. `sccache` sera o cache comum para Rust e C/C++. O helper versionado
   `scripts/build-env.sh` sera carregado com `source` e so ativara o cache
   quando `sccache` estiver no `PATH`.
2. O helper usara `SCCACHE_DIR=${HOME}/.cache/sccache` e
   `SCCACHE_CACHE_SIZE=80G` por padrao, preservando valores ja exportados.
   `SCCACHE_SERVER_UDS` apontara por padrao para um socket dentro desse
   diretorio, para nao herdar um servidor sccache antigo configurado para outro
   cache.
   Tambem usara um `CARGO_TARGET_DIR` compartilhado por padrao, preservando um
   caminho explicitamente fornecido pelo operador.
3. `RUSTC_WRAPPER=sccache`, os tres launchers CMake e `CCACHE=sccache` serao
   exportados pelo helper. O ultimo continua cobrindo o `nvcc` executado
   diretamente por `crates/llama-engine/build.rs`; `RUSTC_WRAPPER` tambem e
   reconhecido pelo crate `cc` usado pelas pontes C/C++.
4. O `.cargo/config.toml` nao definira wrappers incondicionalmente: Cargo nao
   oferece uma condicao baseada em `command -v`, e um valor fixo quebraria
   maquinas sem `sccache`. A ausencia do helper conserva o build atual.
5. Nao sera adicionado um `LLAMA_CPP_LIB_DIR` nesta mudanca. Um arquivo `.a`
   ou dylib prebuilt nao carrega prova suficiente de source, target, feature,
   compilador e opcoes CMake; reutiliza-lo por caminho poderia ligar uma
   combinacao incompativel. A deduplicacao segura fica no CAS do `sccache` e no
   target Cargo compartilhado, enquanto o `build.rs` continua controlando
   bindings, wrappers, pacotes CMake e backends.
6. Quando a feature Vulkan for usada em macOS, o `build.rs` tambem publicara o
   diretorio e o link de `libvulkan.dylib` a partir de `CMAKE_Vulkan_LIBRARY`,
   `Vulkan_LIBRARY` ou `VULKAN_SDK`. O CMake gera objetos Vulkan estaticos e o
   binario final precisa do loader explicitamente.

## Validacao

- O contrato do helper cobre o modo com `sccache` e o fallback sem ele.
- `cargo build --release --features metal` sera medido limpo sem cache e com
  cache, incluindo tempo, tamanho do target e estatisticas do `sccache`.
- Um segundo build em target limpo usando o mesmo cache verificara hits sem
  depender de um segundo checkout modificado.
- `cargo test --workspace` e a verificacao de shell serao executados quando o
  ambiente permitir.

## Fora de escopo

- Nenhuma alteracao no codigo C++ ou CUDA do fork.
- Nenhuma mudanca no comportamento de runtime do POM.
- A integracao de `bin/fm-spawn.sh` e `bin/fm-teardown.sh` fica como follow-up
  do firstmate.
