//! Typed decisions (`POST /v1/systemone`, alias `/v1/decisions`): the
//! TypeSafe Jev API shape served by the POM's own models.
//!
//! A request carries a `state` and a map of typed `questions` (`noul`,
//! `choice`, `score`). Each question becomes one prompt whose answer options
//! carry single-token labels (`A`, `B`, ... for choice; `0`..`9` for score;
//! `Yes`/`No` for noul). Instead of generating, the last peer reports how much
//! of the model's next-token distribution falls on each label (the prefill
//! `probe`), so every answer is a probability distribution read from one
//! forward pass, never parsed from text.
//!
//! The prompt is ordered from the most stable part to the most volatile one:
//! a fixed system turn, then the question (instructions and answer format),
//! then the state, then the options. The text before the state is the same
//! for every request that asks this question, so the gateway keys the prefix
//! cache by it and tells the head to retain only that part
//! (`DecisionProbe::retain_prefix_bytes`): a client asking the same question
//! about a new state on every call (a game loop, a classifier over a stream)
//! prefills only the state and options. Questions run one after the other and
//! are isolated: no question sees another's answer.
use crate::gateway::{GatewayState, GenerationInput};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

const MAX_QUESTIONS: usize = 64;
const MAX_CHOICE_OPTIONS: usize = 26;
const MAX_SCORE_LEVELS: usize = 10;
const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    Noul {
        yes: Option<String>,
        no: Option<String>,
    },
    Choice {
        options: Vec<(String, Option<String>)>,
    },
    Score {
        levels: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Question {
    pub id: String,
    pub instructions: String,
    pub kind: Kind,
}

/// Text of a field the API accepts as string, object or array.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// Parses and validates the `questions` map. Questions and choice options come
/// out in key order (the crate's JSON maps are sorted); answers are keyed by
/// id and option name, so the order never changes a result.
pub(crate) fn parse_questions(value: &Value) -> Result<Vec<Question>, String> {
    let map = value
        .as_object()
        .ok_or("'questions' must be an object of typed questions")?;
    if map.is_empty() || map.len() > MAX_QUESTIONS {
        return Err(format!(
            "'questions' must hold 1 to {MAX_QUESTIONS} questions"
        ));
    }
    let mut questions = Vec::with_capacity(map.len());
    for (id, question) in map {
        let kind = question
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let instructions = question
            .get("instructions")
            .map(text_of)
            .unwrap_or_default();
        if instructions.trim().is_empty() {
            return Err(format!("question '{id}' needs 'instructions'"));
        }
        let criteria = question.get("criteria").unwrap_or(&Value::Null);
        let kind = match kind {
            "noul" => Kind::Noul {
                yes: criteria
                    .get("true")
                    .map(text_of)
                    .filter(|text| !text.is_empty()),
                no: criteria
                    .get("false")
                    .map(text_of)
                    .filter(|text| !text.is_empty()),
            },
            "choice" => {
                let options = criteria
                    .as_object()
                    .ok_or_else(|| format!("choice '{id}' needs 'criteria' as a map of options"))?;
                if options.len() < 2 || options.len() > MAX_CHOICE_OPTIONS {
                    return Err(format!(
                        "choice '{id}' needs 2 to {MAX_CHOICE_OPTIONS} options"
                    ));
                }
                Kind::Choice {
                    options: options
                        .iter()
                        .map(|(name, detail)| {
                            (
                                name.clone(),
                                Some(text_of(detail)).filter(|text| !text.is_empty()),
                            )
                        })
                        .collect(),
                }
            }
            "score" => {
                let levels = criteria.as_array().ok_or_else(|| {
                    format!("score '{id}' needs 'criteria' as an ordered array of levels")
                })?;
                if levels.len() < 2 || levels.len() > MAX_SCORE_LEVELS {
                    return Err(format!("score '{id}' needs 2 to {MAX_SCORE_LEVELS} levels"));
                }
                Kind::Score {
                    levels: levels.iter().map(text_of).collect(),
                }
            }
            other => {
                return Err(format!(
                    "question '{id}' has unknown type '{other}' (noul, choice or score)"
                ))
            }
        };
        questions.push(Question {
            id: id.clone(),
            instructions,
            kind,
        });
    }
    Ok(questions)
}

impl Question {
    /// The single-token labels the last peer scores, in option order.
    pub(crate) fn labels(&self) -> Vec<String> {
        match &self.kind {
            Kind::Noul { .. } => vec!["Yes".into(), "No".into()],
            Kind::Choice { options } => (0..options.len())
                .map(|i| (LETTERS[i] as char).to_string())
                .collect(),
            Kind::Score { levels } => (0..levels.len()).map(|i| i.to_string()).collect(),
        }
    }

    /// The fixed head of the question turn: instructions and answer format.
    /// It never depends on the state, so it is the reusable prefix.
    pub(crate) fn question_head(&self) -> String {
        let format = match &self.kind {
            Kind::Noul { .. } => "Answer with exactly one word: Yes or No.",
            Kind::Choice { .. } => "Answer with exactly one letter: the best option.",
            Kind::Score { .. } => "Answer with exactly one digit: the level that fits best.",
        };
        format!("QUESTION:\n{}\n\n{format}", self.instructions.trim())
    }

    /// The labelled options. They come after the state because clients often
    /// describe options in terms of the state (and change them with it).
    pub(crate) fn options_block(&self) -> String {
        let mut text = String::new();
        match &self.kind {
            Kind::Noul { yes, no } => {
                text.push_str("OPTIONS:\nYes");
                if let Some(yes) = yes {
                    text.push_str(&format!(" - {yes}"));
                }
                text.push_str("\nNo");
                if let Some(no) = no {
                    text.push_str(&format!(" - {no}"));
                }
            }
            Kind::Choice { options } => {
                text.push_str("OPTIONS:");
                for (index, (name, detail)) in options.iter().enumerate() {
                    let letter = LETTERS[index] as char;
                    match detail {
                        Some(detail) => text.push_str(&format!("\n{letter}) {name} - {detail}")),
                        None => text.push_str(&format!("\n{letter}) {name}")),
                    }
                }
            }
            Kind::Score { levels } => {
                text.push_str("LEVELS:");
                for (index, level) in levels.iter().enumerate() {
                    text.push_str(&format!("\n{index}) {level}"));
                }
            }
        }
        text
    }

    /// The question turn: question head, then the state, then the options.
    pub(crate) fn user_message(&self, state: &Value) -> String {
        format!(
            "{}{STATE_MARKER}{}\n\n{}",
            self.question_head(),
            state_text(state),
            self.options_block()
        )
    }

    /// Option names in label order, for the trace.
    pub(crate) fn option_names(&self) -> Vec<String> {
        match &self.kind {
            Kind::Noul { .. } => vec!["true".into(), "false".into()],
            Kind::Choice { options } => options.iter().map(|(name, _)| name.clone()).collect(),
            Kind::Score { levels } => levels.clone(),
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match &self.kind {
            Kind::Noul { .. } => "noul",
            Kind::Choice { .. } => "choice",
            Kind::Score { .. } => "score",
        }
    }
}

/// Where the state starts in the question turn; the text before it is the
/// reusable prefix.
const STATE_MARKER: &str = "\n\nSTATE:\n";

/// The state as the model reads it: text as is, JSON compact. Pretty-printed
/// JSON put every coordinate of a game state on its own lines and roughly
/// doubled the tokens a remote pipeline had to carry.
pub(crate) fn state_text(state: &Value) -> String {
    match state {
        Value::String(text) => text.trim().to_owned(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Byte offset in `prompt` where the reusable prefix of `question` ends: just
/// before the state. `None` when the rendering did not keep the question turn
/// verbatim (then nothing extra is retained).
pub(crate) fn reusable_prefix_end(prompt: &str, question: &Question) -> Option<usize> {
    let head = question.question_head();
    let anchor = format!("{head}{STATE_MARKER}");
    prompt.find(&anchor).map(|start| start + head.len())
}

/// The system turn, identical for every decision.
pub(crate) const SYSTEM_MESSAGE: &str =
    "You are a decision engine. You read the STATE and answer one typed question about it. \
     You never explain: you reply with only the label of your answer.";

/// Choice confidence published by TypeSafe: `(n * max - 1) / (n - 1)`.
pub(crate) fn choice_confidence(probabilities: &[f64]) -> f64 {
    let count = probabilities.len() as f64;
    if count < 2.0 {
        return 1.0;
    }
    let peak = probabilities.iter().copied().fold(0.0, f64::max);
    ((count * peak - 1.0) / (count - 1.0)).clamp(0.0, 1.0)
}

/// Score confidence published by TypeSafe: one minus the probability-weighted
/// distance to the peak level, relative to an even spread.
pub(crate) fn score_confidence(probabilities: &[f64]) -> f64 {
    let count = probabilities.len();
    if count < 2 {
        return 1.0;
    }
    let peak = probabilities
        .iter()
        .enumerate()
        .fold((0, f64::MIN), |best, (index, value)| {
            if *value > best.1 {
                (index, *value)
            } else {
                best
            }
        })
        .0;
    let spread: f64 = probabilities
        .iter()
        .enumerate()
        .map(|(i, p)| p * (i as f64 - peak as f64).abs())
        .sum();
    let middle = (count as f64 - 1.0) / 2.0;
    let even: f64 = (0..count).map(|i| (i as f64 - middle).abs()).sum::<f64>() / count as f64;
    (1.0 - spread / even).clamp(0.0, 1.0)
}

/// Normalizes the label probabilities the last peer reported (uniform when
/// the model put no mass on any label).
pub(crate) fn normalized(raw: &[f64]) -> Vec<f64> {
    let total: f64 = raw.iter().sum();
    if total <= 0.0 || !total.is_finite() {
        return vec![1.0 / raw.len().max(1) as f64; raw.len()];
    }
    raw.iter().map(|value| value / total).collect()
}

fn round(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// The Jev-shaped answer for one question from its label probabilities.
pub(crate) fn answer(question: &Question, raw: &[f64]) -> Value {
    let probabilities = normalized(raw);
    match &question.kind {
        Kind::Noul { .. } => json!({"type": "noul", "noul": round(probabilities[0])}),
        Kind::Choice { options } => {
            let best = probabilities
                .iter()
                .enumerate()
                .fold(
                    (0, f64::MIN),
                    |best, (i, p)| if *p > best.1 { (i, *p) } else { best },
                )
                .0;
            let map: Map<String, Value> = options
                .iter()
                .zip(&probabilities)
                .map(|((name, _), p)| (name.clone(), json!(round(*p))))
                .collect();
            json!({
                "type": "choice",
                "choice": options[best].0,
                "probabilities": map,
                "confidence": round(choice_confidence(&probabilities)),
            })
        }
        Kind::Score { levels } => {
            let score: f64 = probabilities
                .iter()
                .enumerate()
                .map(|(i, p)| i as f64 * p)
                .sum();
            let legend: Map<String, Value> = levels
                .iter()
                .enumerate()
                .map(|(i, level)| (i.to_string(), json!(level)))
                .collect();
            let map: Map<String, Value> = probabilities
                .iter()
                .enumerate()
                .map(|(i, p)| (i.to_string(), json!(round(*p))))
                .collect();
            json!({
                "type": "score",
                "score": round(score),
                "legend": legend,
                "probabilities": map,
                "confidence": round(score_confidence(&probabilities)),
            })
        }
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "invalid_request_error"}})),
    )
        .into_response()
}

/// `POST /v1/systemone` and `POST /v1/decisions`.
pub(crate) async fn systemone(
    auth: crate::auth::ApiKeyAuth,
    State(state): State<GatewayState>,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let Some(state_value) = body.get("state").filter(|value| !value.is_null()) else {
        return error(StatusCode::BAD_REQUEST, "'state' is required");
    };
    let questions = match parse_questions(body.get("questions").unwrap_or(&Value::Null)) {
        Ok(questions) => questions,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    // Jev clients send `jev-latest`; any name the POM does not serve falls
    // back to the active model, so those clients work unchanged.
    let model = match body
        .get("model")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        Some(name) => match crate::openai::resolve_model(&state, name.to_owned()).await {
            Ok(model) => model,
            Err(_) => state.active_model().await,
        },
        None => state.active_model().await,
    };
    let inference_token = auth.token().to_string();
    let caller = auth.into_caller();

    let mut answers = Map::new();
    let mut details = Map::new();
    let mut input_tokens = 0_u64;
    for question in &questions {
        let messages: Vec<crate::openai::ChatMessage> = match serde_json::from_value(json!([
            {"role": "system", "content": SYSTEM_MESSAGE},
            {"role": "user", "content": question.user_message(state_value)},
        ])) {
            Ok(messages) => messages,
            Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        };
        let (prompt, _) = match crate::openai::render_prompt_with_images(&messages, &[]) {
            Ok(rendered) => rendered,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        // Key the prefix cache by the reusable prefix only: the full prompt
        // changes with every state, so as a key it could never match again.
        let retain_prefix_bytes = reusable_prefix_end(&prompt, question);
        let cache_key = match retain_prefix_bytes {
            Some(end) => prompt[..end].to_owned(),
            None => crate::openai::conversation_cache_key(&messages, &[]),
        };
        let limit = state
            .request_output_limit_for_model(Some(&model), 1, &prompt)
            .await;
        if let Some(message) = limit.rejection_reason() {
            let status = if limit.capacity_pending() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            };
            return error(status, message);
        }
        if let Some(denial) = crate::quota::admit(
            &state,
            &caller,
            &prompt,
            crate::openai::approx_tokens(&prompt),
            &json!({}),
        )
        .await
        {
            return denial.decorate(error(StatusCode::TOO_MANY_REQUESTS, denial.message.clone()));
        }
        let labels = question.labels();
        let result = crate::gateway::generate_blocking_with_stops(
            "/v1/systemone",
            caller.clone(),
            GenerationInput {
                shared_prefix: Default::default(),
                trace_id: Some(format!("dec-{}", uuid::Uuid::new_v4().simple())),
                state: state.clone(),
                inference_token: inference_token.clone(),
                model: model.clone(),
                prompt,
                images: Vec::new(),
                chat_template: true,
                thinking_enabled: false,
                budget: limit.budget(),
                stops: Vec::new(),
                cache_key: Some(cache_key),
                preferred_head: None,
                request_details: json!({"question": question.id, "state": state_value, "type": "systemone"}),
                probe: Some(crate::prefill::DecisionProbe {
                    labels: labels.clone(),
                    retain_prefix_bytes,
                    question: Some(question.id.clone()),
                    question_type: Some(question.type_name().to_owned()),
                    options: question.option_names(),
                }),
            },
        )
        .await;
        let summary = match result {
            Ok((_, summary)) => summary,
            Err(message) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    format!("question '{}': {message}", question.id),
                )
            }
        };
        let trace = summary.trace.as_ref();
        let Some(probe) = trace.and_then(|trace| trace.probe.clone()) else {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("question '{}': the last peer did not report label probabilities (update every node)", question.id),
            );
        };
        let raw: Vec<f64> = probe["probabilities"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_f64().unwrap_or(0.0))
                    .collect()
            })
            .unwrap_or_default();
        if raw.len() != labels.len() {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("question '{}': malformed probe reply", question.id),
            );
        }
        answers.insert(question.id.clone(), answer(question, &raw));
        if let Some(trace) = trace {
            input_tokens += u64::from(trace.input_tokens);
            details.insert(
                question.id.clone(),
                json!({
                    "label_mass": round(probe["mass"].as_f64().unwrap_or(0.0)),
                    "input_tokens": trace.input_tokens,
                    "cached_tokens": trace.cached_tokens,
                    "duration_ms": summary.duration_ms,
                }),
            );
        }
    }
    Json(json!({
        "model": model,
        "answers": answers,
        "usage": {"input_tokens": input_tokens, "output_tokens": questions.len()},
        "x_pom": {"questions": details, "duration_ms": started.elapsed().as_millis() as u64},
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> Vec<Question> {
        parse_questions(&json!({
            "is_urgent": {"type": "noul", "instructions": "Does this convey urgency?", "criteria": {"true": "Time-sensitive", "false": "No urgency"}},
            "department": {"type": "choice", "instructions": "Which team?", "criteria": {"billing": "Payments", "technical": null, "sales": "Pricing"}},
            "frustration": {"type": "score", "instructions": "How frustrated?", "criteria": ["Calm", "Frustrated", "Very angry"]}
        }))
        .unwrap()
    }

    #[test]
    fn questions_keep_their_order_and_get_single_token_labels() {
        let parsed = questions();
        assert_eq!(
            parsed.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
            ["department", "frustration", "is_urgent"]
        );
        assert_eq!(parsed[2].labels(), ["Yes", "No"]);
        assert_eq!(parsed[0].labels(), ["A", "B", "C"]);
        assert_eq!(parsed[1].labels(), ["0", "1", "2"]);
        let state = json!("s");
        let choice = parsed[0].user_message(&state);
        assert!(
            choice.ends_with("A) billing - Payments\nB) sales - Pricing\nC) technical"),
            "{choice}"
        );
        assert!(parsed[2]
            .user_message(&state)
            .contains("Yes - Time-sensitive\nNo - No urgency"));
        assert!(parsed[1]
            .user_message(&state)
            .contains("0) Calm\n1) Frustrated\n2) Very angry"));
    }

    #[test]
    fn invalid_questions_are_refused_with_a_reason() {
        for (bad, reason) in [
            (json!({}), "1 to"),
            (json!({"q": {"type": "noul"}}), "instructions"),
            (
                json!({"q": {"type": "choice", "instructions": "x", "criteria": {"only": null}}}),
                "2 to 26",
            ),
            (
                json!({"q": {"type": "score", "instructions": "x", "criteria": {"a": 1}}}),
                "ordered array",
            ),
            (
                json!({"q": {"type": "vibes", "instructions": "x"}}),
                "unknown type",
            ),
        ] {
            assert!(parse_questions(&bad).unwrap_err().contains(reason), "{bad}");
        }
    }

    #[test]
    fn answers_follow_the_jev_shapes_and_confidence_formulas() {
        let parsed = questions();
        assert_eq!(
            answer(&parsed[2], &[0.38, 0.02]),
            json!({"type": "noul", "noul": 0.95})
        );

        let choice = answer(&parsed[0], &[0.88, 0.0, 0.12]);
        assert_eq!(choice["choice"], "billing");
        assert_eq!(choice["probabilities"]["technical"], 0.12);
        // (3 * 0.88 - 1) / 2
        assert_eq!(choice["confidence"], 0.82);

        let score = answer(&parsed[1], &[0.0, 0.95, 0.05]);
        assert_eq!(score["score"], 1.05);
        assert_eq!(score["legend"]["2"], "Very angry");
        // spread 0.05, even spread 2/3: 1 - 0.075
        assert_eq!(score["confidence"], 0.925);

        // No mass on any label: uniform, never NaN.
        assert_eq!(answer(&parsed[2], &[0.0, 0.0])["noul"], 0.5);
    }

    #[test]
    fn the_question_comes_before_the_compact_state_and_the_options_last() {
        let parsed = questions();
        let state = json!({"snake": [[5, 10], [4, 10]], "food": [9, 3]});
        let text = parsed[0].user_message(&state);
        let question = text.find("QUESTION:").unwrap();
        let format = text.find("Answer with exactly one letter").unwrap();
        let state_at = text
            .find("STATE:\n{\"food\":[9,3],\"snake\":[[5,10],[4,10]]}")
            .unwrap();
        let options = text.find("OPTIONS:").unwrap();
        assert!(
            question < format && format < state_at && state_at < options,
            "{text}"
        );
        assert_eq!(state_text(&json!("  plain text ")), "plain text");
    }

    #[test]
    fn the_reusable_prefix_ends_right_before_the_state() {
        let parsed = questions();
        let render = |state: Value| {
            let messages: Vec<crate::openai::ChatMessage> = serde_json::from_value(json!([
                {"role": "system", "content": SYSTEM_MESSAGE},
                {"role": "user", "content": parsed[0].user_message(&state)},
            ]))
            .unwrap();
            crate::openai::render_prompt_with_images(&messages, &[])
                .unwrap()
                .0
        };
        let first = render(json!({"food": [1, 2]}));
        let second = render(json!({"food": [7, 7], "snake": [[0, 0]]}));
        let a = reusable_prefix_end(&first, &parsed[0]).unwrap();
        let b = reusable_prefix_end(&second, &parsed[0]).unwrap();
        assert_eq!(&first[..a], &second[..b]);
        assert!(first[..a].ends_with("the best option."));
        assert!(first[a..].starts_with("\n\nSTATE:\n"));
        assert!(first[..a].contains(SYSTEM_MESSAGE));
    }
}
