//! Detecção conservadora de ciclos na saída já amostrada.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

/// O ciclo só é considerado depois de três janelas completas. Isso deixa uma
/// tabela ou uma lista curta passar, mesmo quando ela repete a mesma moldura.
pub const DEFAULT_CYCLE_REPETITIONS: u32 = 3;
/// Janelas menores que isto têm alta chance de serem uma estrutura legítima.
pub const DEFAULT_CYCLE_MIN_WINDOW_TOKENS: u32 = 32;
/// Limite de memória e de trabalho por sessão do detector.
pub const DEFAULT_CYCLE_MAX_WINDOW_TOKENS: u32 = 512;
pub const MAX_CYCLE_WINDOW_TOKENS: usize = 1024;
pub const MAX_CYCLE_REPETITIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleConfig {
    pub enabled: bool,
    pub min_window_tokens: usize,
    pub max_window_tokens: usize,
    pub repetitions: usize,
}

impl Default for CycleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_window_tokens: DEFAULT_CYCLE_MIN_WINDOW_TOKENS as usize,
            max_window_tokens: DEFAULT_CYCLE_MAX_WINDOW_TOKENS as usize,
            repetitions: DEFAULT_CYCLE_REPETITIONS as usize,
        }
    }
}

impl CycleConfig {
    fn normalized(self) -> Self {
        let min_window_tokens = self.min_window_tokens.clamp(1, MAX_CYCLE_WINDOW_TOKENS);
        let max_window_tokens = self
            .max_window_tokens
            .clamp(min_window_tokens, MAX_CYCLE_WINDOW_TOKENS);
        Self {
            enabled: self.enabled,
            min_window_tokens,
            max_window_tokens,
            repetitions: self.repetitions.clamp(2, MAX_CYCLE_REPETITIONS),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleMatch {
    pub period: usize,
    pub repetitions: usize,
}

#[derive(Debug, Clone)]
struct TokenSignature {
    canonical: String,
    fingerprint: u64,
}

#[derive(Debug, Default)]
struct IdentifierNormalizer {
    in_identifier: bool,
    in_numeric_suffix: bool,
}

impl IdentifierNormalizer {
    /// Troca somente dígitos que estão dentro de um identificador por `#`.
    /// Números separados por espaços, barras ou pontuação continuam distintos
    /// para que linhas de tabela com valores novos não virem falsos ciclos.
    fn canonical_piece(&mut self, piece: &str) -> String {
        let mut canonical = String::with_capacity(piece.len());
        for character in piece.chars() {
            if character.is_ascii_digit() {
                if self.in_identifier {
                    if !self.in_numeric_suffix {
                        canonical.push('#');
                    }
                    self.in_numeric_suffix = true;
                } else {
                    canonical.push(character);
                    self.in_numeric_suffix = false;
                }
                continue;
            }

            canonical.push(character);
            self.in_identifier = character.is_ascii_alphanumeric() || character == '_';
            self.in_numeric_suffix = false;
        }
        canonical
    }
}

/// Observa a sequência de peças sem influenciar a amostragem.
pub struct CycleDetector {
    config: CycleConfig,
    tokens: VecDeque<TokenSignature>,
    normalizer: IdentifierNormalizer,
}

impl CycleDetector {
    pub fn new(config: CycleConfig) -> Self {
        let config = config.normalized();
        Self {
            config,
            tokens: VecDeque::new(),
            normalizer: IdentifierNormalizer::default(),
        }
    }

    /// Adiciona uma peça de token e retorna o primeiro ciclo confirmado.
    /// Quando desligado, o caminho retorna antes de alocar ou normalizar algo.
    pub fn observe(&mut self, token_id: u32, piece: &str) -> Option<CycleMatch> {
        if !self.config.enabled {
            return None;
        }

        let mut canonical = self.normalizer.canonical_piece(piece);
        if canonical.is_empty() {
            canonical = format!("<empty-token:{token_id}>");
        }
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        self.tokens.push_back(TokenSignature {
            canonical,
            fingerprint: hasher.finish(),
        });

        let max_history = self
            .config
            .max_window_tokens
            .saturating_mul(self.config.repetitions);
        while self.tokens.len() > max_history {
            self.tokens.pop_front();
        }

        let first_check_at = self.config.min_window_tokens * self.config.repetitions;
        if self.tokens.len() < first_check_at {
            return None;
        }
        self.find_cycle()
    }

    fn find_cycle(&self) -> Option<CycleMatch> {
        let end = self.tokens.len();
        for period in (self.config.min_window_tokens..=self.config.max_window_tokens).rev() {
            let required = period.saturating_mul(self.config.repetitions);
            if required > end {
                continue;
            }
            let start = end - required;
            // Most output is not periodic. The endpoint check keeps the
            // normal path to a bounded integer scan; canonical strings are
            // compared only for a candidate and still provide collision-safe
            // confirmation below.
            if self.tokens[end - 1].fingerprint != self.tokens[end - period - 1].fingerprint {
                continue;
            }
            let first = start..start + period;
            let fingerprint_matches = (1..self.config.repetitions).all(|repetition| {
                let offset = start + repetition * period;
                first
                    .clone()
                    .zip(offset..offset + period)
                    .all(|(left, right)| {
                        self.tokens[left].fingerprint == self.tokens[right].fingerprint
                    })
            });
            if fingerprint_matches {
                let matches = (1..self.config.repetitions).all(|repetition| {
                    let offset = start + repetition * period;
                    first
                        .clone()
                        .zip(offset..offset + period)
                        .all(|(left, right)| {
                            self.tokens[left].canonical == self.tokens[right].canonical
                        })
                });
                if !matches {
                    continue;
                }
                return Some(CycleMatch {
                    period,
                    repetitions: self.config.repetitions,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(min_window_tokens: usize, max_window_tokens: usize) -> CycleConfig {
        CycleConfig {
            enabled: true,
            min_window_tokens,
            max_window_tokens,
            repetitions: 3,
        }
    }

    fn feed(detector: &mut CycleDetector, tokens: &[(u32, &str)]) -> Option<CycleMatch> {
        tokens
            .iter()
            .find_map(|(token_id, piece)| detector.observe(*token_id, piece))
    }

    #[test]
    fn detects_three_identical_token_windows() {
        let mut detector = CycleDetector::new(config(3, 3));
        let tokens = [
            (1, "alpha"),
            (2, " beta"),
            (3, " gamma"),
            (1, "alpha"),
            (2, " beta"),
            (3, " gamma"),
            (1, "alpha"),
            (2, " beta"),
            (3, " gamma"),
        ];

        let found = feed(&mut detector, &tokens).expect("o ciclo deve ser detectado");
        assert_eq!(found.period, 3);
        assert_eq!(found.repetitions, 3);
    }

    #[test]
    fn detects_a_cycle_that_starts_after_unrelated_output() {
        let mut detector = CycleDetector::new(config(2, 2));
        let tokens = [
            (1, "seed "),
            (2, "alpha "),
            (3, "beta "),
            (4, "alpha "),
            (5, "beta "),
            (6, "alpha "),
            (7, "beta "),
        ];

        let found = feed(&mut detector, &tokens).expect("o ciclo deve ser detectado");
        assert_eq!(found.period, 2);
    }

    #[test]
    fn detects_incrementing_identifier_suffixes_without_collapsing_all_numbers() {
        let mut detector = CycleDetector::new(config(3, 3));
        let mut found = None;
        for (index, suffix) in ["2", "3", "4"].into_iter().enumerate() {
            let block = [
                (index as u32, "\nlet SRS_FINAL_FULL_"),
                (100 + index as u32, suffix),
                (200 + index as u32, " = value;\n// mesmo comentario\n"),
            ];
            found = found.or_else(|| feed(&mut detector, &block));
        }

        let found = found.expect("o ciclo com sufixo incremental deve ser detectado");
        assert_eq!(found.period, 3);
    }

    #[test]
    fn legitimate_table_rows_with_different_values_pass_intact() {
        let mut detector = CycleDetector::new(config(3, 8));
        let rows = [
            ("1", "alpha"),
            ("2", "beta"),
            ("3", "gamma"),
            ("4", "delta"),
            ("5", "epsilon"),
        ];

        for (number, value) in rows {
            let row = [(1, "| "), (2, number), (3, " | "), (4, value), (5, " |\n")];
            assert!(
                feed(&mut detector, &row).is_none(),
                "uma tabela com dados novos não pode parecer um ciclo"
            );
        }
    }

    #[test]
    fn normal_non_repeating_output_is_not_changed() {
        let mut detector = CycleDetector::new(config(3, 12));
        for (index, word) in ["one", "two", "three", "four", "five", "six"]
            .into_iter()
            .enumerate()
        {
            assert!(detector.observe(index as u32, word).is_none());
        }
    }

    #[test]
    fn disabled_detection_has_no_effect() {
        let mut detector = CycleDetector::new(CycleConfig {
            enabled: false,
            min_window_tokens: 1,
            max_window_tokens: 1,
            repetitions: 2,
        });

        for _ in 0..32 {
            assert!(detector.observe(7, "same").is_none());
        }
    }

    #[test]
    fn invalid_limits_are_clamped_to_bounded_values() {
        let mut detector = CycleDetector::new(CycleConfig {
            enabled: true,
            min_window_tokens: 2_000,
            max_window_tokens: 1,
            repetitions: 99,
        });

        for (token_id, piece) in [(1, "a"), (2, "b"), (3, "a"), (4, "b")] {
            assert!(detector.observe(token_id, piece).is_none());
        }
    }
}
