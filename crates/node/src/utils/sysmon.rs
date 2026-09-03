//! Host RAM / VRAM sampling, per platform.
//!
//! The two machines this runs on are not comparable, and the readings say so
//! instead of pretending otherwise:
//!
//! * **Linux + NVIDIA** — RAM from `/proc/meminfo`, VRAM from `nvidia-smi`,
//!   including the per-process breakdown so a number can be cross-checked
//!   against `nvidia-smi` by hand.
//! * **macOS / Apple Silicon** — RAM from `sysctl` + `vm_stat`. There is no
//!   separate VRAM pool: GPU buffers are wired pages of the same RAM. So the
//!   GPU section reports `kind: "unified"` with the wired total, and no
//!   `vram_total` at all, rather than inventing a dedicated-memory figure.
//! * **Windows** - RAM e disco vêm direto do kernel32 (`GlobalMemoryStatusEx`,
//!   `GetDiskFreeSpaceExW`), sem depender de nenhum executável externo. VRAM
//!   continua saindo do `nvidia-smi`, que o driver da NVIDIA instala no PATH;
//!   sem placa NVIDIA a seção de GPU volta vazia com um aviso, em vez de
//!   inventar número. Windows não tem load average: os três valores ficam em
//!   zero e a tela mostra isso como "não se aplica".

