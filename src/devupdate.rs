//! Atualizar e reiniciar um nó pela interface, em desenvolvimento.
//!
//! Isto é execução remota de código: quem tem a chave de conexão passa a poder
//! rodar um build e trocar o binário da máquina. Por isso o portão é uma
//! variável de ambiente, ligada por processo por quem é dono da máquina, e não
//! um campo de config — a config é reescrita pela própria interface, o que
//! deixaria o portão ligável pela rede.

/// Nome da variável que abre o recurso.
pub const ENV_GATE: &str = "RDMA_P2P_DEV_UPDATE";

/// Variável que diz com que features esta máquina compila.
pub const ENV_FEATURES: &str = "RDMA_P2P_BUILD_FEATURES";

/// Este nó aceita pedidos de atualização?
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV_GATE).ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    v == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O portão é por processo e não é ligável pela rede — diferente da config,
    /// que a própria interface reescreve. Só "1" liga: um "true" ou "yes" que
    /// alguém escreveu achando que ligava não deve abrir execução remota.
    #[test]
    fn so_o_valor_1_liga_o_portao() {
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("true")));
        assert!(!enabled_from(Some("")));
        assert!(!enabled_from(None));
    }
}
