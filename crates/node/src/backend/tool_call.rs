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
//! A Strata `IQ3_XXS` session also opened a call with `<parameter=NAME>`
//! where `<function=NAME>` belongs, the rest well formed and closed by
//! `</function>`; `repair_parameter_opening` reads that shape as a call.
//!
//! A Gemma 4 session, given the same `<tool_call><function=NAME>` instruction,
//! wrote each parameter as its own element (`<path>a.html</path>`) and shut
//! the call with `</NAME>`; `parse_parameters` reads that shape as well.
//! Under Hermes Agent's long system prompt the same model dropped the wrapper
//! and `function=` too, writing the tool as the element itself:
//! `<execute_code><code>...</code></execute_code>` (`CallKind::NamedElement`).
//! For a Hermes Agent turn from Telegram it fell back to its own native
//! format, `<|tool_call>call:NAME{…}<tool_call|>` (`CallKind::Native`).
//!
//! Some Qwen turns also fall back to the older
//! `<function_calls><invoke name="…"><parameter name="…">` dialect after a
//! tool validation error. That recovery form is accepted as one container
//! holding one or more calls, but the canonical format emitted into prompts
//! and replayed history remains `<tool_call><function=…>`.
//!
//! See `crate::messages`'s module doc comment for why this is
//! prompt-engineered and text-parsed rather than protocol-level: this
//! backend has no grammar-constrained decoding anywhere.

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

/// One entry from a request's `tools` array — name + description + JSON
/// Schema parameters. Both Anthropic (`input_schema`) and OpenAI
/// (`function.parameters`) map onto this the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: serde_json::Value,
    /// Fine-grained streaming for **this** tool: delivers the argument
    /// fragments as the model generates them, without holding them back to
    /// validate first.
    ///
    /// Anthropic's shape, verified against the live documentation on 2026-08-29
    /// (`platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming`):
    /// an **optional field in the tool definition**, with no beta header - the
    /// legacy `fine-grained-tool-streaming-2025-05-14` header was replaced by
    /// it. Absent or `false` keeps the holding back, which there is **per
    /// parameter value**, not for the whole call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
}

impl ToolDef {
    pub(crate) fn streams_eagerly(&self) -> bool {
        self.eager_input_streaming.unwrap_or(false)
    }
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
    let mut out = String::from("# Tools\n\nYou have access to the following functions:\n\n<tools>");
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
const FUNCTION_CALLS_OPEN: &str = "<function_calls>";
const FUNCTION_CALLS_CLOSE: &str = "</function_calls>";
/// No `=`: this is the name-only `<function>NAME</function>` shape. An
/// unwrapped `<function=NAME>` is its own guarded kind, `CallKind::BareXml`.
const BARE_FUNCTION_OPEN: &str = "<function>";
const BARE_FUNCTION_CLOSE: &str = "</function>";
/// Misspelt wrappers seen from the `IQ3_XXS` quants, read as `<tool_call>`.
const FUNCTION_CALL_OPEN: &str = "<function_call>";
const FUNCTION_CALL_CLOSE: &str = "</function_call>";
const PARAMETER_CALL_OPEN: &str = "<parameter_call>";
const PARAMETER_CALL_CLOSE: &str = "</parameter_call>";
/// Trailing space on purpose: only `<invoke name=…` opens a call.
const INVOKE_OPEN: &str = "<invoke ";
const INVOKE_CLOSE: &str = "</invoke>";
const BARE_XML_OPEN: &str = "<function=";
/// The XML namespace prefix a Strata session put on the invoke dialect's tag
/// names (`<NS:invoke name="bash">`, then a plain `</invoke>`). Read as the
/// plain tags; see [`strip_namespace`].
const TAG_NAMESPACE: &str = "antml:";
const NS_INVOKE_OPEN: &str = "<antml:invoke ";
const NS_INVOKE_CLOSE: &str = "</antml:invoke>";
const NS_FUNCTION_CALLS_OPEN: &str = "<antml:function_calls>";
const NS_FUNCTION_CALLS_CLOSE: &str = "</antml:function_calls>";
/// Gemma 4's own call markers and string quote, verified against the
/// `tokenizer.chat_template` of the Gemma 4 vocabulary GGUF:
/// `<|tool_call>call:NAME{key:<|"|>text<|"|>,n:3}<tool_call|>`.
const NATIVE_CALL_OPEN: &str = "<|tool_call>";
const NATIVE_CALL_CLOSE: &str = "<tool_call|>";
const NATIVE_QUOTE: &str = "<|\"|>";
/// Closers that can trail a call that already ended; dropped, not shown.
const STRAY_CLOSERS: &[&str] = &[
    TOOL_CALL_CLOSE,
    FUNCTION_CALL_CLOSE,
    PARAMETER_CALL_CLOSE,
    FUNCTION_CALLS_CLOSE,
    INVOKE_CLOSE,
    BARE_FUNCTION_CLOSE,
    NS_INVOKE_CLOSE,
    NS_FUNCTION_CALLS_CLOSE,
];

/// Which opening marker started the block currently being scanned — decides
/// both which closing tag to look for and how to interpret what's captured.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CallKind {
    /// `<tool_call>...</tool_call>` — the normal case.
    Wrapped,
    /// `<function_calls><invoke name="…">...</invoke></function_calls>` — a
    /// compatibility dialect observed while a model tried to repair a call.
    FunctionCalls,
    /// `<function>NAME</function>` with no wrapper at all — a tolerated
    /// fallback for output that doesn't even attempt the real convention;
    /// see the module doc comment.
    BareFunction,
    /// `<invoke name="…">...</invoke>` with no `<function_calls>` container.
    Invoke,
    /// `<function=NAME>...</function>` with no `<tool_call>` opener, which a
    /// Strata session wrote straight after `</think>`. Guarded: it ends only
    /// on a `</function>` outside any parameter value, and with tools
    /// declared the name must be one of them, so prose quoting the tag stays
    /// prose.
    BareXml,
    /// `<NAME><param>value</param></NAME>`: a **declared** tool's name as the
    /// element, followed by a parameter element (Gemma 4 under Hermes Agent).
    /// Its closer depends on the name, so it is not in [`CALL_FORMATS`]:
    /// [`TagScanner`] keeps the name and resolves this kind itself.
    NamedElement,
    /// `<|tool_call>call:NAME{…}<tool_call|>`: Gemma 4's native format, which
    /// it falls back to despite the `<tool_call>` instruction (a Hermes Agent
    /// session over Telegram). See [`parse_native_call`].
    Native,
}

impl CallKind {
    fn format(self) -> &'static CallFormat {
        CALL_FORMATS
            .iter()
            .find(|format| format.kind == self)
            .expect("every call kind has a registered format")
    }

    fn open(self) -> &'static str {
        self.format().open
    }

    fn close(self) -> &'static str {
        self.format().close
    }

    /// Every tag that ends a span of this kind; the earliest one wins. The
    /// quantized models mismatch the pair they opened (`<function_call>` shut
    /// by `</tool_call>`, `<tool_call>` shut by `</function_calls>`), so the
    /// closer is not tied to the opener.
    fn closers(self) -> &'static [&'static str] {
        match self {
            CallKind::Wrapped => &[
                TOOL_CALL_CLOSE,
                FUNCTION_CALL_CLOSE,
                PARAMETER_CALL_CLOSE,
                FUNCTION_CALLS_CLOSE,
                NS_FUNCTION_CALLS_CLOSE,
            ],
            CallKind::FunctionCalls => &[
                FUNCTION_CALLS_CLOSE,
                TOOL_CALL_CLOSE,
                FUNCTION_CALL_CLOSE,
                NS_FUNCTION_CALLS_CLOSE,
            ],
            CallKind::BareFunction => &[BARE_FUNCTION_CLOSE],
            // An invoke the model shut with another dialect's wrapper
            // (`<NS:invoke …>…</function></tool_call>`) ends there too.
            CallKind::Invoke => &[
                INVOKE_CLOSE,
                NS_INVOKE_CLOSE,
                TOOL_CALL_CLOSE,
                FUNCTION_CALLS_CLOSE,
                NS_FUNCTION_CALLS_CLOSE,
            ],
            CallKind::BareXml => &[BARE_FUNCTION_CLOSE],
            CallKind::NamedElement => &[],
            CallKind::Native => &[NATIVE_CALL_CLOSE],
        }
    }
}

/// Textual boundaries accepted by the shared scanner. Keep this registry
/// declarative: add a pair only after verifying it in the active GGUF's
/// `tokenizer.chat_template`, never because a model name happens to suggest
/// one. The first entry wins when two markers begin at the same byte.
struct CallFormat {
    kind: CallKind,
    open: &'static str,
    close: &'static str,
}

const CALL_FORMATS: &[CallFormat] = &[
    CallFormat {
        kind: CallKind::Wrapped,
        open: TOOL_CALL_OPEN,
        close: TOOL_CALL_CLOSE,
    },
    CallFormat {
        kind: CallKind::FunctionCalls,
        open: FUNCTION_CALLS_OPEN,
        close: FUNCTION_CALLS_CLOSE,
    },
    CallFormat {
        kind: CallKind::BareFunction,
        open: BARE_FUNCTION_OPEN,
        close: BARE_FUNCTION_CLOSE,
    },
    CallFormat {
        kind: CallKind::Wrapped,
        open: FUNCTION_CALL_OPEN,
        close: FUNCTION_CALL_CLOSE,
    },
    CallFormat {
        kind: CallKind::Wrapped,
        open: PARAMETER_CALL_OPEN,
        close: PARAMETER_CALL_CLOSE,
    },
    CallFormat {
        kind: CallKind::Invoke,
        open: INVOKE_OPEN,
        close: INVOKE_CLOSE,
    },
    CallFormat {
        kind: CallKind::Invoke,
        open: NS_INVOKE_OPEN,
        close: INVOKE_CLOSE,
    },
    CallFormat {
        kind: CallKind::FunctionCalls,
        open: NS_FUNCTION_CALLS_OPEN,
        close: FUNCTION_CALLS_CLOSE,
    },
    CallFormat {
        kind: CallKind::BareXml,
        open: BARE_XML_OPEN,
        close: BARE_FUNCTION_CLOSE,
    },
    CallFormat {
        kind: CallKind::Native,
        open: NATIVE_CALL_OPEN,
        close: NATIVE_CALL_CLOSE,
    },
];

/// The request's declared tools, by name and parameter names. It lets the
/// scanner attribute a call the model wrote with no name at all
/// (`<tool_call><parameter name="command">ls</parameter></invoke>`): the
/// parameters name the tool when exactly one declared tool takes them all.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolSignatures(std::sync::Arc<Vec<(String, Vec<String>)>>);

impl ToolSignatures {
    pub(crate) fn of(tools: &[ToolDef]) -> Self {
        Self(std::sync::Arc::new(
            tools
                .iter()
                .map(|tool| {
                    let params = tool
                        .input_schema
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| p.keys().cloned().collect())
                        .unwrap_or_default();
                    (tool.name.clone(), params)
                })
                .collect(),
        ))
    }

    /// Whether `name` may be called: any name when no tools were declared.
    fn declares(&self, name: &str) -> bool {
        self.0.is_empty() || self.0.iter().any(|(declared, _)| declared == name)
    }

    /// The earliest `<NAME>` of a declared tool that opens a
    /// [`CallKind::NamedElement`] call: one followed by a parameter element.
    /// `Undecided` while only whitespace or part of that element has
    /// arrived; prose like `use the <terminal> tool` never opens one.
    fn element_opening(&self, text: &str) -> Option<ElementOpening> {
        let mut earliest: Option<ElementOpening> = None;
        for (name, _) in self.0.iter() {
            let tag = format!("<{name}>");
            let mut from = 0;
            while let Some(relative) = text[from..].find(&tag) {
                let at = from + relative;
                from = at + tag.len();
                let opening = match element_follows(&text[from..]) {
                    Some(true) => ElementOpening::Opens {
                        at,
                        len: tag.len(),
                        name: name.clone(),
                    },
                    Some(false) => ElementOpening::Undecided { at },
                    None => continue,
                };
                if earliest.as_ref().is_none_or(|e| at < e.at()) {
                    earliest = Some(opening);
                }
                break;
            }
        }
        earliest
    }

    /// How much of `text`'s end could still grow into a declared `<NAME>`.
    fn element_prefix_hold(&self, text: &str) -> usize {
        self.0
            .iter()
            .map(|(name, _)| longest_prefix_overlap(text, &format!("<{name}>")))
            .max()
            .unwrap_or(0)
    }

    fn attribute(&self, arguments: &serde_json::Value) -> Option<String> {
        let keys = arguments.as_object().filter(|keys| !keys.is_empty())?;
        let mut matching = self
            .0
            .iter()
            .filter(|(_, params)| keys.keys().all(|key| params.contains(key)));
        let (name, _) = matching.next()?;
        matching.next().is_none().then(|| name.clone())
    }
}

/// Text after a complete call is not an answer: the tool has not run, so
/// whatever the model writes next is its guess at the result (Gemma 4 wrote
/// "The script outputted `[0, 1, 1, ...]`" after an `execute_code` call). The
/// tools block already asks for no suffix; this enforces it, as vLLM's Hermes
/// parser does. Text before the first call stays, later calls still count,
/// and a malformed call (degraded to text) is not a call.
fn drop_text_after_calls(events: Vec<ScanEvent>, called: &mut bool) -> Vec<ScanEvent> {
    events
        .into_iter()
        .filter(|event| match event {
            ScanEvent::Text(_) => !*called,
            ScanEvent::ToolCallClose(Ok(_)) => {
                *called = true;
                true
            }
            _ => true,
        })
        .collect()
}

enum ElementOpening {
    Opens { at: usize, len: usize, name: String },
    Undecided { at: usize },
}

impl ElementOpening {
    fn at(&self) -> usize {
        match self {
            ElementOpening::Opens { at, .. } | ElementOpening::Undecided { at } => *at,
        }
    }
}

/// Whether what follows a tool's `<NAME>` starts a parameter element:
/// `Some(true)` it does, `Some(false)` not known yet, `None` it does not.
fn element_follows(after: &str) -> Option<bool> {
    let rest = after.trim_start();
    let Some(tag) = rest.strip_prefix('<') else {
        return rest.is_empty().then_some(false);
    };
    let name_len = tag
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(tag.len());
    if name_len == tag.len() {
        return Some(false);
    }
    let starts_well = tag
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let closes = tag[name_len..].starts_with('>')
        || (&tag[..name_len] == "parameter" && tag[name_len..].starts_with(['=', ' ']));
    (starts_well && closes).then_some(true)
}

/// A [`CallKind::NamedElement`] body read as the `<function=NAME>` call it
/// stands for, or the literal markup when it does not parse.
fn named_element_call(name: &str, raw: &str) -> Result<(String, serde_json::Value), String> {
    parse_function_call_xml(&format!("<function={name}>{raw}</{name}>"))
        .ok_or_else(|| format!("<{name}>{raw}</{name}>"))
}

