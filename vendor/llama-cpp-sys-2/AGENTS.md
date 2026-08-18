# llama-cpp-sys-2

Este diretório contém o wrapper Rust e o script de build do binding nativo.
O código do fork `llama.cpp` fica fora deste diretório e só pode ser alterado
com autorização explícita. O wrapper compila CUDA Graphs desligado por padrão
por segurança de VRAM; `POM_CUDA_GRAPHS=1` é uma opção de build para habilitá-los.
