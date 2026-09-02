//! Cache LRU global de experts.
//!
//! Uma instância vive no servidor de inferência e é compartilhada por todas
//! as sessões. A chave inclui modelo, camada, expert e formato, portanto uma
//! entrada de uma requisição pode ser reutilizada por outra sem misturar
//! pesos. Leases mantêm entradas ativas protegidas contra eviction.

#[cfg(any(feature = "cuda", test))]
use super::admission::CacheBudgetSnapshot;
use super::admission::MemoryAccount;
use std::collections::HashMap;
use std::fmt;
use std::io;
#[cfg(any(feature = "cuda", test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(any(feature = "cuda", test))]
use std::time::Duration;
#[cfg(any(feature = "cuda", test))]
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpertKey {
    pub model: String,
    pub layer: u32,
    pub expert: u32,
    pub format: String,
}

impl ExpertKey {
    pub fn new(
        model: impl Into<String>,
        layer: u32,
        expert: u32,
        format: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            layer,
            expert,
            format: format.into(),
        }
    }
}

pub trait FreeMemoryProbe: Send + Sync {
    fn free_bytes(&self) -> io::Result<u64>;
}

/// Mede a memória livre que uma nova alocação consegue usar.
///
/// Em uma máquina unificada isto é RAM livre, que é também o pool usado por
/// Metal. Em GPU discreta a integração deverá fornecer uma sonda do backend
/// que conhece VRAM; a política aceita qualquer sonda pela trait acima.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemFreeMemory;

impl FreeMemoryProbe for SystemFreeMemory {
    fn free_bytes(&self) -> io::Result<u64> {
        system_free_bytes()
    }
}

#[cfg(target_os = "linux")]
fn system_free_bytes() -> io::Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    let kib = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "MemAvailable ausente"))?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    kib.checked_mul(1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemAvailable excede u64"))
}

#[cfg(target_os = "macos")]
fn system_free_bytes() -> io::Result<u64> {
    let page_size = command_number("sysctl", &["-n", "hw.pagesize"])?;
    let output = std::process::Command::new("vm_stat").output()?;
    if !output.status.success() {
        return Err(io::Error::other("vm_stat falhou"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let available_pages = [
        "Pages free",
        "Pages inactive",
        "Pages speculative",
        "Pages purgeable",
    ]
    .iter()
    .filter_map(|name| page_value(&text, name))
    .sum::<u64>();
    available_pages
        .checked_mul(page_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "páginas livres excedem u64"))
}

#[cfg(target_os = "macos")]
fn command_number(program: &str, args: &[&str]) -> io::Result<u64> {
    let output = std::process::Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!("{program} falhou")));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "macos")]
fn page_value(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.split_whitespace().last())
        .map(|value| value.trim_end_matches('.'))
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(windows)]
fn system_free_bytes() -> io::Result<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: the Windows API fills the correctly sized repr(C) structure.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status.ull_avail_phys)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn system_free_bytes() -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sonda de memória livre não implementada para este alvo",
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    EmptyExpert,
    MemoryProbe(String),
    NoBudget { bytes: usize, limit: u64 },
    BusyAndNoEviction { bytes: usize, limit: u64 },
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExpert => write!(formatter, "expert vazio não pode entrar no cache"),
            Self::MemoryProbe(error) => write!(formatter, "não foi possível medir memória livre: {error}"),
            Self::NoBudget { bytes, limit } => write!(
                formatter,
                "expert de {bytes} bytes excede o orçamento medido de {limit} bytes"
            ),
            Self::BusyAndNoEviction { bytes, limit } => write!(
                formatter,
                "não há entrada ociosa para admitir expert de {bytes} bytes no limite de {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for CacheError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpertCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub admissions: u64,
    pub evictions: u64,
    pub rejections: u64,
    pub resident_bytes: usize,
    pub entries: usize,
}

struct Entry {
    payload: Arc<[u8]>,
    active: usize,
    last_used: u64,
}

struct State {
    entries: HashMap<ExpertKey, Entry>,
    clock: u64,
    stats: ExpertCacheStats,
}

pub struct ExpertCache {
    account: MemoryAccount,
    probe: Arc<dyn FreeMemoryProbe>,
    state: Mutex<State>,
}

impl ExpertCache {
    pub fn new(account: MemoryAccount, probe: Arc<dyn FreeMemoryProbe>) -> Arc<Self> {
        Arc::new(Self {
            account,
            probe,
            state: Mutex::new(State {
                entries: HashMap::new(),
                clock: 0,
                stats: ExpertCacheStats::default(),
            }),
        })
    }