/// A `Wrapped` call that failed to parse only because it carries no name:
/// its body is parameters alone. Attributed through `tools`, or left as the
/// failure it was.
fn attribute_nameless(
    events: Vec<ScanEvent>,
    raw: &str,
    kind: CallKind,
    tools: &ToolSignatures,
) -> Vec<ScanEvent> {
    if kind == CallKind::BareXml {
        return match events.as_slice() {
            [ScanEvent::ToolCallClose(Ok((name, _)))] if !tools.declares(name) => vec![
                ScanEvent::ToolCallClose(Err(format!("{BARE_XML_OPEN}{raw}{BARE_FUNCTION_CLOSE}"))),
            ],
            _ => events,
        };
    }
    if kind != CallKind::Wrapped || !matches!(events.as_slice(), [ScanEvent::ToolCallClose(Err(_))]) {
        return events;
    }
    let normalized = normalize_wrapped_body(raw);
    // A nameless body may still end in the `</function>` its missing opener
    // would have matched.
    let body = normalized
        .strip_suffix(BARE_FUNCTION_CLOSE)
        .unwrap_or(&normalized)
        .trim_end();
    let call = body
        .starts_with("<parameter")
        .then(|| parse_invoke_parameters(body))
        .flatten()
        .and_then(|arguments| Some((tools.attribute(&arguments)?, arguments)));
    match call {
        Some(call) => vec![ScanEvent::ToolCallClose(Ok(call))],
        None => events,
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
    /// The call the output cap cut in the middle. It is not the model's choice:
    /// the client's budget ran out with the tag still open. See
    /// [`TagScanner::finish_truncated`] and [`PartialCall`].
    ToolCallTruncated(PartialCall),
}

/// A tool call interrupted by the output cap, in the shape in which the two
/// providers return it.
///
/// OpenAI and Anthropic **do not discard** the cut call and do **not** degrade
/// it into text: it comes back structured, with incomplete arguments, beside
/// the terminal stop reason (`finish_reason: "length"` /
/// `stop_reason: "max_tokens"`). The guidance from both is the same - the
/// client checks the stop reason and repeats the request with a larger cap,
/// without executing anything.
///
/// Established from the live documentation on 2026-08-29:
/// - `platform.claude.com/docs/en/build-with-claude/handling-stop-reasons` -
///   with `stop_reason: "max_tokens"`, the client checks whether the **last
///   block** is an incomplete `tool_use` and repeats with a larger `max_tokens`.
/// - `developers.openai.com/api/reference/…/chat` - `function.arguments` is
///   documented as a string that is not always valid JSON, to be validated
///   before executing.
///
/// See `docs/subsystems/inference.md` (9.1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartialCall {
    pub(crate) name: String,
    /// The arguments as OpenAI delivers them: an **unfinished** JSON string,
    /// cut where the model was cut. It is on purpose that it does not parse -
    /// that is what makes a client that ignores the stop reason fail instead
    /// of executing half a call.
    pub(crate) arguments: String,
    /// The object with whatever could be recovered, for the `input` of
    /// Anthropic's `tool_use` block, which is typed as an object. Empty when
    /// the cut fell before the first parameter closed.
    pub(crate) recovered: serde_json::Value,
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
    /// A call just closed and only whitespace followed: a closing tag that
    /// comes next belongs to that call and is dropped instead of shown.
    after_call: bool,
    tools: ToolSignatures,
    /// The tool named by the open [`CallKind::NamedElement`] call.
    element: Option<String>,
    /// A complete call came out: from here on text is dropped (see
    /// [`drop_text_after_calls`]), while later calls still count.
    called: bool,
}

