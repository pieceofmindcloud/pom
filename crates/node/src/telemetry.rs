//! Nível de custo da telemetria de inferência.
//!
//! `POM_PROFILE_SYNC` continua pertencendo ao motor e controla apenas a
//! sincronização do backend. Este perfil governa a quantidade de trace que o
//! gateway e o cliente coletam, sem criar outra política para o motor.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Off,
    #[default]
    Basic,
    Trace,
}

impl Profile {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "trace" => Self::Trace,
            _ => Self::Basic,
        }
    }

    pub fn current() -> Self {
        std::env::var("POM_PROFILE")
            .ok()
            .map(|value| Self::parse(&value))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Trace => "trace",
        }
    }

    pub fn collects_spans(self) -> bool {
        !matches!(self, Self::Off)
    }

    #[cfg(test)]
    pub fn collects_chunk_details(self) -> bool {
        matches!(self, Self::Trace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_profile_falls_back_to_basic_and_the_names_are_stable() {
        assert_eq!(Profile::parse("unexpected"), Profile::Basic);
        assert_eq!(Profile::Off.as_str(), "off");
        assert_eq!(Profile::Basic.as_str(), "basic");
        assert_eq!(Profile::Trace.as_str(), "trace");
    }

    #[test]
    fn the_profile_controls_spans_and_chunk_details() {
        assert!(!Profile::Off.collects_spans());
        assert!(Profile::Basic.collects_spans());
        assert!(!Profile::Basic.collects_chunk_details());
        assert!(Profile::Trace.collects_chunk_details());
    }
}
