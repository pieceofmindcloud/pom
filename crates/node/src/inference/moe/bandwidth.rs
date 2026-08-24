//! Perfil medido e divisão de misses entre CPU e GPU.
//!
//! Esta camada não mede nem inventa banda. O captain injeta os bytes por
//! segundo obtidos no hardware e a política só transforma os números em uma
//! fração determinística de misses enviados pela transferência.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandwidthProfile {
    /// Banda do GEMV de expert na CPU durante a disputa real com a cópia.
    pub cpu_overlap_bytes_per_sec: f64,
    /// Banda da cópia host para o dispositivo durante a disputa real.
    pub transfer_overlap_bytes_per_sec: f64,
    /// Fallback para perfis antigos sem ensaio simultâneo.
    pub cpu_bytes_per_sec: f64,
    pub transfer_bytes_per_sec: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthError {
    MissingCpuMeasurement,
    MissingTransferMeasurement,
    InvalidMeasurement,
}

impl fmt::Display for BandwidthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCpuMeasurement => write!(formatter, "banda CPU sem medição"),
            Self::MissingTransferMeasurement => {
                write!(formatter, "banda de transferência sem medição")
            }
            Self::InvalidMeasurement => write!(formatter, "perfil de banda não finito ou negativo"),
        }
    }
}

impl std::error::Error for BandwidthError {}

impl BandwidthProfile {
    pub fn fetch_fraction(self) -> Result<f32, BandwidthError> {
        let overlap_cpu = valid(self.cpu_overlap_bytes_per_sec);
        let overlap_transfer = valid(self.transfer_overlap_bytes_per_sec);
        let standalone_cpu = valid(self.cpu_bytes_per_sec);
        let standalone_transfer = valid(self.transfer_bytes_per_sec);

        let (cpu, transfer) = if overlap_cpu && overlap_transfer {
            (
                self.cpu_overlap_bytes_per_sec,
                self.transfer_overlap_bytes_per_sec,
            )
        } else if standalone_cpu && standalone_transfer {
            (self.cpu_bytes_per_sec, self.transfer_bytes_per_sec)
        } else {
            if !overlap_cpu && !standalone_cpu {
                return Err(BandwidthError::MissingCpuMeasurement);
            }
            if !overlap_transfer && !standalone_transfer {
                return Err(BandwidthError::MissingTransferMeasurement);
            }
            return Err(BandwidthError::InvalidMeasurement);
        };

        let denominator = cpu + transfer;
        if !denominator.is_finite() || denominator <= 0.0 {
            return Err(BandwidthError::InvalidMeasurement);
        }
        Ok((transfer / denominator).clamp(0.0, 1.0) as f32)
    }

    pub fn split_misses(self, misses: &[u32]) -> Result<MissSplit, BandwidthError> {
        let fraction = self.fetch_fraction()?;
        let fetched = (misses.len() as f32 * fraction).round() as usize;
        Ok(MissSplit {
            fetched: misses[..fetched.min(misses.len())].to_vec(),
            cpu: misses[fetched.min(misses.len())..].to_vec(),
            fraction,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MissSplit {
    pub fetched: Vec<u32>,
    pub cpu: Vec<u32>,
    pub fraction: f32,
}

fn valid(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> BandwidthProfile {
        BandwidthProfile {
            cpu_overlap_bytes_per_sec: 3.0,
            transfer_overlap_bytes_per_sec: 1.0,
            cpu_bytes_per_sec: 30.0,
            transfer_bytes_per_sec: 10.0,
        }
    }

    #[test]
    fn usa_a_banda_sobreposta_quando_existe() {
        assert!((profile().fetch_fraction().unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn divide_misses_de_forma_deterministica() {
        let split = profile().split_misses(&[10, 11, 12, 13]).unwrap();
        assert_eq!(split.fetched, vec![10]);
        assert_eq!(split.cpu, vec![11, 12, 13]);
    }

    #[test]
    fn usa_fallback_de_perfil_antigo() {
        let mut old = profile();
        old.cpu_overlap_bytes_per_sec = 0.0;
        old.transfer_overlap_bytes_per_sec = 0.0;
        assert!((old.fetch_fraction().unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn perfil_incompleto_nao_escolhe_overlap() {
        let mut incomplete = profile();
        incomplete.cpu_overlap_bytes_per_sec = 0.0;
        incomplete.transfer_overlap_bytes_per_sec = 0.0;
        incomplete.cpu_bytes_per_sec = 0.0;
        assert_eq!(
            incomplete.fetch_fraction(),
            Err(BandwidthError::MissingCpuMeasurement)
        );
    }
}
