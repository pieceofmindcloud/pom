//! Primitivas de execução simultânea das lanes CPU e GPU.

use super::bandwidth::{BandwidthError, BandwidthProfile};
use std::fmt;
use std::thread;

#[derive(Debug)]
pub enum OverlapError<CE, GE> {
    Cpu(CE),
    Gpu(GE),
    WorkerPanicked,
}

impl<CE: fmt::Display, GE: fmt::Display> fmt::Display for OverlapError<CE, GE> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu(error) => write!(formatter, "executor CPU falhou: {error}"),
            Self::Gpu(error) => write!(formatter, "executor GPU falhou: {error}"),
            Self::WorkerPanicked => formatter.write_str("worker de co-execução entrou em panic"),
        }
    }
}

impl<CE: fmt::Debug + fmt::Display, GE: fmt::Debug + fmt::Display> std::error::Error
    for OverlapError<CE, GE>
{
}

pub fn run_overlap<CT, GT, CE, GE, CF, GF>(
    cpu: CF,
    gpu: GF,
) -> Result<(CT, GT), OverlapError<CE, GE>>
where
    CT: Send,
    GT: Send,
    CE: Send,
    GE: Send,
    CF: FnOnce() -> Result<CT, CE> + Send,
    GF: FnOnce() -> Result<GT, GE> + Send,
{
    thread::scope(|scope| {
        let cpu_handle = scope.spawn(cpu);
        let gpu_handle = scope.spawn(gpu);
        let cpu = cpu_handle
            .join()
            .map_err(|_| OverlapError::WorkerPanicked)?
            .map_err(OverlapError::Cpu)?;
        let gpu = gpu_handle
            .join()
            .map_err(|_| OverlapError::WorkerPanicked)?
            .map_err(OverlapError::Gpu)?;
        Ok((cpu, gpu))
    })
}

#[derive(Debug)]
pub enum MeasuredOverlapError<CE, GE> {
    Profile(BandwidthError),
    Workers(OverlapError<CE, GE>),
}

impl<CE: fmt::Display, GE: fmt::Display> fmt::Display for MeasuredOverlapError<CE, GE> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(error) => error.fmt(formatter),
            Self::Workers(error) => error.fmt(formatter),
        }
    }
}

impl<CE: fmt::Debug + fmt::Display, GE: fmt::Debug + fmt::Display> std::error::Error
    for MeasuredOverlapError<CE, GE>
{
}

pub fn run_measured<CT, GT, CE, GE, CF, GF>(
    profile: BandwidthProfile,
    misses: &[u32],
    cpu: CF,
    gpu: GF,
) -> Result<(CT, GT), MeasuredOverlapError<CE, GE>>
where
    CT: Send,
    GT: Send,
    CE: Send,
    GE: Send,
    CF: FnOnce(&[u32]) -> Result<CT, CE> + Send,
    GF: FnOnce(&[u32]) -> Result<GT, GE> + Send,
{
    let split = profile
        .split_misses(misses)
        .map_err(MeasuredOverlapError::Profile)?;
    run_overlap(|| cpu(&split.cpu), || gpu(&split.gpu)).map_err(MeasuredOverlapError::Workers)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferError {
    InvalidSlot(usize),
    Busy,
    NotInFlight,
    BudgetOverflow,
    InsufficientBudget { needed: u64, available: u64 },
}

impl fmt::Display for BufferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlot(slot) => write!(formatter, "buffer {slot} não existe"),
            Self::Busy => formatter.write_str("buffers duplos sem slot livre"),
            Self::NotInFlight => formatter.write_str("buffer não está em transferência"),
            Self::BudgetOverflow => formatter.write_str("reserva dos buffers duplos excede u64"),
            Self::InsufficientBudget { needed, available } => write!(
                formatter,
                "buffers duplos exigem {needed} bytes, mas há {available} disponíveis"
            ),
        }
    }
}

impl std::error::Error for BufferError {}

#[derive(Debug)]
struct BufferSlot<T> {
    value: Option<T>,
    in_flight: bool,
}

