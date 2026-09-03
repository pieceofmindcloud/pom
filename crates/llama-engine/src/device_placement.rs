const MIN_CONFIDENT_SPEED_RATIO: f64 = 1.25;

/// Returns a device permutation ordered from slowest to fastest only when the
/// measurements are complete and every adjacent speed boundary is clear.
///
/// A missing/non-finite/non-positive score, or a difference small enough to be
/// benchmark noise, returns `None` so callers keep llama.cpp's current order.
pub fn confident_slow_to_fast_order(scores: &[Option<f64>]) -> Option<Vec<usize>> {
    if scores.len() <= 1 {
        return None;
    }

    if scores
        .iter()
        .any(|score| !matches!(score, Some(value) if value.is_finite() && *value > 0.0))
    {
        return None;
    }

    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&left, &right| {
        scores[left]
            .unwrap()
            .partial_cmp(&scores[right].unwrap())
            .expect("finite device speed scores are comparable")
    });

    if order.windows(2).any(|pair| {
        let slower = scores[pair[0]].unwrap();
        let faster = scores[pair[1]].unwrap();
        faster / slower < MIN_CONFIDENT_SPEED_RATIO
    }) {
        return None;
    }

    Some(order)
}

/// Returns a speed order only when all devices use the same backend.
///
/// The llama.cpp scheduler supports an explicit device order for a homogeneous
/// backend, but a mixed backend graph has backend-specific split and copy
/// paths. Keep llama.cpp's default order when backend identity is incomplete or
/// heterogeneous, so a benchmark cannot change that graph topology.
pub fn confident_slow_to_fast_order_for_backends(
    scores: &[Option<f64>],
    backend_names: &[Option<&str>],
) -> Option<Vec<usize>> {
    if scores.len() != backend_names.len()
        || backend_names.len() <= 1
        || backend_names.iter().any(Option::is_none)
        || backend_names.windows(2).any(|pair| pair[0] != pair[1])
    {
        return None;
    }

    confident_slow_to_fast_order(scores)
}

#[cfg(test)]
mod tests {
    use super::{confident_slow_to_fast_order, confident_slow_to_fast_order_for_backends};

    #[test]
    fn um_dispositivo_mantem_a_ordem_atual() {
        assert_eq!(confident_slow_to_fast_order(&[Some(100.0)]), None);
    }

    #[test]
    fn velocidades_claramente_diferentes_ordenam_do_lento_ao_rapido() {
        assert_eq!(
            confident_slow_to_fast_order(&[Some(400.0), Some(100.0)]),
            Some(vec![1, 0])
        );
    }

    #[test]
    fn velocidades_equivalentes_mantem_a_ordem_atual() {
        assert_eq!(
            confident_slow_to_fast_order(&[Some(100.0), Some(105.0)]),
            None
        );
    }

    #[test]
    fn velocidade_indeterminada_mantem_a_ordem_atual() {
        assert_eq!(confident_slow_to_fast_order(&[Some(400.0), None]), None);
        assert_eq!(confident_slow_to_fast_order(&[None, None]), None);
        assert_eq!(confident_slow_to_fast_order(&[Some(f64::NAN), Some(400.0)]), None);
        assert_eq!(confident_slow_to_fast_order(&[Some(0.0), Some(400.0)]), None);
    }

    #[test]
    fn pouca_memoria_nao_passa_a_frente_da_velocidade_mensuravel() {
        // O dispositivo 0 tem mais memória, mas o 1 é o rápido. A ordem de
        // carregamento deve continuar sendo decidida pela medição, não pelos
        // bytes livres que serão usados pelo fit.
        let synthetic_devices = [(80.0, 16_u64), (400.0, 4_u64)];
        let scores = synthetic_devices
            .iter()
            .map(|&(speed, _free_gib)| Some(speed))
            .collect::<Vec<_>>();
        let order = confident_slow_to_fast_order(&scores);

        assert_eq!(order, Some(vec![0, 1]));
    }

    #[test]
    fn backends_heterogeneos_nao_recebem_ordem_explicita() {
        let scores = [Some(400.0), Some(100.0)];
        let backends = [Some("Vulkan"), Some("CUDA")];

        assert_eq!(
            confident_slow_to_fast_order_for_backends(&scores, &backends),
            None
        );
    }

    #[test]
    fn backend_homogeneo_preserva_ordenacao_confiavel() {
        let scores = [Some(400.0), Some(100.0)];
        let backends = [Some("CUDA"), Some("CUDA")];

        assert_eq!(
            confident_slow_to_fast_order_for_backends(&scores, &backends),
            Some(vec![1, 0])
        );
    }
}
