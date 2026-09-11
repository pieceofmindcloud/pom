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
    /// Streaming de granularidade fina para **esta** ferramenta: entrega os
    /// fragmentos do argumento conforme o modelo os gera, sem represar para
    /// validar antes.
    ///
    /// Forma da Anthropic, verificada na documentação viva em 2026-08-29
    /// (`platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming`):
    /// campo **opcional na definição da ferramenta**, sem cabeçalho beta - o
    /// header legado `fine-grained-tool-streaming-2025-05-14` foi substituído
    /// por ele. Ausente ou `false` mantém o represamento, que lá é **por valor
    /// de parâmetro**, não pela chamada inteira.
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
        kind: CallKind::BareFunction,
        open: BARE_FUNCTION_OPEN,
        close: BARE_FUNCTION_CLOSE,
    },
];

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
    /// A chamada que o teto de saída cortou ao meio. Não é escolha do modelo:
    /// o orçamento do cliente acabou com a tag ainda aberta. Ver
    /// [`TagScanner::finish_truncated`] e [`PartialCall`].
    ToolCallTruncated(PartialCall),
}

/// Uma chamada de ferramenta interrompida pelo teto de saída, na forma em que
/// os dois provedores a devolvem.
///
/// OpenAI e Anthropic **não descartam** a chamada cortada e **não** a degradam
/// em texto: ela volta estruturada, com os argumentos incompletos, ao lado do
/// motivo de parada terminal (`finish_reason: "length"` /
/// `stop_reason: "max_tokens"`). A orientação dos dois é a mesma - o cliente
/// confere o motivo de parada e repete o pedido com um teto maior, sem
/// executar nada.
///
/// Apurado na documentação viva em 2026-08-29:
/// - `platform.claude.com/docs/en/build-with-claude/handling-stop-reasons` -
///   com `stop_reason: "max_tokens"`, o cliente confere se o **último bloco**
///   é um `tool_use` incompleto e repete com `max_tokens` maior.
/// - `developers.openai.com/api/reference/…/chat` - `function.arguments` é
///   documentado como string que nem sempre é JSON válido, a validar antes de
///   executar.
///
/// Ver `docs/subsystems/inferencia.md` (9.1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartialCall {
    pub(crate) name: String,
    /// Os argumentos como a OpenAI os entrega: uma string JSON **inacabada**,
    /// cortada onde o modelo foi cortado. É de propósito que ela não parseia -
    /// é o que faz um cliente que ignore o motivo de parada falhar em vez de
    /// executar meia chamada.
    pub(crate) arguments: String,
    /// O objeto com o que deu para recuperar, para o `input` do bloco
    /// `tool_use` da Anthropic, que é tipado como objeto. Vazio quando o corte
    /// caiu antes do primeiro parâmetro fechar.
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
                    // Earliest opener wins; the registry order breaks ties in
                    // favour of the real wrapper (higher-confidence sighting).
                    let chosen = CALL_FORMATS
                        .iter()
                        .enumerate()
                        .filter_map(|(priority, format)| {
                            self.pending
                                .find(format.open)
                                .map(|position| (position, priority, format.kind))
                        })
                        .min_by_key(|(position, priority, _)| (*position, *priority))
                        .map(|(position, _, kind)| (position, kind));
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
                            let hold = CALL_FORMATS
                                .iter()
                                .map(|format| longest_prefix_overlap(&self.pending, format.open))
                                .max()
                                .unwrap_or(0);
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

    /// Whatever never resolved into a complete tag by the time the **model**
    /// stopped — a dangling `<tool_c` fragment, or a call the model abandoned
    /// before closing. Nothing is dropped: o modelo escolheu parar ali, e o
    /// texto que ele produziu é a resposta dele.
    ///
    /// Para o fim que não é escolha do modelo - o orçamento acabou - use
    /// [`TagScanner::finish_truncated`], que descarta em vez de degradar.
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

    /// Como [`TagScanner::finish`], mas para o fim imposto de fora: o teto de
    /// saída pedido pelo cliente se esgotou no meio da geração.
    ///
    /// Com uma chamada **aberta e não fechada**, ela sai estruturada e
    /// incompleta - [`ScanEvent::ToolCallTruncated`] - que é o que OpenAI e
    /// Anthropic fazem. Nem degradar em texto (marcação crua entregue como
    /// prosa, o defeito original) nem descartar: o cliente precisa ver que
    /// **aquela** chamada não coube, para repetir o pedido com um teto maior.
    /// Ver [`PartialCall`].
    ///
    /// Cortada cedo demais para ter nome, não há chamada nenhuma a nomear e
    /// nada sai: um nome que o modelo não terminou de escrever não é
    /// ferramenta, é palpite.
    ///
    /// O `bool` de retorno é `true` sempre que o corte caiu dentro de uma
    /// chamada, com nome ou sem, e é o que faz `x_pom` dizer a verdade.
    ///
    /// Fora de uma chamada nada muda - texto truncado continua sendo texto, e
    /// um prefixo pendente de tag (`<tool_c`) nunca chegou a abrir chamada
    /// nenhuma.
    pub(crate) fn finish_truncated(self) -> (Vec<ScanEvent>, bool) {
        match self.state {
            ScanState::Inside(raw, kind) => {
                let body = format!("{raw}{}", self.pending);
                match parse_partial_call_body(&body, kind) {
                    Some(partial) => (vec![ScanEvent::ToolCallTruncated(partial)], true),
                    None => (Vec::new(), true),
                }
            }
            _ => (self.finish(), false),
        }
    }
}

