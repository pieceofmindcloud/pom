#!/usr/bin/env bash
# Builds and packages one POM release variant on the ksia self-hosted runner.
#
# Variants:
#   linux-cpu    x86_64-unknown-linux-gnu, CPU
#   linux-cuda   x86_64-unknown-linux-gnu, CUDA with every GPU architecture the
#                toolkit supports (CUDA 12.9: compute_50 up to sm_120)
#   windows-cpu  x86_64-pc-windows-msvc, CPU, cross-compiled with cargo-xwin
#
# Everything that should survive between runs lives under POM_RELEASE_CACHE
# (default ~/.cache/pom-release): the llama.cpp checkout, one Cargo target
# directory per variant, cargo-xwin with its MSVC/Windows SDK download, and the
# ninja binary. Nothing is stored on GitHub: no artifacts, no Actions cache.
# The runner user needs no sudo; see docs/subsystems/build-and-run.md,
# "Release na ksia".
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
variant=""
version=""
output="${root}/dist-release-ksia"
dry_run=0
debug_symbols=0
cache="${POM_RELEASE_CACHE:-${HOME}/.cache/pom-release}"
cuda_home="${POM_RELEASE_CUDA_HOME:-/usr/local/cuda-12.9}"
llvm_bin="${POM_RELEASE_LLVM_BIN:-/usr/lib/llvm-20/bin}"
llama_repo="${POM_LLAMA_CPP_REPOSITORY:-https://github.com/pieceofmindcloud/llama.cpp.git}"
llama_ref="${POM_LLAMA_CPP_REF:-pom-tensor-parallel}"
ninja_version="1.12.1"
windows_target="x86_64-pc-windows-msvc"

usage() {
  printf '%s\n' \
    'Usage: scripts/release-ksia.sh --variant <linux-cpu|linux-cuda|windows-cpu> --version <vX.Y.Z> [options]' \
    '  --output <dir>   package directory (default: dist-release-ksia)' \
    '  --dry-run        print the commands instead of running them' \
    '  --debug-symbols  windows-cpu only: ship pom.pdb (full debug info, no LTO)'
}

die() {
  printf 'release-ksia: %s\n' "$1" >&2
  exit 2
}

run() {
  if ((dry_run)); then
    printf '+ %s\n' "$*"
  else
    "$@"
  fi
}

