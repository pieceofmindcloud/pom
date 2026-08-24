use crate::config::KvCacheType;
use anyhow::{bail, Result};

/// Entradas medidas do cache KV de uma fatia.
///
/// A capacidade de contexto é por sessão; `max_sessions` transforma-a na
/// quantidade total de células que o contexto reserva. Nenhuma função deste
/// módulo consulta device, FFI ou arquivo de modelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KvBudgetInput {
    pub context_tokens: u64,
    pub max_sessions: u64,
    pub layers: u64,
    pub kv_heads: u64,
    pub key_head_dim: u64,
    pub value_head_dim: u64,
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KvLayerGeometry {
    pub kv_heads: u64,
    pub key_head_dim: u64,
    pub value_head_dim: u64,
}

/// Bytes de KV para a geometria informada, com falha explícita em aritmética
/// inválida ou overflow.
pub(crate) fn kv_bytes(input: &KvBudgetInput) -> Result<u64> {
    for (name, value) in [
        ("contexto", input.context_tokens),
        ("sessões", input.max_sessions),
        ("camadas", input.layers),
        ("cabeças KV", input.kv_heads),
        ("dimensão K", input.key_head_dim),
        ("dimensão V", input.value_head_dim),
    ] {
        if value == 0 {
            bail!("geometria KV inválida: {name} é zero");
        }
    }

    let geometry = KvLayerGeometry {
        kv_heads: input.kv_heads,
        key_head_dim: input.key_head_dim,
        value_head_dim: input.value_head_dim,
    };
    let bytes_per_layer = kv_bytes_per_layer(&geometry, input.cache_type_k, input.cache_type_v)?;
    let cells = input
        .context_tokens
        .checked_mul(input.max_sessions)
        .ok_or_else(|| anyhow::anyhow!("overflow em context_tokens * max_sessions"))?;
    cells
        .checked_mul(input.layers)
        .and_then(|v| v.checked_mul(bytes_per_layer))
        .ok_or_else(|| anyhow::anyhow!("overflow ao calcular bytes totais do KV"))
}

pub(crate) fn kv_bytes_for_layers(
    context_tokens: u64,
    max_sessions: u64,
    layers: &[KvLayerGeometry],
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
) -> Result<u64> {
    if context_tokens == 0 || max_sessions == 0 || layers.is_empty() {
        bail!("geometria KV inválida: contexto, sessões ou camadas é zero");
    }
    let bytes_per_layer = layers.iter().try_fold(0u64, |total, geometry| {
        total
            .checked_add(kv_bytes_per_layer(geometry, cache_type_k, cache_type_v)?)
            .ok_or_else(|| anyhow::anyhow!("overflow ao somar camadas do KV"))
    })?;
    context_tokens
        .checked_mul(max_sessions)
        .and_then(|cells| cells.checked_mul(bytes_per_layer))
        .ok_or_else(|| anyhow::anyhow!("overflow ao calcular bytes totais do KV"))
}

fn kv_bytes_per_layer(
    geometry: &KvLayerGeometry,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
) -> Result<u64> {
    if geometry.kv_heads == 0 || geometry.key_head_dim == 0 || geometry.value_head_dim == 0 {
        bail!("geometria KV inválida: cabeça ou dimensão é zero");
    }
    let key_elements = geometry
        .kv_heads
        .checked_mul(geometry.key_head_dim)
        .ok_or_else(|| anyhow::anyhow!("overflow ao calcular largura K do KV"))?;
    let value_elements = geometry
        .kv_heads
        .checked_mul(geometry.value_head_dim)
        .ok_or_else(|| anyhow::anyhow!("overflow ao calcular largura V do KV"))?;
    row_bytes(cache_type_k, key_elements)?
        .checked_add(row_bytes(cache_type_v, value_elements)?)
        .ok_or_else(|| anyhow::anyhow!("overflow ao somar linhas K e V do KV"))
}

/// Replica `ggml_row_size` para os tipos de cache aceitos pelo produto.
///
/// Os tipos quantizados só formam uma linha inteira quando a largura fecha o
/// bloco ggml. Recusar aqui evita que o planejador subestime a memória por
/// arredondamento implícito.
fn row_bytes(kind: KvCacheType, elements: u64) -> Result<u64> {
    let (block_bytes, block_elements) = match kind {
        KvCacheType::F32 => (4, 1),
        KvCacheType::F16 | KvCacheType::Bf16 => (2, 1),
        KvCacheType::Q8_0 => (34, 32),
        KvCacheType::Q4_0 | KvCacheType::Iq4Nl => (18, 32),
        KvCacheType::Q4_1 => (20, 32),
        KvCacheType::Q5_0 => (22, 32),
        KvCacheType::Q5_1 => (24, 32),
    };
    if elements % block_elements != 0 {
        bail!("largura KV de {elements} elementos não fecha o bloco de {block_elements} elementos");
    }
    (elements / block_elements)
        .checked_mul(block_bytes)
        .ok_or_else(|| anyhow::anyhow!("overflow ao calcular linha do KV"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KvCacheType;

    #[test]
    fn calcula_kv_com_tipos_assimetricos_e_sessoes() {
        let input = KvBudgetInput {
            context_tokens: 1024,
            max_sessions: 2,
            layers: 4,
            kv_heads: 8,
            key_head_dim: 64,
            value_head_dim: 64,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::Q8_0,
        };

        assert_eq!(kv_bytes(&input).unwrap(), 12_845_056);
    }

    #[test]
    fn recusa_dimensao_que_nao_fecha_bloco_quantizado() {
        let input = KvBudgetInput {
            context_tokens: 1,
            max_sessions: 1,
            layers: 1,
            kv_heads: 1,
            key_head_dim: 33,
            value_head_dim: 32,
            cache_type_k: KvCacheType::Q8_0,
            cache_type_v: KvCacheType::F16,
        };

        assert!(kv_bytes(&input).unwrap_err().to_string().contains("bloco"));
    }

    #[test]
    fn recusa_overflow_em_vez_de_enrolar_o_planejamento() {
        let input = KvBudgetInput {
            context_tokens: u64::MAX,
            max_sessions: 2,
            layers: 2,
            kv_heads: 8,
            key_head_dim: 128,
            value_head_dim: 128,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
        };

        assert!(kv_bytes(&input).is_err());
    }

    #[test]
    fn recusa_zero_sessoes_em_vez_de_inventar_uma_sessao() {
        let input = KvBudgetInput {
            context_tokens: 1024,
            max_sessions: 0,
            layers: 1,
            kv_heads: 8,
            key_head_dim: 128,
            value_head_dim: 128,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
        };

        assert!(kv_bytes(&input)
            .unwrap_err()
            .to_string()
            .contains("sessões"));
    }
}