/// O que dá para recuperar do corpo de uma chamada que o teto cortou.
///
/// Espelha [`parse_call_body`], mas tolerando o fim ausente: o `</function>`
/// que nunca veio, o `</parameter>` que ficou aberto, o JSON sem a chave de
/// fechamento. `None` quando nem o nome da ferramenta chegou a sair inteiro.
fn parse_partial_call_body(raw: &str, kind: CallKind) -> Option<PartialCall> {
    match kind {
        // `<function>NAME` sem fechamento: `NAME` pode estar pela metade, e
        // um nome pela metade não identifica ferramenta nenhuma.
        CallKind::BareFunction => None,
        CallKind::Wrapped => {
            let trimmed = raw.trim_start();
            if trimmed.starts_with("<function=") {
                parse_partial_function_call_xml(trimmed)
            } else {
                parse_partial_function_call_json(trimmed)
            }
        }
    }
}

/// Serializa os pares recuperados na forma exata em que a OpenAI entrega uma
/// chamada cortada: um objeto JSON **sem fechar**, interrompido onde o modelo
/// foi interrompido. `partial` é o par cujo valor ainda estava sendo escrito -
/// ele entra sem a aspa de fechamento.
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
        // A aspa de abertura entra, a de fechamento não: o valor não acabou.
        let escaped = serde_json::Value::String(value.to_string()).to_string();
        out.push_str(&escaped[..escaped.len() - 1]);
    }
    out
}