    /// Cria o cache com baseline e memória atual medidos no próprio processo.
    /// `weights_bytes`, `fixed_bytes` e `reserve_bytes` continuam explícitos
    /// para que o orçamento não confunda memória livre com memória disponível.
    pub fn measured_system(
        weights_bytes: u64,
        fixed_bytes: u64,
        reserve_bytes: u64,
    ) -> Result<Arc<Self>, CacheError> {
        let probe: Arc<dyn FreeMemoryProbe> = Arc::new(SystemFreeMemory);
        let baseline = probe
            .free_bytes()
            .map_err(|error| CacheError::MemoryProbe(error.to_string()))?;
        Ok(Self::new(
            MemoryAccount {
                baseline_free_bytes: baseline,
                compute_bytes: 0,
                weights_bytes,
                fixed_bytes,
                reserve_bytes,
            },
            probe,
        ))
    }

    pub fn lookup(self: &Arc<Self>, key: &ExpertKey) -> Option<ExpertLease> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.saturating_add(1);
        let now = state.clock;
        let payload = {
            let entry = state.entries.get_mut(key)?;
            entry.active = entry.active.saturating_add(1);
            entry.last_used = now;
            Arc::clone(&entry.payload)
        };
        state.stats.hits = state.stats.hits.saturating_add(1);
        Some(ExpertLease {
            cache: Arc::clone(self),
            key: key.clone(),
            payload,
        })
    }

    /// Snapshot monotônico das operações do LRU compartilhado pelo runtime.
    /// O contador é diagnóstico: não participa da política de admissão.
    pub fn stats(&self) -> ExpertCacheStats {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats
    }

    pub fn admit(
        self: &Arc<Self>,
        key: ExpertKey,
        payload: Vec<u8>,
    ) -> Result<ExpertLease, CacheError> {
        if payload.is_empty() {
            return Err(CacheError::EmptyExpert);
        }
        let bytes = payload.len();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.saturating_add(1);
        let now = state.clock;

        if state.entries.contains_key(&key) {
            let payload = {
                let entry = state
                    .entries
                    .get_mut(&key)
                    .expect("entrada acabou de ser vista");
                entry.active = entry.active.saturating_add(1);
                entry.last_used = now;
                Arc::clone(&entry.payload)
            };
            state.stats.hits = state.stats.hits.saturating_add(1);
            return Ok(ExpertLease {
                cache: Arc::clone(self),
                key,
                payload,
            });
        }

        state.stats.misses = state.stats.misses.saturating_add(1);
        let current_free = self
            .probe
            .free_bytes()
            .map_err(|error| CacheError::MemoryProbe(error.to_string()))?;

        let limit = self
            .account
            .cache_limit_bytes(current_free, state.stats.resident_bytes as u64)
            .unwrap_or(0);
        if bytes as u64 > limit {
            state.stats.rejections = state.stats.rejections.saturating_add(1);
            return Err(CacheError::NoBudget { bytes, limit });
        }

        while state.stats.resident_bytes.saturating_add(bytes) as u64 > limit {
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.active == 0)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                state.stats.rejections = state.stats.rejections.saturating_add(1);
                return Err(CacheError::BusyAndNoEviction { bytes, limit });
            };
            if let Some(entry) = state.entries.remove(&victim) {
                state.stats.resident_bytes = state
                    .stats
                    .resident_bytes
                    .saturating_sub(entry.payload.len());
                state.stats.entries = state.stats.entries.saturating_sub(1);
                state.stats.evictions = state.stats.evictions.saturating_add(1);
            }
        }

        let payload: Arc<[u8]> = Arc::from(payload);
        state.stats.resident_bytes = state.stats.resident_bytes.saturating_add(bytes);
        state.stats.entries = state.entries.len().saturating_add(1);
        state.stats.admissions = state.stats.admissions.saturating_add(1);
        state.entries.insert(
            key.clone(),
            Entry {
                payload: Arc::clone(&payload),
                active: 1,
                last_used: now,
            },
        );
        Ok(ExpertLease {
            cache: Arc::clone(self),
            key,
            payload,
        })
    }

    fn release(&self, key: &ExpertKey) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = state.entries.get_mut(key) {
            entry.active = entry.active.saturating_sub(1);
        }
    }
}