use serde::Serialize;
use std::ffi::CStr;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Chamadas ao kernel32 declaradas à mão.
///
/// São duas funções estáveis desde o Windows 2000 e ambas de assinatura
/// pequena, então declará-las aqui evita arrastar uma dependência inteira
/// (`windows-sys`) que só compila num alvo e cujo nome de *feature* não teria
/// como ser conferido a partir do macOS.
#[cfg(windows)]
mod win {
    /// Layout de `MEMORYSTATUSEX` (winbase.h). Os dois `u32` iniciais somam 8
    /// bytes, então os `u64` já caem alinhados: `repr(C)` reproduz o struct
    /// exatamente.
    /// A maior parte dos campos não é lida por nós, mas todos precisam existir:
    /// o kernel escreve o struct inteiro a partir de `dw_length`.
    #[repr(C)]
    #[allow(dead_code)]
    pub struct MemoryStatusEx {
        pub dw_length: u32,
        pub dw_memory_load: u32,
        pub ull_total_phys: u64,
        pub ull_avail_phys: u64,
        pub ull_total_page_file: u64,
        pub ull_avail_page_file: u64,
        pub ull_total_virtual: u64,
        pub ull_avail_virtual: u64,
        pub ull_avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
        pub fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    /// Um caminho como o Win32 espera: UTF-16 terminado em zero.
    pub fn wide(path: &std::path::Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
}

/// Samples are cached this long; the UI polls faster than the numbers move,
/// and `nvidia-smi` costs a process spawn.
const CACHE_TTL: Duration = Duration::from_millis(1200);

/// Execute a short host utility without registering it with Tokio's SIGCHLD
/// reaper. Tokio's process reaper installs a process-wide signal handler, and
/// that handler can interrupt the blocking UDP receive used by the pkarr DHT.
/// The caller remains asynchronous because the std process is isolated in a
/// blocking worker.
async fn command_output(program: &'static str, args: Vec<String>) -> Option<std::process::Output> {
    tokio::task::spawn_blocking(move || std::process::Command::new(program).args(args).output())
        .await
        .ok()?
        .ok()
}

#[derive(Debug, Clone, Serialize)]
pub struct MemStat {
    pub total_mb: f64,
    pub used_mb: f64,
    pub available_mb: f64,
    pub used_pct: f64,
}

impl MemStat {
    fn new(total_mb: f64, available_mb: f64) -> Self {
        let used_mb = (total_mb - available_mb).max(0.0);
        Self {
            total_mb,
            used_mb,
            available_mb,
            used_pct: if total_mb > 0.0 {
                used_mb / total_mb * 100.0
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuStat {
    pub index: u32,
    pub name: String,
    /// De quem é esta placa. `gpu_kind` resume a máquina; isto identifica a
    /// peça - é o que permite ao painel mostrar uma Radeon e uma NVIDIA lado a
    /// lado sem adivinhar pelo nome.
    pub vendor: GpuVendor,
    /// Dedicated VRAM. `None` on unified-memory machines, where the concept
    /// does not apply.
    pub vram_total_mb: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub vram_used_pct: Option<f64>,
    /// GPU core utilisation, when the platform reports it.
    pub utilization_pct: Option<f64>,
    pub temperature_c: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuProcess {
    pub pid: u32,
    pub name: String,
    pub used_mb: f64,
}

/// O fabricante de uma placa específica.
///
/// Separado de `GpuKind` de propósito: `GpuKind` descreve a **máquina** (e por
/// isso tem `None` e `Mixed`), enquanto isto descreve **uma placa**, onde essas
/// duas variantes não fariam sentido.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    Nvidia,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Amd,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Apple,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    /// Discrete NVIDIA card with its own VRAM.
    Nvidia,
    /// Discrete AMD card with its own VRAM, servida pelo backend Vulkan.
    ///
    /// Mesma observação da `Unified`: só o Linux constrói esta variante, mas ela
    /// não pode sair por `cfg` porque `GpuKind` é serializado e um nó macOS
    /// mostra no painel o estado de peers que rodam com Radeon.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Amd,
    /// Placas de fabricantes diferentes na mesma máquina (NVIDIA + Radeon).
    ///
    /// Existe porque `gpu_kind` é um resumo, e resumir duas placas de marcas
    /// diferentes como uma delas esconderia a outra de quem lê só este campo.
    /// Quem precisa do detalhe lê `gpus[].vendor`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Mixed,
    /// Apple Silicon: GPU and CPU share one memory pool.
    ///
    /// Só `read_gpu` no macOS constrói esta variante, então fora do macOS ela é
    /// dead code de verdade - e some do binário. Ela não pode ser cfg'ada para
    /// fora: `GpuKind` é o que o `/api/ui/*` serializa, e um nó Linux mostra no
    /// painel o estado de peers que rodam no MacBook.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Unified,
    /// Nothing detected.
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemLoad {
    pub hostname: String,
    pub os: &'static str,
    pub arch: &'static str,
    pub cpu_count: usize,
    /// 1 / 5 / 15 minute load averages.
    pub load_avg: [f64; 3],
    pub ram: MemStat,
    pub gpu_kind: GpuKind,
    /// Plain-language caveat for whatever the GPU numbers mean here.
    pub gpu_note: Option<String>,
    pub gpus: Vec<GpuStat>,
    /// Processes holding GPU memory (NVIDIA only).
    pub gpu_processes: Vec<GpuProcess>,
    /// Memory wired down by the kernel — where Metal buffers live on macOS.
    pub wired_mb: Option<f64>,
    /// Free space where the model slices live, so the screen can say whether
    /// another slice fits before anyone starts downloading one.
    pub disk: DiskStat,
    pub sampled_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskStat {
    pub total_mb: u64,
    pub free_mb: u64,
    pub mount_point: String,
}

impl Default for DiskStat {
    fn default() -> Self {
        DiskStat {
            total_mb: 0,
            free_mb: 0,
            mount_point: String::new(),
        }
    }
}

// ── Disk ─────────────────────────────────────────────────────────────────────

pub fn sample_disk() -> DiskStat {
    // The slices live under the project root, which is not necessarily the
    // working directory — see `config::project_root`.
    sample_disk_at(&crate::config::project_root())
}

/// Measure the volume that contains `path`, including when it is a configured
/// destination on another disk. A not-yet-created destination is probed via
/// its nearest existing parent, so planning never has to create directories.
pub fn sample_disk_at(path: &Path) -> DiskStat {
    let probe = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or(path);
    let mount = path.to_string_lossy().to_string();

    // Windows não tem `df`; o volume que contém o diretório responde direto.
    #[cfg(windows)]
    {
        let path = win::wide(probe);
        let (mut free_caller, mut total, mut free_total) = (0u64, 0u64, 0u64);
        let ok = unsafe {
            win::GetDiskFreeSpaceExW(path.as_ptr(), &mut free_caller, &mut total, &mut free_total)
        } != 0;
        if ok {
            // `free_caller` já desconta a cota do usuário, que é o número que
            // importa para saber se uma fatia cabe.
            return DiskStat {
                total_mb: total / 1_048_576,
                free_mb: free_caller / 1_048_576,
                mount_point: mount,
            };
        }
        return DiskStat {
            total_mb: 0,
            free_mb: 0,
            mount_point: mount,
        };
    }

    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("df")
            .arg("-m")
            .arg(probe)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok());

        if let Some(output) = out {
            let lines: Vec<&str> = output.lines().collect();
            if lines.len() >= 2 {
                let fields: Vec<&str> = lines[1].split_whitespace().collect();
                if fields.len() >= 4 {
                    if let (Ok(total), Ok(free)) =
                        (fields[1].parse::<u64>(), fields[3].parse::<u64>())
                    {
                        return DiskStat {
                            total_mb: total,
                            free_mb: free,
                            mount_point: mount,
                        };
                    }
                }
            }
        }

        DiskStat {
            total_mb: 0,
            free_mb: 0,
            mount_point: mount,
        }
    }
}

// ── Cache ────────────────────────────────────────────────────────────────────

struct Cached {
    at: Instant,
    load: SystemLoad,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Current host load, cached for [`CACHE_TTL`].
pub async fn sample() -> SystemLoad {
    {
        let guard = cache().lock().await;
        if let Some(ref c) = *guard {
            if c.at.elapsed() < CACHE_TTL {
                return c.load.clone();
            }
        }
    }

    let load = sample_uncached().await;
    *cache().lock().await = Some(Cached {
        at: Instant::now(),
        load: load.clone(),
    });
    load
}

async fn sample_uncached() -> SystemLoad {
    let (ram, wired_mb) = read_memory().await;
    let (gpu_kind, gpu_note, gpus, gpu_processes) = read_gpu(&ram).await;

    SystemLoad {
        hostname: hostname().await,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_count: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        load_avg: read_load_avg().await,
        disk: tokio::task::spawn_blocking(sample_disk)
            .await
            .unwrap_or_else(|_| DiskStat::default()),
        ram,
        gpu_kind,
        gpu_note,
        gpus,
        gpu_processes,
        wired_mb,
        sampled_at: chrono::Utc::now().to_rfc3339(),
    }
}

// ── Memory ───────────────────────────────────────────────────────────────────

/// Returns (RAM, wired pages in MB).
async fn read_memory() -> (MemStat, Option<f64>) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(txt) = tokio::fs::read_to_string("/proc/meminfo").await {
            let kb = |key: &str| -> Option<f64> {
                txt.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<f64>().ok())
            };
            let total = kb("MemTotal:").unwrap_or(0.0) / 1024.0;
            // MemAvailable is the kernel's own estimate of what a new workload
            // could get — closer to the truth than MemFree.
            let avail = kb("MemAvailable:").unwrap_or(0.0) / 1024.0;
            return (MemStat::new(total, avail), None);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let total_bytes = sysctl_number("hw.memsize").await.unwrap_or(0.0);
        let total_mb = total_bytes / 1_048_576.0;
        if let Some((used_mb, wired_mb)) = vm_stat_used().await {
            return (
                MemStat::new(total_mb, (total_mb - used_mb).max(0.0)),
                Some(wired_mb),
            );
        }
        return (MemStat::new(total_mb, 0.0), None);
    }

    #[cfg(windows)]
    {
        // `ullAvailPhys` é o equivalente honesto do MemAvailable do Linux: o
        // que uma carga nova conseguiria sem paginar.
        let mut st = win::MemoryStatusEx {
            dw_length: std::mem::size_of::<win::MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        let ok = unsafe { win::GlobalMemoryStatusEx(&mut st) } != 0;
        if ok {
            let mb = |b: u64| b as f64 / 1_048_576.0;
            return (
                MemStat::new(mb(st.ull_total_phys), mb(st.ull_avail_phys)),
                None,
            );
        }
        // Falhou: zeros dizem "não sei", e é assim que o resto do sistema lê
        // (ver os guardas `> 0` em `deploy.rs`).
        return (MemStat::new(0.0, 0.0), None);
    }

    #[allow(unreachable_code)]
    (MemStat::new(0.0, 0.0), None)
}

#[cfg(target_os = "macos")]
async fn sysctl_number(key: &str) -> Option<f64> {
    let out = command_output("sysctl", vec!["-n".into(), key.to_string()]).await?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

/// Parse `vm_stat` into (used MB, wired MB), matching how Activity Monitor
/// counts "Memory Used": active + wired + compressed.
#[cfg(target_os = "macos")]
async fn vm_stat_used() -> Option<(f64, f64)> {
    let out = command_output("vm_stat", Vec::new()).await?;
    let txt = String::from_utf8_lossy(&out.stdout);

    let page_size = txt
        .lines()
        .next()
        .and_then(|l| {
            l.split("page size of ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
        })
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(16384.0);

    let pages = |key: &str| -> f64 {
        txt.lines()
            .find(|l| l.trim_start().starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .map(|v| v.trim().trim_end_matches('.'))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };

    let active = pages("Pages active");
    let wired = pages("Pages wired down");
    let compressed = pages("Pages occupied by compressor");

    let to_mb = |p: f64| p * page_size / 1_048_576.0;
    Some((to_mb(active + wired + compressed), to_mb(wired)))
}

// ── GPU ──────────────────────────────────────────────────────────────────────

async fn read_gpu(ram: &MemStat) -> (GpuKind, Option<String>, Vec<GpuStat>, Vec<GpuProcess>) {
    // As duas famílias são somadas, não escolhidas: numa máquina com Radeon e
    // NVIDIA juntas, reportar só uma esconderia metade do hardware de quem
    // decide onde implantar. `gpu_kind` vira o resumo; cada placa carrega o
    // próprio `vendor`.
    // Só o ramo Linux abaixo estende a lista, então fora dele o `mut` sobra -
    // e tirá-lo por `cfg` partiria a linha em duas versões do mesmo código.
    #[cfg(target_os = "linux")]
    let pci_ids = read_pci_ids().await;

    let backend_gpus = backend_gpu_devices();
    let (mut gpus, procs) = nvidia_smi().await.unwrap_or_default();

    #[cfg(target_os = "linux")]
    {
        // Índices continuam a contagem da NVIDIA para não haver dois `index: 0`
        // - a interface usa esse número para falar de uma placa específica.
        gpus.extend(amd_sysfs(gpus.len() as u32, &backend_gpus, pci_ids.as_deref()).await);
    }

    // O nó só reporta as placas que pode usar. Os índices originais são
    // preservados de propósito: é por eles que a `pom.json` e o painel falam de
    // uma placa, e renumerar faria `gpus: [1]` aparecer como `#0`.
    if let Ok(escolha) = SELECAO.read() {
        if let Some(permitidas) = escolha.as_ref() {
            gpus.retain(|g| permitidas.contains(&g.index));
        }
    }
    let nvidia = gpus
        .iter()
        .filter(|g| g.vendor == GpuVendor::Nvidia)
        .count();
    let amd = gpus.iter().filter(|g| g.vendor == GpuVendor::Amd).count();

    if !gpus.is_empty() {
        let kind = match (nvidia, amd) {
            (0, _) => GpuKind::Amd,
            (_, 0) => GpuKind::Nvidia,
            _ => GpuKind::Mixed,
        };
        let note = (amd > 0).then(|| {
            "Radeon lida pelo sysfs do amdgpu e servida pelo backend Vulkan \
             (compile com `--features vulkan`). Ao contrário do nvidia-smi, esse \
             caminho não lista processos por GPU."
                .to_string()
        });
        return (kind, note, gpus, procs);
    }

    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH == "aarch64" {
            let name = mac_gpu_name(&backend_gpus)
                .await
                .unwrap_or_else(|| "Apple Silicon GPU".to_string());
            return (
                GpuKind::Unified,
                Some(
                    "Memória unificada: a GPU usa a mesma RAM do sistema, \
                     não existe VRAM dedicada para medir. O indicador mais \
                     próximo é a memória wired, onde ficam os buffers do Metal."
                        .to_string(),
                ),
                vec![GpuStat {
                    index: 0,
                    name,
                    vendor: GpuVendor::Apple,
                    vram_total_mb: None,
                    vram_used_mb: None,
                    vram_used_pct: None,
                    utilization_pct: None,
                    temperature_c: None,
                }],
                Vec::new(),
            );
        }
    }

    #[cfg(windows)]
    {
        // Sem NVIDIA não há de onde ler VRAM sem falar com DXGI ou WMI, e
        // nenhum dos dois diria quanto da placa está em uso pelo nosso
        // processo. Então o honesto é não reportar GPU nenhuma e dizer por quê.
        return (
            GpuKind::None,
            Some(
                "Nenhuma GPU NVIDIA encontrada (o `nvidia-smi` não respondeu). \
                 Neste alvo a inferência roda em CPU e não há leitura de VRAM \
                 para mostrar."
                    .to_string(),
            ),
            Vec::new(),
            Vec::new(),
        );
    }

    #[allow(unreachable_code)]
    {
        let _ = ram;
        (GpuKind::None, None, Vec::new(), Vec::new())
    }
}

/// Lê a VRAM usada pelo processo atual, somando as linhas do próprio PID.
///
/// Isto é deliberadamente uma sondagem pontual e síncrona: o caminho só é
/// usado durante o fit, onde precisamos correlacionar o valor do driver com o
/// snapshot que o llama.cpp recebeu. Em máquinas sem NVIDIA, ou quando o
/// `nvidia-smi` não está disponível, devolve `None`.
pub fn current_process_gpu_memory_mb() -> Option<f64> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    parse_current_process_gpu_memory_mb(
        &String::from_utf8_lossy(&output.stdout),
        std::process::id(),
    )
}

fn parse_current_process_gpu_memory_mb(output: &str, pid: u32) -> Option<f64> {
    let mut total_mb = 0.0;
    let mut found = false;
    for line in output.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 2 || fields[0].parse::<u32>().ok() != Some(pid) {
            continue;
        }
        let used_mb = fields[1].parse::<f64>().ok()?;
        total_mb += used_mb;
        found = true;
    }
    found.then_some(total_mb)
}

/// Query NVIDIA cards. Returns `None` when `nvidia-smi` is absent or fails,
/// which is the normal case on the MacBook.
async fn nvidia_smi() -> Option<(Vec<GpuStat>, Vec<GpuProcess>)> {
    let out = command_output(
        "nvidia-smi",
        vec![
            "--query-gpu=index,name,pci.bus_id,memory.total,memory.used,utilization.gpu,temperature.gpu".into(),
            "--format=csv,noheader,nounits".into(),
        ],
    )
    .await?;
    if !out.status.success() {
        return None;
    }

    let mut gpus = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if f.len() < 7 {
            continue;
        }
        let total: f64 = f[3].parse().unwrap_or(0.0);
        let used: f64 = f[4].parse().unwrap_or(0.0);
        let fallback = format!("NVIDIA GPU ({})", f[2]);
        gpus.push(GpuStat {
            index: f[0].parse().unwrap_or(0),
            name: resolve_gpu_name(Some(f[1]), None, fallback),
            vendor: GpuVendor::Nvidia,
            vram_total_mb: Some(total),
            vram_used_mb: Some(used),
            vram_used_pct: if total > 0.0 {
                Some(used / total * 100.0)
            } else {
                None
            },
            utilization_pct: f[5].parse().ok(),
            temperature_c: f[6].parse().ok(),
        });
    }

    let mut procs = Vec::new();
    if let Some(out) = command_output(
        "nvidia-smi",
        vec![
            "--query-compute-apps=pid,process_name,used_memory".into(),
            "--format=csv,noheader,nounits".into(),
        ],
    )
    .await
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if f.len() < 3 {
                continue;
            }
            procs.push(GpuProcess {
                pid: f[0].parse().unwrap_or(0),
                name: f[1].rsplit('/').next().unwrap_or(f[1]).to_string(),
                used_mb: f[2].parse().unwrap_or(0.0),
            });
        }
    }

    Some((gpus, procs))
}

/// As Radeon desta máquina, lidas do sysfs do `amdgpu`.
///
/// Sem binário externo de propósito: o equivalente ao `nvidia-smi` seria o
/// `rocm-smi`, que só existe se o ROCm inteiro estiver instalado - e o backend
/// que usamos é o Vulkan, que não depende de ROCm. O `amdgpu` já publica tudo
/// o que o painel mostra, e uma máquina sem Radeon simplesmente não tem os
/// arquivos.
///
/// O driver proprietário da NVIDIA não usa este sysfs, então o filtro por
/// `vendor == 0x1002` nunca confunde as duas placas de uma máquina mista.
#[cfg(target_os = "linux")]
async fn amd_sysfs(
    first_index: u32,
    backend_gpus: &[BackendGpu],
    pci_ids: Option<&str>,
) -> Vec<GpuStat> {
    /// Vendor PCI da AMD/ATI.
    const AMD: &str = "0x1002";

    let read = |p: std::path::PathBuf| async move {
        tokio::fs::read_to_string(p)
            .await
            .ok()
            .map(|v| v.trim().to_string())
    };

    let mut entries = match tokio::fs::read_dir("/sys/class/drm").await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    // `card0`, `card1`, ... sem os conectores (`card1-DP-2`), que apontam para o
    // mesmo dispositivo e contariam a placa várias vezes.
    let mut cards: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("card") && !name[4..].contains('-') {
            cards.push(name);
        }
    }
    cards.sort();