/// `<function=NAME>` com zero ou mais `<parameter=P>v</parameter>` completos e,
/// possivelmente, um último parâmetro aberto que o corte pegou no meio.
fn parse_partial_function_call_xml(raw: &str) -> Option<PartialCall> {
    let rest = raw.strip_prefix("<function=")?;
    // Sem o `>` o próprio nome está pela metade.
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
            // `<parameter=pa` - nem o nome do parâmetro fechou.
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
                // O que ainda pode virar `</parameter>` não é conteúdo: sem
                // este recuo, um corte no meio da tag entregaria `</paramet`
                // dentro do valor. É o mesmo recuo de `open_parameter_prefix`,
                // e é o que mantém a versão truncada sempre estendendo o que
                // o streaming já transmitiu.
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

/// A forma Hermes (`{"name":…,"arguments":{…`) cortada. O nome só serve se a
/// string dele fechou; o resto do corpo já é, ele próprio, JSON inacabado - e
/// é entregue como está, que é exatamente o que a OpenAI devolve.
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
    // Um corpo que por acaso parseia inteiro ainda é chamada cortada: a tag de
    // fechamento não veio, então mais argumentos podiam estar a caminho.
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

/// O valor de uma chave de string JSON, **só** quando a string fechou. Um
/// nome cortado ao meio não identifica ferramenta nenhuma, então `None`.
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
/// O valor de um `<parameter=…>`: JSON quando o texto é JSON, string crua
/// quando não é. O parser completo e o parcial dividem esta função para nunca
/// discordarem sobre o tipo de um mesmo valor.
fn decode_parameter_value(value_raw: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(value_raw)
        .unwrap_or_else(|_| serde_json::Value::String(value_raw.to_string()))
}

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
        let close_idx = after_name.find(PARAM_CLOSE)?;
        let value_raw = after_name[..close_idx].trim();
        args.insert(pname.to_string(), decode_parameter_value(value_raw));
        cursor = &after_name[close_idx + PARAM_CLOSE.len()..];
    }
    Some((name.to_string(), serde_json::Value::Object(args)))
}

// ── Streaming incremental dos argumentos ─────────────────────────────────────

/// A partir de quantos bytes um valor de parâmetro pode ser transmitido em
/// fragmentos no modo ansioso.
///
/// O streaming de granularidade fina existe para "reduzir o tempo até o
/// primeiro fragmento de um parâmetro **grande**". Um valor que coube em 64
/// bytes não tinha latência para resolver, e represá-lo custa nada e preserva
/// o tipo dele - que é o que separa `{"limit":5}` de `{"limit":"5"}`. A
/// marcação do POM não carrega tipo, então este é o preço de transmitir sem
/// esperar, e é por isso que ele só é pago onde há o que ganhar.
const EAGER_MIN_VALUE_BYTES: usize = 64;

/// Este valor de parâmetro vai ser transmitido em fragmentos?
///
/// **A resposta é estável enquanto o valor cresce**, e é disso que depende a
/// monotonicidade de `streamable_arguments_prefix`: o comprimento só aumenta,
/// e o primeiro caractere não muda. Um parâmetro que começou a ser transmitido
/// continua sendo até fechar.
///
/// **Só é transmitido o valor que comprovadamente não pode ser JSON.** É essa
/// a regra que faz o modo ansioso e o represado concordarem sobre o tipo: um
/// valor que nunca vai parsear como JSON vira `String` nos dois caminhos, e
/// `{"limit":5}` jamais degenera em `{"limit":"5"}`.
///
/// Acima do limiar, só estes primeiros caracteres ainda podem começar um JSON
/// válido: `{` e `[` (contêineres), `"` (string literal), `-` e dígito
/// (número). `true`/`false`/`null` são literais curtos, então uma letra num
/// valor longo já garante que ele não é JSON - por isso letra passa.
///
/// Represar um arquivo que começa em `{` custa latência num caso raro; entregar
/// um array virado em texto custa correção. A escolha é óbvia.
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