pub struct ExpertLease {
    cache: Arc<ExpertCache>,
    key: ExpertKey,
    payload: Arc<[u8]>,
}

impl ExpertLease {
    pub fn bytes(&self) -> &[u8] {
        &self.payload
    }
}

impl Drop for ExpertLease {
    fn drop(&mut self) {
        self.cache.release(&self.key);
    }
}

/// Uma alocação opaca no device que executa os experts.
///
/// O cache não conhece CUDA, Vulkan, Metal, arquitetura ou dtype. O backend
/// devolve apenas o handle que o kernel entende e o tamanho que deve ser
/// contabilizado no orçamento. A implementação é dona de liberar o handle no
/// `Drop` do objeto concreto.
#[cfg(any(feature = "cuda", test))]
pub trait DeviceAllocation: Send + Sync {
    fn bytes(&self) -> usize;
    fn handle(&self) -> usize;
}

/// Operações mínimas que um backend precisa oferecer ao cache residente.
#[cfg(any(feature = "cuda", test))]
pub trait DeviceAllocator: Send + Sync {
    fn device_name(&self) -> &str;
    fn free_bytes(&self) -> io::Result<u64>;
    fn upload(&self, payload: &[u8]) -> Result<Box<dyn DeviceAllocation>, String>;
}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCacheError {
    EmptyExpert {
        key: ExpertKey,
    },
    MemoryProbe {
        device: String,
        reason: String,
    },
    Upload {
        key: ExpertKey,
        device: String,
        reason: String,
    },
    NoBudget {
        key: ExpertKey,
        device: String,
        bytes: usize,
        limit: u64,
        budget: CacheBudgetSnapshot,
    },
    BudgetExhausted {
        device: String,
        budget: CacheBudgetSnapshot,
    },
    BusyAndNoEviction {
        key: ExpertKey,
        device: String,
        bytes: usize,
        limit: u64,
        budget: CacheBudgetSnapshot,
    },
}

#[cfg(any(feature = "cuda", test))]
impl fmt::Display for DeviceCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExpert { key } => write!(formatter, "expert vazio não pode entrar no cache de device: {key:?}"),
            Self::MemoryProbe { device, reason } => {
                write!(formatter, "não foi possível medir memória livre de {device}: {reason}")
            }
            Self::Upload { key, device, reason } => {
                write!(formatter, "upload do expert {key:?} para {device} falhou: {reason}")
            }
            Self::NoBudget {
                key,
                device,
                bytes,
                limit,
                budget,
            } => write!(
                formatter,
                "expert {key:?} de {bytes} bytes não cabe no orçamento medido de {limit} bytes em {device}: {}",
                budget.summary()
            ),
            Self::BudgetExhausted { device, budget } => write!(
                formatter,
                "orçamento global do cache CUDA insuficiente em {device}: {}; reduza contexto/microlote ou aceite experts fora do cache",
                budget.summary()
            ),
            Self::BusyAndNoEviction {
                key,
                device,
                bytes,
                limit,
                budget,
            } => write!(
                formatter,
                "não há entrada ociosa para admitir expert {key:?} de {bytes} bytes no limite de {limit} bytes em {device}: {}",
                budget.summary()
            ),
        }
    }
}

#[cfg(any(feature = "cuda", test))]
impl std::error::Error for DeviceCacheError {}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeviceCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub admissions: u64,
    pub evictions: u64,
    pub rejections: u64,
    pub uploads: u64,
    pub upload_bytes: u64,
    pub upload_ms: f64,
    pub resident_bytes: usize,
    pub entries: usize,
}

#[cfg(any(feature = "cuda", test))]
struct DeviceEntry {
    allocation: Box<dyn DeviceAllocation>,
    active: usize,
    last_used: u64,
}

#[cfg(any(feature = "cuda", test))]
struct DeviceState {
    entries: HashMap<ExpertKey, DeviceEntry>,
    clock: u64,
    stats: DeviceCacheStats,
}

/// LRU global de handles residentes no device.
///
/// A fonte fria continua sendo lida pelo executor somente em um miss. O
/// O payload não fica preso neste struct: depois do upload, a entrada mantém o
/// handle concreto e o tamanho alocado pelo backend. Isso impede que um hit de
/// CUDA seja confundido com um hit da LRU de RAM e permite validar a geometria
/// antes de passar o handle para um kernel.
#[cfg(any(feature = "cuda", test))]
pub struct DeviceExpertCache {
    account: MemoryAccount,
    allocator: Arc<dyn DeviceAllocator>,
    state: Mutex<DeviceState>,
}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug, Clone)]
pub struct DeviceWarmupOptions {
    pub max_duration: Duration,
    pub cancellation: Arc<AtomicBool>,
}

