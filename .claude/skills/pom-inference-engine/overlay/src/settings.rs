//! The engine settings this plugin offers to a deployment.
//!
//! The POM knows no engine: an inference service publishes the settings it
//! takes (`host.services`, field `settings`), the deploy inspector draws
//! them, and the chosen values come back in `engine.load`
//! (`params.settings`). `launch::command` maps them to the engine's flags.
//! Publish only settings the engine really reads, with the engine's own
//! limits; `placement: "pom"` puts one in the POM settings card instead of
//! the engine's.

use serde_json::{json, Map, Value};

pub struct Setting {
    pub key: &'static str,
    /// `number`, `integer`, `boolean` or `select`.
    pub kind: &'static str,
    pub en: (&'static str, &'static str),
    pub pt: (&'static str, &'static str),
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub options: &'static [&'static str],
    pub pom: bool,
}

/// The engine's settings: `(label, description)` in English and Portuguese.
pub fn all() -> Vec<Setting> {
    vec![Setting {
        key: "threads",
        kind: "integer",
        en: (
            "CPU threads",
            "Threads the engine uses on the CPU (0 = its default).",
        ),
        pt: (
            "Threads de CPU",
            "Threads que o motor usa na CPU (0 = o padrão dele).",
        ),
        min: Some(0.0),
        max: Some(256.0),
        options: &[],
        pom: false,
    }]
}

pub fn schema() -> Value {
    Value::Array(
        all()
            .into_iter()
            .map(|setting| {
                let mut entry = json!({
                    "key": setting.key,
                    "type": setting.kind,
                    "label": {"en": setting.en.0, "pt-BR": setting.pt.0},
                    "description": {"en": setting.en.1, "pt-BR": setting.pt.1},
                });
                if let Some(min) = setting.min {
                    entry["min"] = json!(min);
                }
                if let Some(max) = setting.max {
                    entry["max"] = json!(max);
                }
                if !setting.options.is_empty() {
                    entry["options"] = json!(setting.options);
                }
                if setting.pom {
                    entry["placement"] = json!("pom");
                }
                entry
            })
            .collect(),
    )
}

/// The values of a load request that name a published key and have its type;
/// the rest are dropped, never passed to the engine.
pub fn select(values: &Value) -> Map<String, Value> {
    let Some(values) = values.as_object() else {
        return Map::new();
    };
    all()
        .into_iter()
        .filter_map(|setting| {
            let value = values.get(setting.key)?;
            let in_range = |number: f64| {
                setting.min.is_none_or(|min| number >= min)
                    && setting.max.is_none_or(|max| number <= max)
            };
            let valid = match setting.kind {
                "integer" => value.as_i64().is_some_and(|number| in_range(number as f64)),
                "number" => value.as_f64().is_some_and(in_range),
                "boolean" => value.is_boolean(),
                "select" => value
                    .as_str()
                    .is_some_and(|choice| setting.options.contains(&choice)),
                _ => false,
            };
            valid.then(|| (setting.key.to_owned(), value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_both_languages_and_a_known_type() {
        for setting in all() {
            assert!(["number", "integer", "boolean", "select"].contains(&setting.kind));
            assert!(!setting.en.0.is_empty() && !setting.pt.0.is_empty());
            assert_eq!(setting.kind == "select", !setting.options.is_empty());
        }
    }

    #[test]
    fn only_published_and_valid_values_reach_the_engine() {
        let chosen = select(&json!({"threads": 8, "unknown": 1}));
        assert_eq!(chosen.get("threads"), Some(&json!(8)));
        assert!(chosen.get("unknown").is_none());
        assert!(select(&json!({"threads": -1})).is_empty());
        assert!(select(&json!({"threads": "8"})).is_empty());
        assert!(select(&Value::Null).is_empty());
    }
}
