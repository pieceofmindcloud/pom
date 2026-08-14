//! Protocol-agnostic half of tool calling, shared by [`crate::messages`]
//! (Anthropic) and [`crate::openai`] (OpenAI-compatible).
//!
//! Both adapters sit in front of the same GGUF, fine-tuned on the same
//! Hermes-style `<tool_call>{"name":...,"arguments":...}</tool_call>`
//! convention — verified straight from `tokenizer.chat_template` in the
//! actually-deployed model, not guessed. Only the request/response
//! *envelope* differs between the two APIs, so the tools-block wording
//! ([`render_tools_block`]) and the raw-output scanner ([`TagScanner`],
//! [`parse_output`]) live here exactly once. Two independent copies of this
//! logic could silently drift and disagree about the same model output;
//! one shared copy can't.
//!
//! See `crate::messages`'s module doc comment for why this is
//! prompt-engineered and text-parsed rather than protocol-level: this
//! backend has no grammar-constrained decoding anywhere.

use serde::Deserialize;
use serde_json::json;

/// One entry from a request's `tools` array — name + description + JSON
/// Schema parameters. Both Anthropic (`input_schema`) and OpenAI
/// (`function.parameters`) map onto this the same way.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// Text block instructing the model on the tools it has and the exact
/// `<tool_call>` format to answer with — copied verbatim (down to the
/// wording) from `tokenizer.chat_template` in the deployed GGUF, since that
/// is what the model was actually fine-tuned to recognise. Empty when there
/// are no tools, so a tool-less request renders identically to before tool
/// support existed.
pub(crate) fn render_tools_block(tools: &[ToolDef]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Tools\n\nYou may call one or more functions to assist with the user query.\n\n\
         You are provided with function signatures within <tools></tools> XML tags:\n<tools>",
    );
    for t in tools {
        out.push('\n');
        out.push_str(
            &json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description.clone().unwrap_or_default(),
                    "parameters": t.input_schema,
                }
            })
            .to_string(),
        );
    }
    out.push_str(
        "\n</tools>\n\nFor each function call, return a json object with function name and \
         arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n\
         {\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call>",
    );
    out
}

// ── Tool-call scanning ───────────────────────────────────────────────────────

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";

/// One classified span of raw model output.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScanEvent {
    Text(String),
    ToolCallOpen,
    ToolCallChunk(String),
    /// `Ok` once `{"name":...,"arguments":...}` parses and names a function;
    /// `Err` carries the full literal `<tool_call>...</tool_call>` span to
    /// fall back to as plain text — malformed model output must degrade
    /// gracefully, never crash or vanish.
    ToolCallClose(Result<(String, serde_json::Value), String>),
}

enum ScanState {
    Outside,
    Inside(String),
}

/// Incrementally splits a token stream into plain text and
/// `<tool_call>...</tool_call>` spans, tolerating the tags landing anywhere
/// relative to token boundaries — a single decoded token's text can split a
/// tag at any byte. Both adapters' blocking path (via [`parse_output`]) and
/// streaming path run every piece of model output through this one scanner,
/// so they can never disagree about how a given piece of text gets
/// classified.
pub(crate) struct TagScanner {
    pending: String,
    state: ScanState,
}