    let mut out = Vec::new();
    for name in cards {
        let dev = std::path::Path::new("/sys/class/drm")
            .join(&name)
            .join("device");
        if read(dev.join("vendor")).await.as_deref() != Some(AMD) {
            continue;
        }

        let mb = |bytes: Option<String>| {
            bytes
                .and_then(|v| v.parse::<f64>().ok())
                .map(|b| b / 1024.0 / 1024.0)
        };
        let total = mb(read(dev.join("mem_info_vram_total")).await);
        let used = mb(read(dev.join("mem_info_vram_used")).await);

        let device_id = read(dev.join("device")).await;
        let pci_bus_id = read(dev.join("uevent"))
            .await
            .and_then(|uevent| uevent_value(&uevent, "PCI_SLOT_NAME"));
        let fallback = format!(
            "AMD Radeon ({})",
            device_id.as_deref().unwrap_or("unknown PCI device")
        );
        let pci_name = pci_ids.and_then(|ids| {
            pci_device_name(
                ids,
                AMD.trim_start_matches("0x"),
                device_id
                    .as_deref()
                    .unwrap_or_default()
                    .trim_start_matches("0x"),
            )
        });

        out.push(GpuStat {
            index: first_index + out.len() as u32,
            vendor: GpuVendor::Amd,
            name: resolve_gpu_name(
                backend_gpu_name(backend_gpus, "Vulkan", pci_bus_id.as_deref()),
                pci_name.as_deref(),
                fallback,
            ),
            vram_total_mb: total,
            vram_used_mb: used,
            vram_used_pct: match (total, used) {
                (Some(t), Some(u)) if t > 0.0 => Some(u / t * 100.0),
                _ => None,
            },
            utilization_pct: read(dev.join("gpu_busy_percent"))
                .await
                .and_then(|v| v.parse::<f64>().ok()),
            temperature_c: amd_temperature(&dev).await,
        });
    }
    out
}

/// Temperatura da placa, em `hwmon/hwmon<N>/temp1_input` (milésimos de grau).
///
/// O `<N>` é atribuído na ordem em que os drivers registram, então varia entre
/// arranques e não pode ser fixado - daí a varredura do diretório.
#[cfg(target_os = "linux")]
async fn amd_temperature(device: &std::path::Path) -> Option<f64> {
    let mut hwmon = tokio::fs::read_dir(device.join("hwmon")).await.ok()?;
    while let Ok(Some(entry)) = hwmon.next_entry().await {
        if let Ok(raw) = tokio::fs::read_to_string(entry.path().join("temp1_input")).await {
            if let Ok(milli) = raw.trim().parse::<f64>() {
                return Some(milli / 1000.0);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
async fn mac_gpu_name(backend_gpus: &[BackendGpu]) -> Option<String> {
    if let Some(name) = backend_gpu_name(backend_gpus, "Metal", None) {
        return Some(name.to_string());
    }

    let out = command_output("system_profiler", vec!["SPDisplaysDataType".into()]).await?;
    system_profiler_gpu_name(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct BackendGpu {
    backend: String,
    description: String,
    device_id: Option<String>,
}

fn backend_gpu_devices() -> Vec<BackendGpu> {
    if !BACKEND_INITIALIZED.load(Ordering::Acquire) {
        return Vec::new();
    }

    unsafe {
        let mut raw = ptr::null_mut();
        let status = llama_cpp_sys_2::llama_rs_backend_devices_json(&mut raw);
        if status != llama_cpp_sys_2::LLAMA_RS_STATUS_OK || raw.is_null() {
            return Vec::new();
        }
        let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
        llama_cpp_sys_2::llama_rs_string_free(raw);
        serde_json::from_str(&text).unwrap_or_default()
    }
}

static BACKEND_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub(crate) fn mark_backend_initialized() {
    BACKEND_INITIALIZED.store(true, Ordering::Release);
}

fn backend_gpu_name<'a>(
    entries: &'a [BackendGpu],
    backend: &str,
    device_id: Option<&str>,
) -> Option<&'a str> {
    entries
        .iter()
        .filter(|entry| entry.backend.eq_ignore_ascii_case(backend))
        .find(|entry| {
            let same_device = match device_id {
                Some(id) => entry
                    .device_id
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(id)),
                None => true,
            };
            same_device && !entry.description.trim().is_empty()
        })
        .map(|entry| entry.description.trim())
}

fn resolve_gpu_name(primary: Option<&str>, secondary: Option<&str>, fallback: String) -> String {
    primary
        .into_iter()
        .chain(secondary)
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if fallback.trim().is_empty() {
                "GPU desconhecida".to_string()
            } else {
                fallback
            }
        })
}

#[cfg(target_os = "linux")]
fn uevent_value(uevent: &str, key: &str) -> Option<String> {
    uevent.lines().find_map(|line| {
        let (candidate, value) = line.split_once('=')?;
        (candidate == key).then(|| value.trim().to_string())
    })
}

#[cfg(target_os = "linux")]
async fn read_pci_ids() -> Option<String> {
    for path in ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"] {
        if let Ok(ids) = tokio::fs::read_to_string(path).await {
            return Some(ids);
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn pci_device_name(contents: &str, vendor: &str, device: &str) -> Option<String> {
    let vendor = vendor.trim_start_matches("0x");
    let device = device.trim_start_matches("0x");
    let mut in_vendor = false;
    for line in contents.lines() {
        if !line.starts_with('\t') {
            in_vendor = line.split_whitespace().next() == Some(vendor);
            continue;
        }
        if in_vendor && !line.starts_with("\t\t") {
            let mut fields = line.trim_start_matches('\t').splitn(2, char::is_whitespace);
            if fields
                .next()
                .is_some_and(|id| id.eq_ignore_ascii_case(device))
            {
                let name = fields.next()?.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn system_profiler_gpu_name(contents: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim() == "Chipset Model")
        .map(|(_, value)| value.trim().to_string())
        .filter(|name| !name.is_empty())
}

// ── Misc host facts ──────────────────────────────────────────────────────────

/// Load average de 1/5/15 minutos.
///
/// Windows não tem o conceito: o contador equivalente do WMI mede fila de
/// processador, que não é a mesma grandeza e enganaria quem compara os nós
/// lado a lado. Lá os três valores ficam em zero de propósito.
async fn read_load_avg() -> [f64; 3] {
    #[cfg(target_os = "linux")]
    {
        if let Ok(txt) = tokio::fs::read_to_string("/proc/loadavg").await {
            let v: Vec<f64> = txt
                .split_whitespace()
                .take(3)
                .filter_map(|s| s.parse().ok())
                .collect();
            if v.len() == 3 {
                return [v[0], v[1], v[2]];
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(out) = command_output("sysctl", vec!["-n".into(), "vm.loadavg".into()]).await {
            let txt = String::from_utf8_lossy(&out.stdout);
            let v: Vec<f64> = txt
                .trim()
                .trim_matches(|c| c == '{' || c == '}')
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if v.len() >= 3 {
                return [v[0], v[1], v[2]];
            }
        }
    }

    [0.0, 0.0, 0.0]
}

async fn hostname() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    if let Some(h) = CACHED.get() {
        return h.clone();
    }
    // No Windows o nome vem de COMPUTERNAME, que o sistema sempre define, e
    // isso poupa um spawn de processo. O `hostname.exe` existe e responderia,
    // mas cada spawn abre um console por um instante.
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();

    #[cfg(not(windows))]
    let name = command_output("hostname", Vec::new())
        .await
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();

    let name = if name.trim().is_empty() {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
    } else {
        name.trim().to_string()
    };
    let _ = CACHED.set(name.clone());
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_stat_derives_used_and_pct() {
        let m = MemStat::new(1000.0, 250.0);
        assert_eq!(m.used_mb, 750.0);
        assert!((m.used_pct - 75.0).abs() < 1e-9);
    }

    #[test]
    fn mem_stat_never_reports_negative_use() {
        let m = MemStat::new(100.0, 200.0);
        assert_eq!(m.used_mb, 0.0);
    }

    #[tokio::test]
    async fn sample_reports_real_ram() {
        let s = sample().await;
        assert!(s.ram.total_mb > 0.0, "total RAM should be discoverable");
        assert!(s.cpu_count > 0);
    }

    #[tokio::test]
    async fn external_command_output_uses_the_blocking_process_path() {
        let output = command_output("printf", vec!["ok".into()])
            .await
            .expect("printf should run");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
    }

    #[test]
    fn current_process_gpu_memory_sums_only_the_requested_pid() {
        let output = "100, 12\n42, 700\n42, 30\n";

        assert_eq!(parse_current_process_gpu_memory_mb(output, 42), Some(730.0));
        assert_eq!(parse_current_process_gpu_memory_mb(output, 101), None);
    }
}

/// Traduz a escolha de GPUs da `pom.json` para o que cada backend do ggml lê.
///
/// **Precisa rodar antes do primeiro `llama_backend_init`.** Os backends
/// enumeram os dispositivos ao serem registrados, e a partir daí a lista está
/// congelada: mexer nas variáveis depois não tira nem devolve placa nenhuma.
///
/// A tradução existe porque os índices vivem em espaços diferentes. O nó publica
/// uma lista só (`system.gpus[]`, NVIDIA primeiro, Radeon depois) e é esse
/// número que o operador vê no painel e escreve na config; já o CUDA numera só
/// as placas NVIDIA e o Vulkan só as dele. Mandar o índice do nó direto para o
/// backend selecionaria a placa errada assim que houvesse duas marcas.
///
/// Devolve o que aplicou, para o log dizer em que placas o nó vai rodar.
/// A escolha de GPUs em vigor, para `read_gpu` filtrar o que reporta.
///
/// `None` = sem restrição, que cobre tanto "a config não pediu nada" quanto
/// "a config pediu todas" - nos dois casos não há o que filtrar.
static SELECAO: std::sync::RwLock<Option<Vec<u32>>> = std::sync::RwLock::new(None);

pub fn apply_gpu_selection(selection: Option<&[u32]>, detected: &[GpuStat]) -> Vec<String> {
    // Guardado antes de qualquer saída: o que o nó **reporta** tem que ser o que
    // ele pode **usar**. Não é cosmética - `deploy::memory_capacity` soma a VRAM
    // de `system.gpus[]` para decidir quantas camadas cabem, então um nó preso a
    // uma placa que anunciasse as duas receberia trabalho que não cabe nele.
    if let Ok(mut guarda) = SELECAO.write() {
        *guarda = selection.map(|s| s.to_vec());
    }

    let Some(selection) = selection else {
        // Ausente é "todas": não tocar nas variáveis é justamente o que deixa
        // cada backend enxergar tudo o que existe.
        return Vec::new();
    };

    // Índice do nó -> índice dentro do próprio fabricante.
    let mut cuda = Vec::new();
    let mut vulkan = Vec::new();
    let (mut n_nvidia, mut n_amd) = (0u32, 0u32);
    let mut escolhidas = Vec::new();

    for gpu in detected {
        let local = match gpu.vendor {
            GpuVendor::Nvidia => {
                let i = n_nvidia;
                n_nvidia += 1;
                i
            }
            GpuVendor::Amd => {
                let i = n_amd;
                n_amd += 1;
                i
            }
            // Apple não tem seleção por dispositivo: o Metal serve a única GPU
            // da máquina, e não há variável para restringir.
            GpuVendor::Apple => continue,
        };
        if !selection.contains(&gpu.index) {
            continue;
        }
        escolhidas.push(format!("{} (#{})", gpu.name, gpu.index));
        match gpu.vendor {
            GpuVendor::Nvidia => cuda.push(local.to_string()),
            GpuVendor::Amd => vulkan.push(local.to_string()),
            GpuVendor::Apple => {}
        }
    }

    // Lista vazia é escolha explícita por CPU, então as variáveis vão vazias -
    // que é como CUDA e Vulkan dizem "nenhum dispositivo".
    std::env::set_var("CUDA_VISIBLE_DEVICES", cuda.join(","));
    std::env::set_var("GGML_VK_VISIBLE_DEVICES", vulkan.join(","));

    // Sem isto o índice de cima está errado, e erra em silêncio.
    //
    // `GGML_VK_VISIBLE_DEVICES` indexa a lista de dispositivos **do Vulkan**, e
    // o Vulkan enumera a NVIDIA junto com a Radeon: o driver da NVIDIA também
    // publica um ICD. Contamos o índice entre as AMD, então pedir a Radeon podia
    // selecionar a NVIDIA - foi o que aconteceu na ksia, com o modelo indo para
    // a placa errada enquanto o log dizia `Vulkan0`.
    //
    // Restringir o *loader* a um ICD é o que torna a conta certa por
    // construção: com só a Radeon exposta, o índice entre as AMD **é** o índice
    // do Vulkan. A NVIDIA não perde nada - quem a serve é o CUDA.
    #[cfg(target_os = "linux")]
    if !vulkan.is_empty() {
        if let Some(icd) = icd_amd() {
            std::env::set_var("VK_DRIVER_FILES", &icd);
            // O nome antigo, para loaders anteriores ao 1.3.207.
            std::env::set_var("VK_ICD_FILENAMES", &icd);
        }
    }

    escolhidas
}

/// O ICD do Mesa/RADV, quando existe nesta máquina.
///
/// Caminho fixo porque é onde o Mesa instala em toda distribuição que empacota
/// Vulkan; achar nada apenas deixa o loader com o comportamento padrão, que é o
/// que já havia antes. O nome varia com a arquitetura (`radeon_icd.x86_64.json`),
/// daí a varredura em vez de um nome literal.
#[cfg(target_os = "linux")]
fn icd_amd() -> Option<String> {
    let dir = std::fs::read_dir("/usr/share/vulkan/icd.d").ok()?;
    for entrada in dir.flatten() {
        let nome = entrada.file_name().to_string_lossy().to_string();
        if nome.starts_with("radeon_icd") && nome.ends_with(".json") {
            return Some(entrada.path().to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(test)]
mod gpu_selection_tests {
    use super::*;

    /// As variáveis de ambiente são do processo inteiro, e o cargo roda os
    /// testes em paralelo: sem este mutex um teste lê o que o outro acabou de
    /// escrever. Serializar é o que torna o resultado determinístico.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gpu(index: u32, vendor: GpuVendor) -> GpuStat {
        GpuStat {
            index,
            name: format!("gpu{index}"),
            vendor,
            vram_total_mb: None,
            vram_used_mb: None,
            vram_used_pct: None,
            utilization_pct: None,
            temperature_c: None,
        }
    }

    fn ambas() -> Vec<GpuStat> {
        vec![
            gpu(0, GpuVendor::Nvidia),
            gpu(1, GpuVendor::Amd),
            gpu(2, GpuVendor::Amd),
        ]
    }

    /// Sem escolha na config, nada é restringido - é o padrão pedido: todas.
    #[test]
    fn ausente_nao_restringe_nada() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CUDA_VISIBLE_DEVICES");
        std::env::remove_var("GGML_VK_VISIBLE_DEVICES");
        assert!(apply_gpu_selection(None, &ambas()).is_empty());
        assert!(std::env::var("CUDA_VISIBLE_DEVICES").is_err());
        assert!(std::env::var("GGML_VK_VISIBLE_DEVICES").is_err());
    }

    /// O ponto todo da tradução: a segunda Radeon é `#2` para o nó e `1` para o
    /// Vulkan. Mandar `2` selecionaria uma placa que não existe naquele backend.
    #[test]
    fn indice_do_no_vira_indice_do_backend() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        apply_gpu_selection(Some(&[2]), &ambas());
        assert_eq!(std::env::var("GGML_VK_VISIBLE_DEVICES").unwrap(), "1");
        assert_eq!(std::env::var("CUDA_VISIBLE_DEVICES").unwrap(), "");
    }

    #[test]
    fn escolher_a_nvidia_nao_libera_as_radeon() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        apply_gpu_selection(Some(&[0]), &ambas());
        assert_eq!(std::env::var("CUDA_VISIBLE_DEVICES").unwrap(), "0");
        assert_eq!(std::env::var("GGML_VK_VISIBLE_DEVICES").unwrap(), "");
    }

    /// O nó tem que **reportar** o que pode **usar**.
    ///
    /// Não é cosmética de painel: `deploy::memory_capacity` soma o
    /// `vram_total_mb` de `system.gpus[]` para decidir quantas camadas cabem num
    /// nó. Um nó presa à Radeon de 4 GB que anunciasse as duas placas diria ter
    /// 20 GB, e receberia trabalho que não cabe nele.
    #[test]
    fn o_que_o_no_reporta_e_so_o_que_ele_pode_usar() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        apply_gpu_selection(Some(&[1]), &ambas());

        let mut vistas = ambas();
        if let Ok(escolha) = SELECAO.read() {
            if let Some(permitidas) = escolha.as_ref() {
                vistas.retain(|g| permitidas.contains(&g.index));
            }
        }
        assert_eq!(vistas.len(), 1, "so a placa escolhida deve ser reportada");
        // O indice original sobrevive: e por ele que a config e o painel falam
        // desta placa, e renumerar faria `gpus: [1]` aparecer como `#0`.
        assert_eq!(vistas[0].index, 1);
    }

    /// Sem restricao nada e filtrado - o padrao continua mostrando tudo.
    #[test]
    fn sem_restricao_reporta_todas() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        apply_gpu_selection(None, &ambas());
        let vistas = SELECAO.read().unwrap().clone();
        assert!(vistas.is_none(), "sem escolha nao deve haver filtro");
    }

    /// Lista vazia é "só CPU", e não "não configurado".
    #[test]
    fn lista_vazia_desliga_as_duas_familias() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        apply_gpu_selection(Some(&[]), &ambas());
        assert_eq!(std::env::var("CUDA_VISIBLE_DEVICES").unwrap(), "");
        assert_eq!(std::env::var("GGML_VK_VISIBLE_DEVICES").unwrap(), "");
    }

    #[test]
    fn descricao_do_backend_tem_precedencia_sobre_pci_ids() {
        assert_eq!(
            resolve_gpu_name(
                Some("AMD Radeon RX 7800 XT (RADV NAVI32)"),
                Some("Navi 32 [Radeon RX 7700 XT / 7800 XT]"),
                "AMD Radeon (0x747e)".to_string(),
            ),
            "AMD Radeon RX 7800 XT (RADV NAVI32)"
        );
    }

    #[test]
    fn pci_ids_eh_usado_quando_o_backend_nao_responde() {
        assert_eq!(
            resolve_gpu_name(
                None,
                Some("Navi 32 [Radeon RX 7700 XT / 7800 XT]"),
                "AMD Radeon (0x747e)".to_string(),
            ),
            "Navi 32 [Radeon RX 7700 XT / 7800 XT]"
        );
    }

    #[test]
    fn fallback_por_identificador_nunca_remove_a_placa() {
        assert_eq!(
            resolve_gpu_name(None, None, "AMD Radeon (0x747e)".to_string()),
            "AMD Radeon (0x747e)"
        );
    }

    #[test]
    fn pci_ids_resolve_o_nome_certo_para_cada_placa() {
        let entries = vec![
            BackendGpu {
                backend: "Vulkan".to_string(),
                description: "Radeon A".to_string(),
                device_id: Some("0000:03:00.0".to_string()),
            },
            BackendGpu {
                backend: "Vulkan".to_string(),
                description: "Radeon B".to_string(),
                device_id: Some("0000:0b:00.0".to_string()),
            },
        ];

        assert_eq!(
            backend_gpu_name(&entries, "Vulkan", Some("0000:03:00.0")),
            Some("Radeon A")
        );
        assert_eq!(
            backend_gpu_name(&entries, "Vulkan", Some("0000:0b:00.0")),
            Some("Radeon B")
        );
    }

    #[test]
    fn pci_ids_parser_le_nome_do_dispositivo_sem_tabela_do_produto() {
        let pci_ids =
            "1002  Advanced Micro Devices, Inc.\n\t747e  Navi 32 [Radeon RX 7700 XT / 7800 XT]\n";
        assert_eq!(
            pci_device_name(pci_ids, "1002", "747e"),
            Some("Navi 32 [Radeon RX 7700 XT / 7800 XT]".to_string())
        );
    }

    #[test]
    fn system_profiler_le_o_nome_da_gpu_e_nao_o_nome_da_cpu() {
        assert_eq!(
            system_profiler_gpu_name(
                "Graphics/Displays:\n\n    Apple M4:\n      Chipset Model: Apple M4\n"
            ),
            Some("Apple M4".to_string())
        );
    }
}