while (($# > 0)); do
  case "$1" in
    --variant) (($# >= 2)) || die '--variant requires a value'; variant="$2"; shift 2 ;;
    --version) (($# >= 2)) || die '--version requires a value'; version="$2"; shift 2 ;;
    --output) (($# >= 2)) || die '--output requires a value'; output="$2"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    --debug-symbols) debug_symbols=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

case "$variant" in
  linux-cpu|linux-cuda|windows-cpu) ;;
  *) die "--variant must be linux-cpu, linux-cuda or windows-cpu" ;;
esac
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || die "--version must look like v0.3.0 (got '${version}')"

if ((debug_symbols)) && [[ "$variant" != windows-cpu ]]; then
  die "--debug-symbols only applies to windows-cpu"
fi

# A debug-symbols build changes the profile of every crate; its own target
# directory keeps it from throwing away the optimized build's cache.
target_dir="${cache}/target-${variant}"
if ((debug_symbols)); then target_dir="${target_dir}-debug"; fi
tools="${cache}/tools"

# --- tools ------------------------------------------------------------------

missing=""
need() {
  command -v "$1" >/dev/null 2>&1 || missing="${missing} $1"
}
for tool in git cargo rustup npm node tar; do need "$tool"; done
case "$variant" in
  linux-*)
    # .cargo/config.toml links Linux binaries with clang + mold.
    need clang
    need mold
    need cmake
    ;;
  windows-cpu)
    need cmake
    need zip
    # The MSVC STL that xwin downloads (14.44+) refuses Clang < 19 (STL1000).
    for tool in clang-cl lld-link llvm-lib llvm-rc; do
      [[ -x "${llvm_bin}/${tool}" ]] || missing="${missing} ${tool}(${llvm_bin}; apt install clang-20 lld-20 llvm-20)"
    done
    if [[ -x "${llvm_bin}/clang" ]]; then
      clang_major="$("${llvm_bin}/clang" -dumpversion | cut -d. -f1)"
      ((clang_major >= 19)) || missing="${missing} clang>=19(${llvm_bin} is ${clang_major})"
    fi
    ;;
esac
if [[ "$variant" == linux-cuda && ! -x "${cuda_home}/bin/nvcc" ]]; then
  missing="${missing} nvcc(${cuda_home})"
fi
if [[ -n "$missing" ]] && ((! dry_run)); then
  die "missing on this machine:${missing}. Provision them once as root (see build-and-run.md, \"Release na ksia\")."
fi

# --- llama.cpp fork -----------------------------------------------------------

llama_source="${cache}/llama.cpp"
if ((dry_run)); then
  printf '+ prepare %s (%s) in %s\n' "$llama_repo" "$llama_ref" "$llama_source"
else
  mkdir -p "$cache"
  wanted="$(git ls-remote "$llama_repo" "refs/heads/${llama_ref}" | cut -f1)"
  [[ -n "$wanted" ]] || die "could not resolve ${llama_ref} in ${llama_repo}"
  current="$(git -C "$llama_source" rev-parse HEAD 2>/dev/null || true)"
  if [[ "$current" != "$wanted" ]]; then
    rm -rf "$llama_source"
    git clone --quiet --depth 1 --branch "$llama_ref" "$llama_repo" "$llama_source"
    [[ "$(git -C "$llama_source" rev-parse HEAD)" == "$wanted" ]] || die 'llama.cpp moved while cloning; run again'
  fi
  printf 'llama.cpp %s at %s\n' "$llama_ref" "$wanted" >&2
fi
export LLAMA_SOURCE="$llama_source"
export CARGO_TARGET_DIR="$target_dir"
# llama.cpp's CMake picks up ccache on its own. Keep it inside the release
# cache so a machine-wide cache_dir (ccache.conf) that this user cannot write
# does not fail every native object with "Permission denied".
export CCACHE_DIR="${cache}/ccache"

# --- web interface ------------------------------------------------------------

if [[ "${POM_RELEASE_SKIP_UI:-0}" != 1 ]]; then
  (
    cd "$root/apps/frontend"
    run npm ci --no-fund --no-audit
    run npm run build
  )
fi

# --- native build -------------------------------------------------------------

binary=""
case "$variant" in
  linux-cpu)
    run cargo build --release --locked -p node -p pom-plugin-host
    binary="${target_dir}/release/pom"
    ;;
  linux-cuda)
    # An empty CUDAARCHS makes the vendored CMake pick the toolkit's whole
    # architecture list instead of this machine's GPU (build-and-run.md,
    # "Arquiteturas de GPU"). Never inherit a narrow development value here.
    unset CMAKE_CUDA_ARCHITECTURES
    export CUDAARCHS=''
    export CUDA_PATH="$cuda_home"
    export CUDACXX="${cuda_home}/bin/nvcc"
    export PATH="${cuda_home}/bin:${PATH}"
    run cargo build --release --locked -p node -p pom-plugin-host --features cuda
    binary="${target_dir}/release/pom"
    ;;
  windows-cpu)
    # cargo-xwin downloads the MSVC CRT and the Windows SDK (license accepted
    # below) and drives clang-cl/lld-link. Ubuntu installs the MSVC front ends
    # only under /usr/lib/llvm-N/bin, so the shims dir puts the chosen LLVM
    # first on PATH.
    shims="${tools}/msvc-shims"
    run mkdir -p "$shims" "${tools}/bin"
    for tool in clang-cl lld-link llvm-lib llvm-rc; do
      run ln -sfn "${llvm_bin}/${tool}" "${shims}/${tool}"
    done
    if [[ ! -x "${tools}/bin/ninja" ]]; then
      run curl -fsSL -o "${tools}/ninja.zip" "https://github.com/ninja-build/ninja/releases/download/v${ninja_version}/ninja-linux.zip"
      run unzip -o -q "${tools}/ninja.zip" -d "${tools}/bin"
    fi
    if [[ ! -x "${tools}/bin/cargo-xwin" ]]; then
      run cargo install --locked --root "$tools" cargo-xwin
    fi
    run rustup target add "$windows_target"
    export PATH="${shims}:${tools}/bin:${PATH}"
    export XWIN_ACCEPT_LICENSE=1
    export XWIN_CACHE_DIR="${cache}/xwin"
    if ((debug_symbols)); then
      # For diagnosing crashes on Windows. With debuginfo rustc makes lld-link
      # write pom.pdb; /Z7 keeps the C/C++ CodeView in the objects so the PDB
      # also covers llama.cpp. Fat LTO with full symbols needs far more linker
      # memory and is pointless for a diagnostic build.
      export CARGO_PROFILE_RELEASE_DEBUG=2
      export CARGO_PROFILE_RELEASE_LTO=false
      export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
      export CFLAGS="${CFLAGS:+${CFLAGS} }/Z7"
      export CXXFLAGS="${CXXFLAGS:+${CXXFLAGS} }/Z7"
    fi
    run cargo xwin build --release --locked -p node -p pom-plugin-host --target "$windows_target"
    binary="${target_dir}/${windows_target}/release/pom.exe"
    ;;
esac

# --- package ------------------------------------------------------------------
# The binary sits at the package root with apps/ next to it: project_root()
# climbs from the executable's directory, so the unpacked folder runs anywhere.

case "$variant" in
  linux-cpu) name="pom-${version}-x86_64-unknown-linux-gnu-cpu" ;;
  linux-cuda) name="pom-${version}-x86_64-unknown-linux-gnu-cuda" ;;
  windows-cpu) name="pom-${version}-x86_64-pc-windows-msvc" ;;
esac
stage="${output}/${name}"
run rm -rf "$stage"
run mkdir -p "${stage}/apps/frontend"
run cp -R "$root/apps/frontend/dist" "${stage}/apps/frontend/dist"
run cp "$binary" "$stage/"
# pom starts plugins only through the pom-plugin-host sibling of its own
# executable (plugin_host::host_executable); a package without it cannot
# install any plugin.
run cp "$(dirname "$binary")/pom-plugin-host${binary##*/pom}" "$stage/"
if ((debug_symbols)); then run cp "${binary%.exe}.pdb" "$stage/"; fi
if [[ -f "$root/README.md" ]]; then run cp "$root/README.md" "$stage/"; fi
case "$variant" in
  linux-*)
    run mkdir -p "${stage}/share"
    run cp -R "$root/assets/brand/linux/hicolor" "${stage}/share/"
    run cp "$root/assets/brand/pom.desktop" "${stage}/share/"
    package="${output}/${name}.tar.gz"
    run tar -czf "$package" -C "$output" "$name"
    ;;
  windows-cpu)
    package="${output}/${name}.zip"
    run rm -f "$package"
    if ((dry_run)); then
      printf '+ (cd %s && zip -qr %s.zip %s)\n' "$output" "$name" "$name"
    else
      (cd "$output" && zip -qr "${name}.zip" "$name")
    fi
    ;;
esac
run rm -rf "$stage"
printf 'package=%s\n' "$package"