impl TagScanner {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            state: ScanState::Outside,
        }
    }

    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<ScanEvent> {
        self.pending.push_str(chunk);
        let mut events = Vec::new();
        loop {
            let tag = match &self.state {
                ScanState::Outside => TOOL_CALL_OPEN,
                ScanState::Inside(_) => TOOL_CALL_CLOSE,
            };
            match self.pending.find(tag) {
                Some(i) => {
                    let head = self.pending[..i].to_string();
                    self.pending = self.pending[i + tag.len()..].to_string();
                    match &mut self.state {
                        ScanState::Outside => {
                            if !head.is_empty() {
                                events.push(ScanEvent::Text(head));
                            }
                            events.push(ScanEvent::ToolCallOpen);
                            self.state = ScanState::Inside(String::new());
                        }
                        ScanState::Inside(raw) => {
                            if !head.is_empty() {
                                raw.push_str(&head);
                                events.push(ScanEvent::ToolCallChunk(head));
                            }
                            let raw = std::mem::take(raw);
                            events.push(ScanEvent::ToolCallClose(parse_tool_call(&raw)));
                            self.state = ScanState::Outside;
                        }
                    }
                }
                None => {
                    let hold = longest_prefix_overlap(&self.pending, tag);
                    let flush_len = self.pending.len() - hold;
                    if flush_len > 0 {
                        let flushed = self.pending[..flush_len].to_string();
                        self.pending = self.pending[flush_len..].to_string();
                        match &mut self.state {
                            ScanState::Outside => events.push(ScanEvent::Text(flushed)),
                            ScanState::Inside(raw) => {
                                raw.push_str(&flushed);
                                events.push(ScanEvent::ToolCallChunk(flushed));
                            }
                        }
                    }
                    break;
                }
            }
        }
        events
    }

    /// Whatever never resolved into a complete tag by the time generation
    /// ends — a dangling `<tool_c` fragment, or a whole tool call cut off by
    /// `max_tokens` before its closing tag arrived. Nothing is dropped.
    pub(crate) fn finish(self) -> Vec<ScanEvent> {
        match self.state {
            ScanState::Outside if self.pending.is_empty() => Vec::new(),
            ScanState::Outside => vec![ScanEvent::Text(self.pending)],
            ScanState::Inside(raw) => vec![ScanEvent::Text(format!(
                "{TOOL_CALL_OPEN}{raw}{}",
                self.pending
            ))],
        }
    }
}

/// Longest suffix of `s` that is also a strict prefix of `tag` — how much of
/// `s`'s tail could still turn into `tag` once more input arrives. Both tags
/// are pure ASCII, so every byte index this returns is guaranteed to land on
/// a UTF-8 char boundary in `s` regardless of non-ASCII text around it.
fn longest_prefix_overlap(s: &str, tag: &str) -> usize {
    let max = s.len().min(tag.len().saturating_sub(1));
    (1..=max).rev().find(|&len| s.ends_with(&tag[..len])).unwrap_or(0)
}

fn parse_tool_call(raw: &str) -> Result<(String, serde_json::Value), String> {
    let fallback = || format!("{TOOL_CALL_OPEN}{raw}{TOOL_CALL_CLOSE}");
    match serde_json::from_str::<serde_json::Value>(raw.trim()) {
        Ok(v) => match v.get("name").and_then(|n| n.as_str()) {
            Some(name) if !name.is_empty() => Ok((
                name.to_string(),
                v.get("arguments").cloned().unwrap_or_else(|| json!({})),
            )),
            _ => Err(fallback()),
        },
        Err(_) => Err(fallback()),
    }
}

// ── Output parsing ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutputPart {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
}

fn events_to_parts(events: Vec<ScanEvent>) -> Vec<OutputPart> {
    let mut parts = Vec::new();
    for e in events {
        match e {
            ScanEvent::Text(s) if !s.is_empty() => parts.push(OutputPart::Text(s)),
            ScanEvent::Text(_) => {}
            ScanEvent::ToolCallOpen | ScanEvent::ToolCallChunk(_) => {}
            ScanEvent::ToolCallClose(Ok((name, arguments))) => {
                parts.push(OutputPart::ToolCall { name, arguments })
            }
            ScanEvent::ToolCallClose(Err(raw)) => parts.push(OutputPart::Text(raw)),
        }
    }
    parts
}

/// Adjacent `Text` parts (e.g. real text immediately followed by a malformed
/// call's literal fallback) collapse into one, so the response reconstructs
/// exactly the original text whenever nothing actually parsed as a call.
fn merge_adjacent_text(parts: Vec<OutputPart>) -> Vec<OutputPart> {
    let mut out: Vec<OutputPart> = Vec::new();
    for p in parts {
        match (&p, out.last_mut()) {
            (OutputPart::Text(s), Some(OutputPart::Text(prev))) => prev.push_str(s),
            _ => out.push(p),
        }
    }
    out
}

