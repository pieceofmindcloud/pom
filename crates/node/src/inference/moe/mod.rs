//! Componentes do executor próprio de modelos MoE.
//!
//! O módulo começa pela política de cache e admissão, que não depende de
//! llama.cpp. O forward completo e o primeiro formato de peso entram depois
//! que a fronteira de carregamento do GGUF estiver coberta por um modelo real.

pub mod admission;
pub mod cache;
pub mod format_q4_0;
