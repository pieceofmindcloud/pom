use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const DEFAULT_LIMIT_C: f64 = 85.0;
pub const DEFAULT_HYSTERESIS_C: f64 = 8.0;
pub const DEFAULT_COOLDOWN_SECS: u64 = 30;
pub const MIN_LIMIT_C: f64 = 70.0;
pub const MAX_COOLDOWN_SECS: u64 = 60;

fn default_limit_c() -> f64 {
    DEFAULT_LIMIT_C
}

fn default_hysteresis_c() -> f64 {
    DEFAULT_HYSTERESIS_C
}

fn default_cooldown_secs() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

/// Thermal limits for the GPUs used by this node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ThermalPolicy {
    #[serde(default = "default_limit_c")]
    pub limit_c: f64,
    #[serde(default = "default_hysteresis_c")]
    pub hysteresis_c: f64,
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

impl Default for ThermalPolicy {
    fn default() -> Self {
        Self {
            limit_c: DEFAULT_LIMIT_C,
            hysteresis_c: DEFAULT_HYSTERESIS_C,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        }
    }
}

impl ThermalPolicy {
    pub fn resume_limit_c(self) -> f64 {
        self.limit_c - self.hysteresis_c
    }

    pub fn validate(self) -> anyhow::Result<()> {
        if !self.limit_c.is_finite() || self.limit_c < MIN_LIMIT_C || self.limit_c > 150.0 {
            anyhow::bail!("o limite térmico precisa estar entre 70 e 150 °C");
        }
        if !self.hysteresis_c.is_finite()
            || self.hysteresis_c <= 0.0
            || self.hysteresis_c >= self.limit_c
        {
            anyhow::bail!("a margem térmica precisa ser maior que zero e menor que o limite");
        }
        if self.cooldown_secs == 0 {
            anyhow::bail!("o tempo de resfriamento precisa ser maior que zero");
        }
        if self.cooldown_secs > MAX_COOLDOWN_SECS {
            anyhow::bail!("o tempo de resfriamento não pode passar de 60 segundos");
        }
        Ok(())
    }
}

/// A decision emitted by the pure thermal state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThermalAction {
    None,
    Suspend { observed_c: f64 },
    Resume { observed_c: f64 },
}

/// Hysteresis and cooldown independent of any specific GPU vendor.
pub struct ThermalMachine {
    policy: ThermalPolicy,
    suspended: bool,
    safe_since: Option<Instant>,
}

impl ThermalMachine {
    pub fn new(policy: ThermalPolicy) -> Self {
        Self {
            policy,
            suspended: false,
            safe_since: None,
        }
    }

    pub fn set_policy(&mut self, policy: ThermalPolicy) {
        if self.policy != policy {
            self.policy = policy;
            self.safe_since = None;
        }
    }