/// Split raw model output into plain text and successfully-parsed tool
/// calls — a thin wrapper over [`TagScanner`], so this and each adapter's
/// streaming path can never disagree about how a given span of text is
/// classified.
pub(crate) fn parse_output(text: &str) -> Vec<OutputPart> {
    let mut scanner = TagScanner::new();
    let mut events = scanner.feed(text);
    events.extend(scanner.finish());
    merge_adjacent_text(events_to_parts(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_tools_block_is_empty_with_no_tools() {
        assert_eq!(render_tools_block(&[]), "");
    }

    #[test]
    fn render_tools_block_describes_each_tool() {
        let tools = vec![ToolDef {
            name: "Bash".into(),
            description: Some("Run a shell command".into()),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let out = render_tools_block(&tools);
        assert!(out.contains("<tools>"));
        assert!(out.contains("</tools>"));
        assert!(out.contains("Bash"));
        assert!(out.contains("<tool_call>"));
    }

    #[test]
    fn scanner_feed_on_empty_chunk_is_a_no_op() {
        let mut sc = TagScanner::new();
        assert_eq!(sc.feed(""), vec![]);
    }

    #[test]
    fn parse_output_no_tool_calls_returns_one_text_part() {
        assert_eq!(
            parse_output("just chatting"),
            vec![OutputPart::Text("just chatting".into())]
        );
    }

    #[test]
    fn parse_output_extracts_a_single_tool_call() {
        let out = parse_output(r#"<tool_call>{"name":"Bash","arguments":{"command":"ls"}}</tool_call>"#);
        assert_eq!(out.len(), 1);
        match &out[0] {
            OutputPart::ToolCall { name, arguments } => {
                assert_eq!(name, "Bash");
                assert_eq!(arguments["command"], "ls");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn parse_output_interleaves_text_and_tool_calls() {
        let out = parse_output(
            r#"Sure, let me check.<tool_call>{"name":"Bash","arguments":{"command":"ls"}}</tool_call>Done."#,
        );
        assert_eq!(out[0], OutputPart::Text("Sure, let me check.".into()));
        assert!(matches!(out[1], OutputPart::ToolCall { .. }));
        assert_eq!(out[2], OutputPart::Text("Done.".into()));
    }

    #[test]
    fn parse_output_handles_two_parallel_tool_calls_back_to_back() {
        let out = parse_output(
            r#"<tool_call>{"name":"A","arguments":{}}</tool_call><tool_call>{"name":"B","arguments":{}}</tool_call>"#,
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "A"));
        assert!(matches!(&out[1], OutputPart::ToolCall { name, .. } if name == "B"));
    }

    #[test]
    fn parse_output_falls_back_to_text_on_malformed_json() {
        let raw = "<tool_call>not json</tool_call>";
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    #[test]
    fn parse_output_falls_back_to_text_when_name_is_missing() {
        let raw = r#"<tool_call>{"arguments":{}}</tool_call>"#;
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    #[test]
    fn parse_output_recovers_an_unclosed_tool_call() {
        let raw = r#"<tool_call>{"name":"Bash", "argum"#; // cut off mid-generation
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    #[test]
    fn parse_output_recovers_a_dangling_tag_prefix() {
        let raw = "plain <tool_c";
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    fn feed_in_chunks(text: &str, chunk_size: usize) -> Vec<OutputPart> {
        let mut sc = TagScanner::new();
        let mut events = Vec::new();
        for chunk in text.as_bytes().chunks(chunk_size.max(1)) {
            events.extend(sc.feed(std::str::from_utf8(chunk).unwrap()));
        }
        events.extend(sc.finish());
        merge_adjacent_text(events_to_parts(events))
    }

    #[test]
    fn tag_split_across_arbitrary_chunk_boundaries_matches_the_one_chunk_result() {
        let full = r#"before <tool_call>{"name":"Bash","arguments":{"command":"ls -la"}}</tool_call> after"#;
        let whole = parse_output(full);
        assert_eq!(feed_in_chunks(full, full.len()), whole);
        assert_eq!(feed_in_chunks(full, 1), whole); // byte-at-a-time
        assert_eq!(feed_in_chunks(full, 3), whole); // arbitrary split
    }
}
