//! Protocol-agnostic half of tool calling, shared by [`crate::messages`]
//! (Anthropic) and [`crate::openai`] (OpenAI-compatible).
//!
//! Both adapters sit in front of the same cluster, so the tools-block
//! wording ([`render_tools_block`]) and the raw-output scanner
//! ([`TagScanner`], [`parse_output`]) live here exactly once. Two
//! independent copies of this logic could silently drift and disagree about
//! the same model output; one shared copy can't.
//!
//! **The tag convention is a moving target, verified against the deployed
//! GGUF, not guessed — and re-verified twice now.** The outer boundary is
//! stable across every Qwen chat template seen so far: one
//! `<tool_call>...</tool_call>` pair per call. What goes *inside* is not:
//! `Qwen3.5-9B-DeepSeek-V4-Flash` uses a Hermes-style JSON object
//! (`{"name":...,"arguments":...}`), but `Qwen3.6-35B-A3B` — the model
//! actually deployed when this was caught, verified straight from its own
//! `tokenizer.chat_template` — uses nested XML instead:
//! `<function=NAME><parameter=P>value</parameter></function>`. A real
//! opencode session mixing the two (instructed in JSON, fine-tuned on XML)
//! produced exactly the garbled, unparseable output that caught this.
//! `parse_call_body` accepts *both* inner forms for that reason — cluster
//! model swaps shouldn't silently break this again — while
//! `render_tools_block`/`render_tool_call` emit whichever is verified
//! current. If this drifts again: dump `tokenizer.chat_template` straight
//! from the active GGUF (`strings model.gguf | grep -A30 tool_call`, or the
//! Python KV-reader used both times before) rather than guessing.
//!
//! **A third case, verified a third time, doesn't fit the "wrong convention"
//! story at all.** `Ornith-1.0-35B` (a 3-bit `IQ3_XXS` quant) has the exact
//! same verified `<function=NAME><parameter=P>` template as `Qwen3.6-35B-A3B`
//! — but a real session produced bare `<function>read</function>`, with no
//! `<tool_call>` wrapper and no `=`/parameters at all, matching *nothing*
//! ever verified against a template. That looks like the model itself not
//! reliably following its own documented format (plausibly the aggressive
//! quantization), not a convention mismatch — no amount of format-guessing
//! fixes that. [`TagScanner`] tolerates this exact shape anyway (see
//! `CallKind::BareFunction`), recovering a name with **no arguments**, since
//! none are ever present in it — a real, accepted limit, not an oversight.
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
/// wording) from `tokenizer.chat_template` in the deployed GGUF (currently
/// `Qwen3.6-35B-A3B`'s nested `<function=NAME><parameter=P>` form — see the
/// module doc comment), since that is what the model was actually
/// fine-tuned to recognise. Empty when there are no tools, so a tool-less
/// request renders identically to before tool support existed.
pub(crate) fn render_tools_block(tools: &[ToolDef]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Tools\n\nYou have access to the following functions:\n\n<tools>",
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
        "\n</tools>\n\n\
         If you choose to call a function ONLY reply in the following format with NO suffix:\n\n\
         <tool_call>\n\
         <function=example_function_name>\n\
         <parameter=example_parameter_1>\n\
         value_1\n\
         </parameter>\n\
         <parameter=example_parameter_2>\n\
         This is the value for the second parameter\n\
         that can span\n\
         multiple lines\n\
         </parameter>\n\
         </function>\n\
         </tool_call>\n\n\
         <IMPORTANT>\n\
         Reminder:\n\
         - Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n\
         - Required parameters MUST be specified\n\
         - You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n\
         - If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls\n\
         </IMPORTANT>",
    );
    out
}

/// Render one tool call back to the text form the model itself would
/// produce — `<function=NAME>` with one `<parameter=P>value</parameter>`
/// per argument (string values written raw, everything else re-serialized
/// to JSON text, mirroring the deployed template's own
/// `is string`/`tojson` split). Shared by both adapters so replayed history
/// always matches exactly what [`parse_call_body`] expects back.
pub(crate) fn render_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    let mut out = format!("<tool_call>\n<function={name}>\n");
    if let serde_json::Value::Object(map) = arguments {
        for (k, v) in map {
            let value_text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str(&format!("<parameter={k}>\n{value_text}\n</parameter>\n"));
        }
    }
    out.push_str("</function>\n</tool_call>");
    out
}

// ── Tool-call scanning ───────────────────────────────────────────────────────

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";
/// No `=`, deliberately — `<function=NAME>` stays exclusively the *inner*
/// content of an already-`<tool_call>`-wrapped span (see `parse_call_body`'s
/// `Wrapped` arm). This is the unwrapped fallback shape, and only that shape.
const BARE_FUNCTION_OPEN: &str = "<function>";
const BARE_FUNCTION_CLOSE: &str = "</function>";