#[cfg(any(feature = "cuda", test))]
impl DeviceWarmupOptions {
    pub fn new(max_duration: Duration) -> Self {
        Self {
            max_duration,
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceWarmupStop {
    Completed,
    BudgetExhausted,
    Cancelled,
    TimedOut,
    Failed,
}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug, Clone)]
pub struct DeviceWarmupReport {
    pub stop: DeviceWarmupStop,
    pub candidates: u64,
    pub admitted: u64,
    /// Total de bytes aceitos nesta tentativa, sem contar hits.
    pub admitted_bytes: u64,
    pub error: Option<String>,
    pub elapsed_ms: f64,
    pub resident_bytes: usize,
    pub budget_limit_bytes: u64,
}

#[cfg(any(feature = "cuda", test))]
impl DeviceExpertCache {
    pub fn new(account: MemoryAccount, allocator: Arc<dyn DeviceAllocator>) -> Arc<Self> {
        Arc::new(Self {
            account,
            allocator,
            state: Mutex::new(DeviceState {
                entries: HashMap::new(),
                clock: 0,
                stats: DeviceCacheStats::default(),
            }),
        })
    }

    pub fn lookup(self: &Arc<Self>, key: &ExpertKey) -> Option<DeviceExpertLease> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.saturating_add(1);
        let now = state.clock;
        let (handle, bytes) = {
            let entry = state.entries.get_mut(key)?;
            entry.active = entry.active.saturating_add(1);
            entry.last_used = now;
            (entry.allocation.handle(), entry.allocation.bytes())
        };
        state.stats.hits = state.stats.hits.saturating_add(1);
        Some(DeviceExpertLease {
            cache: Arc::clone(self),
            key: key.clone(),
            handle,
            bytes,
        })
    }

    pub fn stats(&self) -> DeviceCacheStats {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats
    }

