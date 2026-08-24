//! Orçamento puro para o cache de experts.
//!
//! A política recebe leituras de memória feitas pelo processo chamador. Não
//! escolhe uma constante por placa e não consulta CUDA: isso deixa a aritmética
//! testável sem runtime nativo.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAccount {
    /// Memória livre medida antes de reservar pesos e caches deste executor.
    pub baseline_free_bytes: u64,
    /// Peso já residente, contado contra o baseline.
    pub weights_bytes: u64,
    /// Buffers fixos que não entram no LRU.
    pub fixed_bytes: u64,
    /// Margem que deve continuar livre para ativações e alocações transitórias.
    pub reserve_bytes: u64,
}

impl MemoryAccount {
    /// Limite total de bytes do cache neste instante.
    ///
    /// O primeiro termo é o orçamento estável do processo. O segundo usa uma
    /// nova leitura de memória livre e o cache já residente, protegendo contra
    /// outro processo que tenha consumido memória desde a inicialização.
    pub fn cache_limit_bytes(&self, current_free_bytes: u64, resident_bytes: u64) -> Option<u64> {
        let fixed = self
            .weights_bytes
            .checked_add(self.fixed_bytes)?
            .checked_add(self.reserve_bytes)?;
        let baseline_limit = self.baseline_free_bytes.checked_sub(fixed)?;
        let dynamic_limit = current_free_bytes
            .checked_add(resident_bytes)?
            .checked_sub(self.reserve_bytes)?;
        Some(baseline_limit.min(dynamic_limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limita_pelo_baseline_medido_descontando_peso_e_reserva() {
        let account = MemoryAccount {
            baseline_free_bytes: 1_000,
            weights_bytes: 200,
            fixed_bytes: 100,
            reserve_bytes: 100,
        };

        assert_eq!(account.cache_limit_bytes(900, 0), Some(600));
    }

    #[test]
    fn uma_nova_medicao_reduz_o_limite_sem_expulsar_o_cache_existente() {
        let account = MemoryAccount {
            baseline_free_bytes: 1_000,
            weights_bytes: 200,
            fixed_bytes: 100,
            reserve_bytes: 100,
        };

        // A leitura atual informa 250 bytes livres e há 300 bytes residentes.
        // Apenas 450 bytes podem continuar no cache sem consumir a reserva.
        assert_eq!(account.cache_limit_bytes(250, 300), Some(450));
    }

    #[test]
    fn recusa_aritmetica_sem_orcamento() {
        let account = MemoryAccount {
            baseline_free_bytes: 100,
            weights_bytes: 80,
            fixed_bytes: 30,
            reserve_bytes: 20,
        };

        assert_eq!(account.cache_limit_bytes(1, 0), None);
    }

    #[test]
    fn recusa_overflow_na_soma_do_orcamento() {
        let account = MemoryAccount {
            baseline_free_bytes: u64::MAX,
            weights_bytes: u64::MAX,
            fixed_bytes: 1,
            reserve_bytes: 1,
        };

        assert_eq!(account.cache_limit_bytes(0, 0), None);
    }
}