impl TagScanner {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            state: ScanState::Outside,
            after_call: false,
            tools: ToolSignatures::default(),
            element: None,
            called: false,
        }
    }

    /// A scanner that can attribute a nameless call to one of `tools`.
    pub(crate) fn with_tools(tools: ToolSignatures) -> Self {
        Self {
            tools,
            ..Self::new()
        }
    }

    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<ScanEvent> {
        let events = self.scan(chunk);
        drop_text_after_calls(events, &mut self.called)
    }

    fn scan(&mut self, chunk: &str) -> Vec<ScanEvent> {
        self.pending.push_str(chunk);
        let mut events = Vec::new();
        loop {
            match &self.state {
                ScanState::Outside => {
                    while self.after_call {
                        let trimmed = self.pending.trim_start();
                        match STRAY_CLOSERS.iter().find(|c| trimmed.starts_with(**c)) {
                            Some(closer) => {
                                self.pending = trimmed[closer.len()..].to_string();
                            }
                            None => break,
                        }
                    }
                    // Earliest opener wins; the registry order breaks ties in
                    // favour of the real wrapper (higher-confidence sighting).
                    let chosen = CALL_FORMATS
                        .iter()
                        .enumerate()
                        .filter_map(|(priority, format)| {
                            self.pending
                                .find(format.open)
                                .map(|position| (position, priority, format))
                        })
                        .min_by_key(|(position, priority, _)| (*position, *priority))
                        .map(|(position, _, format)| (position, format));
                    let before_static = |at: usize| chosen.is_none_or(|(i, _)| at < i);
                    match self.tools.element_opening(&self.pending) {
                        Some(ElementOpening::Opens { at, len, name }) if before_static(at) => {
                            let head = self.pending[..at].to_string();
                            self.pending = self.pending[at + len..].to_string();
                            if !head.is_empty() {
                                events.push(ScanEvent::Text(head));
                            }
                            self.after_call = false;
                            events.push(ScanEvent::ToolCallOpen);
                            self.element = Some(name);
                            self.state = ScanState::Inside(String::new(), CallKind::NamedElement);
                            continue;
                        }
                        Some(ElementOpening::Undecided { at }) if before_static(at) => {
                            if at > 0 {
                                let head = self.pending[..at].to_string();
                                if !head.trim().is_empty() {
                                    self.after_call = false;
                                }
                                events.push(ScanEvent::Text(head));
                                self.pending = self.pending[at..].to_string();
                            }
                            break;
                        }
                        _ => {}
                    }
                    match chosen {
                        Some((i, format)) => {
                            let kind = format.kind;
                            let head = self.pending[..i].to_string();
                            self.pending = self.pending[i + format.open.len()..].to_string();
                            if !head.is_empty() {
                                events.push(ScanEvent::Text(head));
                            }
                            self.after_call = false;
                            events.push(ScanEvent::ToolCallOpen);
                            self.state = ScanState::Inside(String::new(), kind);
                        }
                        None => {
                            // Hold back whatever could still complete *either*
                            // candidate tag once more input arrives.
                            let mut hold = CALL_FORMATS
                                .iter()
                                .map(|format| longest_prefix_overlap(&self.pending, format.open))
                                .max()
                                .unwrap_or(0)
                                .max(self.tools.element_prefix_hold(&self.pending));
                            if self.after_call {
                                // A stray closer may still be arriving; keep it
                                // (and the whitespace before it) back.
                                let trimmed = self.pending.trim_start();
                                if !trimmed.is_empty()
                                    && STRAY_CLOSERS.iter().any(|c| c.starts_with(trimmed))
                                {
                                    hold = hold.max(self.pending.len());
                                }
                            }
                            let flush_len = self.pending.len() - hold;
                            if flush_len > 0 {
                                let flushed = self.pending[..flush_len].to_string();
                                if !flushed.trim().is_empty() {
                                    self.after_call = false;
                                }
                                events.push(ScanEvent::Text(flushed));
                                self.pending = self.pending[flush_len..].to_string();
                            }
                            break;
                        }
                    }
                }
                ScanState::Inside(_, CallKind::NamedElement) => {
                    let name = self.element.clone().unwrap_or_default();
                    let close = format!("</{name}>");
                    match self.pending.find(&close) {
                        Some(i) => {
                            let head = self.pending[..i].to_string();
                            self.pending = self.pending[i + close.len()..].to_string();
                            self.after_call = true;
                            self.element = None;
                            let ScanState::Inside(mut raw, _) =
                                std::mem::replace(&mut self.state, ScanState::Outside)
                            else {
                                unreachable!("matched Inside above")
                            };
                            if !head.is_empty() {
                                raw.push_str(&head);
                                events.push(ScanEvent::ToolCallChunk(head));
                            }
                            events.push(ScanEvent::ToolCallClose(named_element_call(&name, &raw)));
                        }
                        None => {
                            let hold = longest_prefix_overlap(&self.pending, &close);
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
                ScanState::Inside(raw, kind) => {
                    let closed = kind
                        .closers()
                        .iter()
                        .filter_map(|tag| {
                            let at = if *kind == CallKind::Native {
                                native_closer_position(raw, &self.pending)
                            } else {
                                let own = *tag == kind.close() && *kind != CallKind::BareXml;
                                closer_position(raw, &self.pending, tag, own)
                            };
                            at.map(|at| (at, tag.len()))
                        })
                        .min_by_key(|(at, _)| *at);
                    match closed {
                        Some((i, close_len)) => {
                            let head = self.pending[..i].to_string();
                            self.pending = self.pending[i + close_len..].to_string();
                            self.after_call = true;
                            let ScanState::Inside(mut raw, kind) =
                                std::mem::replace(&mut self.state, ScanState::Outside)
                            else {
                                unreachable!("matched Inside above")
                            };
                            if !head.is_empty() {
                                raw.push_str(&head);
                                events.push(ScanEvent::ToolCallChunk(head));
                            }
                            events.extend(attribute_nameless(
                                complete_call_events(&raw, kind),
                                &raw,
                                kind,
                                &self.tools,
                            ));
                        }
                        None => {
                            let hold = kind
                                .closers()
                                .iter()
                                .map(|tag| longest_prefix_overlap(&self.pending, tag))
                                .max()
                                .unwrap_or(0);
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

    /// Whatever never resolved into a complete tag by the time the **model**
    /// stopped — a dangling `<tool_c` fragment, or a call the model abandoned
    /// before closing. Nothing is dropped: the model chose to stop there, and
    /// the text it produced is its answer.
    ///
    /// For the end that is not the model's choice - the budget ran out - use
    /// [`TagScanner::finish_truncated`], which discards instead of degrading.
    pub(crate) fn finish(self) -> Vec<ScanEvent> {
        let mut called = self.called;
        drop_text_after_calls(self.finish_scan(), &mut called)
    }

    fn finish_scan(self) -> Vec<ScanEvent> {
        match self.state {
            ScanState::Outside if self.pending.is_empty() => Vec::new(),
            ScanState::Outside => vec![ScanEvent::Text(self.pending)],
            ScanState::Inside(raw, CallKind::NamedElement) => {
                let name = self.element.unwrap_or_default();
                let body = format!("{raw}{}", self.pending);
                match named_element_call(&name, &body) {
                    Ok(call) => {
                        let mut events = Vec::new();
                        if !self.pending.is_empty() {
                            events.push(ScanEvent::ToolCallChunk(self.pending));
                        }
                        events.push(ScanEvent::ToolCallClose(Ok(call)));
                        events
                    }
                    Err(_) => vec![ScanEvent::Text(format!("<{name}>{body}"))],
                }
            }
            ScanState::Inside(raw, kind) => {
                // The model stopped without shutting the tag. When what it
                // wrote is a whole call, it is one: the missing closer is the
                // quantized model's slip, not a reason to show the markup.
                let body = format!("{raw}{}", self.pending);
                let calls =
                    attribute_nameless(complete_call_events(&body, kind), &body, kind, &self.tools);
                let whole = kind != CallKind::BareFunction
                    && !calls.is_empty()
                    && calls
                        .iter()
                        .all(|e| matches!(e, ScanEvent::ToolCallOpen | ScanEvent::ToolCallClose(Ok(_))));
                if whole {
                    let mut events = Vec::new();
                    if !self.pending.is_empty() {
                        events.push(ScanEvent::ToolCallChunk(self.pending));
                    }
                    events.extend(calls);
                    events
                } else {
                    vec![ScanEvent::Text(format!("{}{body}", kind.open()))]
                }
            }
        }
    }

    /// Like [`TagScanner::finish`], but for the end imposed from outside: the
    /// output cap requested by the client ran out in the middle of generation.
    ///
    /// With a call **open and not closed**, it goes out structured and
    /// incomplete - [`ScanEvent::ToolCallTruncated`] - which is what OpenAI and
    /// Anthropic do. Neither degrade into text (raw markup delivered as prose,
    /// the original defect) nor discard: the client needs to see that **that**
    /// call did not fit, to repeat the request with a larger cap.
    /// See [`PartialCall`].
    ///
    /// Cut too early to have a name, there is no call to name and nothing goes
    /// out: a name the model did not finish writing is not a tool, it is a
    /// guess.
    ///
    /// The returned `bool` is `true` whenever the cut fell inside a call, with
    /// a name or without, and it is what makes `x_pom` tell the truth.
    ///
    /// Outside a call nothing changes - truncated text is still text, and a
    /// pending tag prefix (`<tool_c`) never got to open any call.
    pub(crate) fn finish_truncated(self) -> (Vec<ScanEvent>, bool) {
        let mut called = self.called;
        let (events, cut) = self.finish_truncated_scan();
        (drop_text_after_calls(events, &mut called), cut)
    }

    fn finish_truncated_scan(self) -> (Vec<ScanEvent>, bool) {
        match self.state {
            ScanState::Inside(raw, CallKind::NamedElement) => {
                let name = self.element.unwrap_or_default();
                let body = format!("{raw}{}", self.pending);
                let events = parse_partial_function_call_xml(&format!("<function={name}>{body}"))
                    .map(ScanEvent::ToolCallTruncated)
                    .into_iter()
                    .collect();
                (events, true)
            }
            ScanState::Inside(raw, kind) => {
                let body = format!("{raw}{}", self.pending);
                let events = if kind == CallKind::FunctionCalls {
                    partial_function_call_events(&strip_namespace(&body))
                } else if kind == CallKind::Invoke {
                    partial_function_call_events(&strip_namespace(&format!("{INVOKE_OPEN}{body}")))
                } else {
                    parse_partial_call_body(&body, kind)
                        .map(ScanEvent::ToolCallTruncated)
                        .into_iter()
                        .collect()
                };
                (events, true)
            }
            _ => (self.finish(), false),
        }
    }
}

/// What can be recovered from the body of a call the cap cut off.
///
/// Mirrors [`parse_call_body`], but tolerating the missing end: the
/// `</function>` that never came, the `</parameter>` left open, the JSON
/// without the closing brace. `None` when not even the tool name came out whole.
fn parse_partial_call_body(raw: &str, kind: CallKind) -> Option<PartialCall> {
    match kind {
        // `<function>NAME` with no closing: `NAME` may be half written, and
        // half a name identifies no tool at all.
        CallKind::BareFunction | CallKind::FunctionCalls | CallKind::Invoke => None,
        // Resolved by `TagScanner::finish_truncated`, which knows the name.
        CallKind::NamedElement => None,
        CallKind::Native => parse_partial_native_call(raw),
        CallKind::BareXml => parse_partial_function_call_xml(&format!("{BARE_XML_OPEN}{raw}")),
        CallKind::Wrapped => {
            let trimmed = raw.trim_start();
            if trimmed.starts_with("<function=") {
                parse_partial_function_call_xml(trimmed)
            } else if let Some(repaired) = repair_parameter_opening(trimmed) {
                parse_partial_function_call_xml(&repaired)
            } else {
                parse_partial_function_call_json(trimmed)
            }
        }
    }
}

/// Serializes the recovered pairs in the exact shape in which OpenAI delivers
/// a cut call: an **unclosed** JSON object, interrupted where the model was
/// interrupted. `partial` is the pair whose value was still being written -
/// it goes in without the closing quote.
fn unterminated_arguments(
    complete: &[(String, serde_json::Value)],
    partial: Option<(&str, &str)>,
) -> String {
    let mut out = String::from("{");
    for (i, (name, value)) in complete.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::Value::String(name.clone()).to_string());
        out.push(':');
        out.push_str(&value.to_string());
    }
    if let Some((name, value)) = partial {
        if !complete.is_empty() {
            out.push(',');
        }
        out.push_str(&serde_json::Value::String(name.to_string()).to_string());
        out.push(':');
        // The opening quote goes in, the closing one does not: the value is not over.
        let escaped = serde_json::Value::String(value.to_string()).to_string();
        out.push_str(&escaped[..escaped.len() - 1]);
    }
    out
}

/// `<function=NAME>` with zero or more complete `<parameter=P>v</parameter>`
/// and, possibly, a last open parameter the cut caught in the middle.
fn parse_partial_function_call_xml(raw: &str) -> Option<PartialCall> {
    let rest = raw.strip_prefix("<function=")?;
    // Without the `>` the name itself is half written.
    let (name, mut cursor) = rest.split_once('>')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let mut complete: Vec<(String, serde_json::Value)> = Vec::new();
    let mut partial: Option<(String, String)> = None;
    loop {
        let trimmed = cursor.trim_start();
        let Some(after_open) = trimmed.strip_prefix("<parameter=") else {
            break;
        };
        let Some((pname, after_name)) = after_open.split_once('>') else {
            // `<parameter=pa` - not even the parameter name closed.
            break;
        };
        let pname = pname.trim().to_string();
        match after_name.find(PARAM_CLOSE) {
            Some(close) => {
                let value_raw = after_name[..close].trim();
                complete.push((pname, decode_parameter_value(value_raw)));
                cursor = &after_name[close + PARAM_CLOSE.len()..];
            }
            None => {
                // What can still become `</parameter>` is not content: without
                // this hold-back, a cut in the middle of the tag would deliver
                // `</paramet` inside the value. It is the same hold-back as in
                // `open_parameter_prefix`, and it is what keeps the truncated
                // version always extending what the streaming already sent.
                let hold = [PARAM_CLOSE, "</function>"]
                    .iter()
                    .map(|tag| longest_prefix_overlap(after_name, tag))
                    .max()
                    .unwrap_or(0);
                let visible = &after_name[..after_name.len() - hold];
                partial = Some((pname, visible.trim_start().to_string()));
                break;
            }
        }
    }

    let mut recovered = serde_json::Map::new();
    for (pname, value) in &complete {
        recovered.insert(pname.clone(), value.clone());
    }
    if let Some((pname, value)) = &partial {
        recovered.insert(pname.clone(), serde_json::Value::String(value.clone()));
    }

    Some(PartialCall {
        name: name.to_string(),
        arguments: unterminated_arguments(
            &complete,
            partial.as_ref().map(|(p, v)| (p.as_str(), v.as_str())),
        ),
        recovered: serde_json::Value::Object(recovered),
    })
}

/// The Hermes form (`{"name":…,"arguments":{…`) cut off. The name is only
/// usable if its string closed; the rest of the body is already, in itself,
/// unfinished JSON - and it is delivered as is, which is exactly what OpenAI
/// returns.
fn parse_partial_function_call_json(raw: &str) -> Option<PartialCall> {
    let name = complete_json_string_field(raw, "name")?;
    let arguments = match raw.find("\"arguments\"") {
        Some(at) => {
            let after = &raw[at + "\"arguments\"".len()..];
            let after = after.trim_start();
            let after = after.strip_prefix(':').unwrap_or(after);
            after.trim_start().to_string()
        }
        None => String::from("{"),
    };
    // A body that happens to parse whole is still a cut call: the closing tag
    // did not come, so more arguments could have been on the way.
    let recovered = serde_json::from_str::<serde_json::Value>(&arguments)
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| json!({}));
    Some(PartialCall {
        name,
        arguments,
        recovered,
    })
}

/// The value of a JSON string key, **only** when the string closed. A name
/// cut in the middle identifies no tool at all, so `None`.
fn complete_json_string_field(raw: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let at = raw.find(&key)?;
    let after = raw[at + key.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let after = after.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = after.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value).filter(|v| !v.is_empty()),
            '\\' => value.push(chars.next()?),
            _ => value.push(c),
        }
    }
    None
}

/// Longest suffix of `s` that is also a strict prefix of `tag` — how much of
/// `s`'s tail could still turn into `tag` once more input arrives. Both tags
/// are pure ASCII, so every byte index this returns is guaranteed to land on
/// a UTF-8 char boundary in `s` regardless of non-ASCII text around it.
fn longest_prefix_overlap(s: &str, tag: &str) -> usize {
    let max = s.len().min(tag.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|&len| s.ends_with(&tag[..len]))
        .unwrap_or(0)
}

/// One bare invoke rebuilt from the text after its opener: namespace taken
/// off, a trailing `</function>` (another dialect's closer) dropped, and the
/// `</invoke>` the scanner consumed, or the model never wrote, put back.
fn invoke_body(raw: &str) -> String {
    let normalized = strip_namespace(raw);
    let mut body = normalized.trim_end();
    while let Some(rest) = body
        .strip_suffix(BARE_FUNCTION_CLOSE)
        .or_else(|| body.strip_suffix(INVOKE_CLOSE))
    {
        body = rest.trim_end();
    }
    format!("{INVOKE_OPEN}{body}{INVOKE_CLOSE}")
}

/// Where `tag` ends the span opened by `raw`, in `pending`. The kind's own
/// closer ends it wherever it appears, as it always did. A closer of another
/// dialect only counts outside a parameter value: a `write` whose content
/// mentions `</function_call>` must not be cut there.
fn closer_position(raw: &str, pending: &str, tag: &str, own: bool) -> Option<usize> {
    if own {
        return pending.find(tag);
    }
    let mut from = 0;
    while let Some(relative) = pending[from..].find(tag) {
        let at = from + relative;
        if open_parameters(raw) + open_parameters(&pending[..at]) <= 0 {
            return Some(at);
        }
        from = at + tag.len();
    }
    None
}

/// Where `<tool_call|>` ends a [`CallKind::Native`] span: only outside a
/// `<|"|>` string, so a value that mentions the closer is not cut there.
fn native_closer_position(raw: &str, pending: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(relative) = pending[from..].find(NATIVE_CALL_CLOSE) {
        let at = from + relative;
        let quotes = raw.matches(NATIVE_QUOTE).count() + pending[..at].matches(NATIVE_QUOTE).count();
        if quotes.is_multiple_of(2) {
            return Some(at);
        }
        from = at + NATIVE_CALL_CLOSE.len();
    }
    None
}

/// The body of a [`CallKind::Native`] call, `call:NAME{key:value,…}`, in the
/// grammar of Gemma 4's `format_argument`: bare keys, strings between `<|"|>`
/// (no escapes), and objects, arrays, numbers and booleans as JSON writes
/// them. JSON-quoted keys and strings are read too. `None` on anything else,
/// so the caller falls back to the literal text.
fn parse_native_call(raw: &str) -> Option<(String, serde_json::Value)> {
    let (name, mut args) = native_call_head(raw)?;
    let arguments = args.object()?;
    args.skip_whitespace();
    args.0.is_empty().then_some((name, serde_json::Value::Object(arguments)))
}

/// The tool name and a cursor on what follows its `{`; `None` until the `{`
/// arrives, since before it the name may be half written.
fn native_call_head(raw: &str) -> Option<(String, NativeArgs<'_>)> {
    let body = raw.trim_start();
    let body = body.strip_prefix("call:").unwrap_or(body);
    let (name, rest) = body.split_once('{')?;
    let name = name.trim();
    if name.is_empty() || name.contains(|c: char| c.is_whitespace() || "<>{}:\"".contains(c)) {
        return None;
    }
    Some((name.to_string(), NativeArgs(rest)))
}

/// A cut [`CallKind::Native`] call: the top-level pairs that closed, plus a
/// `<|"|>` string value the cap caught open.
fn parse_partial_native_call(raw: &str) -> Option<PartialCall> {
    let (name, mut args) = native_call_head(raw)?;
    let mut complete: Vec<(String, serde_json::Value)> = Vec::new();
    let mut partial: Option<(String, String)> = None;
    loop {
        args.skip_whitespace();
        if args.0.starts_with('}') {
            break;
        }
        let Some(key) = args.key() else { break };
        args.skip_whitespace();
        if !args.eat(":") {
            break;
        }
        args.skip_whitespace();
        if let Some(open) = args.0.strip_prefix(NATIVE_QUOTE) {
            if !open.contains(NATIVE_QUOTE) {
                partial = Some((key, open.to_string()));
                break;
            }
        }
        let Some(value) = args.value() else { break };
        complete.push((key, value));
        args.skip_whitespace();
        if !args.eat(",") {
            break;
        }
    }
    let mut recovered: serde_json::Map<String, serde_json::Value> = complete.iter().cloned().collect();
    if let Some((key, value)) = &partial {
        recovered.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    Some(PartialCall {
        name,
        arguments: unterminated_arguments(
            &complete,
            partial.as_ref().map(|(k, v)| (k.as_str(), v.as_str())),
        ),
        recovered: serde_json::Value::Object(recovered),
    })
}

/// A cursor over Gemma 4's argument grammar; see [`parse_native_call`].
struct NativeArgs<'a>(&'a str);

impl NativeArgs<'_> {
    fn skip_whitespace(&mut self) {
        self.0 = self.0.trim_start();
    }

    fn eat(&mut self, token: &str) -> bool {
        match self.0.strip_prefix(token) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    /// The members after an object's `{`, through its `}`.
    fn object(&mut self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut map = serde_json::Map::new();
        self.skip_whitespace();
        if self.eat("}") {
            return Some(map);
        }
        loop {
            self.skip_whitespace();
            let key = self.key()?;
            self.skip_whitespace();
            if !self.eat(":") {
                return None;
            }
            let value = self.value()?;
            map.insert(key, value);
            self.skip_whitespace();
            if self.eat("}") {
                return Some(map);
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn key(&mut self) -> Option<String> {
        if self.0.starts_with(NATIVE_QUOTE) || self.0.starts_with('"') {
            return match self.value()? {
                serde_json::Value::String(key) => Some(key),
                _ => None,
            };
        }
        let end = self.0.find(|c: char| c == ':' || c.is_whitespace())?;
        let key = &self.0[..end];
        if key.is_empty() || key.contains(|c: char| "{}[],<>\"".contains(c)) {
            return None;
        }
        self.0 = &self.0[end..];
        Some(key.to_string())
    }

    fn value(&mut self) -> Option<serde_json::Value> {
        self.skip_whitespace();
        if let Some(rest) = self.0.strip_prefix(NATIVE_QUOTE) {
            let (text, after) = rest.split_once(NATIVE_QUOTE)?;
            self.0 = after;
            return Some(serde_json::Value::String(text.to_string()));
        }
        if self.eat("{") {
            return self.object().map(serde_json::Value::Object);
        }
        if self.eat("[") {
            let mut items = Vec::new();
            self.skip_whitespace();
            if self.eat("]") {
                return Some(serde_json::Value::Array(items));
            }
            loop {
                items.push(self.value()?);
                self.skip_whitespace();
                if self.eat("]") {
                    return Some(serde_json::Value::Array(items));
                }
                if !self.eat(",") {
                    return None;
                }
            }
        }
        if self.0.starts_with('"') {
            let mut stream = serde_json::Deserializer::from_str(self.0).into_iter::<serde_json::Value>();
            let value = stream.next()?.ok()?;
            self.0 = &self.0[stream.byte_offset()..];
            return Some(value);
        }
        // A number, a boolean or null: up to the delimiter that must follow.
        let end = self.0.find([',', '}', ']'])?;
        let literal = self.0[..end].trim();
        if literal.is_empty() {
            return None;
        }
        self.0 = &self.0[end..];
        Some(decode_parameter_value(literal))
    }
}

/// Parameter tags opened minus closed in `text`, in either spelling.
fn open_parameters(text: &str) -> isize {
    let text = strip_namespace(text);
    let opened = text.matches("<parameter=").count() + text.matches("<parameter ").count();
    opened as isize - text.matches(PARAM_CLOSE).count() as isize
}

/// The tags of the call dialects with the XML namespace prefix taken off:
/// `<NS:invoke`, `</NS:parameter>` and the like read as their plain forms.
/// Only those tag names are touched, never other text.
fn strip_namespace(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(TAG_NAMESPACE) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = text.to_string();
    for tag in ["invoke", "parameter", "function_calls"] {
        out = out
            .replace(&format!("<{TAG_NAMESPACE}{tag}"), &format!("<{tag}"))
            .replace(&format!("</{TAG_NAMESPACE}{tag}>"), &format!("</{tag}>"));
    }
    std::borrow::Cow::Owned(out)
}

/// Resolves the close of one scanner span. The compatibility
/// `<function_calls>` container may hold several `<invoke>` calls; each one
/// becomes its own open/close pair so streaming adapters allocate distinct
/// tool-call indexes without ever exposing the container markup as content.
fn complete_call_events(raw: &str, kind: CallKind) -> Vec<ScanEvent> {
    let fallback = || format!("{}{raw}{}", kind.open(), kind.close());
    match kind {
        CallKind::Wrapped | CallKind::BareFunction | CallKind::Native => {
            return vec![ScanEvent::ToolCallClose(parse_call_body(raw, kind))];
        }
        // `<function_calls>` around the `<function=NAME>` body of a
        // `<tool_call>`: one call, read the way the real wrapper is.
        CallKind::FunctionCalls if raw.trim_start().starts_with("<function=") => {
            let call = parse_call_body(raw, CallKind::Wrapped).map_err(|_| fallback());
            return vec![ScanEvent::ToolCallClose(call)];
        }
        CallKind::Invoke => {
            let call = parse_function_calls_xml(&invoke_body(raw))
                .filter(|calls| calls.len() == 1)
                .and_then(|mut calls| calls.pop())
                .ok_or_else(fallback);
            return vec![ScanEvent::ToolCallClose(call)];
        }
        CallKind::BareXml => {
            let call = parse_function_call_xml(&format!("{BARE_XML_OPEN}{raw}{BARE_FUNCTION_CLOSE}"))
                .ok_or_else(fallback);
            return vec![ScanEvent::ToolCallClose(call)];
        }
        // Resolved by `TagScanner`, which knows the name its closer needs.
        CallKind::NamedElement => return vec![ScanEvent::ToolCallClose(Err(raw.to_string()))],
        CallKind::FunctionCalls => {}
    }
    match parse_function_calls_xml(&strip_namespace(raw)) {
        Some(calls) if !calls.is_empty() => calls
            .into_iter()
            .enumerate()
            .flat_map(|(index, call)| {
                let mut events = Vec::with_capacity(2);
                if index > 0 {
                    events.push(ScanEvent::ToolCallOpen);
                }
                events.push(ScanEvent::ToolCallClose(Ok(call)));
                events
            })
            .collect(),
        _ => vec![ScanEvent::ToolCallClose(Err(fallback()))],
    }
}

/// `Wrapped`: two inner formats are accepted, tried in this order — the
/// nested `<function=NAME><parameter=P>...` form (`Qwen3.6-35B-A3B`,
/// `Ornith-1.0-35B`) and the legacy Hermes JSON-object form
/// (`Qwen3.5-9B-DeepSeek-V4-Flash`) — see the module doc comment for why
/// both matter on a cluster that swaps models. `BareFunction`: just a name,
/// see `parse_bare_function_call`. `FunctionCalls` is handled by
/// [`complete_call_events`] because one container may contain several calls.
/// Any failure degrades to the literal tagged text, never a crash or a dropped
/// response.
fn parse_call_body(raw: &str, kind: CallKind) -> Result<(String, serde_json::Value), String> {
    let fallback = || format!("{}{raw}{}", kind.open(), kind.close());
    match kind {
        CallKind::BareFunction => parse_bare_function_call(raw).ok_or_else(fallback),
        CallKind::BareXml => parse_function_call_xml(&format!("{BARE_XML_OPEN}{raw}{BARE_FUNCTION_CLOSE}"))
            .ok_or_else(fallback),
        CallKind::FunctionCalls | CallKind::Invoke => parse_function_calls_xml(raw)
            .filter(|calls| calls.len() == 1)
            .and_then(|mut calls| calls.pop())
            .ok_or_else(fallback),
        // Resolved by `TagScanner`, which knows the name its closer needs.
        CallKind::NamedElement => Err(raw.to_string()),
        CallKind::Native => parse_native_call(raw).ok_or_else(fallback),
        CallKind::Wrapped => {
            let normalized = normalize_wrapped_body(raw);
            let raw = normalized.as_str();
            if raw.trim_start().starts_with("<invoke") {
                return parse_function_calls_xml(raw)
                    .filter(|calls| calls.len() == 1)
                    .and_then(|mut calls| calls.pop())
                    .ok_or_else(fallback);
            }
            if raw.trim_start().starts_with("<function=") {
                return parse_function_call_xml(raw).ok_or_else(fallback);
            }
            if let Some(repaired) = repair_parameter_opening(raw) {
                if raw.trim_end().ends_with("</function>") {
                    return parse_function_call_xml(&repaired).ok_or_else(fallback);
                }
            }
            match serde_json::from_str::<serde_json::Value>(raw.trim()) {
                Ok(v) => match v.get("name").and_then(|n| n.as_str()) {
                    Some(name) if !name.is_empty() => Ok((
                        name.to_string(),
                        decode_json_arguments(
                            v.get("arguments").cloned().unwrap_or_else(|| json!({})),
                        ),
                    )),
                    _ => Err(fallback()),
                },
                Err(_) => Err(fallback()),
            }
        }
    }
}

/// The text between a wrapper's tags with the slips of the quantized models
/// undone, so the dialect parsers see one canonical shape: a repeated opener
/// (`<tool_call><tool_call><function=…>`) collapses, closers of another
/// dialect trailing the call (`…</function></invoke>`) go, and a doubled
/// `</function>` becomes one. An `<invoke>` body is shut when it was not.
fn normalize_wrapped_body(raw: &str) -> String {
    let normalized = strip_namespace(raw);
    let mut body = normalized.trim();
    loop {
        let stripped = [TOOL_CALL_OPEN, FUNCTION_CALL_OPEN, PARAMETER_CALL_OPEN, FUNCTION_CALLS_OPEN]
            .iter()
            .find_map(|open| body.strip_prefix(open))
            .map(str::trim_start);
        match stripped {
            Some(rest) => body = rest,
            None => break,
        }
    }
    let is_invoke = body.starts_with("<invoke");
    loop {
        // `</invoke>` is part of an invoke body and noise after anything else.
        let noise: &[&str] = if is_invoke {
            &[TOOL_CALL_CLOSE, FUNCTION_CALL_CLOSE, FUNCTION_CALLS_CLOSE]
        } else {
            &[TOOL_CALL_CLOSE, FUNCTION_CALL_CLOSE, FUNCTION_CALLS_CLOSE, INVOKE_CLOSE]
        };
        if let Some(rest) = noise.iter().find_map(|close| body.strip_suffix(close)) {
            body = rest.trim_end();
            continue;
        }
        if let Some(rest) = body.strip_suffix(BARE_FUNCTION_CLOSE) {
            if rest.trim_end().ends_with(BARE_FUNCTION_CLOSE) {
                body = rest.trim_end();
                continue;
            }
        }
        break;
    }
    if is_invoke && !body.ends_with(INVOKE_CLOSE) {
        return format!("{body}{INVOKE_CLOSE}");
    }
    body.to_string()
}

/// A call whose opening tag the model wrote as `<parameter=NAME>` instead of
/// `<function=NAME>` (seen from a 3-bit Strata quant on a long turn):
/// `<tool_call><parameter=bash><parameter=command>ls</parameter></function></tool_call>`.
/// Rewritten to the `<function=NAME>` form only when the next tag is another
/// `<parameter=`, so a tag holding a value is never taken for a name; the
/// complete parser also requires the closing `</function>`.
fn repair_parameter_opening(raw: &str) -> Option<String> {
    // Either spelling: `<parameter=edit>` or `<parameter name="edit">`.
    let (name, after) = parameter_open(raw.trim_start())?;
    if name.contains(char::is_whitespace) {
        return None;
    }
    let after = after.trim_start();
    is_parameter_tag(after).then(|| format!("<function={name}>\n{after}"))
}

/// `<parameter=` or `<parameter ` - the start of a parameter tag in either
/// spelling, and not `<parameters>` or another word.
fn is_parameter_tag(text: &str) -> bool {
    text.starts_with("<parameter=") || text.starts_with("<parameter ")
}

/// Hermes-style templates normally emit `arguments` as an object, but some
/// templates serialize that object once more inside a JSON string. Decode that
/// extra layer when it is valid JSON; a non-JSON string remains a string so an
/// unrecognized model output is never silently discarded or turned into a
/// different call.
fn decode_json_arguments(arguments: serde_json::Value) -> serde_json::Value {
    match arguments {
        serde_json::Value::String(raw) => {
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw))
        }
        other => other,
    }
}

/// Some model turns put the complete arguments object inside one parameter
/// literally named `arguments`, yielding `{"arguments": {…}}`. Providers
/// expect the inner object. Only the unambiguous single-key/object shape is
/// unwrapped; a scalar or a real call with other parameters is left intact.
fn normalize_arguments_wrapper(arguments: serde_json::Value) -> serde_json::Value {
    let mut object = match arguments {
        serde_json::Value::Object(object) => object,
        other => return other,
    };
    if object.len() != 1 {
        return serde_json::Value::Object(object);
    }
    let Some(wrapped) = object.remove("arguments") else {
        return serde_json::Value::Object(object);
    };
    let decoded = decode_json_arguments(wrapped);
    if decoded.is_object() {
        decoded
    } else {
        object.insert("arguments".to_string(), decoded);
        serde_json::Value::Object(object)
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
    Some((
        name.to_string(),
        serde_json::Value::Object(serde_json::Map::new()),
    ))
}

/// Parses `<function=NAME>(<parameter=P>value</parameter>)*</function>`.
/// Each parameter value is speculatively JSON-parsed (recovers numbers,
/// bools, arrays, objects the model wrote as literal JSON text) and falls
/// back to a plain string otherwise (the common case — a raw command, a
/// path, ...); `.trim()` strips only the tag-adjacent newlines, preserving
/// internal ones, since the template explicitly allows multi-line values.
/// `None` on any structural mismatch — missing `>`, unclosed `</function>`
/// or `</parameter>` — so the caller can fall back to literal text.
/// The value of a `<parameter=…>`: JSON when the text is JSON, a raw string
/// when it is not. The complete parser and the partial one share this function
/// so that they never disagree about the type of the same value.
fn decode_parameter_value(value_raw: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(value_raw)
        .unwrap_or_else(|_| serde_json::Value::String(value_raw.to_string()))
}

/// Splits `<parameter=NAME>` or `<parameter name="NAME">` into the name and
/// what follows the tag. The models mix the two spellings inside one call.
fn parameter_open(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix("<parameter")?;
    let (head, after) = rest.split_once('>')?;
    let name = match head.strip_prefix('=') {
        Some(name) => name,
        None => quoted_attribute(head, "name")?,
    };
    let name = name.trim();
    (!name.is_empty()).then_some((name, after))
}

fn parse_function_call_xml(raw: &str) -> Option<(String, serde_json::Value)> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("<function=")?;
    let (name, rest) = rest.split_once('>')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    // A Gemma 4 session shut the call with `</NAME>`, the tool's own name.
    let body = rest
        .strip_suffix("</function>")
        .or_else(|| rest.strip_suffix(&format!("</{name}>")))?
        .trim();
    let args = parse_parameters(body, serde_json::Map::new())?;
    Some((
        name.to_string(),
        normalize_arguments_wrapper(serde_json::Value::Object(args)),
    ))
}

/// The parameters of a `<function=NAME>` body, in order. Besides both
/// `<parameter=…>` spellings, a parameter written as its own element
/// (`<path>a.html</path>`, seen from Gemma 4) is read. An element's value may
/// hold the same closer as text (an SVG `</path>` in a file's content), so
/// its value ends at the first `</P>` after which the rest still parses.
fn parse_parameters(
    body: &str,
    mut args: serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let cursor = body.trim_start();
    if cursor.is_empty() {
        return Some(args);
    }
    if let Some((pname, after_name)) = parameter_open(cursor) {
        let close_idx = after_name.find(PARAM_CLOSE)?;
        let value_raw = after_name[..close_idx].trim();
        args.insert(pname.to_string(), decode_parameter_value(value_raw));
        return parse_parameters(&after_name[close_idx + PARAM_CLOSE.len()..], args);
    }
    let (pname, after_name) = element_open(cursor)?;
    let close_tag = format!("</{pname}>");
    let mut from = 0;
    while let Some(found) = after_name[from..].find(&close_tag) {
        let close_idx = from + found;
        let mut candidate = args.clone();
        let value_raw = after_name[..close_idx].trim();
        candidate.insert(pname.to_string(), decode_parameter_value(value_raw));
        if let Some(done) = parse_parameters(&after_name[close_idx + close_tag.len()..], candidate) {
            return Some(done);
        }
        from = close_idx + close_tag.len();
    }
    None
}

/// Splits `<NAME>` into the name and what follows it, for a parameter
/// written as its own element. Only a plain identifier is a name, so a tag
/// with attributes or a closer is never taken for one.
fn element_open(s: &str) -> Option<(&str, &str)> {
    let (name, after) = s.strip_prefix('<')?.split_once('>')?;
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_well = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (starts_well && rest_well).then_some((name, after))
}

/// Reads one quoted XML-like attribute from an opening tag. The compatibility
/// dialect uses only simple names, so entities and namespace syntax are not
/// interpreted here.
fn quoted_attribute<'a>(tag: &'a str, attribute: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let marker = format!("{attribute}={quote}");
        let mut from = 0;
        while let Some(relative) = tag[from..].find(&marker) {
            let at = from + relative;
            let boundary = at == 0
                || tag[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            if boundary {
                let value = &tag[at + marker.len()..];
                let end = value.find(quote)?;
                return Some(&value[..end]);
            }
            from = at + marker.len();
        }
    }
    None
}

fn parse_invoke_parameters(body: &str) -> Option<serde_json::Value> {
    let mut args = serde_json::Map::new();
    let mut cursor = body;
    loop {
        let trimmed = cursor.trim_start();
        if trimmed.is_empty() {
            break;
        }
        if !trimmed.starts_with("<parameter") {
            return None;
        }
        let (name, after_open) = parameter_open(trimmed)?;
        let close = after_open.find(PARAM_CLOSE)?;
        args.insert(
            name.to_string(),
            decode_parameter_value(after_open[..close].trim()),
        );
        cursor = &after_open[close + PARAM_CLOSE.len()..];
    }
    Some(normalize_arguments_wrapper(serde_json::Value::Object(args)))
}

/// Parses the older repair dialect, including several calls in one outer
/// `<function_calls>` container.
fn parse_function_calls_xml(raw: &str) -> Option<Vec<(String, serde_json::Value)>> {
    let mut calls = Vec::new();
    let mut cursor = raw;
    loop {
        let trimmed = cursor.trim_start();
        if trimmed.is_empty() {
            break;
        }
        // A foreign closer after the last invoke (`</function>` where
        // `</function_calls>` belongs) ends the container; it is not a reason
        // to throw away the calls already read.
        if !calls.is_empty() {
            if let Some(closer) = STRAY_CLOSERS.iter().find(|c| trimmed.starts_with(**c)) {
                cursor = &trimmed[closer.len()..];
                continue;
            }
        }
        if !trimmed.starts_with("<invoke") {
            return None;
        }
        let open_end = trimmed.find('>')?;
        let opening = &trimmed[..=open_end];
        let name = quoted_attribute(opening, "name")?.trim();
        if name.is_empty() {
            return None;
        }
        let after_open = &trimmed[open_end + 1..];
        let close = after_open.find("</invoke>")?;
        let arguments = parse_invoke_parameters(&after_open[..close])?;
        calls.push((name.to_string(), arguments));
        cursor = &after_open[close + "</invoke>".len()..];
    }
    (!calls.is_empty()).then_some(calls)
}

/// A cut `<invoke>` has the same provider shape as a cut canonical call: a
/// complete name and an unfinished JSON argument string. Closed parameters
/// retain their decoded types; the last open parameter is a string fragment.
fn parse_partial_invoke(opening: &str, body: &str) -> Option<PartialCall> {
    let name = quoted_attribute(opening, "name")?.trim();
    if name.is_empty() {
        return None;
    }
    let mut complete: Vec<(String, serde_json::Value)> = Vec::new();
    let mut partial: Option<(String, String)> = None;
    let mut cursor = body;
    loop {
        let trimmed = cursor.trim_start();
        let Some(after_marker) = trimmed.strip_prefix("<parameter") else {
            break;
        };
        let Some(open_end) = after_marker.find('>') else {
            break;
        };
        let opening = &trimmed[.."<parameter".len() + open_end + 1];
        let Some(parameter) = quoted_attribute(opening, "name") else {
            break;
        };
        let parameter = parameter.trim().to_string();
        if parameter.is_empty() {
            break;
        }
        let after_open = &after_marker[open_end + 1..];
        match after_open.find(PARAM_CLOSE) {
            Some(close) => {
                complete.push((
                    parameter,
                    decode_parameter_value(after_open[..close].trim()),
                ));
                cursor = &after_open[close + PARAM_CLOSE.len()..];
            }
            None => {
                let hold = [PARAM_CLOSE, "</invoke>"]
                    .iter()
                    .map(|tag| longest_prefix_overlap(after_open, tag))
                    .max()
                    .unwrap_or(0);
                let visible = &after_open[..after_open.len() - hold];
                partial = Some((parameter, visible.trim_start().to_string()));
                break;
            }
        }
    }
    let mut recovered = serde_json::Map::new();
    for (parameter, value) in &complete {
        recovered.insert(parameter.clone(), value.clone());
    }
    if let Some((parameter, value)) = &partial {
        recovered.insert(parameter.clone(), serde_json::Value::String(value.clone()));
    }
    Some(PartialCall {
        name: name.to_string(),
        arguments: unterminated_arguments(
            &complete,
            partial
                .as_ref()
                .map(|(parameter, value)| (parameter.as_str(), value.as_str())),
        ),
        recovered: serde_json::Value::Object(recovered),
    })
}

/// Recovers every complete invoke before the cut and, when its name is whole,
/// the final incomplete invoke. The scanner already emitted one outer open;
/// subsequent recovered calls therefore receive synthetic opens.
fn partial_function_call_events(raw: &str) -> Vec<ScanEvent> {
    enum Recovered {
        Complete((String, serde_json::Value)),
        Partial(PartialCall),
    }

    let mut recovered = Vec::new();
    let mut cursor = raw;
    loop {
        let trimmed = cursor.trim_start();
        if !trimmed.starts_with("<invoke") {
            break;
        }
        let Some(open_end) = trimmed.find('>') else {
            break;
        };
        let opening = &trimmed[..=open_end];
        let after_open = &trimmed[open_end + 1..];
        match after_open.find("</invoke>") {
            Some(close) => {
                let Some(name) = quoted_attribute(opening, "name")
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    break;
                };
                let Some(arguments) = parse_invoke_parameters(&after_open[..close]) else {
                    break;
                };
                recovered.push(Recovered::Complete((name.to_string(), arguments)));
                cursor = &after_open[close + "</invoke>".len()..];
            }
            None => {
                if let Some(partial) = parse_partial_invoke(opening, after_open) {
                    recovered.push(Recovered::Partial(partial));
                }
                break;
            }
        }
    }

    recovered
        .into_iter()
        .enumerate()
        .flat_map(|(index, call)| {
            let mut events = Vec::with_capacity(2);
            if index > 0 {
                events.push(ScanEvent::ToolCallOpen);
            }
            events.push(match call {
                Recovered::Complete(call) => ScanEvent::ToolCallClose(Ok(call)),
                Recovered::Partial(partial) => ScanEvent::ToolCallTruncated(partial),
            });
            events
        })
        .collect()
}

// ── Incremental streaming of the arguments ───────────────────────────────────

/// From how many bytes on a parameter value may be transmitted in fragments
/// in eager mode.
///
/// Fine-grained streaming exists to "reduce the time to the first fragment of
/// a **large** parameter". A value that fit in 64 bytes had no latency to
/// solve, and holding it back costs nothing and preserves its type - which is
/// what separates `{"limit":5}` from `{"limit":"5"}`. POM's markup carries no
/// type, so this is the price of transmitting without waiting, and that is why
/// it is only paid where there is something to gain.
const EAGER_MIN_VALUE_BYTES: usize = 64;

/// Is this parameter value going to be transmitted in fragments?
///
/// **The answer is stable while the value grows**, and that is what the
/// monotonicity of `streamable_arguments_prefix` depends on: the length only
/// increases, and the first character does not change. A parameter that
/// started being transmitted keeps being so until it closes.
///
/// **Only a value that provably cannot be JSON is transmitted.** That is the
/// rule that makes eager mode and held-back mode agree about the type: a value
/// that will never parse as JSON becomes a `String` on both paths, and
/// `{"limit":5}` never degenerates into `{"limit":"5"}`.
///
/// Above the threshold, only these first characters can still start valid
/// JSON: `{` and `[` (containers), `"` (string literal), `-` and a digit
/// (number). `true`/`false`/`null` are short literals, so a letter in a long
/// value already guarantees it is not JSON - that is why a letter passes.
///
/// Holding back a file that starts with `{` costs latency in a rare case;
/// delivering an array turned into text costs correctness. The choice is obvious.
fn streams_eagerly(eager: bool, value_seen: &str) -> bool {
    if !eager {
        return false;
    }
    let value = value_seen.trim_start();
    if value.len() <= EAGER_MIN_VALUE_BYTES {
        return false;
    }
    !matches!(
        value.as_bytes().first(),
        Some(b'{' | b'[' | b'"' | b'-' | b'+' | b'0'..=b'9')
    )
}

/// The body of a JSON string, already escaped, without the quotes.
fn json_string_body(raw: &str) -> String {
    let quoted = serde_json::Value::String(raw.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// The longest prefix of the JSON arguments that can already be transmitted,
/// given the call body seen so far.
///
/// **Invariant:** the result only grows as `raw` grows. Nothing that leaves
/// here is taken back later - there is no unsending an SSE fragment. All the
/// care in this module (the whitespace hold-back, the tag prefix retention,
/// the stability of `streams_eagerly`) exists to uphold that.
///
/// `None` means "nothing can be said yet": the function name did not close,
/// or the body is not the XML form. The Hermes form (`{"name":…}`) is not
/// transcoded live because the name lives inside the very JSON that would be
/// transmitted; it goes out whole at the close, as before.
fn streamable_arguments_prefix(raw: &str, eager: bool) -> Option<String> {
    let trimmed = raw.trim_start();
    let rest = trimmed.strip_prefix("<function=")?;
    let (name, mut cursor) = rest.split_once('>')?;
    if name.trim().is_empty() {
        return None;
    }

    let mut out = String::from("{");
    let mut first = true;
    let mut parameters = 0usize;
    let mut sole_arguments_object = false;
    loop {
        let t = cursor.trim_start();
        // Both spellings, as the complete parser reads them: a parameter this
        // skipped would be missing from the streamed arguments for good.
        let Some((pname, after_name)) = parameter_open(t) else {
            break;
        };
        match after_name.find("</parameter>") {
            Some(close) => {
                let value_raw = after_name[..close].trim();
                parameters += 1;
                sole_arguments_object = parameters == 1
                    && pname == "arguments"
                    && decode_json_arguments(decode_parameter_value(value_raw)).is_object();
                if parameters > 1 {
                    sole_arguments_object = false;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&serde_json::Value::String(pname.to_string()).to_string());
                out.push(':');
                // The parameter that was transmitted in fragments closes as a
                // string, because that is how it went out. The others carry
                // the decoded type, same as the non-streaming path.
                if streams_eagerly(eager, value_raw) {
                    out.push('"');
                    out.push_str(&json_string_body(value_raw));
                    out.push('"');
                } else {
                    out.push_str(&decode_parameter_value(value_raw).to_string());
                }
                cursor = &after_name[close + "</parameter>".len()..];
            }
            None => {
                // Parameter still open. Only eager mode emits anything of it,
                // and only after the value gets large enough.
                if let Some(partial) = open_parameter_prefix(after_name, eager) {
                    if !first {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String(pname.to_string()).to_string());
                    out.push(':');
                    out.push('"');
                    out.push_str(&partial);
                }
                break;
            }
        }
    }
    // Do not stream a sole `arguments` object under its redundant outer key:
    // the complete parser unwraps it. Holding it here keeps the streaming and
    // blocking provider payloads identical.
    if parameters == 1 && sole_arguments_object {
        Some("{".to_string())
    } else {
        Some(out)
    }
}

/// The already transmittable piece of a parameter that has not closed yet,
/// escaped.
///
/// Two hold-backs, both for monotonicity:
///
/// - **tag prefix**: what can still become `</parameter>` or `</function>` is
///   retained, otherwise `</par` would go out as content and would have to be
///   taken back when the tag completed.
/// - **trailing whitespace**: the final value is `trim()`med, so whitespace at
///   the end of what was seen so far may be the whitespace that will vanish.
///   It only goes out when something that is not whitespace comes after it -
///   and then it was internal content.
fn open_parameter_prefix(value_seen: &str, eager: bool) -> Option<String> {
    let hold = [PARAM_CLOSE, "</function>"]
        .iter()
        .map(|tag| longest_prefix_overlap(value_seen, tag))
        .max()
        .unwrap_or(0);
    let visible = &value_seen[..value_seen.len() - hold];
    if !streams_eagerly(eager, visible) {
        return None;
    }
    let emittable = visible.trim_start().trim_end();
    (!emittable.is_empty()).then(|| json_string_body(emittable))
}

const PARAM_CLOSE: &str = "</parameter>";

/// The tool name, as soon as it closes - before the call ends.
///
/// It is what allows announcing `tool_calls[i].function.name` in the first
/// fragment, as OpenAI does: without the name, an open index is good for
/// nothing. `None` while the `<function=NOME>` has not closed, or in the Hermes
/// form, which is not transcoded live.
pub(crate) fn peek_call_name(raw_body: &str) -> Option<String> {
    let rest = raw_body.trim_start().strip_prefix("<function=")?;
    let (name, _) = rest.split_once('>')?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Transmits the arguments of **one** tool call in fragments, as the model
/// generates them.
///
/// The defect it exists to fix, measured on the live ksia: the 663 characters
/// of the argument all arrived at once, at the end of generation, in two
/// pieces both at 11,01 s. The tag reader accumulated the whole body and only
/// flushed when the call closed, so the user stared at a frozen screen and
/// received the file all of a sudden. Anthropic and OpenAI emit the fragments
/// as they generate.
///
/// **Central invariant:** the concatenation of everything that leaves here is
/// exactly the arguments string the non-streaming path would produce -
/// including when the cap cuts the call in the middle, in which case it is the
/// unfinished string of Phase 1. `emitted` keeps what already went out
/// precisely so that the close emits only what is missing, and never repeats
/// what the client already has.
pub(crate) struct CallArgsStreamer {
    eager: bool,
    emitted: String,
}

impl CallArgsStreamer {
    pub(crate) fn new(eager: bool) -> Self {
        Self {
            eager,
            emitted: String::new(),
        }
    }

    /// How much already went out. `0` means the client has not yet seen any
    /// argument of this call.
    pub(crate) fn emitted_len(&self) -> usize {
        self.emitted.len()
    }

    /// The next fragment, given the call body seen so far.
    /// `None` when there is nothing new to say.
    pub(crate) fn advance(&mut self, raw_body_so_far: &str) -> Option<String> {
        let prefix = streamable_arguments_prefix(raw_body_so_far, self.eager)?;
        // The brace alone is not content: while no parameter has closed (or
        // started being transmitted) this call has not started going out yet.
        // That is what lets a malformed call keep falling back to text at the
        // close, as before - after the first fragment there would be no way
        // to go back.
        if self.emitted.is_empty() && prefix == "{" {
            return None;
        }
        self.take_delta(prefix)
    }

    /// The close of a call that **completed**.
    ///
    /// The string comes from the same transcoder that generated the
    /// fragments, plus the closing brace - and not from `parsed.to_string()`.
    /// The reason is concrete: `serde_json::Map` is an ordered tree, so
    /// `parsed.to_string()` returns the keys in **alphabetical** order, while
    /// the fragments went out in the order in which the model wrote them.
    /// Closing with the sorted version would not extend what the client
    /// already accumulated. The content is the same; the key order of a JSON
    /// object is not semantic.
    ///
    /// `parsed` is the fallback for the Hermes form, which is not transcoded
    /// live and therefore had no fragment at all: there the whole string goes
    /// out at once, as before.
    pub(crate) fn finish_complete(
        &mut self,
        raw_body: &str,
        parsed: &serde_json::Value,
    ) -> Option<String> {
        match streamable_arguments_prefix(raw_body, self.eager) {
            Some(mut prefix) => {
                prefix.push('}');
                // A redundant sole `arguments` object is intentionally held
                // back and unwrapped by the complete parser. No fragment can
                // have gone out in that case, so emit the normalized object.
                let normalized_differs = serde_json::from_str::<serde_json::Value>(&prefix)
                    .is_ok_and(|streamed| &streamed != parsed);
                if self.emitted.is_empty() && normalized_differs {
                    self.take_delta(parsed.to_string())
                } else {
                    self.take_delta(prefix)
                }
            }
            None => self.take_delta(parsed.to_string()),
        }
    }

    /// The close of a call the cap **cut**: `full` is the unfinished string of
    /// Phase 1. Only what had not gone out yet goes out, so the client does
    /// not receive twice what it already accumulated.
    pub(crate) fn finish_truncated(&mut self, full: &str) -> Option<String> {
        self.take_delta(full.to_string())
    }

    /// The delta, with the safety net: if the new prefix does not extend what
    /// already went out, monotonicity was broken somewhere. Emitting the
    /// delta then would corrupt the client's JSON, so nothing goes out and
    /// the divergent fragment is logged. Degrading to "did not transmit" is
    /// always better than transmitting wrong.
    fn take_delta(&mut self, prefix: String) -> Option<String> {
        if !prefix.starts_with(&self.emitted) {
            warn!(
                emitted = self.emitted.len(),
                new = prefix.len(),
                "argument prefix does not extend what was already transmitted; fragment suppressed so the client JSON is not corrupted"
            );
            return None;
        }
        if prefix.len() == self.emitted.len() {
            return None;
        }
        let delta = prefix[self.emitted.len()..].to_string();
        self.emitted = prefix;
        Some(delta)
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
    /// The call the client's cap cut in the middle, structured and
    /// incomplete - see [`PartialCall`].
    ToolCallTruncated(PartialCall),
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
            ScanEvent::ToolCallTruncated(partial) => {
                parts.push(OutputPart::ToolCallTruncated(partial))
            }
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
#[cfg(test)]
pub(crate) fn parse_output(text: &str) -> Vec<OutputPart> {
    parse_output_with_tools(text, &ToolSignatures::default())
}

/// [`parse_output`] that can attribute a nameless call to a declared tool.
pub(crate) fn parse_output_with_tools(text: &str, tools: &ToolSignatures) -> Vec<OutputPart> {
    let mut scanner = TagScanner::with_tools(tools.clone());
    let mut events = scanner.feed(text);
    events.extend(scanner.finish());
    merge_adjacent_text(events_to_parts(events))
}

/// [`parse_output`] for an output cut by the token cap, and not by the model:
/// a call open and not closed comes back **structured and incomplete** instead
/// of degrading into text. The `bool` says whether the cut fell inside a
/// call - see [`TagScanner::finish_truncated`].
#[cfg(test)]
pub(crate) fn parse_output_truncated(text: &str) -> (Vec<OutputPart>, bool) {
    parse_output_truncated_with_tools(text, &ToolSignatures::default())
}

/// [`parse_output_truncated`] that can attribute a nameless call.
pub(crate) fn parse_output_truncated_with_tools(
    text: &str,
    tools: &ToolSignatures,
) -> (Vec<OutputPart>, bool) {
    let mut scanner = TagScanner::with_tools(tools.clone());
    let mut events = scanner.feed(text);
    let (tail, discarded) = scanner.finish_truncated();
    events.extend(tail);
    (merge_adjacent_text(events_to_parts(events)), discarded)
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
            eager_input_streaming: None,
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
        let raw =
            "<function=Write>\n<parameter=content>\nline 1\nline 2\n</parameter>\n</function>";
        let (name, parsed) = parse_function_call_xml(raw).expect("parses");
        assert_eq!(name, "Write");
        assert_eq!(parsed["content"], "line 1\nline 2");
    }

    #[test]
    fn parse_function_call_xml_rejects_a_missing_closing_function_tag() {
        assert!(parse_function_call_xml(
            "<function=Bash>\n<parameter=command>\nls\n</parameter>\n"
        )
        .is_none());
    }

    #[test]
    fn parse_function_call_xml_rejects_an_unclosed_parameter() {
        assert!(
            parse_function_call_xml("<function=Bash>\n<parameter=command>\nls\n</function>")
                .is_none()
        );
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

    #[test]
    fn a_single_arguments_object_parameter_is_unwrapped() {
        let raw = "<tool_call><function=bash><parameter=arguments>\n{\"command\":\"pwd\"}\n</parameter></function></tool_call>";
        assert_eq!(
            parse_output(raw),
            vec![OutputPart::ToolCall {
                name: "bash".into(),
                arguments: json!({"command": "pwd"}),
            }]
        );

        // The OpenAI streaming transcoder must produce the same normalized
        // JSON, not the rejected {"arguments": {...}} shape.
        let body =
            "<function=bash><parameter=arguments>\n{\"command\":\"pwd\"}\n</parameter></function>";
        let mut streamer = CallArgsStreamer::new(true);
        assert_eq!(streamer.advance(body), None);
        assert_eq!(
            streamer.finish_complete(body, &json!({"command": "pwd"})),
            Some(r#"{"command":"pwd"}"#.into())
        );

        let encoded = r#"<tool_call><function=bash><parameter=arguments>"{\"command\":\"pwd\"}"</parameter></function></tool_call>"#;
        assert_eq!(
            parse_output(encoded),
            vec![OutputPart::ToolCall {
                name: "bash".into(),
                arguments: json!({"command": "pwd"}),
            }]
        );
        let encoded_body =
            r#"<function=bash><parameter=arguments>"{\"command\":\"pwd\"}"</parameter></function>"#;
        let mut streamer = CallArgsStreamer::new(true);
        assert_eq!(streamer.advance(encoded_body), None);
        assert_eq!(
            streamer.finish_complete(encoded_body, &json!({"command": "pwd"})),
            Some(r#"{"command":"pwd"}"#.into())
        );
    }

    #[test]
    fn an_arguments_scalar_or_an_additional_parameter_is_not_unwrapped() {
        let scalar =
            "<tool_call><function=f><parameter=arguments>raw</parameter></function></tool_call>";
        let extra = "<tool_call><function=f><parameter=arguments>{\"x\":1}</parameter><parameter=mode>safe</parameter></function></tool_call>";
        assert!(matches!(
            &parse_output(scalar)[0],
            OutputPart::ToolCall { arguments, .. } if arguments == &json!({"arguments": "raw"})
        ));
        assert!(matches!(
            &parse_output(extra)[0],
            OutputPart::ToolCall { arguments, .. }
                if arguments == &json!({"arguments": {"x": 1}, "mode": "safe"})
        ));
    }

    // ── `<function_calls><invoke …>` compatibility dialect ─────────────────

    #[test]
    fn parse_output_recovers_the_invoke_dialect_without_leaking_markup() {
        let raw = "before <function_calls><invoke name=\"bash\"><parameter name=\"command\">pwd</parameter></invoke></function_calls>";
        assert_eq!(
            parse_output(raw),
            vec![
                OutputPart::Text("before ".into()),
                OutputPart::ToolCall {
                    name: "bash".into(),
                    arguments: json!({"command": "pwd"}),
                },
            ]
        );
        assert_eq!(feed_in_chunks(raw, 1), parse_output(raw));
        assert_eq!(feed_in_chunks(raw, 7), parse_output(raw));
    }

    #[test]
    fn one_function_calls_container_can_hold_multiple_invokes() {
        let raw = "<function_calls>\n<invoke name=\"read\"><parameter name=\"path\">a</parameter></invoke>\n<invoke name='glob'><parameter name='pattern'>*.rs</parameter></invoke>\n</function_calls>";
        assert_eq!(
            parse_output(raw),
            vec![
                OutputPart::ToolCall {
                    name: "read".into(),
                    arguments: json!({"path": "a"}),
                },
                OutputPart::ToolCall {
                    name: "glob".into(),
                    arguments: json!({"pattern": "*.rs"}),
                },
            ]
        );
        assert_eq!(feed_in_chunks(raw, 1), parse_output(raw));
    }

    #[test]
    fn function_calls_shut_by_a_bare_function_closer_keeps_its_invokes() {
        // Seen in a real session: two complete invokes, the container shut
        // with `</function>`; the whole block leaked to the client as text.
        let raw = "<function_calls>\n<invoke name=\"Bash\">\n<parameter name=\"command\">grep -n a b.yml</parameter>\n<parameter name=\"description\">Check compose</parameter>\n</invoke>\n<invoke name=\"Bash\">\n<parameter name=\"command\">grep -n c d.rs</parameter>\n<parameter name=\"description\">Check hashing</parameter>\n</invoke>\n</function>";
        let expected = vec![
            OutputPart::ToolCall {
                name: "Bash".into(),
                arguments: json!({"command": "grep -n a b.yml", "description": "Check compose"}),
            },
            OutputPart::ToolCall {
                name: "Bash".into(),
                arguments: json!({"command": "grep -n c d.rs", "description": "Check hashing"}),
            },
        ];
        assert_eq!(parse_output(raw), expected);
        assert_eq!(feed_in_chunks(raw, 1), expected);
        assert_eq!(feed_in_chunks(raw, 7), expected);
    }

    #[test]
    fn a_truncated_invoke_is_structured_and_never_becomes_raw_content() {
        let raw = "intro <function_calls><invoke name=\"write\"><parameter name=\"path\">/tmp/a</parameter><parameter name=\"content\">line 1";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        assert_eq!(parts[0], OutputPart::Text("intro ".into()));
        let OutputPart::ToolCallTruncated(partial) = &parts[1] else {
            panic!("expected a truncated invoke: {parts:?}");
        };
        assert_eq!(partial.name, "write");
        assert_eq!(
            partial.recovered,
            json!({"path": "/tmp/a", "content": "line 1"})
        );
        assert_eq!(partial.arguments, r#"{"path":"/tmp/a","content":"line 1"#);
        assert!(parts.iter().all(|part| !matches!(part, OutputPart::Text(text) if text.contains("function_calls") || text.contains("invoke"))));
    }

    #[test]
    fn complete_invokes_before_a_truncated_one_are_all_recovered() {
        let raw = "<function_calls><invoke name=\"read\"><parameter name=\"path\">a</parameter></invoke><invoke name=\"write\"><parameter name=\"path\">b";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        assert!(matches!(&parts[0], OutputPart::ToolCall { name, .. } if name == "read"));
        assert!(
            matches!(&parts[1], OutputPart::ToolCallTruncated(partial) if partial.name == "write")
        );
    }

    // ── bare `<function>NAME</function>`, no wrapper ─────────────────────────

    #[test]
    fn parse_output_recovers_a_call_opened_with_a_parameter_tag() {
        let text = "Looking.\n<tool_call>\n<parameter=bash>\n<parameter=command>\ncd /x && grep -n \"seal(\" a.rs | head -20\n</parameter>\n</function>\n</tool_call>";
        assert_eq!(
            parse_output(text),
            vec![
                OutputPart::Text("Looking.\n".into()),
                OutputPart::ToolCall {
                    name: "bash".into(),
                    arguments: json!({"command": "cd /x && grep -n \"seal(\" a.rs | head -20"}),
                },
            ]
        );
    }

    #[test]
    fn a_parameter_tag_holding_a_value_is_not_taken_for_a_call_name() {
        let text = "<tool_call>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>";
        assert_eq!(parse_output(text), vec![OutputPart::Text(text.into())]);
    }

    #[test]
    fn a_parameter_opened_call_cut_by_the_cap_keeps_its_name() {
        let (parts, truncated) = parse_output_truncated(
            "<tool_call>\n<parameter=bash>\n<parameter=command>\nls -la",
        );
        assert!(truncated);
        match parts.as_slice() {
            [OutputPart::ToolCallTruncated(partial)] => assert_eq!(partial.name, "bash"),
            other => panic!("expected one truncated call, got {other:?}"),
        }
    }

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
        assert_eq!(out.len(), 2); // read, glob: text after a call is dropped
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "read"));
        assert!(matches!(&out[1], OutputPart::ToolCall { name, .. } if name == "glob"));
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
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], OutputPart::ToolCall { name, .. } if name == "Bash"));
        assert!(matches!(&out[1], OutputPart::ToolCall { name, .. } if name == "read"));
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
        let out =
            parse_output(r#"<tool_call>{"name":"Bash","arguments":{"command":"ls"}}</tool_call>"#);
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
    fn parse_output_reproduces_the_audited_call_with_text_before_it() {
        let raw = "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n<tool_call>\n<function=skill>\n<parameter=name>\nbrainstorming\n</parameter>\n</function>\n</tool_call>";
        assert_eq!(
            parse_output(raw),
            vec![
                OutputPart::Text(
                    "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n".into()
                ),
                OutputPart::ToolCall {
                    name: "skill".into(),
                    arguments: json!({"name": "brainstorming"}),
                },
            ]
        );
    }

    #[test]
    fn parse_output_decodes_json_encoded_arguments() {
        let raw =
            r#"<tool_call>{"name":"skill","arguments":"{\"name\":\"brainstorming\"}"}</tool_call>"#;
        assert_eq!(
            parse_output(raw),
            vec![OutputPart::ToolCall {
                name: "skill".into(),
                arguments: json!({"name": "brainstorming"}),
            }]
        );
    }

    #[test]
    fn parse_output_interleaves_text_and_tool_calls() {
        let out = parse_output(
            r#"Sure, let me check.<tool_call>{"name":"Bash","arguments":{"command":"ls"}}</tool_call>Done."#,
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], OutputPart::Text("Sure, let me check.".into()));
        assert!(matches!(out[1], OutputPart::ToolCall { .. }));
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

    // ── the ceiling cut inside a call ───────────────────────────────────────

    /// The shape both providers return: structured call, recovered name,
    /// arguments in an **unfinished** JSON string. It does not parse, on
    /// purpose - that is what makes a client that ignores the stop reason
    /// fail instead of executing half a call.
    #[test]
    fn a_call_cut_mid_parameter_keeps_the_name_and_the_unterminated_arguments() {
        let raw = "<tool_call>\n<function=Write>\n<parameter=path>\n/tmp/a.py\n</parameter>\n\
                   <parameter=content>\nlinha 1\nlinha 2";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("expected a cut call, got {parts:?}");
        };
        assert_eq!(partial.name, "Write");
        assert_eq!(
            partial.arguments,
            r#"{"path":"/tmp/a.py","content":"linha 1\nlinha 2"#
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&partial.arguments).is_err(),
            "arguments of a cut call cannot parse"
        );
        // And the recovered object, for Anthropic's `input`, is valid JSON
        // with whatever could be recovered.
        assert_eq!(partial.recovered["path"], "/tmp/a.py");
        assert_eq!(partial.recovered["content"], "linha 1\nlinha 2");
    }

    /// Cut exactly at the boundary of a `</parameter>`: there is no half
    /// parameter, but the call is still incomplete - the object's closing
    /// brace never comes, because more parameters could be on the way.
    #[test]
    fn a_call_cut_between_parameters_still_returns_unterminated_arguments() {
        let raw = "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</parameter>\n";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("expected a cut call, got {parts:?}");
        };
        assert_eq!(partial.arguments, r#"{"command":"ls""#);
        assert_eq!(partial.recovered["command"], "ls");
    }

    /// Cut before the first parameter: the name is out, the arguments are not.
    #[test]
    fn a_call_cut_before_any_parameter_keeps_the_name_and_an_empty_object() {
        let raw = "<tool_call>\n<function=Bash>\n";
        let (parts, _) = parse_output_truncated(raw);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("expected a cut call, got {parts:?}");
        };
        assert_eq!(partial.name, "Bash");
        assert_eq!(partial.arguments, "{");
        assert_eq!(partial.recovered, json!({}));
    }

    /// Cut too early to have a name: nothing comes out. A name the model
    /// did not finish writing is not a tool, it is a guess - and degrading it
    /// to text would deliver raw markup as if it were prose.
    #[test]
    fn a_call_cut_before_the_name_closed_yields_nothing_executable() {
        for raw in [
            "<tool_call>\n<function=Wri",
            "<tool_call>\n",
            "<function>rea",
        ] {
            let (parts, cut_inside) = parse_output_truncated(raw);
            assert!(cut_inside, "{raw:?} opened a call");
            assert!(parts.is_empty(), "{raw:?} produziu {parts:?}");
        }
    }

    /// The text before the opening is the model's answer and survives the cut.
    #[test]
    fn text_before_a_cut_call_survives_and_never_carries_raw_markup() {
        let raw = "Vou escrever.\n<tool_call>\n<function=Write>\n<parameter=p>\nx";
        let (parts, _) = parse_output_truncated(raw);
        assert_eq!(parts[0], OutputPart::Text("Vou escrever.\n".into()));
        assert!(matches!(parts[1], OutputPart::ToolCallTruncated(_)));
        for part in &parts {
            if let OutputPart::Text(t) = part {
                assert!(
                    !t.contains("<tool_call>") && !t.contains("<function="),
                    "{t:?}"
                );
            }
        }
    }

    /// The cut Hermes form: the name comes out if its string closed, and the
    /// arguments body is itself already unfinished JSON.
    #[test]
    fn a_cut_hermes_style_call_keeps_the_name_and_the_raw_partial_json() {
        let raw = r#"<tool_call>{"name":"Bash","arguments":{"command":"ls -"#;
        let (parts, _) = parse_output_truncated(raw);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("expected a cut call, got {parts:?}");
        };
        assert_eq!(partial.name, "Bash");
        assert_eq!(partial.arguments, r#"{"command":"ls -"#);
        assert_eq!(partial.recovered, json!({}));
    }

    /// Outside a call nothing changes: cut text stays text, and a pending
    /// tag prefix never got to open any call.
    #[test]
    fn truncation_outside_a_call_behaves_exactly_like_a_normal_finish() {
        for raw in ["prose cut in half", "prosa <tool_c"] {
            let (parts, cut_inside) = parse_output_truncated(raw);
            assert!(!cut_inside, "{raw:?}");
            assert_eq!(parts, parse_output(raw), "{raw:?}");
        }
    }

    /// A call that **closed** before the cut is not a cut call: the cut
    /// fell after it, in text.
    #[test]
    fn a_call_that_closed_before_the_cut_is_a_complete_call() {
        let raw = "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</parameter>\n\
                   </function>\n</tool_call>depois";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(!cut_inside);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(matches!(&parts[0], OutputPart::ToolCall { name, .. } if name == "Bash"));
    }

    // ── incremental streaming of the arguments ──────────────────────────────

    /// The script of the defect measured in ksia: a call with one large
    /// argument, delivered token by token.
    fn long_call_script() -> Vec<String> {
        let mut script = vec![
            "<tool_call>\n".to_string(),
            "<function=write_file>\n".to_string(),
            "<parameter=path>\n".to_string(),
            "/tmp/calc.py".to_string(),
            "\n</parameter>\n".to_string(),
            "<parameter=content>\n".to_string(),
        ];
        for l in 0..40 {
            script.push(format!("linha_{l} = {l} * 2\n"));
        }
        script.push("\n</parameter>\n".to_string());
        script.push("</function>\n".to_string());
        script.push("</tool_call>".to_string());
        script
    }

    /// Feeds the script piece by piece and returns the emitted fragments.
    /// `until` cuts the generation midway, for the ceiling case.
    fn fragments(eager: bool, until: Option<usize>) -> Vec<String> {
        let script = long_call_script();
        let script = &script[..until.unwrap_or(script.len())];
        let mut sc = TagScanner::new();
        let mut streamer = CallArgsStreamer::new(eager);
        let mut body = String::new();
        let mut out = Vec::new();
        let mut parsed_final = None;
        for chunk in script {
            for ev in sc.feed(chunk) {
                match ev {
                    ScanEvent::ToolCallChunk(f) => {
                        body.push_str(&f);
                        if let Some(d) = streamer.advance(&body) {
                            out.push(d);
                        }
                    }
                    ScanEvent::ToolCallClose(Ok((_, args))) => parsed_final = Some(args),
                    _ => {}
                }
            }
        }
        match (until, parsed_final) {
            (None, Some(args)) => {
                if let Some(d) = streamer.finish_complete(&body, &args) {
                    out.push(d);
                }
            }
            _ => {
                let (tail, _) = sc.finish_truncated();
                if let Some(ScanEvent::ToolCallTruncated(p)) = tail.into_iter().next() {
                    if let Some(d) = streamer.finish_truncated(&p.arguments) {
                        out.push(d);
                    }
                }
            }
        }
        out
    }

    /// **The defect, turned inside out.** The arguments used to come out in one piece, at
    /// the end; now they come out in many, and the first arrives long before the last.
    #[test]
    fn a_long_argument_streams_in_many_fragments_instead_of_one_blob() {
        let frags = fragments(true, None);
        assert!(
            frags.len() > 5,
            "the arguments are still held up: {} fragment(s)",
            frags.len()
        );
    }

    /// **The central invariant:** concatenating the fragments in order
    /// reproduces exactly the arguments string, and it is valid JSON with the
    /// same content as the non-streaming path.
    #[test]
    fn concatenating_the_fragments_reproduces_the_final_argument_exactly() {
        let whole: String = fragments(true, None).concat();
        let json: serde_json::Value =
            serde_json::from_str(&whole).expect("the concatenation is valid JSON");
        assert_eq!(json["path"], "/tmp/calc.py");
        assert!(json["content"]
            .as_str()
            .expect("content")
            .ends_with("linha_39 = 39 * 2"));

        // And it is the same object the non-streaming path produces - only the order
        // of the keys differs, because `serde_json::Map` sorts and the model does not.
        let raw: String = long_call_script().concat();
        let OutputPart::ToolCall { arguments, .. } = &parse_output(&raw)[0] else {
            panic!("expected a complete call");
        };
        assert_eq!(&json, arguments);
    }

    /// **The held-back mode preserves the old behavior:** nothing comes out while
    /// the large parameter has not closed. It is Anthropic's default - the holding back
    /// is per parameter value, so `path` comes out early and `content` only at the
    /// end.
    #[test]
    fn buffered_mode_emits_nothing_until_each_parameter_closes() {
        let frags = fragments(false, None);
        // One fragment per parameter that closed, plus the closing brace.
        assert_eq!(frags.len(), 3, "{frags:?}");
        // What matters: the large parameter comes out WHOLE in a single fragment - it is
        // the old behavior, and it is what eager mode trades for latency.
        let large = frags
            .iter()
            .find(|f| f.contains("linha_39"))
            .expect("the content went out");
        assert!(large.contains("linha_0 = 0 * 2"), "{large:?}");
        let whole = frags.concat();
        assert!(whole.starts_with(r#"{"path":"/tmp/calc.py""#), "{whole:?}");
        let json: serde_json::Value = serde_json::from_str(&whole).expect("valid JSON");
        assert!(json["content"]
            .as_str()
            .expect("content")
            .contains("linha_39"));
    }

    /// **The ceiling cut is not confused with a partial fragment.** What had
    /// already been transmitted counts: the close emits only what is missing, and
    /// the concatenation is exactly the unfinished string of Phase 1 - which does
    /// not parse, on purpose.
    #[test]
    fn a_truncated_call_never_repeats_what_was_already_streamed() {
        let raw_cut: String = long_call_script()[..20].concat();
        let (parts, _) = parse_output_truncated(&raw_cut);
        let OutputPart::ToolCallTruncated(expected) = &parts[0] else {
            panic!("expected a cut call");
        };

        let whole: String = fragments(true, Some(20)).concat();
        assert_eq!(
            whole, expected.arguments,
            "the streaming concatenation must match that of Phase 1"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&whole).is_err(),
            "arguments of a cut call cannot parse: {whole:?}"
        );
    }

    /// A short call stays correct and with its type intact: a value that fit
    /// under the threshold is never transmitted in fragments, so `5` stays a number.
    #[test]
    fn a_short_call_keeps_its_argument_types_even_in_eager_mode() {
        let raw = "<function=Bash>\n<parameter=command>\nls -la\n</parameter>\n                     <parameter=limit>\n5\n</parameter>\n";
        let prefix = streamable_arguments_prefix(raw, true).expect("transcodes");
        assert_eq!(prefix, r#"{"command":"ls -la","limit":5"#);
    }

    /// A large value that is already structured JSON is held back even in eager
    /// mode: transmitting it as a string would silently change its type.
    #[test]
    fn a_long_structured_value_stays_buffered_so_its_type_survives() {
        let array = format!(
            "[{}]",
            (0..40).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        );
        assert!(array.len() > EAGER_MIN_VALUE_BYTES);
        assert!(
            !streams_eagerly(true, &array),
            "a long array cannot be transmitted"
        );
        assert!(!streams_eagerly(true, &format!("{{\"a\": {array}}}")));
        assert!(!streams_eagerly(true, &format!("\"{}\"", "x".repeat(80))));
        // Already text that cannot be JSON, that one yes.
        assert!(streams_eagerly(true, &"linha de codigo ".repeat(10)));
    }

    /// **Monotonicity**, feeding byte by byte: the transmittable prefix
    /// never shrinks nor diverges. There is no un-sending an SSE fragment, so
    /// this is the property the whole module protects.
    #[test]
    fn the_streamable_prefix_only_ever_grows_byte_by_byte() {
        let raw: String = long_call_script().concat();
        for eager in [true, false] {
            let mut sc = TagScanner::new();
            let mut body = String::new();
            let mut anterior = String::new();
            for byte in raw.as_bytes().chunks(1) {
                let Ok(chunk) = std::str::from_utf8(byte) else {
                    continue;
                };
                for ev in sc.feed(chunk) {
                    if let ScanEvent::ToolCallChunk(f) = ev {
                        body.push_str(&f);
                        if let Some(p) = streamable_arguments_prefix(&body, eager) {
                            assert!(
                                p.starts_with(&anterior),
                                "eager={eager}: the prefix diverged\nbefore: {anterior:?}\nafter: {p:?}"
                            );
                            anterior = p;
                        }
                    }
                }
            }
        }
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

    fn only_call(text: &str) -> (String, serde_json::Value) {
        let parts = parse_output(text);
        let calls: Vec<_> = parts
            .iter()
            .filter_map(|p| match p {
                OutputPart::ToolCall { name, arguments } => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{parts:?}");
        for part in &parts {
            if let OutputPart::Text(t) = part {
                assert!(!t.contains('<'), "markup leaked: {t:?}");
            }
        }
        calls.into_iter().next().unwrap()
    }

    #[test]
    fn a_function_call_wrapper_is_read_as_a_call() {
        let (name, args) = only_call(
            "go\n<function_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn a_parameter_call_wrapper_is_read_as_a_call() {
        let (name, _) = only_call(
            "<parameter_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(name, "bash");
    }

    #[test]
    fn function_calls_around_a_function_body_is_one_call() {
        let (name, args) = only_call(
            "<function_calls>\n<function=execute>\n<parameter=code>\nreturn 1;\n</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(name, "execute");
        assert_eq!(args["code"], "return 1;");
    }

    #[test]
    fn a_bare_invoke_is_a_call() {
        let (name, args) = only_call(
            "<invoke name=\"read\">\n<parameter name=\"path\">a.rs</parameter>\n</invoke>",
        );
        assert_eq!(name, "read");
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn a_repeated_tool_call_opener_collapses() {
        let (name, args) = only_call(
            "<tool_call>\n\n<tool_call>\n<function=read>\n<parameter=path>\nx\n</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(name, "read");
        assert_eq!(args["path"], "x");
    }

    #[test]
    fn both_parameter_spellings_mix_inside_one_call() {
        let (_, args) = only_call(
            "<tool_call>\n<function=edit>\n<parameter=path>\nf\n</parameter>\n<parameter name=\"oldText\">a</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(args["path"], "f");
        assert_eq!(args["oldText"], "a");
    }

    #[test]
    fn an_invoke_inside_a_tool_call_wrapper_is_a_call() {
        let (name, args) =
            only_call("<tool_call>\n<invoke name=\"bash\">\n<parameter name=\"command\">ls</parameter>\n</invoke>\n</tool_call>");
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn stray_closers_after_a_call_are_dropped() {
        let (name, _) = only_call(
            "<tool_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call></function>\n</tool_call>",
        );
        assert_eq!(name, "bash");
    }

    #[test]
    fn a_doubled_function_close_without_tool_call_close_is_a_call() {
        let (name, args) = only_call(
            "<function_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</function>",
        );
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn an_unshut_complete_call_at_end_of_output_is_a_call() {
        let (name, _) = only_call(
            "<tool_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n",
        );
        assert_eq!(name, "bash");
    }

    #[test]
    fn an_unfinished_call_at_end_of_output_stays_text() {
        let parts = parse_output("<tool_call>\n<function=bash>\n<parameter=command>\nls");
        assert!(parts.iter().all(|p| matches!(p, OutputPart::Text(_))), "{parts:?}");
    }

    #[test]
    fn the_new_dialects_survive_any_chunking() {
        let text = "hi\n<function_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call></function>\nbye";
        let whole = parse_output(text);
        for size in 1..9 {
            let mut scanner = TagScanner::new();
            let mut events = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                events.extend(scanner.feed(std::str::from_utf8(chunk).unwrap()));
            }
            events.extend(scanner.finish());
            assert_eq!(merge_adjacent_text(events_to_parts(events)), whole, "chunk {size}");
        }
    }


    fn signatures() -> ToolSignatures {
        let tool = |name: &str, params: serde_json::Value| ToolDef {
            name: name.to_string(),
            description: None,
            input_schema: json!({"type": "object", "properties": params}),
            eager_input_streaming: None,
        };
        ToolSignatures::of(&[
            tool("read", json!({"path": {}, "offset": {}, "limit": {}})),
            tool("bash", json!({"command": {}, "timeout": {}})),
            tool("write", json!({"path": {}, "content": {}})),
        ])
    }

    #[test]
    fn a_nameless_call_is_attributed_by_its_parameters() {
        let text = "<tool_call>\n<parameter name=\"command\">ls -la</parameter>\n</invoke>\n";
        let parts = parse_output_with_tools(text, &signatures());
        assert_eq!(
            parts,
            vec![OutputPart::ToolCall {
                name: "bash".to_string(),
                arguments: json!({"command": "ls -la"}),
            }]
        );
    }

    #[test]
    fn a_nameless_call_shut_by_a_function_closer_is_attributed() {
        let text = "<tool_call>\n<parameter=command>\ngrep -n \"a\" f.rs | head\n</parameter>\n</function>\n</tool_call>";
        let parts = parse_output_with_tools(text, &signatures());
        assert_eq!(
            parts,
            vec![OutputPart::ToolCall {
                name: "bash".to_string(),
                arguments: json!({"command": "grep -n \"a\" f.rs | head"}),
            }]
        );
        let mut scanner = TagScanner::with_tools(signatures());
        let mut events = Vec::new();
        for c in text.chars() {
            events.extend(scanner.feed(&c.to_string()));
        }
        events.extend(scanner.finish());
        let calls: Vec<_> = events.iter().filter(|e| matches!(e, ScanEvent::ToolCallClose(_))).collect();
        assert!(matches!(calls.as_slice(), [ScanEvent::ToolCallClose(Ok(_))]), "{events:?}");
    }

    #[test]
    fn a_nameless_call_two_tools_could_take_stays_text() {
        let text = "<tool_call>\n<parameter=path>\na.rs\n</parameter>\n</tool_call>";
        let parts = parse_output_with_tools(text, &signatures());
        assert!(parts.iter().all(|p| matches!(p, OutputPart::Text(_))), "{parts:?}");
    }

    /// The shape a Gemma 4 session wrote twice in a row: each parameter as
    /// its own element and the call shut by `</write>`, the tool's name.
    const ELEMENT_PARAMETERS_CALL: &str = "<tool_call>\n<function=write>\n<path>index.html</path>\n<content><svg><path d=\"M0 0\"></path></svg>\n</content>\n</write>\n</tool_call>\n\nDone.";

    #[test]
    fn a_call_with_element_parameters_shut_by_its_name_is_a_call() {
        let parts = parse_output_with_tools(ELEMENT_PARAMETERS_CALL, &signatures());
        assert_eq!(
            parts,
            vec![OutputPart::ToolCall {
                name: "write".to_string(),
                arguments: json!({
                    "path": "index.html",
                    "content": "<svg><path d=\"M0 0\"></path></svg>",
                }),
            },]
        );
    }

    #[test]
    fn a_call_with_element_parameters_streams_its_arguments_whole() {
        let body = ELEMENT_PARAMETERS_CALL
            .strip_prefix("<tool_call>")
            .and_then(|rest| rest.split("</tool_call>").next())
            .unwrap();
        let (_, parsed) = parse_call_body(body, CallKind::Wrapped).unwrap();
        let mut streamer = CallArgsStreamer::new(true);
        let mut out = String::new();
        for end in (0..=body.len()).filter(|&i| body.is_char_boundary(i)) {
            if let Some(delta) = streamer.advance(&body[..end]) {
                out.push_str(&delta);
            }
        }
        if let Some(delta) = streamer.finish_complete(body, &parsed) {
            out.push_str(&delta);
        }
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), parsed);
    }

    fn hermes_signatures() -> ToolSignatures {
        ToolSignatures::of(&[
            ToolDef {
                name: "execute_code".to_string(),
                description: None,
                input_schema: json!({"type": "object", "properties": {"code": {}}}),
                eager_input_streaming: None,
            },
            ToolDef {
                name: "terminal".to_string(),
                description: None,
                input_schema: json!({"type": "object", "properties": {"command": {}}}),
                eager_input_streaming: None,
            },
        ])
    }

    /// What Gemma 4 wrote, three times out of three, under Hermes Agent's
    /// 20k-character system prompt and 21 tools: the declared tool's name as
    /// the element, its parameters as children, no `<tool_call>` and no
    /// `<function=`.
    const NAMED_ELEMENT_CALL: &str = "<execute_code>\n<code>\nif __name__ == \"__main__\":\n    print([0, 1, 1, 2])\n</code>\n</execute_code>";

    #[test]
    fn a_call_written_as_the_tool_element_is_a_call() {
        let parts = parse_output_with_tools(
            &format!("Running it.\n{NAMED_ELEMENT_CALL}"),
            &hermes_signatures(),
        );
        assert_eq!(
            parts,
            vec![
                OutputPart::Text("Running it.\n".to_string()),
                OutputPart::ToolCall {
                    name: "execute_code".to_string(),
                    arguments: json!({"code": "if __name__ == \"__main__\":\n    print([0, 1, 1, 2])"}),
                },
            ]
        );
    }

    #[test]
    fn a_call_written_as_the_tool_element_survives_any_chunking() {
        let text = format!("Running it.\n{NAMED_ELEMENT_CALL}");
        let whole = parse_output_with_tools(&text, &hermes_signatures());
        for size in 1..12 {
            let mut scanner = TagScanner::with_tools(hermes_signatures());
            let mut events = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                events.extend(scanner.feed(std::str::from_utf8(chunk).unwrap()));
            }
            events.extend(scanner.finish());
            assert_eq!(
                merge_adjacent_text(events_to_parts(events)),
                whole,
                "chunk {size}"
            );
        }
    }

    /// The tail Gemma 4 wrote after its call: the result of a tool that has
    /// not run yet. Text before a call stays; after one it is never an answer.
    #[test]
    fn text_after_a_call_is_dropped_and_later_calls_still_count() {
        let text = format!(
            "I will run it.\n{NAMED_ELEMENT_CALL}\n\nThe script outputted `[0, 1, 1, 2]`.\n<tool_call>\n<function=terminal>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>\nDone."
        );
        let expected = vec![
            OutputPart::Text("I will run it.\n".to_string()),
            OutputPart::ToolCall {
                name: "execute_code".to_string(),
                arguments: json!({"code": "if __name__ == \"__main__\":\n    print([0, 1, 1, 2])"}),
            },
            OutputPart::ToolCall {
                name: "terminal".to_string(),
                arguments: json!({"command": "ls"}),
            },
        ];
        assert_eq!(
            parse_output_with_tools(&text, &hermes_signatures()),
            expected
        );
        for size in 1..12 {
            let mut scanner = TagScanner::with_tools(hermes_signatures());
            let mut events = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                events.extend(scanner.feed(std::str::from_utf8(chunk).unwrap()));
            }
            events.extend(scanner.finish());
            assert_eq!(
                merge_adjacent_text(events_to_parts(events)),
                expected,
                "chunk {size}"
            );
        }
    }

    #[test]
    fn a_malformed_call_is_not_a_call_so_the_text_after_it_stays() {
        let text =
            "<tool_call>\n<function=write>\n<path>a.html</path>\n</read>\n</tool_call>\nafter";
        let parts = parse_output(text);
        assert!(
            parts.iter().all(|p| matches!(p, OutputPart::Text(_))),
            "{parts:?}"
        );
        assert!(
            matches!(parts.last(), Some(OutputPart::Text(t)) if t.ends_with("after")),
            "{parts:?}"
        );
    }

    #[test]
    fn a_tool_element_the_model_left_unshut_is_still_a_call() {
        let text = NAMED_ELEMENT_CALL.strip_suffix("</execute_code>").unwrap();
        let parts = parse_output_with_tools(text, &hermes_signatures());
        assert!(
            matches!(parts.as_slice(), [OutputPart::ToolCall { name, .. }] if name == "execute_code"),
            "{parts:?}"
        );
    }

    #[test]
    fn a_tool_element_cut_by_the_cap_comes_back_structured_and_incomplete() {
        let (parts, cut) = parse_output_truncated_with_tools(
            "<execute_code>\n<code>\nprint(",
            &hermes_signatures(),
        );
        assert!(cut);
        assert!(
            parts
                .iter()
                .all(|p| !matches!(p, OutputPart::Text(t) if t.contains('<'))),
            "{parts:?}"
        );
    }

    #[test]
    fn a_tool_name_in_prose_or_an_undeclared_element_stays_text() {
        for text in [
            "Use the <terminal> tool, then <execute_code>.",
            "<foo>\n<code>x</code>\n</foo>",
            "Inline <terminal>\n tag and more prose",
        ] {
            let parts = parse_output_with_tools(text, &hermes_signatures());
            assert_eq!(parts, vec![OutputPart::Text(text.to_string())], "{text}");
        }
    }

    #[test]
    fn without_declared_tools_a_tool_element_stays_text() {
        assert_eq!(
            parse_output(NAMED_ELEMENT_CALL),
            vec![OutputPart::Text(NAMED_ELEMENT_CALL.to_string())]
        );
    }

    /// What Gemma 4 answered a Hermes Agent turn from Telegram with, the whole
    /// output: its native call format instead of the instructed `<tool_call>`.
    const NATIVE_CALL: &str = "<|tool_call>call:web_search{query:<|\"|>notícias principais globo.com hoje<|\"|>}<tool_call|>\n";

    #[test]
    fn a_gemma_native_call_is_a_call() {
        assert_eq!(
            parse_output(NATIVE_CALL),
            vec![OutputPart::ToolCall {
                name: "web_search".to_string(),
                arguments: json!({"query": "notícias principais globo.com hoje"}),
            }]
        );
    }

    #[test]
    fn a_gemma_native_call_survives_any_chunking() {
        let text = format!("Searching.\n{NATIVE_CALL}");
        let whole = parse_output(&text);
        for size in 1..14 {
            let mut scanner = TagScanner::new();
            let mut events = Vec::new();
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let mut end = size.min(rest.len());
                while !rest.is_char_boundary(end) {
                    end += 1;
                }
                events.extend(scanner.feed(&rest[..end]));
                rest = &rest[end..];
            }
            events.extend(scanner.finish());
            assert_eq!(merge_adjacent_text(events_to_parts(events)), whole, "chunk {size}");
        }
    }

    #[test]
    fn a_gemma_native_call_reads_every_argument_type() {
        let text = "<|tool_call>call:write_file{path:<|\"|>a, b.txt<|\"|>,content:<|\"|>say <tool_call|> and }<|\"|>,count:3,ratio:0.5,force:true,tags:[<|\"|>x<|\"|>,2],opts:{mode:<|\"|>w<|\"|>,\"quoted\":\"j\"}}<tool_call|>";
        assert_eq!(
            parse_output(text),
            vec![OutputPart::ToolCall {
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "a, b.txt",
                    "content": "say <tool_call|> and }",
                    "count": 3,
                    "ratio": 0.5,
                    "force": true,
                    "tags": ["x", 2],
                    "opts": {"mode": "w", "quoted": "j"},
                }),
            }]
        );
    }

    #[test]
    fn a_gemma_native_call_with_no_arguments_or_left_unshut_is_still_a_call() {
        for text in ["<|tool_call>call:list_tools{}<tool_call|>", "<|tool_call>call:list_tools{}"] {
            assert_eq!(
                parse_output(text),
                vec![OutputPart::ToolCall { name: "list_tools".to_string(), arguments: json!({}) }],
                "{text}"
            );
        }
    }

    #[test]
    fn a_malformed_gemma_native_call_stays_text() {
        for text in [
            "<|tool_call>call:web search{q:1}<tool_call|>",
            "<|tool_call>call:web_search{query:<|\"|>open}<tool_call|><|\"|>",
            "<|tool_call>call:web_search{query}<tool_call|>",
            "<|tool_call>call:web_search{q:1} trailing<tool_call|>",
        ] {
            assert!(
                parse_output(text).iter().all(|p| matches!(p, OutputPart::Text(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn a_gemma_native_call_cut_by_the_cap_keeps_the_name_and_the_open_value() {
        let (parts, cut) =
            parse_output_truncated("<|tool_call>call:web_search{limit:5,query:<|\"|>notícias de");
        assert!(cut);
        let [OutputPart::ToolCallTruncated(partial)] = parts.as_slice() else {
            panic!("expected one truncated call: {parts:?}");
        };
        assert_eq!(partial.name, "web_search");
        assert_eq!(partial.arguments, "{\"limit\":5,\"query\":\"notícias de");
        assert_eq!(partial.recovered, json!({"limit": 5, "query": "notícias de"}));
        let (parts, cut) = parse_output_truncated("<|tool_call>call:web_se");
        assert!(cut);
        assert!(parts.is_empty(), "{parts:?}");
    }

    #[test]
    fn a_call_shut_by_another_name_stays_text() {
        let text = "<tool_call>\n<function=write>\n<path>a.html</path>\n</read>\n</tool_call>";
        assert!(parse_output(text).iter().all(|p| matches!(p, OutputPart::Text(_))));
    }

    #[test]
    fn without_declared_tools_a_nameless_call_stays_text() {
        let text = "<tool_call>\n<parameter name=\"command\">ls</parameter>\n</tool_call>";
        assert!(parse_output(text).iter().all(|p| matches!(p, OutputPart::Text(_))));
    }

    #[test]
    fn a_mixed_spelling_call_streams_every_parameter() {
        let raw = "<function=edit>\n<parameter=path>\nf\n</parameter>\n<parameter name=\"oldText\">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</parameter>\n</function>\n";
        let mut streamer = CallArgsStreamer::new(true);
        let mut out = String::new();
        for end in 1..=raw.len() {
            if let Some(delta) = streamer.advance(&raw[..end]) {
                out.push_str(&delta);
            }
        }
        let (_, parsed) = parse_function_call_xml(raw).unwrap();
        if let Some(delta) = streamer.finish_complete(raw, &parsed) {
            out.push_str(&delta);
        }
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), parsed);
    }

    #[test]
    fn a_bare_invoke_cut_by_the_cap_comes_back_structured() {
        let (parts, cut) =
            parse_output_truncated("<invoke name=\"bash\">\n<parameter name=\"command\">ls -l");
        assert!(cut);
        assert!(
            matches!(parts.as_slice(), [OutputPart::ToolCallTruncated(p)] if p.name == "bash"),
            "{parts:?}"
        );
    }


    #[test]
    fn a_foreign_closer_inside_a_value_does_not_cut_the_call() {
        let text = "<tool_call>\n<function=write>\n<parameter=content>\nends with </function_call> and </function_calls>\n</parameter>\n</function>\n</tool_call>";
        let (name, args) = only_call(text);
        assert_eq!(name, "write");
        assert_eq!(args["content"], "ends with </function_call> and </function_calls>");
        // Byte by byte, the same answer.
        let mut scanner = TagScanner::new();
        let mut events = Vec::new();
        for (i, _) in text.char_indices() {
            events.extend(scanner.feed(&text[i..i + 1]));
        }
        events.extend(scanner.finish());
        assert_eq!(merge_adjacent_text(events_to_parts(events)), parse_output(text));
    }

    #[test]
    fn a_foreign_closer_outside_a_value_still_ends_the_call() {
        let (name, _) = only_call(
            "<tool_call>\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n</function_calls>",
        );
        assert_eq!(name, "bash");
    }


    #[test]
    fn a_call_named_by_a_quoted_parameter_tag_is_read_as_that_call() {
        // Logged verbatim (Strata, 2026-10-20): the name in the quoted
        // spelling and never closed, inside a misspelt wrapper.
        let (name, args) = only_call(
            "Let me add it. <function_call>\n<parameter name=\"edit\">\n<parameter name=\"path\">crates/node/src/backend/db.rs</parameter>\n<parameter name=\"edits\">[{\"newText\": \"a\", \"oldText\": \"b\"}]\n</parameter>\n</function>\n</tool_call>\n",
        );
        assert_eq!(name, "edit");
        assert_eq!(args["path"], "crates/node/src/backend/db.rs");
        assert_eq!(args["edits"], json!([{"newText": "a", "oldText": "b"}]));
    }


    #[test]
    fn a_call_with_no_opener_after_the_reasoning_is_a_call() {
        // Logged verbatim (Strata, 2026-10-20), right after `</think>`.
        let (name, args) = only_call(
            "<function=shell>\n<parameter=command>\nsed -n '1870,1895p' crates/node/src/network/network.rs\n</parameter>\n</function>\n</tool_call>\n",
        );
        assert_eq!(name, "shell");
        assert_eq!(args["command"], "sed -n '1870,1895p' crates/node/src/network/network.rs");
    }

    #[test]
    fn a_bare_function_tag_with_an_undeclared_name_stays_text() {
        let text = "The parser reads <function=NAME><parameter=P>v</parameter></function> here.";
        let parts = parse_output_with_tools(text, &signatures());
        assert_eq!(parts, vec![OutputPart::Text(text.to_string())]);
    }

    #[test]
    fn a_function_close_inside_a_value_does_not_end_a_bare_call() {
        let (_, args) = only_call(
            "<function=write>\n<parameter=content>\nends </function> here\n</parameter>\n</function>",
        );
        assert_eq!(args["content"], "ends </function> here");
    }


    #[test]
    fn a_namespaced_invoke_is_a_call() {
        // Logged verbatim (Strata, 2026-10-21): the opener carries the XML
        // namespace, the closer does not.
        let (name, args) = only_call(
            "<antml:invoke name=\"bash\">\n<parameter name=\"command\">cd /x && sed -n '1526,1580p' admin.rs</parameter>\n</invoke>\n",
        );
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "cd /x && sed -n '1526,1580p' admin.rs");
    }

    #[test]
    fn a_namespaced_invoke_shut_by_another_dialect_is_a_call() {
        let (name, args) = only_call(
            "<antml:invoke name=\"edit\">\n<parameter name=\"path\">a.rs</parameter>\n<parameter name=\"oldText\">x</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(name, "edit");
        assert_eq!(args["oldText"], "x");
    }

    #[test]
    fn a_namespaced_container_with_namespaced_tags_is_a_call() {
        let (name, args) = only_call(
            "<antml:function_calls>\n<antml:invoke name=\"read\">\n<antml:parameter name=\"path\">a.rs</antml:parameter>\n</antml:invoke>\n</antml:function_calls>",
        );
        assert_eq!(name, "read");
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn a_namespaced_invoke_survives_any_chunking() {
        let text = "ok <antml:invoke name=\"bash\">\n<parameter name=\"command\">ls</parameter>\n</invoke>\n";
        let whole = parse_output(text);
        assert!(whole.iter().any(|p| matches!(p, OutputPart::ToolCall { .. })), "{whole:?}");
        for size in 1..9 {
            let mut scanner = TagScanner::new();
            let mut events = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                events.extend(scanner.feed(std::str::from_utf8(chunk).unwrap()));
            }
            events.extend(scanner.finish());
            assert_eq!(merge_adjacent_text(events_to_parts(events)), whole, "chunk {size}");
        }
    }

}