    /// Admite candidatos lazily até o orçamento, um prazo ou cancelamento.
    ///
    /// O iterador só produz o próximo payload depois da verificação de
    /// cancelamento e prazo. Assim o aquecimento não precisa materializar o
    /// banco inteiro de experts em RAM e pode parar sem consumir mais disco,
    /// CPU ou VRAM do que o orçamento medido permite.
    pub fn warm<I>(
        self: &Arc<Self>,
        candidates: I,
        options: &DeviceWarmupOptions,
    ) -> DeviceWarmupReport
    where
        I: IntoIterator<Item = Result<(ExpertKey, Vec<u8>), String>>,
    {
        let started = Instant::now();
        let mut report = DeviceWarmupReport {
            stop: DeviceWarmupStop::Completed,
            candidates: 0,
            admitted: 0,
            admitted_bytes: 0,
            error: None,
            elapsed_ms: 0.0,
            resident_bytes: self.stats().resident_bytes,
            budget_limit_bytes: 0,
        };
        let initial_budget = match self.budget_snapshot() {
            Ok(budget) => budget,
            Err(error) => {
                report.stop = DeviceWarmupStop::Failed;
                report.error = Some(error.to_string());
                report.elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
                return report;
            }
        };
        report.budget_limit_bytes = initial_budget.effective_limit_bytes.unwrap_or(0);
        if report.budget_limit_bytes == 0 {
            report.stop = DeviceWarmupStop::BudgetExhausted;
            report.error = Some(
                DeviceCacheError::BudgetExhausted {
                    device: self.allocator.device_name().to_owned(),
                    budget: initial_budget,
                }
                .to_string(),
            );
            report.elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
            return report;
        }
        let mut candidates = candidates.into_iter();

        loop {
            if options.cancellation.load(Ordering::Acquire) {
                report.stop = DeviceWarmupStop::Cancelled;
                break;
            }
            if started.elapsed() >= options.max_duration {
                report.stop = DeviceWarmupStop::TimedOut;
                break;
            }
            let Some(candidate) = candidates.next() else {
                break;
            };
            report.candidates = report.candidates.saturating_add(1);
            let (key, payload) = match candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    report.stop = DeviceWarmupStop::Failed;
                    report.error = Some(error);
                    break;
                }
            };
            let bytes = payload.len() as u64;
            match self.admit_inner(key, &payload, false) {
                Ok(lease) => {
                    report.admitted = report.admitted.saturating_add(1);
                    report.admitted_bytes = report.admitted_bytes.saturating_add(bytes);
                    drop(lease);
                }
                Err(error)
                    if matches!(
                        error,
                        DeviceCacheError::BudgetExhausted { .. }
                            | DeviceCacheError::NoBudget { .. }
                            | DeviceCacheError::BusyAndNoEviction { .. }
                    ) =>
                {
                    report.stop = DeviceWarmupStop::BudgetExhausted;
                    report.error = Some(error.to_string());
                    break;
                }
                Err(error) => {
                    report.stop = DeviceWarmupStop::Failed;
                    report.error = Some(error.to_string());
                    break;
                }
            }
        }

        report.elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        report.resident_bytes = self.stats().resident_bytes;
        report
    }

    pub fn budget_snapshot(&self) -> Result<CacheBudgetSnapshot, DeviceCacheError> {
        let device = self.allocator.device_name().to_owned();
        let current_free =
            self.allocator
                .free_bytes()
                .map_err(|error| DeviceCacheError::MemoryProbe {
                    device,
                    reason: error.to_string(),
                })?;
        let resident = self.stats().resident_bytes as u64;
        Ok(self.account.budget_snapshot(current_free, resident))
    }

    pub fn admit(
        self: &Arc<Self>,
        key: ExpertKey,
        payload: &[u8],
    ) -> Result<DeviceExpertLease, DeviceCacheError> {
        self.admit_inner(key, payload, true)
    }

    fn admit_inner(
        self: &Arc<Self>,
        key: ExpertKey,
        payload: &[u8],
        allow_eviction: bool,
    ) -> Result<DeviceExpertLease, DeviceCacheError> {
        if payload.is_empty() {
            return Err(DeviceCacheError::EmptyExpert { key });
        }
        let bytes = payload.len();
        let device = self.allocator.device_name().to_owned();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.saturating_add(1);
        let now = state.clock;

        if let Some(entry) = state.entries.get_mut(&key) {
            entry.active = entry.active.saturating_add(1);
            entry.last_used = now;
            let handle = entry.allocation.handle();
            let resident_bytes = entry.allocation.bytes();
            state.stats.hits = state.stats.hits.saturating_add(1);
            return Ok(DeviceExpertLease {
                cache: Arc::clone(self),
                key,
                handle,
                bytes: resident_bytes,
            });
        }

        state.stats.misses = state.stats.misses.saturating_add(1);
        let current_free =
            self.allocator
                .free_bytes()
                .map_err(|error| DeviceCacheError::MemoryProbe {
                    device: device.clone(),
                    reason: error.to_string(),
                })?;
        let budget = self
            .account
            .budget_snapshot(current_free, state.stats.resident_bytes as u64);
        let limit = budget.effective_limit_bytes.unwrap_or(0);
        if limit == 0 {
            state.stats.rejections = state.stats.rejections.saturating_add(1);
            return Err(DeviceCacheError::BudgetExhausted { device, budget });
        }
        if bytes as u64 > limit {
            state.stats.rejections = state.stats.rejections.saturating_add(1);
            return Err(DeviceCacheError::NoBudget {
                key,
                device,
                bytes,
                limit,
                budget,
            });
        }

        while state.stats.resident_bytes.saturating_add(bytes) as u64 > limit {
            if !allow_eviction {
                state.stats.rejections = state.stats.rejections.saturating_add(1);
                return Err(DeviceCacheError::BudgetExhausted { device, budget });
            }
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.active == 0)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                state.stats.rejections = state.stats.rejections.saturating_add(1);
                return Err(DeviceCacheError::BusyAndNoEviction {
                    key,
                    device,
                    bytes,
                    limit,
                    budget,
                });
            };
            if let Some(entry) = state.entries.remove(&victim) {
                state.stats.resident_bytes = state
                    .stats
                    .resident_bytes
                    .saturating_sub(entry.allocation.bytes());
                state.stats.entries = state.stats.entries.saturating_sub(1);
                state.stats.evictions = state.stats.evictions.saturating_add(1);
            }
        }

        let upload_started = Instant::now();
        let allocation =
            self.allocator
                .upload(payload)
                .map_err(|reason| DeviceCacheError::Upload {
                    key: key.clone(),
                    device,
                    reason,
                })?;
        if allocation.bytes() != bytes {
            return Err(DeviceCacheError::Upload {
                key,
                device: self.allocator.device_name().to_owned(),
                reason: format!(
                    "backend alocou {} bytes para payload de {bytes}",
                    allocation.bytes()
                ),
            });
        }
        let handle = allocation.handle();
        state.stats.resident_bytes = state.stats.resident_bytes.saturating_add(bytes);
        state.stats.entries = state.entries.len().saturating_add(1);
        state.stats.admissions = state.stats.admissions.saturating_add(1);
        state.stats.uploads = state.stats.uploads.saturating_add(1);
        state.stats.upload_bytes = state.stats.upload_bytes.saturating_add(bytes as u64);
        state.stats.upload_ms += upload_started.elapsed().as_secs_f64() * 1_000.0;
        state.entries.insert(
            key.clone(),
            DeviceEntry {
                allocation,
                active: 1,
                last_used: now,
            },
        );
        Ok(DeviceExpertLease {
            cache: Arc::clone(self),
            key,
            handle,
            bytes,
        })
    }

    fn release(&self, key: &ExpertKey) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = state.entries.get_mut(key) {
            entry.active = entry.active.saturating_sub(1);
        }
    }
}

