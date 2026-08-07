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

use serde::Serialize;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Samples are cached this long; the UI polls faster than the numbers move,
/// and `nvidia-smi` costs a process spawn.
const CACHE_TTL: Duration = Duration::from_millis(1200);

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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    /// Discrete NVIDIA card with its own VRAM.
    Nvidia,
    /// Apple Silicon: GPU and CPU share one memory pool.
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
    pub sampled_at: String,
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

    #[allow(unreachable_code)]
    (MemStat::new(0.0, 0.0), None)
}

#[cfg(target_os = "macos")]
async fn sysctl_number(key: &str) -> Option<f64> {
    let out = Command::new("sysctl").arg("-n").arg(key).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

/// Parse `vm_stat` into (used MB, wired MB), matching how Activity Monitor
/// counts "Memory Used": active + wired + compressed.
#[cfg(target_os = "macos")]
async fn vm_stat_used() -> Option<(f64, f64)> {
    let out = Command::new("vm_stat").output().await.ok()?;
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

async fn read_gpu(
    ram: &MemStat,
) -> (GpuKind, Option<String>, Vec<GpuStat>, Vec<GpuProcess>) {
    if let Some((gpus, procs)) = nvidia_smi().await {
        if !gpus.is_empty() {
            return (GpuKind::Nvidia, None, gpus, procs);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH == "aarch64" {
            let name = mac_gpu_name()
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

    let _ = ram;
    (GpuKind::None, None, Vec::new(), Vec::new())
}

/// Query NVIDIA cards. Returns `None` when `nvidia-smi` is absent or fails,
/// which is the normal case on the MacBook.
async fn nvidia_smi() -> Option<(Vec<GpuStat>, Vec<GpuProcess>)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,memory.total,memory.used,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let mut gpus = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if f.len() < 6 {
            continue;
        }
        let total: f64 = f[2].parse().unwrap_or(0.0);
        let used: f64 = f[3].parse().unwrap_or(0.0);
        gpus.push(GpuStat {
            index: f[0].parse().unwrap_or(0),
            name: f[1].to_string(),
            vram_total_mb: Some(total),
            vram_used_mb: Some(used),
            vram_used_pct: if total > 0.0 {
                Some(used / total * 100.0)
            } else {
                None
            },
            utilization_pct: f[4].parse().ok(),
            temperature_c: f[5].parse().ok(),
        });
    }

    let mut procs = Vec::new();
    if let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
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

#[cfg(target_os = "macos")]
async fn mac_gpu_name() -> Option<String> {
    let out = Command::new("sysctl")
        .arg("-n")
        .arg("machdep.cpu.brand_string")
        .output()
        .await
        .ok()?;
    let cpu = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if cpu.is_empty() {
        None
    } else {
        Some(format!("{cpu} (GPU integrada)"))
    }
}

// ── Misc host facts ──────────────────────────────────────────────────────────

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
        if let Ok(out) = Command::new("sysctl").arg("-n").arg("vm.loadavg").output().await {
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
    let name = match Command::new("hostname").output().await {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => String::new(),
    };
    let name = if name.is_empty() {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
    } else {
        name
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
}