    #[cfg(test)]
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn observe(&mut self, readings: &[Option<f64>], now: Instant) -> ThermalAction {
        let known: Vec<f64> = readings
            .iter()
            .copied()
            .flatten()
            .filter(|value| value.is_finite())
            .collect();
        let Some(&observed_c) = known.iter().max_by(|a, b| a.total_cmp(b)) else {
            self.safe_since = None;
            return ThermalAction::None;
        };

        if observed_c > self.policy.limit_c {
            self.safe_since = None;
            if !self.suspended {
                self.suspended = true;
                return ThermalAction::Suspend { observed_c };
            }
            return ThermalAction::None;
        }

        if !self.suspended {
            return ThermalAction::None;
        }

        // Unknown temperatures are deliberately not high, but they also cannot
        // prove that every GPU is safe enough to turn the model back on.
        let all_known = readings.len() == known.len()
            && known
                .iter()
                .all(|temperature| *temperature <= self.policy.resume_limit_c());
        if !all_known {
            self.safe_since = None;
            return ThermalAction::None;
        }

        let since = *self.safe_since.get_or_insert(now);
        if now.saturating_duration_since(since) < Duration::from_secs(self.policy.cooldown_secs) {
            return ThermalAction::None;
        }

        self.suspended = false;
        self.safe_since = None;
        ThermalAction::Resume { observed_c }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn policy() -> ThermalPolicy {
        ThermalPolicy {
            limit_c: 85.0,
            hysteresis_c: 8.0,
            cooldown_secs: 30,
        }
    }

    #[test]
    fn suspende_apenas_quando_uma_temperatura_conhecida_passa_do_limite() {
        let now = Instant::now();
        let mut machine = ThermalMachine::new(policy());

        assert_eq!(machine.observe(&[None], now), ThermalAction::None);
        assert_eq!(
            machine.observe(&[Some(85.0)], now + Duration::from_secs(1)),
            ThermalAction::None,
            "igual ao limite não passa do limite"
        );
        assert_eq!(
            machine.observe(&[Some(85.1)], now + Duration::from_secs(2)),
            ThermalAction::Suspend { observed_c: 85.1 }
        );
    }

    #[test]
    fn leitura_indisponivel_nunca_e_temperatura_alta() {
        let now = Instant::now();
        let mut machine = ThermalMachine::new(policy());

        assert_eq!(machine.observe(&[None, None], now), ThermalAction::None);
        assert!(!machine.is_suspended());
    }

    #[test]
    fn retomada_exige_todas_as_gpus_conhecidas_frias_pelo_cooldown() {
        let now = Instant::now();
        let mut machine = ThermalMachine::new(policy());
        assert_eq!(
            machine.observe(&[Some(90.0), Some(70.0)], now),
            ThermalAction::Suspend { observed_c: 90.0 }
        );

        assert_eq!(
            machine.observe(&[Some(76.0), None], now + Duration::from_secs(60)),
            ThermalAction::None,
            "GPU sem leitura não prova que é seguro religar"
        );
        assert_eq!(
            machine.observe(&[Some(76.0), Some(77.0)], now + Duration::from_secs(61)),
            ThermalAction::None
        );
        assert_eq!(
            machine.observe(&[Some(76.0), Some(77.0)], now + Duration::from_secs(90)),
            ThermalAction::None,
            "a primeira amostra segura inicia o cooldown"
        );
        assert_eq!(
            machine.observe(&[Some(76.0), Some(77.0)], now + Duration::from_secs(92)),
            ThermalAction::Resume { observed_c: 77.0 }
        );
        assert!(!machine.is_suspended());
    }

    #[test]
    fn temperatura_acima_do_limite_de_religar_interrompe_o_cooldown() {
        let now = Instant::now();
        let mut machine = ThermalMachine::new(policy());
        assert_eq!(
            machine.observe(&[Some(90.0)], now),
            ThermalAction::Suspend { observed_c: 90.0 }
        );
        assert_eq!(
            machine.observe(&[Some(76.0)], now + Duration::from_secs(1)),
            ThermalAction::None
        );
        assert_eq!(
            machine.observe(&[Some(78.0)], now + Duration::from_secs(20)),
            ThermalAction::None
        );
        assert_eq!(
            machine.observe(&[Some(76.0)], now + Duration::from_secs(21)),
            ThermalAction::None
        );
        assert_eq!(
            machine.observe(&[Some(76.0)], now + Duration::from_secs(50)),
            ThermalAction::None,
            "o cooldown recomeça depois da leitura acima do limiar"
        );
        assert_eq!(
            machine.observe(&[Some(76.0)], now + Duration::from_secs(52)),
            ThermalAction::Resume { observed_c: 76.0 }
        );
    }

    #[test]
    fn politica_recusa_limites_que_nao_formam_histerese_valida() {
        let mut invalid = policy();
        invalid.limit_c = f64::NAN;
        assert!(invalid.validate().is_err());

        invalid = policy();
        invalid.hysteresis_c = 85.0;
        assert!(invalid.validate().is_err());

        invalid = policy();
        invalid.cooldown_secs = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn politica_respeita_os_limites_de_integridade_da_rede() {
        let mut valid = policy();
        valid.limit_c = 70.0;
        valid.cooldown_secs = 60;
        assert!(valid.validate().is_ok());

        valid.limit_c = 69.9;
        assert!(valid.validate().is_err());

        valid = policy();
        valid.cooldown_secs = 60;
        assert!(valid.validate().is_ok());
        valid.cooldown_secs = 61;
        assert!(valid.validate().is_err());
    }
}