/// Which opening marker started the block currently being scanned — decides
/// both which closing tag to look for and how to interpret what's captured.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CallKind {
    /// `<tool_call>...</tool_call>` — the normal case.
    Wrapped,
    /// `<function>NAME</function>` with no wrapper at all — a tolerated
    /// fallback for output that doesn't even attempt the real convention;
    /// see the module doc comment.
    BareFunction,
}

impl CallKind {
    fn open(self) -> &'static str {
        match self {
            CallKind::Wrapped => TOOL_CALL_OPEN,
            CallKind::BareFunction => BARE_FUNCTION_OPEN,
        }
    }

    fn close(self) -> &'static str {
        match self {
            CallKind::Wrapped => TOOL_CALL_CLOSE,
            CallKind::BareFunction => BARE_FUNCTION_CLOSE,
        }
    }
}

/// One classified span of raw model output.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScanEvent {
    Text(String),
    ToolCallOpen,
    ToolCallChunk(String),
    /// `Ok` once the tagged content parses into a named call (see
    /// `parse_call_body` for the accepted inner formats); `Err` carries the
    /// full literal tagged span (`<tool_call>...</tool_call>` or
    /// `<function>...</function>`, whichever opened it) to fall back to as
    /// plain text — malformed model output must degrade gracefully, never
    /// crash or vanish.
    ToolCallClose(Result<(String, serde_json::Value), String>),
}

enum ScanState {
    Outside,
    Inside(String, CallKind),
}