pub struct DoubleBuffer<T> {
    slots: [BufferSlot<T>; 2],
    next: usize,
}

impl<T> DoubleBuffer<T> {
    pub fn new() -> Self {
        Self {
            slots: [
                BufferSlot {
                    value: None,
                    in_flight: false,
                },
                BufferSlot {
                    value: None,
                    in_flight: false,
                },
            ],
            next: 0,
        }
    }

    pub fn with_budget(slot_bytes: u64, available_bytes: u64) -> Result<Self, BufferError> {
        let needed = slot_bytes
            .checked_mul(2)
            .ok_or(BufferError::BudgetOverflow)?;
        if needed > available_bytes {
            return Err(BufferError::InsufficientBudget {
                needed,
                available: available_bytes,
            });
        }
        Ok(Self::new())
    }

    pub fn begin(&mut self, value: T) -> Result<usize, BufferError> {
        for offset in 0..self.slots.len() {
            let slot = (self.next + offset) % self.slots.len();
            if !self.slots[slot].in_flight {
                self.slots[slot].value = Some(value);
                self.slots[slot].in_flight = true;
                self.next = (slot + 1) % self.slots.len();
                return Ok(slot);
            }
        }
        Err(BufferError::Busy)
    }

    pub fn complete(&mut self, slot: usize) -> Result<T, BufferError> {
        let entry = self
            .slots
            .get_mut(slot)
            .ok_or(BufferError::InvalidSlot(slot))?;
        if !entry.in_flight {
            return Err(BufferError::NotInFlight);
        }
        entry.in_flight = false;
        entry.value.take().ok_or(BufferError::NotInFlight)
    }
}

impl<T> Default for DoubleBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn cpu_e_gpu_comecam_juntos() {
        let barrier = Arc::new(Barrier::new(2));
        let cpu_barrier = Arc::clone(&barrier);
        let gpu_barrier = Arc::clone(&barrier);
        let result = run_overlap(
            move || {
                cpu_barrier.wait();
                Ok::<_, ()>("cpu")
            },
            move || {
                gpu_barrier.wait();
                Ok::<_, ()>("gpu")
            },
        )
        .unwrap();
        assert_eq!(result, ("cpu", "gpu"));
    }

    #[test]
    fn coexecucao_usa_banda_medida_para_dividir_misses() {
        let profile = crate::moe::bandwidth::BandwidthProfile {
            cpu_overlap_bytes_per_sec: 3.0,
            transfer_overlap_bytes_per_sec: 1.0,
            cpu_bytes_per_sec: 30.0,
            transfer_bytes_per_sec: 10.0,
        };
        let result = run_measured(
            profile,
            &[10, 11, 12, 13],
            |cpu| Ok::<_, ()>(cpu.to_vec()),
            |gpu| Ok::<_, ()>(gpu.to_vec()),
        )
        .unwrap();
        assert_eq!(result, (vec![11, 12, 13], vec![10]));
    }

    #[test]
    fn coexecucao_preserva_erro_da_lane_gpu() {
        let profile = crate::moe::bandwidth::BandwidthProfile {
            cpu_overlap_bytes_per_sec: 1.0,
            transfer_overlap_bytes_per_sec: 1.0,
            cpu_bytes_per_sec: 1.0,
            transfer_bytes_per_sec: 1.0,
        };
        let result = run_measured(
            profile,
            &[7],
            |cpu| Ok::<_, &'static str>(cpu.len()),
            |_gpu| Err::<usize, _>("transfer falhou"),
        );
        assert!(matches!(
            result,
            Err(MeasuredOverlapError::Workers(OverlapError::Gpu(
                "transfer falhou"
            )))
        ));
    }

    #[test]
    fn buffer_duplo_recusa_reserva_sem_orcamento() {
        assert!(matches!(
            DoubleBuffer::<u8>::with_budget(9, 17),
            Err(BufferError::InsufficientBudget {
                needed: 18,
                available: 17
            })
        ));
        assert!(DoubleBuffer::<u8>::with_budget(9, 18).is_ok());
    }
}
