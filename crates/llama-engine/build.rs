//! Compila as pontes C/CUDA que o bindgen do `llama-cpp-sys-2` não emite.
//!
//! Estava tudo no `build.rs` de `crates/node`; mudou para cá junto com o código
//! Rust que fala com o llama.cpp, para que editar o backend/rede de `node` não
//! recompile este lado. Ver `docs/subsystems/build-execucao.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/mtp_arch.rs"]
mod mtp_arch;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed=build/mtp_arch.rs");
    mtp_arch::generate(&root.join("llama.cpp"), &out_dir);
    build_mtp_shim();
    build_moe_cuda();
}

/// Compila os kernels do executor MoE próprio quando o binário usa CUDA.
///
/// O host continua em Rust e só chama a ABI C mínima exportada pelo arquivo
/// CUDA. Não há Python, Triton ou processo externo no caminho de inferência.
fn build_moe_cuda() {
    if std::env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }
    println!("cargo:rerun-if-env-changed=CCACHE");

    let source = Path::new("src/moe/cuda_kernels.cu");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out_dir.join("pom_moe_cuda.o");
    let library = out_dir.join("libpom_moe_cuda.a");
    println!("cargo:rerun-if-changed={}", source.display());

    let nvcc = std::env::var_os("CUDACXX")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CUDA_PATH")
                .map(PathBuf::from)
                .map(|path| path.join("bin/nvcc"))
        })
        .unwrap_or_else(|| PathBuf::from("nvcc"));
    let mut compile = if let Some(wrapper) = std::env::var_os("CCACHE") {
        let mut command = Command::new(wrapper);
        command.arg(&nvcc);
        command
    } else {
        Command::new(&nvcc)
    };
    compile.args([
        "-c",
        source.to_str().expect("caminho CUDA UTF-8"),
        "-o",
        object.to_str().expect("objeto CUDA UTF-8"),
        "-O3",
        "--std=c++17",
    ]);
    if let Some(arch) = std::env::var_os("CUDAARCHS") {
        let arch = arch.to_string_lossy();
        let arch = arch.split(';').next().unwrap_or_default();
        if !arch.is_empty() {
            compile.arg(format!("-arch=sm_{arch}"));
        }
    }
    let status = compile
        .status()
        .unwrap_or_else(|error| panic!("não foi possível executar {}: {error}", nvcc.display()));
    assert!(status.success(), "nvcc falhou ao compilar cuda_kernels.cu");

    let status = Command::new("ar")
        .args([
            "crus",
            library.to_str().expect("biblioteca CUDA UTF-8"),
            object.to_str().expect("objeto CUDA UTF-8"),
        ])
        .status()
        .expect("não foi possível executar ar para a biblioteca CUDA");
    assert!(status.success(), "ar falhou ao empacotar os kernels CUDA");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=pom_moe_cuda");
}

/// Compila a ponte C para as entradas NextN/MTP do llama.cpp.
///
/// Ver o cabeçalho de `src/llama_mtp_shim.cpp` para o porquê de existir uma
/// ponte em vez de um `#[link_name]` com o nome mangled. Não acrescenta
/// requisito de build: o `llama-cpp-sys-2` já exige um compilador C++.
///
/// A ordem do link importa em `ld` do GNU (esquerda para a direita): esta
/// biblioteca referencia símbolos do `libllama`, então tem que aparecer antes
/// dele na linha. O rustc emite as bibliotecas nativas do crate atual antes das
/// dos dependentes; como `llama-cpp-sys-2` é dependência deste crate, essa
/// ordem se mantém.
fn build_mtp_shim() {
    println!("cargo:rerun-if-changed=src/llama_mtp_shim.cpp");
    cc::Build::new()
        .cpp(true)
        .file("src/llama_mtp_shim.cpp")
        .compile("pom_llama_mtp_shim");
}