/// O corpo de uma string JSON, já escapado, sem as aspas.
fn json_string_body(raw: &str) -> String {
    let quoted = serde_json::Value::String(raw.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// O maior prefixo dos argumentos JSON que já pode ser transmitido, dado o
/// corpo da chamada visto até agora.
///
/// **Invariante:** o resultado só cresce conforme `raw` cresce. Nada que sai
/// daqui é retirado depois - não existe desenviar um fragmento SSE. Todo
/// cuidado deste módulo (o recuo de espaço em branco, a retenção de prefixo de
/// tag, a estabilidade de `streams_eagerly`) existe para sustentar isso.
///
/// `None` quer dizer "ainda não dá para dizer nada": o nome da função não
/// fechou, ou o corpo não é a forma XML. A forma Hermes (`{"name":…}`) não é
/// transcodificada ao vivo porque o nome mora dentro do próprio JSON que
/// estaria sendo transmitido; ela sai inteira no fechamento, como antes.
fn streamable_arguments_prefix(raw: &str, eager: bool) -> Option<String> {
    let trimmed = raw.trim_start();
    let rest = trimmed.strip_prefix("<function=")?;
    let (name, mut cursor) = rest.split_once('>')?;
    if name.trim().is_empty() {
        return None;
    }

    let mut out = String::from("{");
    let mut first = true;
    loop {
        let t = cursor.trim_start();
        let Some(after_open) = t.strip_prefix("<parameter=") else {
            break;
        };
        let Some((pname, after_name)) = after_open.split_once('>') else {
            break;
        };
        let pname = pname.trim();
        match after_name.find("</parameter>") {
            Some(close) => {
                let value_raw = after_name[..close].trim();
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&serde_json::Value::String(pname.to_string()).to_string());
                out.push(':');
                // O parâmetro que foi transmitido em fragmentos fecha como
                // string, porque foi assim que ele saiu. Os outros levam o
                // tipo decodificado, igual ao caminho não-streaming.
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
                // Parâmetro ainda aberto. Só o modo ansioso emite algo dele, e
                // só depois de o valor ficar grande o bastante.
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
    Some(out)
}

/// O pedaço já transmissível de um parâmetro que ainda não fechou, escapado.
///
/// Dois recuos, os dois pela monotonicidade:
///
/// - **prefixo de tag**: o que ainda pode virar `</parameter>` ou
///   `</function>` fica retido, senão `</par` sairia como conteúdo e teria de
///   ser retirado quando a tag se completasse.
/// - **espaço em branco no fim**: o valor final é `trim()`ado, então espaço no
///   fim do que se viu até agora pode ser o espaço que vai sumir. Ele só sai
///   quando algo que não é espaço vem depois - e aí ele era conteúdo interno.
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

/// O nome da ferramenta, assim que ele fecha - antes de a chamada terminar.
///
/// É o que permite anunciar `tool_calls[i].function.name` no primeiro
/// fragmento, como a OpenAI faz: sem o nome, um índice aberto não serve para
/// nada. `None` enquanto o `<function=NOME>` não fechou, ou na forma Hermes,
/// que não é transcodificada ao vivo.
pub(crate) fn peek_call_name(raw_body: &str) -> Option<String> {
    let rest = raw_body.trim_start().strip_prefix("<function=")?;
    let (name, _) = rest.split_once('>')?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Transmite os argumentos de **uma** chamada de ferramenta em fragmentos,
/// conforme o modelo os gera.
///
/// O defeito que ela existe para consertar, medido no ksia vivo: os 663
/// caracteres do argumento chegavam todos de uma vez, no fim da geração, em
/// dois pedaços ambos aos 11,01 s. O leitor de tags acumulava o corpo inteiro
/// e só descarregava quando a chamada fechava, então o usuário encarava uma
/// tela parada e recebia o arquivo de supetão. Anthropic e OpenAI emitem os
/// fragmentos conforme geram.
///
/// **Invariante central:** a concatenação de tudo que sai daqui é exatamente a
/// string de argumentos que o caminho não-streaming produziria - inclusive
/// quando o teto corta a chamada ao meio, caso em que ela é a string inacabada
/// da Fase 1. `emitted` guarda o que já saiu justamente para que o fechamento
/// emita só o que falta, e nunca repita o que o cliente já tem.
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

    /// Quanto já saiu. `0` quer dizer que o cliente ainda não viu argumento
    /// nenhum desta chamada.
    pub(crate) fn emitted_len(&self) -> usize {
        self.emitted.len()
    }

    /// O próximo fragmento, dado o corpo da chamada visto até agora.
    /// `None` quando não há nada novo a dizer.
    pub(crate) fn advance(&mut self, raw_body_so_far: &str) -> Option<String> {
        let prefix = streamable_arguments_prefix(raw_body_so_far, self.eager)?;
        // A chave sozinha não é conteúdo: enquanto nenhum parâmetro fechou (ou
        // começou a ser transmitido) esta chamada ainda não começou a sair.
        // Isso é o que deixa uma chamada malformada continuar caindo em texto
        // no fechamento, como antes - depois do primeiro fragmento não haveria
        // como voltar atrás.
        if self.emitted.is_empty() && prefix == "{" {
            return None;
        }
        self.take_delta(prefix)
    }

    /// O fechamento de uma chamada que **completou**.
    ///
    /// A string sai do mesmo transcodificador que gerou os fragmentos, mais a
    /// chave de fechamento - e não de `parsed.to_string()`. O motivo é
    /// concreto: `serde_json::Map` é uma árvore ordenada, então
    /// `parsed.to_string()` devolve as chaves em ordem **alfabética**,
    /// enquanto os fragmentos saíram na ordem em que o modelo as escreveu.
    /// Fechar com a versão ordenada não estenderia o que o cliente já
    /// acumulou. O conteúdo é o mesmo; a ordem das chaves de um objeto JSON
    /// não é semântica.
    ///
    /// `parsed` é o recurso da forma Hermes, que não é transcodificada ao vivo
    /// e portanto não teve fragmento nenhum: ali a string inteira sai de uma
    /// vez, como antes.
    pub(crate) fn finish_complete(
        &mut self,
        raw_body: &str,
        parsed: &serde_json::Value,
    ) -> Option<String> {
        match streamable_arguments_prefix(raw_body, self.eager) {
            Some(mut prefix) => {
                prefix.push('}');
                self.take_delta(prefix)
            }
            None => self.take_delta(parsed.to_string()),
        }
    }

    /// O fechamento de uma chamada que o teto **cortou**: `full` é a string
    /// inacabada da Fase 1. Sai só o que ainda não tinha saído, para o cliente
    /// não receber duas vezes o que já acumulou.
    pub(crate) fn finish_truncated(&mut self, full: &str) -> Option<String> {
        self.take_delta(full.to_string())
    }

    /// O delta, com a rede de segurança: se o novo prefixo não estende o que
    /// já saiu, a monotonicidade foi quebrada em algum lugar. Emitir o delta
    /// aí corromperia o JSON do cliente, então nada sai e o fragmento
    /// divergente é registrado. Degradar para "não transmitiu" é sempre
    /// melhor que transmitir errado.
    fn take_delta(&mut self, prefix: String) -> Option<String> {
        if !prefix.starts_with(&self.emitted) {
            warn!(
                emitido = self.emitted.len(),
                novo = prefix.len(),
                "prefixo de argumentos não estende o que já foi transmitido; \
                 fragmento suprimido para não corromper o JSON do cliente"
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
    /// A chamada que o teto do cliente cortou ao meio, estruturada e
    /// incompleta - ver [`PartialCall`].
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
pub(crate) fn parse_output(text: &str) -> Vec<OutputPart> {
    let mut scanner = TagScanner::new();
    let mut events = scanner.feed(text);
    events.extend(scanner.finish());
    merge_adjacent_text(events_to_parts(events))
}

/// [`parse_output`] para uma saída cortada pelo teto de tokens, e não pelo
/// modelo: uma chamada aberta e não fechada volta **estruturada e incompleta**
/// em vez de degradar em texto. O `bool` diz se o corte caiu dentro de uma
/// chamada - ver [`TagScanner::finish_truncated`].
pub(crate) fn parse_output_truncated(text: &str) -> (Vec<OutputPart>, bool) {
    let mut scanner = TagScanner::new();
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

    // ── o corte do teto dentro de uma chamada ───────────────────────────────

    /// A forma que os dois provedores devolvem: chamada estruturada, nome
    /// recuperado, argumentos numa string JSON **inacabada**. Ela não parseia
    /// de propósito - é o que faz um cliente que ignore o motivo de parada
    /// falhar em vez de executar meia chamada.
    #[test]
    fn a_call_cut_mid_parameter_keeps_the_name_and_the_unterminated_arguments() {
        let raw = "<tool_call>\n<function=Write>\n<parameter=path>\n/tmp/a.py\n</parameter>\n\
                   <parameter=content>\nlinha 1\nlinha 2";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("esperava chamada cortada, veio {parts:?}");
        };
        assert_eq!(partial.name, "Write");
        assert_eq!(
            partial.arguments,
            r#"{"path":"/tmp/a.py","content":"linha 1\nlinha 2"#
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&partial.arguments).is_err(),
            "argumentos de chamada cortada não podem parsear"
        );
        // E o objeto recuperado, para o `input` da Anthropic, é JSON válido
        // com o que deu para recuperar.
        assert_eq!(partial.recovered["path"], "/tmp/a.py");
        assert_eq!(partial.recovered["content"], "linha 1\nlinha 2");
    }

    /// Corte exatamente na fronteira de um `</parameter>`: não há parâmetro
    /// pela metade, mas a chamada continua incompleta - a chave do objeto
    /// nunca fecha, porque mais parâmetros podiam estar a caminho.
    #[test]
    fn a_call_cut_between_parameters_still_returns_unterminated_arguments() {
        let raw = "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</parameter>\n";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(cut_inside);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("esperava chamada cortada, veio {parts:?}");
        };
        assert_eq!(partial.arguments, r#"{"command":"ls""#);
        assert_eq!(partial.recovered["command"], "ls");
    }

    /// Corte antes do primeiro parâmetro: o nome já saiu, os argumentos não.
    #[test]
    fn a_call_cut_before_any_parameter_keeps_the_name_and_an_empty_object() {
        let raw = "<tool_call>\n<function=Bash>\n";
        let (parts, _) = parse_output_truncated(raw);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("esperava chamada cortada, veio {parts:?}");
        };
        assert_eq!(partial.name, "Bash");
        assert_eq!(partial.arguments, "{");
        assert_eq!(partial.recovered, json!({}));
    }

    /// Cortada cedo demais para ter nome: nada sai. Um nome que o modelo não
    /// terminou de escrever não é ferramenta, é palpite - e degradá-lo em
    /// texto entregaria marcação crua como se fosse prosa.
    #[test]
    fn a_call_cut_before_the_name_closed_yields_nothing_executable() {
        for raw in [
            "<tool_call>\n<function=Wri",
            "<tool_call>\n",
            "<function>rea",
        ] {
            let (parts, cut_inside) = parse_output_truncated(raw);
            assert!(cut_inside, "{raw:?} abriu chamada");
            assert!(parts.is_empty(), "{raw:?} produziu {parts:?}");
        }
    }

    /// O texto antes da abertura é resposta do modelo e sobrevive ao corte.
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

    /// A forma Hermes cortada: o nome sai se a string dele fechou, e o corpo
    /// dos argumentos já é, ele próprio, JSON inacabado.
    #[test]
    fn a_cut_hermes_style_call_keeps_the_name_and_the_raw_partial_json() {
        let raw = r#"<tool_call>{"name":"Bash","arguments":{"command":"ls -"#;
        let (parts, _) = parse_output_truncated(raw);
        let OutputPart::ToolCallTruncated(partial) = &parts[0] else {
            panic!("esperava chamada cortada, veio {parts:?}");
        };
        assert_eq!(partial.name, "Bash");
        assert_eq!(partial.arguments, r#"{"command":"ls -"#);
        assert_eq!(partial.recovered, json!({}));
    }

    /// Fora de uma chamada nada muda: texto cortado continua texto, e um
    /// prefixo de tag pendente nunca chegou a abrir chamada nenhuma.
    #[test]
    fn truncation_outside_a_call_behaves_exactly_like_a_normal_finish() {
        for raw in ["prosa cortada ao meio", "prosa <tool_c"] {
            let (parts, cut_inside) = parse_output_truncated(raw);
            assert!(!cut_inside, "{raw:?}");
            assert_eq!(parts, parse_output(raw), "{raw:?}");
        }
    }

    /// A chamada que **fechou** antes do corte não é chamada cortada: o corte
    /// caiu depois dela, em texto.
    #[test]
    fn a_call_that_closed_before_the_cut_is_a_complete_call() {
        let raw = "<tool_call>\n<function=Bash>\n<parameter=command>\nls\n</parameter>\n\
                   </function>\n</tool_call>depois";
        let (parts, cut_inside) = parse_output_truncated(raw);
        assert!(!cut_inside);
        assert!(matches!(&parts[0], OutputPart::ToolCall { name, .. } if name == "Bash"));
        assert_eq!(parts[1], OutputPart::Text("depois".into()));
    }

    // ── streaming incremental dos argumentos ────────────────────────────────

    /// O roteiro do defeito medido no ksia: uma chamada com um argumento
    /// grande, entregue token a token.
    fn roteiro_chamada_longa() -> Vec<String> {
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

    /// Alimenta o roteiro pedaço a pedaço e devolve os fragmentos emitidos.
    /// `ate` corta a geração no meio, para o caso do teto.
    fn fragmentos(eager: bool, ate: Option<usize>) -> Vec<String> {
        let script = roteiro_chamada_longa();
        let script = &script[..ate.unwrap_or(script.len())];
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
        match (ate, parsed_final) {
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

    /// **O defeito, virado do avesso.** Os argumentos saíam num pedaço só, no
    /// fim; agora saem em muitos, e o primeiro chega muito antes do último.
    #[test]
    fn a_long_argument_streams_in_many_fragments_instead_of_one_blob() {
        let frags = fragmentos(true, None);
        assert!(
            frags.len() > 5,
            "os argumentos continuam represados: {} fragmento(s)",
            frags.len()
        );
    }

    /// **O invariante central:** concatenar os fragmentos na ordem reproduz
    /// exatamente a string de argumentos, e ela é JSON válido com o mesmo
    /// conteúdo do caminho não-streaming.
    #[test]
    fn concatenating_the_fragments_reproduces_the_final_argument_exactly() {
        let inteiro: String = fragmentos(true, None).concat();
        let json: serde_json::Value =
            serde_json::from_str(&inteiro).expect("a concatenação é JSON válido");
        assert_eq!(json["path"], "/tmp/calc.py");
        assert!(json["content"]
            .as_str()
            .expect("content")
            .ends_with("linha_39 = 39 * 2"));

        // E é o mesmo objeto que o caminho não-streaming produz - só a ordem
        // das chaves difere, porque `serde_json::Map` ordena e o modelo não.
        let bruto: String = roteiro_chamada_longa().concat();
        let OutputPart::ToolCall { arguments, .. } = &parse_output(&bruto)[0] else {
            panic!("esperava chamada completa");
        };
        assert_eq!(&json, arguments);
    }

    /// **O modo represado preserva o comportamento antigo:** nada sai enquanto
    /// o parâmetro grande não fecha. É o padrão da Anthropic - o represamento
    /// é por valor de parâmetro, então o `path` sai cedo e o `content` só no
    /// fim.
    #[test]
    fn buffered_mode_emits_nothing_until_each_parameter_closes() {
        let frags = fragmentos(false, None);
        // Um fragmento por parâmetro que fechou, mais a chave de fechamento.
        assert_eq!(frags.len(), 3, "{frags:?}");
        // O que importa: o parâmetro grande sai INTEIRO num fragmento só - é
        // o comportamento antigo, e é o que o modo ansioso troca por latência.
        let grande = frags
            .iter()
            .find(|f| f.contains("linha_39"))
            .expect("o content saiu");
        assert!(grande.contains("linha_0 = 0 * 2"), "{grande:?}");
        let inteiro = frags.concat();
        assert!(
            inteiro.starts_with(r#"{"path":"/tmp/calc.py""#),
            "{inteiro:?}"
        );
        let json: serde_json::Value = serde_json::from_str(&inteiro).expect("JSON válido");
        assert!(json["content"]
            .as_str()
            .expect("content")
            .contains("linha_39"));
    }

    /// **O corte do teto não se confunde com fragmento parcial.** O que já
    /// tinha sido transmitido conta: o fechamento emite só o que falta, e a
    /// concatenação é exatamente a string inacabada da Fase 1 - que não
    /// parseia, de propósito.
    #[test]
    fn a_truncated_call_never_repeats_what_was_already_streamed() {
        let bruto_cortado: String = roteiro_chamada_longa()[..20].concat();
        let (parts, _) = parse_output_truncated(&bruto_cortado);
        let OutputPart::ToolCallTruncated(esperado) = &parts[0] else {
            panic!("esperava chamada cortada");
        };

        let inteiro: String = fragmentos(true, Some(20)).concat();
        assert_eq!(
            inteiro, esperado.arguments,
            "a concatenação do streaming tem de bater com a da Fase 1"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&inteiro).is_err(),
            "argumentos de chamada cortada não podem parsear: {inteiro:?}"
        );
    }

    /// Chamada curta continua correta e com o tipo intacto: um valor que coube
    /// no limiar nunca é transmitido em fragmentos, então `5` continua número.
    #[test]
    fn a_short_call_keeps_its_argument_types_even_in_eager_mode() {
        let bruto = "<function=Bash>\n<parameter=command>\nls -la\n</parameter>\n                     <parameter=limit>\n5\n</parameter>\n";
        let prefixo = streamable_arguments_prefix(bruto, true).expect("transcodifica");
        assert_eq!(prefixo, r#"{"command":"ls -la","limit":5"#);
    }

    /// Um valor grande que já é JSON estruturado fica represado mesmo no modo
    /// ansioso: transmiti-lo como string trocaria o tipo dele em silêncio.
    #[test]
    fn a_long_structured_value_stays_buffered_so_its_type_survives() {
        let array = format!(
            "[{}]",
            (0..40).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        );
        assert!(array.len() > EAGER_MIN_VALUE_BYTES);
        assert!(
            !streams_eagerly(true, &array),
            "array longo não pode ser transmitido"
        );
        assert!(!streams_eagerly(true, &format!("{{\"a\": {array}}}")));
        assert!(!streams_eagerly(true, &format!("\"{}\"", "x".repeat(80))));
        // Já texto que não pode ser JSON, esse sim.
        assert!(streams_eagerly(true, &"linha de codigo ".repeat(10)));
    }

    /// **Monotonicidade**, alimentando byte a byte: o prefixo transmissível
    /// nunca encolhe nem diverge. Não existe desenviar um fragmento SSE, então
    /// esta é a propriedade que o módulo inteiro protege.
    #[test]
    fn the_streamable_prefix_only_ever_grows_byte_by_byte() {
        let bruto: String = roteiro_chamada_longa().concat();
        for eager in [true, false] {
            let mut sc = TagScanner::new();
            let mut body = String::new();
            let mut anterior = String::new();
            for byte in bruto.as_bytes().chunks(1) {
                let Ok(pedaco) = std::str::from_utf8(byte) else {
                    continue;
                };
                for ev in sc.feed(pedaco) {
                    if let ScanEvent::ToolCallChunk(f) = ev {
                        body.push_str(&f);
                        if let Some(p) = streamable_arguments_prefix(&body, eager) {
                            assert!(
                                p.starts_with(&anterior),
                                "eager={eager}: o prefixo divergiu\nantes: {anterior:?}\ndepois: {p:?}"
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
}
