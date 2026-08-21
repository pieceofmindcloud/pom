# llama-cpp-sys-2

Este diretório contém o wrapper Rust e o script de build do binding nativo.
O código do fork `llama.cpp` fica fora deste diretório e só pode ser alterado
com autorização explícita. O wrapper compila CUDA Graphs desligado por padrão
por segurança de VRAM; `POM_CUDA_GRAPHS=1` é uma opção de build para habilitá-los.

## O `llama.cpp` daqui não vem do git

`llama.cpp`, neste diretório, é um **symlink versionado** para a raiz do
repositório, e a pasta que ele aponta (`/llama.cpp/`) está no `.gitignore`. Ou
seja: num clone novo - um worktree recém-criado, ou o checkout que a CI faz - o
symlink aponta para o vazio e o `build.rs` morre com
`Failed to obtain entry ... NotFound` antes de compilar qualquer coisa.

O `build.rs` lê `<CARGO_MANIFEST_DIR>/llama.cpp` (linha ~409), então clonar o
fork **por cima do symlink** resolve em qualquer sistema:

```bash
rm -rf vendor/llama-cpp-sys-2/llama.cpp
git clone --depth 1 --branch pom-tensor-parallel \
  https://github.com/pereirajair/llama.cpp.git vendor/llama-cpp-sys-2/llama.cpp
```

É o que os três workflows de release fazem. No Windows é obrigatório clonar
nesse caminho, e não na raiz: lá o checkout não cria symlink - materializa um
arquivo de texto com o caminho dentro -, então repor a pasta na raiz não
adiantaria. Ver `docs/subsystems/build-execucao.md`, "Release pelo GitHub
Actions".