/// Incrementally splits a token stream into plain text and tool-call spans
/// (`<tool_call>...</tool_call>`, or the bare `<function>...</function>`
/// fallback), tolerating the tags landing anywhere relative to token
/// boundaries — a single decoded token's text can split a tag at any byte.
/// Both adapters' blocking path (via [`parse_output`]) and streaming path
/// run every piece of model output through this one scanner, so they can
/// never disagree about how a given piece of text gets classified.
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
            match &self.state {
                ScanState::Outside => {
                    // Two candidate openers; earliest wins, ties favor the
                    // real wrapper (higher-confidence sighting).
                    let tool_call_pos = self.pending.find(TOOL_CALL_OPEN);
                    let function_pos = self.pending.find(BARE_FUNCTION_OPEN);
                    let chosen = match (tool_call_pos, function_pos) {
                        (Some(a), Some(b)) if b < a => Some((b, CallKind::BareFunction)),
                        (Some(a), _) => Some((a, CallKind::Wrapped)),
                        (None, Some(b)) => Some((b, CallKind::BareFunction)),
                        (None, None) => None,
                    };
                    match chosen {
                        Some((i, kind)) => {
                            let head = self.pending[..i].to_string();
                            self.pending = self.pending[i + kind.open().len()..].to_string();
                            if !head.is_empty() {
                                events.push(ScanEvent::Text(head));
                            }
                            events.push(ScanEvent::ToolCallOpen);
                            self.state = ScanState::Inside(String::new(), kind);
                        }
                        None => {
                            // Hold back whatever could still complete *either*
                            // candidate tag once more input arrives.
                            let hold = longest_prefix_overlap(&self.pending, TOOL_CALL_OPEN)
                                .max(longest_prefix_overlap(&self.pending, BARE_FUNCTION_OPEN));
                            let flush_len = self.pending.len() - hold;
                            if flush_len > 0 {
                                events.push(ScanEvent::Text(self.pending[..flush_len].to_string()));
                                self.pending = self.pending[flush_len..].to_string();
                            }
                            break;
                        }
                    }
                }
                ScanState::Inside(_, kind) => {
                    let close_tag = kind.close();
                    match self.pending.find(close_tag) {
                        Some(i) => {
                            let head = self.pending[..i].to_string();
                            self.pending = self.pending[i + close_tag.len()..].to_string();
                            let ScanState::Inside(mut raw, kind) =
                                std::mem::replace(&mut self.state, ScanState::Outside)
                            else {
                                unreachable!("matched Inside above")
                            };
                            if !head.is_empty() {
                                raw.push_str(&head);
                                events.push(ScanEvent::ToolCallChunk(head));
                            }
                            events.push(ScanEvent::ToolCallClose(parse_call_body(&raw, kind)));
                        }
                        None => {
                            let hold = longest_prefix_overlap(&self.pending, close_tag);
                            let flush_len = self.pending.len() - hold;
                            if flush_len > 0 {
                                let flushed = self.pending[..flush_len].to_string();
                                self.pending = self.pending[flush_len..].to_string();
                                if let ScanState::Inside(raw, _) = &mut self.state {
                                    raw.push_str(&flushed);
                                }
                                events.push(ScanEvent::ToolCallChunk(flushed));
                            }
                            break;
                        }
                    }
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
            ScanState::Inside(raw, kind) => vec![ScanEvent::Text(format!(
                "{}{raw}{}",
                kind.open(),
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

/// `Wrapped`: two inner formats are accepted, tried in this order — the
/// nested `<function=NAME><parameter=P>...` form (`Qwen3.6-35B-A3B`,
/// `Ornith-1.0-35B`) and the legacy Hermes JSON-object form
/// (`Qwen3.5-9B-DeepSeek-V4-Flash`) — see the module doc comment for why
/// both matter on a cluster that swaps models. `BareFunction`: just a name,
/// see `parse_bare_function_call`. Either failing to parse degrades to the
/// literal tagged text, never a crash or a dropped response.
fn parse_call_body(raw: &str, kind: CallKind) -> Result<(String, serde_json::Value), String> {
    let fallback = || format!("{}{raw}{}", kind.open(), kind.close());
    match kind {
        CallKind::BareFunction => parse_bare_function_call(raw).ok_or_else(fallback),
        CallKind::Wrapped => {
            if raw.trim_start().starts_with("<function=") {
                return parse_function_call_xml(raw).ok_or_else(fallback);
            }
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
    }
}

/// Recovers a name from a wrapper-less `<function>NAME</function>` — see the
/// module doc comment for why this exists. There is never argument data to
/// recover here (`arguments` is always an empty object), so a call parsed
/// this way is very unlikely to be functional; it's kept honest that this is
/// a name-only recovery, not a full parse.
///
/// The one guard applied — reject a `name` containing whitespace — is a
/// cheap, deliberately weak defense against ordinary prose incidentally
/// matching this shape (e.g. a sentence mentioning `<function>` literally):
/// a real tool name is one token, so this filters most accidental matches
/// without needing to know which tools were actually declared.
fn parse_bare_function_call(raw: &str) -> Option<(String, serde_json::Value)> {
    let name = raw.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), serde_json::Value::Object(serde_json::Map::new())))
}

/// Parses `<function=NAME>(<parameter=P>value</parameter>)*</function>`.
/// Each parameter value is speculatively JSON-parsed (recovers numbers,
/// bools, arrays, objects the model wrote as literal JSON text) and falls
/// back to a plain string otherwise (the common case — a raw command, a
/// path, ...); `.trim()` strips only the tag-adjacent newlines, preserving
/// internal ones, since the template explicitly allows multi-line values.
/// `None` on any structural mismatch — missing `>`, unclosed `</function>`
/// or `</parameter>` — so the caller can fall back to literal text.
fn parse_function_call_xml(raw: &str) -> Option<(String, serde_json::Value)> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("<function=")?;
    let (name, rest) = rest.split_once('>')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let body = rest.strip_suffix("</function>")?.trim();

    let mut args = serde_json::Map::new();
    let mut cursor = body;
    loop {
        let cursor_trimmed = cursor.trim_start();
        if cursor_trimmed.is_empty() {
            break;
        }
        let after_open = cursor_trimmed.strip_prefix("<parameter=")?;
        let (pname, after_name) = after_open.split_once('>')?;
        let pname = pname.trim();
        let close_idx = after_name.find("</parameter>")?;
        let value_raw = after_name[..close_idx].trim();
        let value = serde_json::from_str::<serde_json::Value>(value_raw)
            .unwrap_or_else(|_| serde_json::Value::String(value_raw.to_string()));
        args.insert(pname.to_string(), value);
        cursor = &after_name[close_idx + "</parameter>".len()..];
    }
    Some((name.to_string(), serde_json::Value::Object(args)))
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
        assert!(out.contains("<function="));
        assert!(out.contains("<parameter="));
    }

    // ── render_tool_call / parse_function_call_xml ──────────────────────────

    #[test]
    fn render_tool_call_round_trips_string_and_numeric_arguments() {
        let arguments = json!({"command": "ls -la", "limit": 5});
        let rendered = render_tool_call("Bash", &arguments);
        assert!(rendered.starts_with("<tool_call>\n<function=Bash>\n"));
        assert!(rendered.ends_with("</function>\n</tool_call>"));
        // The string arg is written raw (no quotes); the number as JSON text.
        assert!(rendered.contains("<parameter=command>\nls -la\n</parameter>"));
        assert!(rendered.contains("<parameter=limit>\n5\n</parameter>"));

        let parts = parse_output(&rendered);
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            OutputPart::ToolCall { name, arguments } => {
                assert_eq!(name, "Bash");
                assert_eq!(arguments["command"], "ls -la");
                assert_eq!(arguments["limit"], 5);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn render_tool_call_with_no_arguments_still_parses() {
        let rendered = render_tool_call("Noop", &json!({}));
        let parts = parse_output(&rendered);
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            OutputPart::ToolCall { name, arguments } => {
                assert_eq!(name, "Noop");
                assert_eq!(*arguments, json!({}));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn parse_function_call_xml_preserves_internal_newlines_in_a_multiline_value() {
        let raw = "<function=Write>\n<parameter=content>\nline 1\nline 2\n</parameter>\n</function>";
        let (name, parsed) = parse_function_call_xml(raw).expect("parses");
        assert_eq!(name, "Write");
        assert_eq!(parsed["content"], "line 1\nline 2");
    }

    #[test]
    fn parse_function_call_xml_rejects_a_missing_closing_function_tag() {
        assert!(parse_function_call_xml("<function=Bash>\n<parameter=command>\nls\n</parameter>\n").is_none());
    }

    #[test]
    fn parse_function_call_xml_rejects_an_unclosed_parameter() {
        assert!(parse_function_call_xml("<function=Bash>\n<parameter=command>\nls\n</function>").is_none());
    }

    #[test]
    fn parse_output_extracts_the_new_xml_tool_call_format() {
        let out = parse_output(
            "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>",
        );
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
    fn parse_output_handles_two_xml_tool_calls_back_to_back() {
        // Confirms the *outer* boundary logic (unchanged by this fix) still
        // separates calls correctly now that the inner content is XML.
        let out = parse_output(
            "<tool_call>\n<function=A>\n</function>\n</tool_call>\
             <tool_call>\n<function=B>\n</function>\n</tool_call>",
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "A"));
        assert!(matches!(&out[1], OutputPart::ToolCall { name, .. } if name == "B"));
    }

    #[test]
    fn parse_output_falls_back_to_text_on_malformed_xml() {
        let raw = "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</tool_call>";
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    // ── bare `<function>NAME</function>`, no wrapper ─────────────────────────

    #[test]
    fn parse_output_recovers_a_bare_function_call_with_no_wrapper() {
        let out = parse_output("<function>read</function>");
        assert_eq!(out.len(), 1);
        match &out[0] {
            OutputPart::ToolCall { name, arguments } => {
                assert_eq!(name, "read");
                assert_eq!(*arguments, json!({}));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn parse_output_handles_two_bare_function_calls_back_to_back() {
        let out = parse_output("<function>read</function>\n<function>glob</function>");
        assert_eq!(out.len(), 3); // read, the newline between them, glob
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "read"));
        assert_eq!(out[1], OutputPart::Text("\n".into()));
        assert!(matches!(&out[2], OutputPart::ToolCall { name, .. } if name == "glob"));
    }

    #[test]
    fn parse_output_rejects_a_bare_function_name_containing_whitespace() {
        // The false-positive guard: ordinary prose incidentally shaped like
        // `<function>...</function>` should not be read as a call.
        let raw = "<function>this has spaces</function>";
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    #[test]
    fn parse_output_recovers_an_unclosed_bare_function_call() {
        let raw = "<function>rea"; // cut off mid-generation
        assert_eq!(parse_output(raw), vec![OutputPart::Text(raw.into())]);
    }

    #[test]
    fn a_stream_can_mix_a_wrapped_call_and_a_bare_call() {
        let out = parse_output(
            r#"<tool_call>{"name":"Bash","arguments":{"command":"ls"}}</tool_call> then <function>read</function>"#,
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "Bash"));
        assert_eq!(out[1], OutputPart::Text(" then ".into()));
        assert!(matches!(&out[2], OutputPart::ToolCall { name, .. } if name == "read"));
    }

    #[test]
    fn bare_function_call_split_across_arbitrary_chunk_boundaries_matches_the_one_chunk_result() {
        let full = "before <function>read</function> after";
        let whole = parse_output(full);
        assert_eq!(feed_in_chunks(full, full.len()), whole);
        assert_eq!(feed_in_chunks(full, 1), whole); // byte-at-a-time
        assert_eq!(feed_in_chunks(full, 3), whole); // arbitrary split
    }

    #[test]
    fn a_dangling_prefix_of_the_shorter_bare_tag_is_held_back_correctly() {
        // "<funct" is a prefix of "<function>" but not of "<tool_call>" — the
        // hold-back must still recognise it needs holding.
        let mut sc = TagScanner::new();
        let events = sc.feed("plain <funct");
        assert_eq!(events, vec![ScanEvent::Text("plain ".into())]);
        // Never completes — finish() must recover the held-back fragment.
        assert_eq!(sc.finish(), vec![ScanEvent::Text("<funct".into())]);
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