#[cfg(any(feature = "cuda", test))]
pub struct DeviceExpertLease {
    cache: Arc<DeviceExpertCache>,
    key: ExpertKey,
    handle: usize,
    bytes: usize,
}

#[cfg(any(feature = "cuda", test))]
impl DeviceExpertLease {
    pub fn handle(&self) -> usize {
        self.handle
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(any(feature = "cuda", test))]
impl Drop for DeviceExpertLease {
    fn drop(&mut self) {
        self.cache.release(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FixedProbe(AtomicU64);

    impl FixedProbe {
        fn new(bytes: u64) -> Self {
            Self(AtomicU64::new(bytes))
        }

        fn set(&self, bytes: u64) {
            self.0.store(bytes, Ordering::Relaxed);
        }
    }

    impl FreeMemoryProbe for FixedProbe {
        fn free_bytes(&self) -> io::Result<u64> {
            Ok(self.0.load(Ordering::Relaxed))
        }
    }

    fn cache_with_baseline(probe: &Arc<FixedProbe>, baseline_free_bytes: u64) -> Arc<ExpertCache> {
        ExpertCache::new(
            MemoryAccount {
                baseline_free_bytes,
                compute_bytes: 0,
                weights_bytes: 0,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            Arc::clone(probe) as Arc<dyn FreeMemoryProbe>,
        )
    }

    fn cache(probe: &Arc<FixedProbe>) -> Arc<ExpertCache> {
        cache_with_baseline(probe, 10)
    }

    fn stats(cache: &ExpertCache) -> ExpertCacheStats {
        cache
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats
    }

    #[test]
    fn stats_expoem_hits_misses_e_residencia_do_cache_global() {
        let probe = Arc::new(FixedProbe::new(100));
        let cache = cache_with_baseline(&probe, 100);
        let first = cache.admit(key(1), vec![1, 2, 3]).unwrap();
        drop(first);
        let hit = cache.lookup(&key(1)).unwrap();
        drop(hit);

        let snapshot = cache.stats();

        assert_eq!(snapshot.hits, 1);
        assert_eq!(snapshot.misses, 1);
        assert_eq!(snapshot.admissions, 1);
        assert_eq!(snapshot.resident_bytes, 3);
        assert_eq!(snapshot.entries, 1);
    }

    fn key(expert: u32) -> ExpertKey {
        ExpertKey::new("model", 3, expert, "q4_0")
    }

    #[test]
    fn hit_reutiliza_payload_e_marca_lease_ativo() {
        let probe = Arc::new(FixedProbe::new(10));
        let cache = cache(&probe);
        let first = cache.admit(key(1), vec![1, 2, 3]).unwrap();
        let second = cache.lookup(&key(1)).unwrap();
        assert_eq!(second.bytes(), &[1, 2, 3]);
        assert_eq!(stats(&cache).resident_bytes, 3);
        assert_eq!(stats(&cache).hits, 1);
        drop(first);
        drop(second);
    }

    #[test]
    fn eviction_e_global_e_preserva_entrada_ativa() {
        let probe = Arc::new(FixedProbe::new(6));
        let cache = cache_with_baseline(&probe, 6);
        let active = cache.admit(key(1), vec![1, 1, 1]).unwrap();
        let idle = cache.admit(key(2), vec![2, 2, 2]).unwrap();
        drop(idle);
        let replacement = cache.admit(key(3), vec![3, 3, 3]).unwrap();
        assert!(cache.lookup(&key(1)).is_some());
        assert!(cache.lookup(&key(2)).is_none());
        assert_eq!(replacement.bytes(), &[3, 3, 3]);
        assert_eq!(stats(&cache).evictions, 1);
        drop(active);
        drop(replacement);
    }

    #[test]
    fn nova_medicao_pode_forcar_eviction_antes_de_alocar() {
        let probe = Arc::new(FixedProbe::new(10));
        let cache = cache(&probe);
        let first = cache.admit(key(1), vec![1; 6]).unwrap();
        drop(first);
        probe.set(2);
        let replacement = cache.admit(key(2), vec![2; 3]).unwrap();
        assert_eq!(replacement.bytes(), &[2, 2, 2]);
        assert_eq!(stats(&cache).evictions, 1);
        assert_eq!(stats(&cache).resident_bytes, 3);
        drop(replacement);
    }

    #[test]
    fn sem_entrada_ociosa_a_admissao_nao_expulsa_lease_ativo() {
        let probe = Arc::new(FixedProbe::new(4));
        let cache = cache_with_baseline(&probe, 4);
        let _active = cache.admit(key(1), vec![1; 4]).unwrap();
        assert!(matches!(
            cache.admit(key(2), vec![2]),
            Err(CacheError::BusyAndNoEviction { .. })
        ));
        assert!(cache.lookup(&key(1)).is_some());
    }

    struct FakeDeviceAllocation {
        handle: usize,
        bytes: usize,
    }

    impl DeviceAllocation for FakeDeviceAllocation {
        fn bytes(&self) -> usize {
            self.bytes
        }

        fn handle(&self) -> usize {
            self.handle
        }
    }

    struct FakeDeviceAllocator {
        free: AtomicU64,
        uploads: AtomicU64,
    }

    impl FakeDeviceAllocator {
        fn new(free: u64) -> Arc<Self> {
            Arc::new(Self {
                free: AtomicU64::new(free),
                uploads: AtomicU64::new(0),
            })
        }
    }

    impl DeviceAllocator for FakeDeviceAllocator {
        fn device_name(&self) -> &str {
            "fake-device"
        }

        fn free_bytes(&self) -> io::Result<u64> {
            Ok(self.free.load(Ordering::Relaxed))
        }

        fn upload(&self, payload: &[u8]) -> Result<Box<dyn DeviceAllocation>, String> {
            self.uploads.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(FakeDeviceAllocation {
                handle: self.uploads.load(Ordering::Relaxed) as usize,
                bytes: payload.len(),
            }))
        }
    }

    fn device_cache(allocator: &Arc<FakeDeviceAllocator>, baseline: u64) -> Arc<DeviceExpertCache> {
        DeviceExpertCache::new(
            MemoryAccount {
                baseline_free_bytes: baseline,
                compute_bytes: 0,
                weights_bytes: 0,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            Arc::clone(allocator) as Arc<dyn DeviceAllocator>,
        )
    }

    #[test]
    fn cache_device_hit_reutiliza_handle_sem_novo_upload() {
        let allocator = FakeDeviceAllocator::new(100);
        let cache = device_cache(&allocator, 100);
        let first = cache.admit(key(1), &[1, 2, 3]).unwrap();
        let handle = first.handle();
        assert_eq!(first.bytes(), 3);
        drop(first);

        let second = cache.lookup(&key(1)).unwrap();
        assert_eq!(second.handle(), handle);
        assert_eq!(second.bytes(), 3);
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().upload_bytes, 3);
    }

    #[test]
    fn cache_device_nao_expulsa_lease_ativo() {
        let allocator = FakeDeviceAllocator::new(4);
        let cache = device_cache(&allocator, 4);
        let active = cache.admit(key(1), &[1, 1, 1]).unwrap();
        assert!(matches!(
            cache.admit(key(2), &[2, 2]),
            Err(DeviceCacheError::BusyAndNoEviction { .. })
        ));
        drop(active);
    }

    #[test]
    fn cache_device_recusa_payload_que_nao_cabe_no_orcamento_medido() {
        let allocator = FakeDeviceAllocator::new(2);
        let cache = device_cache(&allocator, 2);
        let error = match cache.admit(key(1), &[1, 2, 3]) {
            Ok(_) => panic!("payload fora do orçamento foi admitido"),
            Err(error) => error,
        };
        assert!(matches!(error, DeviceCacheError::NoBudget { .. }));
        assert!(error.to_string().contains("fake-device"));
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cache_device_recusa_expert_quando_workspace_medido_consumiu_o_orcamento() {
        let allocator = FakeDeviceAllocator::new(250);
        let account = MemoryAccount::for_device_cache(250, 1_000).unwrap();
        let cache =
            DeviceExpertCache::new(account, Arc::clone(&allocator) as Arc<dyn DeviceAllocator>);
        let key = ExpertKey::new("Gemma4", 29, 7, "nvfp4");

        let error = match cache.admit(key, &[1]) {
            Ok(_) => panic!("expert foi admitido sem orçamento para o workspace"),
            Err(error) => error,
        };

        assert!(matches!(error, DeviceCacheError::BudgetExhausted { .. }));
        assert!(error.to_string().contains("orçamento global"));
        assert!(!error.to_string().contains("Gemma4"));
        assert!(!error.to_string().contains("nvfp4"));
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn warmup_admite_ate_o_orcamento_e_reporta_limite() {
        let allocator = FakeDeviceAllocator::new(5);
        let cache = device_cache(&allocator, 5);
        let candidates = vec![Ok((key(1), vec![1; 3])), Ok((key(2), vec![2; 6]))];

        let report = cache.warm(
            candidates,
            &DeviceWarmupOptions::new(Duration::from_secs(1)),
        );

        assert_eq!(report.stop, DeviceWarmupStop::BudgetExhausted);
        assert_eq!(report.candidates, 2);
        assert_eq!(report.admitted, 1);
        assert_eq!(report.admitted_bytes, 3);
        assert_eq!(report.resident_bytes, 3);
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn warmup_cancelado_nao_carrega_o_primeiro_payload() {
        let allocator = FakeDeviceAllocator::new(10);
        let cache = device_cache(&allocator, 10);
        let options = DeviceWarmupOptions::new(Duration::from_secs(1));
        options.cancellation.store(true, Ordering::Release);

        let report = cache.warm(vec![Ok((key(1), vec![1; 3]))], &options);

        assert_eq!(report.stop, DeviceWarmupStop::Cancelled);
        assert_eq!(report.candidates, 0);
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn warmup_com_prazo_zero_e_observavel() {
        let allocator = FakeDeviceAllocator::new(10);
        let cache = device_cache(&allocator, 10);

        let report = cache.warm(
            vec![Ok((key(1), vec![1; 3]))],
            &DeviceWarmupOptions::new(Duration::ZERO),
        );

        assert_eq!(report.stop, DeviceWarmupStop::TimedOut);
        assert_eq!(report.candidates, 0);
        assert_eq!(allocator.uploads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn warmup_para_ao_encher_sem_repetir_evictions() {
        let allocator = FakeDeviceAllocator::new(5);
        let cache = device_cache(&allocator, 5);
        let candidates = vec![
            Ok((key(1), vec![1; 3])),
            Ok((key(2), vec![2; 3])),
            Ok((key(3), vec![3; 3])),
        ];

        let report = cache.warm(
            candidates,
            &DeviceWarmupOptions::new(Duration::from_secs(1)),
        );

        assert_eq!(report.stop, DeviceWarmupStop::BudgetExhausted);
        assert_eq!(report.candidates, 2);
        assert_eq!(report.admitted, 1);
        assert_eq!(report.admitted_bytes, 3);
        assert_eq!(report.resident_bytes, 3);
        assert_eq!(cache.stats().evictions, 0);
    }

    #[test]
    fn orcamento_zero_e_diagnosticado_sem_falar_de_um_expert() {
        let allocator = FakeDeviceAllocator::new(1);
        let cache = DeviceExpertCache::new(
            MemoryAccount {
                baseline_free_bytes: 1,
                compute_bytes: 0,
                weights_bytes: 2,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            Arc::clone(&allocator) as Arc<dyn DeviceAllocator>,
        );
        let error = match cache.admit(key(1), &[1]) {
            Ok(_) => panic!("admissão deveria expor orçamento zero"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(message.contains("orçamento global"), "{message}");
        assert!(message.contains("baseline"), "{message}");
        assert!(message.contains("compute"), "{message}");
        assert!(message.contains("reserva"), "{message}");
        assert!(!message.contains("expert ExpertKey"), "{message}");
    }
}
