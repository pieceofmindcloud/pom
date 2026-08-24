//! Componentes do executor próprio de modelos MoE.
//!
//! O caminho denso não importa este módulo. O ponto de integração do motor
//! próprio é `Q4_0MoeBlock`, que combina cache, loader GGUF, router e executor
//! CPU para o primeiro formato suportado.

pub mod admission;
pub mod block;
pub mod cache;
pub mod format_iq_quants;
pub mod format_k_quants;
pub mod format_legacy_quants;
pub mod format_nvfp4;
pub mod format_plain;
pub mod format_q4_0;
pub mod router;
pub mod runtime;
