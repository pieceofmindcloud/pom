//! Componentes do executor próprio de modelos MoE.
//!
//! O caminho denso não importa este módulo. O ponto de integração do motor
//! próprio é `Q4_0MoeBlock`, que combina cache, loader GGUF, router e executor
//! CPU para o primeiro formato suportado.

pub mod admission;
pub mod bandwidth;
pub mod block;
pub mod cache;
pub mod coexecution;
pub mod format_q4_0;
pub mod router;
