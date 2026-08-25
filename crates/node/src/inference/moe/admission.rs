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

/// Margem relativa ao buffer de computação medido no contexto llama.cpp.
///
/// O valor não é uma capacidade de placa: ele é aplicado ao número que o
/// próprio llama.cpp reportou para acomodar crescimento de grafo e
/// fragmentação do allocator. Uma margem de 25% preserva um quarto do buffer
/// medido além do tamanho observado, sem depender de modelo, contexto ou
/// hardware específicos.
#[cfg(any(feature = "cuda", test))]
const COMPUTE_HEADROOM_NUMERATOR: u64 = 5;
#[cfg(any(feature = "cuda", test))]
const COMPUTE_HEADROOM_DENOMINATOR: u64 = 4;

/// Converte o tamanho observado do compute buffer em reserva de admissão.
#[cfg(any(feature = "cuda", test))]
pub fn device_cache_reserve_bytes(compute_bytes: u64) -> Option<u64> {
    if compute_bytes == 0 {
        return None;
    }
    compute_bytes
        .checked_mul(COMPUTE_HEADROOM_NUMERATOR)?
        .checked_add(COMPUTE_HEADROOM_DENOMINATOR - 1)?
        .checked_div(COMPUTE_HEADROOM_DENOMINATOR)
}

/// Extrai os compute buffers residentes na memória CUDA do relatório do
/// contexto llama.cpp.
///
/// `CUDA_Host` é memória de host e não pode ser usada como reserva de VRAM.
/// Um relatório ausente, incompleto ou sem compute CUDA positivo retorna
/// `None`, para que o chamador recuse o executor residente em vez de assumir
/// zero.
pub fn cuda_compute_bytes_from_llama_memory(memory: &serde_json::Value) -> Option<u64> {
    let buffers = memory.get("buffers")?.as_array()?;
    let mut total = 0u64;
    let mut found = false;

    for buffer in buffers {
        let name = buffer.get("buffer")?.as_str()?;
        if !name.starts_with("CUDA") || name.ends_with("_Host") {
            continue;
        }
        let device = buffer.get("device")?.as_str()?;
        if !device.starts_with("CUDA") {
            return None;
        }
        let compute = buffer.get("compute")?.as_u64()?;
        total = total.checked_add(compute)?;
        found |= compute > 0;
    }

    found.then_some(total)
}

impl MemoryAccount {
    /// Cria o orçamento do cache de device a partir da medição do llama.cpp.
    ///
    /// Os pesos e buffers do contexto já estão refletidos na leitura de VRAM
    /// livre feita depois da criação do contexto. O compute buffer medido fica
    /// como reserva adicional, com a margem definida acima.
    #[cfg(any(feature = "cuda", test))]
    pub fn for_device_cache(baseline_free_bytes: u64, compute_bytes: u64) -> Option<Self> {
        Some(Self {
            baseline_free_bytes,
            weights_bytes: 0,
            fixed_bytes: 0,
            reserve_bytes: device_cache_reserve_bytes(compute_bytes)?,
        })
    }

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

    #[test]
    fn extrai_compute_cuda_do_relatorio_llama_e_ignora_cuda_host() {
        let memory = serde_json::json!({
            "buffers": [
                {"buffer": "CUDA_Host", "device": "CUDA0", "compute": 200},
                {"buffer": "CUDA0", "device": "CUDA0", "compute": 1000},
                {"buffer": "CPU_Mapped", "device": "CPU", "compute": 3000}
            ]
        });

        assert_eq!(cuda_compute_bytes_from_llama_memory(&memory), Some(1000));
    }

    #[test]
    fn relatorio_sem_compute_cuda_e_recusado() {
        let memory = serde_json::json!({
            "buffers": [
                {"buffer": "CUDA_Host", "device": "CUDA0", "compute": 200},
                {"buffer": "CPU_Mapped", "device": "CPU", "compute": 3000}
            ]
        });

        assert_eq!(cuda_compute_bytes_from_llama_memory(&memory), None);
    }

    #[test]
    fn reserva_do_cache_e_derivada_do_compute_medido_com_margem() {
        assert_eq!(device_cache_reserve_bytes(1_000), Some(1_250));
        assert_eq!(device_cache_reserve_bytes(1), Some(2));
        assert_eq!(device_cache_reserve_bytes(0), None);
    }

    #[test]
    fn account_do_cache_de_device_preserva_a_reserva_medida() {
        let account = MemoryAccount::for_device_cache(10_000, 1_000).unwrap();

        assert_eq!(account.reserve_bytes, 1_250);
        assert_eq!(account.cache_limit_bytes(10_000, 0), Some(8_750));
    }
}
