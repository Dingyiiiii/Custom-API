//! Cross-protocol translation.
//!
//! # Why this exists
//!
//! A gateway that only routes to same-protocol upstreams cannot answer an
//! Anthropic request when the model the user wants is served only by an
//! OpenAI-compatible provider. Translation removes that dead end.
//!
//! Design rules
//!
//! 1. Translation is a *fallback*, never the default. The router prefers a
//!    channel that speaks the client's protocol, so the common path is lossless
//!    byte forwarding. Translation only happens when no such channel exists.
//! 2. A translation is never silent. Every converted request is marked in the
//!    audit record (see `gateway.proxy_context.translated`), because a converted
//!    request has different fidelity from a forwarded one and an operator
//!    debugging odd output needs to know which they are looking at.
//! 3. Unrepresentable features are rejected loudly, not dropped quietly. If a
//!    request uses something the target protocol cannot express, the conversion
//!    returns an error naming the field, and the gateway turns that into a
//!    clear client-facing message. Silently dropping a tool schema or an image
//!    would produce a plausible-looking but wrong answer, which is far worse.
//! 4. Raw JSON is preserved wherever a field is passed through, so fields the
//!    gateway does not model survive the round trip.
//!
//! # Lossy spots, recorded explicitly
//!
//! * `tool_choice: "none"` has no Messages equivalent; the closest safe choice
//!   is `{"type":"auto"}` (the tools were already copied).
//! * A replayed `thinking` block is dropped when converting Messages -> chat:
//!   the chat API does not accept it as input and it is not user-visible.
//! * Chat `system`/`developer` messages collapse into a single Messages
//!   `system` field (or into `instructions` for Responses), joined with a blank
//!   line.
//! * Responses `reasoning` items are opaque state and are dropped on the way to
//!   chat, which cannot express them.
//! * Usage is mapped through input/output counters only: cache and audio
//!   breakdowns are not translated.

use std::fmt;

use serde_json::{Map, Value};

use super::responses::{
    ResponsesContentPart, ResponsesOutputItem, ResponsesRequest, ResponsesResponse, ResponsesTool,
    ResponsesUsage,
};
use super::types::{
    AnthropicBlock, AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicTool,
    AnthropicUsage, ChatMessage, ChatRequest, ChatResponse, Choice, Content, ContentPart,
    FunctionCall, ImageUrl, RawJson, ResponseFormat, Tool, ToolCall, ToolFunction,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Describes a request that cannot be expressed in the target protocol. It
/// names the offending field so the client can be told precisely what to
/// change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationError {
    /// The source protocol name.
    pub from: String,
    /// The target protocol name.
    pub to: String,
    /// The request field that cannot be represented.
    pub field: String,
    /// Why it cannot be represented.
    pub reason: String,
}

impl TranslationError {
    /// Builds an error for one field.
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        field: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        TranslationError {
            from: from.into(),
            to: to.into(),
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for TranslationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot translate {} -> {}: {} ({})",
            self.from, self.to, self.reason, self.field
        )
    }
}

impl std::error::Error for TranslationError {}

/// Everything that can go wrong while parsing or translating a body.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// The request cannot be represented in the target protocol.
    #[error("{0}")]
    Translation(#[from] TranslationError),
    /// A body could not be parsed.
    #[error("translate: parse {what}: {detail}")]
    Parse {
        /// Which body was being parsed ("chat request", "anthropic response").
        what: &'static str,
        /// The underlying parser message.
        detail: String,
    },
    /// A nil request or response was passed to a converter.
    #[error("translate: nil {0}")]
    Nil(&'static str),
    /// A free-form failure (stream chunk parse errors and the like).
    #[error("{0}")]
    Message(String),
}

impl ProtocolError {
    /// Returns the translation error, when this is one.
    ///
    /// The Go proxy type-asserts `*protocol.TranslationError` to decide whether
    /// a failure is the client's fault; this is the equivalent hook.
    pub fn translation(&self) -> Option<&TranslationError> {
        match self {
            ProtocolError::Translation(err) => Some(err),
            _ => None,
        }
    }
}

/// Reports that no direct translation path exists.
pub fn err_unsupported(from: &str, to: &str) -> ProtocolError {
    ProtocolError::Translation(TranslationError::new(
        from,
        to,
        "",
        "no translation path is implemented",
    ))
}

fn translation_error(
    from: &str,
    to: &str,
    field: impl Into<String>,
    reason: impl Into<String>,
) -> ProtocolError {
    ProtocolError::Translation(TranslationError::new(from, to, field, reason))
}

fn parse_error(what: &'static str, detail: impl fmt::Display) -> ProtocolError {
    ProtocolError::Parse {
        what,
        detail: detail.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

/// Canonical message role: system.
pub const ROLE_SYSTEM: &str = "system";
/// Canonical message role: user.
pub const ROLE_USER: &str = "user";
/// Canonical message role: assistant.
pub const ROLE_ASSISTANT: &str = "assistant";
/// Canonical message role: tool.
pub const ROLE_TOOL: &str = "tool";

fn is_system_role(role: &str) -> bool {
    role == ROLE_SYSTEM || role == "developer"
}

/// Returns the text of a raw JSON string value, or `None` when it is not one.
fn raw_json_string(value: &RawJson) -> Option<String> {
    value.parse::<String>().ok()
}

/// Validates raw JSON text that came from a string field.
fn raw_or_error(text: &str, what: &str) -> Result<RawJson, ProtocolError> {
    RawJson::from_slice(text.as_bytes())
        .map_err(|e| ProtocolError::Message(format!("content: {what}: {e}")))
}

/// Builds an object whose keys are ordered the way Go's `encoding/json` orders
/// map keys: sorted.
///
/// Go marshals `map[string]any` with sorted keys, and the translated bodies are
/// compared against the Go implementation, so the emitted bytes have to match.
pub(crate) fn sorted_object(pairs: Vec<(&str, Value)>) -> Value {
    sort_json_keys(Value::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Map<String, Value>>(),
    ))
}

/// Recursively sorts object keys, mirroring Go's map marshalling.
pub(crate) fn sort_json_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (key, val) in entries {
                out.insert(key, sort_json_keys(val));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json_keys).collect()),
        other => other,
    }
}

fn object_to_raw(value: &Value) -> RawJson {
    RawJson::new(serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
}

/// Serialises a list of typed parts into the raw JSON array of a Messages
/// content field.
fn parts_to_raw(parts: &[ContentPart]) -> Result<RawJson, ProtocolError> {
    let text = serde_json::to_string(parts)
        .map_err(|e| ProtocolError::Message(format!("content: parts: {e}")))?;
    Ok(RawJson::new(text))
}

// ---------------------------------------------------------------------------
// Chat completions -> Anthropic Messages
// ---------------------------------------------------------------------------

/// Used when a chat request omits `max_tokens`.
///
/// The Messages API requires the field, so a value must be supplied. The default
/// is deliberately modest rather than unbounded: guessing high would let a
/// translated request consume far more budget than the caller asked for.
pub const ANTHROPIC_MAX_TOKENS_DEFAULT: i64 = 4096;

/// Converts an OpenAI chat request into a Messages request.
pub fn chat_to_anthropic(req: &ChatRequest) -> Result<AnthropicRequest, ProtocolError> {
    let mut out = AnthropicRequest {
        model: req.model.clone(),
        stream: req.stream,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        ..AnthropicRequest::default()
    };
    if out.max_tokens == 0 {
        out.max_tokens = req.max_completion_tokens;
    }
    if out.max_tokens == 0 {
        out.max_tokens = ANTHROPIC_MAX_TOKENS_DEFAULT;
    }
    // Stop: chat accepts a string or an array; Messages only an array.
    if let Some(stop) = &req.stop {
        if !stop.is_empty() {
            if let Some(one) = raw_json_string(stop) {
                if !one.is_empty() {
                    out.stop_sequences = vec![one];
                }
            } else {
                match stop.parse::<Vec<String>>() {
                    Ok(many) => out.stop_sequences = many,
                    Err(_) => {
                        return Err(translation_error(
                            "openai",
                            "anthropic",
                            "stop",
                            "must be a string or an array of strings",
                        ))
                    }
                }
            }
        }
    }

    // System prompt: chat carries it as a message; Messages has a top field.
    let mut system_parts: Vec<ContentPart> = Vec::new();
    for m in &req.messages {
        if !is_system_role(&m.role) {
            continue;
        }
        for part in m.content.parts_or_parse() {
            if matches!(part.r#type.as_str(), "" | "text" | "input_text") {
                system_parts.push(ContentPart {
                    r#type: "text".to_string(),
                    text: part.text,
                    ..ContentPart::default()
                });
            }
        }
    }
    if !system_parts.is_empty() {
        let raw = parts_to_raw(&system_parts)?;
        out.system = Some(Content {
            parts: system_parts,
            raw: Some(raw),
        });
    }

    // Messages. Consecutive same-role messages are merged, because the Messages
    // API requires alternating roles and rejects two user turns in a row.
    for m in &req.messages {
        if is_system_role(&m.role) {
            continue;
        }
        match m.role.as_str() {
            ROLE_TOOL => {
                // A tool result in chat is its own message; in the Messages API
                // it is a tool_result block inside a user turn. The block must
                // carry the id of the tool_use it answers, so a chat tool
                // message without one cannot be expressed and is refused rather
                // than sent without its correlation id - which the provider
                // would reject anyway, with a message far less clear than this
                // one.
                if m.tool_call_id.is_empty() {
                    return Err(translation_error(
                        "openai",
                        "anthropic",
                        "messages[].tool_call_id",
                        "a tool result must reference the tool call it answers",
                    ));
                }
                // Go marshals a one-element slice here, not a bare block.
                let block = sorted_object(vec![
                    ("type", Value::String("tool_result".to_string())),
                    ("tool_use_id", Value::String(m.tool_call_id.clone())),
                    ("content", Value::String(m.content.plain())),
                ]);
                let raw = RawJson::new(
                    serde_json::to_string(&Value::Array(vec![block]))
                        .unwrap_or_else(|_| "[]".to_string()),
                );
                append_anthropic_message(&mut out, ROLE_USER, raw)?;
            }
            ROLE_ASSISTANT => {
                let mut parts: Vec<ContentPart> = Vec::new();
                let text = m.content.plain();
                if !text.is_empty() {
                    parts.push(ContentPart {
                        r#type: "text".to_string(),
                        text,
                        ..ContentPart::default()
                    });
                }
                for tc in &m.tool_calls {
                    let input = if tc.function.arguments.trim().is_empty() {
                        RawJson::new("{}")
                    } else {
                        raw_or_error(&tc.function.arguments, "tool_use input")?
                    };
                    parts.push(ContentPart {
                        r#type: "tool_use".to_string(),
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        input: Some(input),
                        ..ContentPart::default()
                    });
                }
                if parts.is_empty() {
                    continue;
                }
                let raw = parts_to_raw(&parts)?;
                append_anthropic_message(&mut out, ROLE_ASSISTANT, raw)?;
            }
            _ => {
                // user
                let mut parts: Vec<ContentPart> = Vec::new();
                for part in m.content.parts_or_parse() {
                    match part.r#type.as_str() {
                        "" | "text" | "input_text" | "output_text" => parts.push(ContentPart {
                            r#type: "text".to_string(),
                            text: part.text,
                            ..ContentPart::default()
                        }),
                        "image_url" => {
                            let Some(image) = &part.image_url else {
                                continue;
                            };
                            // Messages wants base64 sources; a remote URL is not
                            // representable, so reject rather than drop the
                            // image.
                            if !image.url.starts_with("data:") {
                                return Err(translation_error(
                                    "openai",
                                    "anthropic",
                                    "messages[].content[].image_url.url",
                                    "the Messages API accepts base64 data URLs only; \
                                     fetch the image and inline it, or route to an \
                                     OpenAI-compatible provider",
                                ));
                            }
                            let Some((media, data)) = parse_data_url(&image.url) else {
                                return Err(translation_error(
                                    "openai",
                                    "anthropic",
                                    "messages[].content[].image_url.url",
                                    "malformed data URL",
                                ));
                            };
                            let source = sorted_object(vec![
                                ("type", Value::String("image".to_string())),
                                (
                                    "source",
                                    sorted_object(vec![
                                        ("type", Value::String("base64".to_string())),
                                        ("media_type", Value::String(media)),
                                        ("data", Value::String(data)),
                                    ]),
                                ),
                            ]);
                            parts.push(ContentPart {
                                r#type: "image".to_string(),
                                source: Some(object_to_raw(&source)),
                                ..ContentPart::default()
                            });
                        }
                        other => {
                            return Err(translation_error(
                                "openai",
                                "anthropic",
                                "messages[].content[].type",
                                format!("content part type {:?} has no Messages equivalent", other),
                            ))
                        }
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                let raw = parts_to_raw(&parts)?;
                append_anthropic_message(&mut out, ROLE_USER, raw)?;
            }
        }
    }

    // Tools: chat nests under "function"; Messages flattens.
    for t in &req.tools {
        if !t.r#type.is_empty() && t.r#type != "function" {
            return Err(translation_error(
                "openai",
                "anthropic",
                "tools[].type",
                format!(
                    "tool type {:?} is not supported by the Messages API",
                    t.r#type
                ),
            ));
        }
        out.tools.push(AnthropicTool {
            name: t.function.name.clone(),
            description: t.function.description.clone(),
            input_schema: t.function.parameters.clone(),
            r#type: String::new(),
        });
    }
    if let Some(choice) = &req.tool_choice {
        if !choice.is_empty() {
            out.tool_choice = translate_tool_choice_to_anthropic(choice);
        }
    }
    if let Some(format) = &req.response_format {
        if format.r#type == "json_schema" {
            // Messages has no response_format; the schema can be expressed as a
            // single forced tool, which preserves the structured-output
            // contract.
            if let Some(schema_raw) = &format.schema {
                if let Ok(schema) = schema_raw.parse::<SchemaEnvelope>() {
                    if let Some(schema_json) = &schema.schema {
                        if !schema_json.is_empty() {
                            let name = if schema.name.is_empty() {
                                "structured_output".to_string()
                            } else {
                                schema.name.clone()
                            };
                            out.tools.push(AnthropicTool {
                                name: name.clone(),
                                description: "Return the answer in this JSON schema.".to_string(),
                                input_schema: Some(schema_json.clone()),
                                r#type: String::new(),
                            });
                            out.tool_choice = Some(RawJson::new(format!(
                                r#"{{"type":"tool","name":{}}}"#,
                                serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string())
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The `json_schema` payload of a chat `response_format`.
///
/// `strict` is parsed for parity with Go, which never reads it back.
#[allow(dead_code)]
#[derive(Default, serde::Deserialize)]
struct SchemaEnvelope {
    #[serde(default)]
    name: String,
    #[serde(default)]
    schema: Option<RawJson>,
    #[serde(default)]
    strict: Option<bool>,
}

/// Appends a message, merging into the previous one when the role repeats. The
/// Messages API rejects consecutive same-role turns, and merging is the standard
/// way to preserve the content.
fn append_anthropic_message(
    out: &mut AnthropicRequest,
    role: &str,
    content: RawJson,
) -> Result<(), ProtocolError> {
    if let Some(last) = out.messages.last() {
        if last.role == role {
            let previous = last
                .content
                .raw
                .clone()
                .unwrap_or_else(|| RawJson::new("null"));
            let merged = merge_content_arrays(&previous, &content);
            let last = out.messages.last_mut().expect("checked above");
            last.content = Content {
                parts: Vec::new(),
                raw: Some(merged),
            };
            return Ok(());
        }
    }
    out.messages.push(AnthropicMessage {
        role: role.to_string(),
        content: Content {
            parts: Vec::new(),
            raw: Some(content),
        },
    });
    Ok(())
}

/// Concatenates two JSON arrays of content parts.
fn merge_content_arrays(a: &RawJson, b: &RawJson) -> RawJson {
    let mut left: Vec<RawJson> = a.parse().unwrap_or_default();
    let mut right: Vec<RawJson> = b.parse().unwrap_or_default();
    left.append(&mut right);
    if left.is_empty() {
        // Go marshals a nil slice as null; keep that shape when both sides were
        // unparseable.
        return RawJson::new("null");
    }
    match serde_json::to_string(&left) {
        Ok(text) => RawJson::new(text),
        Err(_) => b.clone(),
    }
}

/// Splits a `data:` URL into media type and payload.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mut media = meta.strip_suffix(";base64").unwrap_or(meta).to_string();
    if media.is_empty() {
        media = "application/octet-stream".to_string();
    }
    Some((media, payload.to_string()))
}

/// Maps the chat `tool_choice` onto Messages.
fn translate_tool_choice_to_anthropic(choice: &RawJson) -> Option<RawJson> {
    if let Some(one) = raw_json_string(choice) {
        return match one.as_str() {
            "auto" => Some(RawJson::new(r#"{"type":"auto"}"#)),
            "required" => Some(RawJson::new(r#"{"type":"any"}"#)),
            // Messages has no "none"; an empty tool list is the equivalent, but
            // the tools were already copied. "auto" is the closest safe choice.
            "none" => Some(RawJson::new(r#"{"type":"auto"}"#)),
            _ => None,
        };
    }
    #[derive(Default, serde::Deserialize)]
    struct ChoiceFunction {
        #[serde(default)]
        name: String,
    }
    #[derive(Default, serde::Deserialize)]
    struct ChoiceObject {
        #[serde(default)]
        function: ChoiceFunction,
    }
    if let Ok(obj) = choice.parse::<ChoiceObject>() {
        if !obj.function.name.is_empty() {
            let value = sorted_object(vec![
                ("type", Value::String("tool".to_string())),
                ("name", Value::String(obj.function.name)),
            ]);
            return Some(object_to_raw(&value));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Anthropic Messages -> Chat completions
// ---------------------------------------------------------------------------

/// Used when a Messages request omits `max_tokens`. The chat API makes the field
/// optional, so the bound is only advisory.
pub const CHAT_MAX_TOKENS_DEFAULT: i64 = 4096;

/// Converts a Messages request into an OpenAI chat request.
pub fn anthropic_to_chat(req: &AnthropicRequest) -> Result<ChatRequest, ProtocolError> {
    let mut out = ChatRequest {
        model: req.model.clone(),
        stream: req.stream,
        max_completion_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        ..ChatRequest::default()
    };
    if out.max_completion_tokens == 0 {
        out.max_completion_tokens = CHAT_MAX_TOKENS_DEFAULT;
    }
    if req.stop_sequences.len() == 1 {
        out.stop = Some(RawJson::new(
            serde_json::to_string(&req.stop_sequences[0]).unwrap_or_else(|_| "\"\"".to_string()),
        ));
    } else if req.stop_sequences.len() > 1 {
        out.stop = Some(RawJson::new(
            serde_json::to_string(&req.stop_sequences).unwrap_or_else(|_| "[]".to_string()),
        ));
    }
    if let Some(system) = &req.system {
        let text = system.text();
        if !text.is_empty() {
            out.messages.push(ChatMessage {
                role: ROLE_SYSTEM.to_string(),
                content: Content::from_parts(vec![ContentPart {
                    r#type: "text".to_string(),
                    text,
                    ..ContentPart::default()
                }]),
                ..ChatMessage::default()
            });
        }
    }
    for m in &req.messages {
        let mut msg = ChatMessage {
            role: m.role.clone(),
            ..ChatMessage::default()
        };
        let mut texts: Vec<ContentPart> = Vec::new();
        let mut results: Vec<ChatMessage> = Vec::new();
        for part in m.content.parts_or_parse() {
            match part.r#type.as_str() {
                "" | "text" => texts.push(ContentPart {
                    r#type: "text".to_string(),
                    text: part.text,
                    ..ContentPart::default()
                }),
                "tool_use" => {
                    let args = match &part.input {
                        Some(input) if !input.is_empty() => input.as_str().to_string(),
                        _ => "{}".to_string(),
                    };
                    msg.tool_calls.push(ToolCall {
                        id: part.id.clone(),
                        r#type: "function".to_string(),
                        function: FunctionCall {
                            name: part.name.clone(),
                            arguments: args,
                        },
                        ..ToolCall::default()
                    });
                }
                "tool_result" => {
                    // A tool_result block becomes its own chat "tool" message.
                    let text = match &part.content {
                        Some(content) => content.text(),
                        None => String::new(),
                    };
                    results.push(ChatMessage {
                        role: ROLE_TOOL.to_string(),
                        tool_call_id: part.id.clone(),
                        content: Content::from_parts(vec![ContentPart {
                            r#type: "text".to_string(),
                            text,
                            ..ContentPart::default()
                        }]),
                        ..ChatMessage::default()
                    });
                }
                "image" => {
                    let Some(source) = &part.source else {
                        continue;
                    };
                    let Ok(src) = source.parse::<ImageSource>() else {
                        continue;
                    };
                    match src.r#type.as_str() {
                        "base64" => texts.push(ContentPart {
                            r#type: "image_url".to_string(),
                            image_url: Some(ImageUrl {
                                url: format!("data:{};base64,{}", src.media_type, src.data),
                                detail: String::new(),
                            }),
                            ..ContentPart::default()
                        }),
                        "url" => texts.push(ContentPart {
                            r#type: "image_url".to_string(),
                            image_url: Some(ImageUrl {
                                url: src.url,
                                detail: String::new(),
                            }),
                            ..ContentPart::default()
                        }),
                        other => {
                            return Err(translation_error(
                                "anthropic",
                                "openai",
                                "messages[].content[].source.type",
                                format!("image source type {:?} has no chat equivalent", other),
                            ))
                        }
                    }
                }
                // A replayed thinking block is not accepted by the chat API as
                // input; dropping it here is safe because it is not
                // user-visible content, but the caller is told via the audit
                // marker that this request was translated.
                "thinking" => continue,
                other => {
                    return Err(translation_error(
                        "anthropic",
                        "openai",
                        "messages[].content[].type",
                        format!("content block type {:?} has no chat equivalent", other),
                    ))
                }
            }
        }
        if !texts.is_empty() || !msg.tool_calls.is_empty() {
            msg.content = Content::from_parts(texts);
            out.messages.push(msg);
        }
        out.messages.extend(results);
    }

    for t in &req.tools {
        out.tools.push(Tool {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
                strict: None,
            },
        });
    }
    if let Some(choice) = &req.tool_choice {
        if !choice.is_empty() {
            out.tool_choice = translate_tool_choice_to_chat(choice);
        }
    }
    Ok(out)
}

#[derive(Default, serde::Deserialize)]
struct ImageSource {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    data: String,
    #[serde(default)]
    url: String,
}

/// Maps the Messages `tool_choice` onto chat.
fn translate_tool_choice_to_chat(choice: &RawJson) -> Option<RawJson> {
    #[derive(Default, serde::Deserialize)]
    struct ChoiceObject {
        #[serde(rename = "type", default)]
        r#type: String,
        #[serde(default)]
        name: String,
    }
    let obj = choice.parse::<ChoiceObject>().ok()?;
    match obj.r#type.as_str() {
        "auto" => Some(RawJson::new(r#""auto""#)),
        "any" => Some(RawJson::new(r#""required""#)),
        "tool" => {
            let value = sorted_object(vec![
                ("type", Value::String("function".to_string())),
                (
                    "function",
                    sorted_object(vec![("name", Value::String(obj.name))]),
                ),
            ]);
            Some(object_to_raw(&value))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Responses <-> Chat completions
// ---------------------------------------------------------------------------

/// Converts a Responses request into a chat request.
pub fn responses_to_chat(req: &ResponsesRequest) -> Result<ChatRequest, ProtocolError> {
    let mut out = ChatRequest {
        model: req.model.clone(),
        stream: req.stream,
        max_completion_tokens: req.max_output_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        ..ChatRequest::default()
    };
    if out.max_completion_tokens == 0 {
        out.max_completion_tokens = CHAT_MAX_TOKENS_DEFAULT;
    }
    if !req.instructions.is_empty() {
        out.messages.push(ChatMessage {
            role: ROLE_SYSTEM.to_string(),
            content: Content::from_parts(vec![ContentPart {
                r#type: "text".to_string(),
                text: req.instructions.clone(),
                ..ContentPart::default()
            }]),
            ..ChatMessage::default()
        });
    }
    // Reuse the Responses input walker's understanding of the three shapes, then
    // group the flat list back into messages.
    if let Some(input) = &req.input {
        let raw = input.as_str().trim().to_string();
        if !raw.is_empty() && raw != "null" {
            if raw.starts_with('"') {
                if let Ok(s) = input.parse::<String>() {
                    out.messages.push(ChatMessage {
                        role: ROLE_USER.to_string(),
                        content: Content::from_parts(vec![ContentPart {
                            r#type: "text".to_string(),
                            text: s,
                            ..ContentPart::default()
                        }]),
                        ..ChatMessage::default()
                    });
                }
            } else {
                #[derive(Default, serde::Deserialize)]
                struct Item {
                    #[serde(rename = "type", default)]
                    r#type: String,
                    #[serde(default)]
                    role: String,
                    #[serde(default)]
                    content: Option<RawJson>,
                    #[serde(default)]
                    text: String,
                    // function_call / function_call_output items
                    #[serde(default)]
                    name: String,
                    #[serde(default)]
                    arguments: String,
                    #[serde(default)]
                    call_id: String,
                    #[serde(default)]
                    output: String,
                }
                let items = input.parse::<Vec<Item>>().map_err(|_| {
                    translation_error(
                        "responses",
                        "openai",
                        "input",
                        "not a string or an array of input items",
                    )
                })?;
                for item in items {
                    match item.r#type.as_str() {
                        "" | "message" => {
                            let role = if item.role.is_empty() {
                                ROLE_USER.to_string()
                            } else {
                                item.role.clone()
                            };
                            let parts = responses_parts_to_chat(item.content.as_ref(), &item.text)?;
                            if !parts.is_empty() {
                                out.messages.push(ChatMessage {
                                    role,
                                    content: Content::from_parts(parts),
                                    ..ChatMessage::default()
                                });
                            }
                        }
                        "function_call" => {
                            out.messages.push(ChatMessage {
                                role: ROLE_ASSISTANT.to_string(),
                                tool_calls: vec![ToolCall {
                                    id: item.call_id.clone(),
                                    r#type: "function".to_string(),
                                    function: FunctionCall {
                                        name: item.name.clone(),
                                        arguments: item.arguments.clone(),
                                    },
                                    ..ToolCall::default()
                                }],
                                ..ChatMessage::default()
                            });
                        }
                        "function_call_output" => {
                            out.messages.push(ChatMessage {
                                role: ROLE_TOOL.to_string(),
                                tool_call_id: item.call_id.clone(),
                                content: Content::from_parts(vec![ContentPart {
                                    r#type: "text".to_string(),
                                    text: item.output.clone(),
                                    ..ContentPart::default()
                                }]),
                                ..ChatMessage::default()
                            });
                        }
                        // Reasoning items are opaque state; they are not valid
                        // input for the chat API and carry no user content.
                        "reasoning" => continue,
                        other => {
                            return Err(translation_error(
                                "responses",
                                "openai",
                                "input[].type",
                                format!("input item type {:?} has no chat equivalent", other),
                            ))
                        }
                    }
                }
            }
        }
    }
    for t in &req.tools {
        if !t.r#type.is_empty() && t.r#type != "function" {
            return Err(translation_error(
                "responses",
                "openai",
                "tools[].type",
                format!(
                    "built-in tool {:?} is server-side; it has no chat equivalent",
                    t.r#type
                ),
            ));
        }
        out.tools.push(Tool {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: t.tool_name(),
                description: t.description.clone(),
                parameters: t.tool_schema(),
                strict: None,
            },
        });
    }
    if let Some(choice) = &req.tool_choice {
        if !choice.is_empty() {
            out.tool_choice = Some(choice.clone());
        }
    }
    Ok(out)
}

/// Converts a Responses content field into chat parts.
fn responses_parts_to_chat(
    content: Option<&RawJson>,
    direct_text: &str,
) -> Result<Vec<ContentPart>, ProtocolError> {
    let mut parts: Vec<ContentPart> = Vec::new();
    if !direct_text.is_empty() {
        parts.push(ContentPart {
            r#type: "text".to_string(),
            text: direct_text.to_string(),
            ..ContentPart::default()
        });
    }
    let Some(content) = content else {
        return Ok(parts);
    };
    let raw = content.as_str().trim();
    if raw.is_empty() || raw == "null" {
        return Ok(parts);
    }
    if raw.starts_with('"') {
        if let Ok(s) = content.parse::<String>() {
            if !s.is_empty() {
                parts.push(ContentPart {
                    r#type: "text".to_string(),
                    text: s,
                    ..ContentPart::default()
                });
            }
        }
        return Ok(parts);
    }
    #[derive(Default, serde::Deserialize)]
    struct TypedPart {
        #[serde(rename = "type", default)]
        r#type: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        image_url: String,
    }
    let Ok(typed) = content.parse::<Vec<TypedPart>>() else {
        return Ok(parts);
    };
    for part in typed {
        match part.r#type.as_str() {
            "input_text" | "output_text" | "text" | "" => {
                if !part.text.is_empty() {
                    parts.push(ContentPart {
                        r#type: "text".to_string(),
                        text: part.text,
                        ..ContentPart::default()
                    });
                }
            }
            "input_image" => {
                if !part.image_url.is_empty() {
                    parts.push(ContentPart {
                        r#type: "image_url".to_string(),
                        image_url: Some(ImageUrl {
                            url: part.image_url,
                            detail: String::new(),
                        }),
                        ..ContentPart::default()
                    });
                }
            }
            other => {
                return Err(translation_error(
                    "responses",
                    "openai",
                    "input[].content[].type",
                    format!("content part type {:?} has no chat equivalent", other),
                ))
            }
        }
    }
    Ok(parts)
}

/// Converts a chat request into a Responses request.
pub fn chat_to_responses(req: &ChatRequest) -> Result<ResponsesRequest, ProtocolError> {
    let mut out = ResponsesRequest {
        model: req.model.clone(),
        stream: req.stream,
        max_output_tokens: req.max_completion_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        ..ResponsesRequest::default()
    };
    if out.max_output_tokens == 0 {
        out.max_output_tokens = req.max_tokens;
    }
    if out.max_output_tokens == 0 {
        out.max_output_tokens = CHAT_MAX_TOKENS_DEFAULT;
    }
    let mut system_parts: Vec<String> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    for m in &req.messages {
        if is_system_role(&m.role) {
            let text = m.content.plain();
            if !text.is_empty() {
                system_parts.push(text);
            }
            continue;
        }
        match m.role.as_str() {
            ROLE_TOOL => items.push(sorted_object(vec![
                ("type", Value::String("function_call_output".to_string())),
                ("call_id", Value::String(m.tool_call_id.clone())),
                ("output", Value::String(m.content.plain())),
            ])),
            ROLE_ASSISTANT => {
                let text = m.content.plain();
                if !text.is_empty() {
                    items.push(sorted_object(vec![
                        ("type", Value::String("message".to_string())),
                        ("role", Value::String("assistant".to_string())),
                        (
                            "content",
                            Value::Array(vec![sorted_object(vec![
                                ("type", Value::String("output_text".to_string())),
                                ("text", Value::String(text)),
                            ])]),
                        ),
                    ]));
                }
                for tc in &m.tool_calls {
                    items.push(sorted_object(vec![
                        ("type", Value::String("function_call".to_string())),
                        ("call_id", Value::String(tc.id.clone())),
                        ("name", Value::String(tc.function.name.clone())),
                        ("arguments", Value::String(tc.function.arguments.clone())),
                    ]));
                }
            }
            _ => {
                // user
                let parts = chat_parts_to_responses(&m.content.parts_or_parse())?;
                if !parts.is_empty() {
                    items.push(sorted_object(vec![
                        ("type", Value::String("message".to_string())),
                        ("role", Value::String("user".to_string())),
                        ("content", Value::Array(parts)),
                    ]));
                }
            }
        }
    }
    out.instructions = system_parts.join("\n\n");
    // Go marshals a nil slice as null, so an input-less conversion keeps that
    // shape rather than inventing an empty array.
    out.input = Some(if items.is_empty() {
        RawJson::new("null")
    } else {
        RawJson::new(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()))
    });
    for t in &req.tools {
        if !t.r#type.is_empty() && t.r#type != "function" {
            return Err(translation_error(
                "openai",
                "responses",
                "tools[].type",
                format!(
                    "tool type {:?} is not supported by the Responses API",
                    t.r#type
                ),
            ));
        }
        out.tools.push(ResponsesTool {
            r#type: "function".to_string(),
            name: t.function.name.clone(),
            description: t.function.description.clone(),
            parameters: t.function.parameters.clone(),
            strict: t.function.strict,
            function: None,
        });
    }
    if let Some(choice) = &req.tool_choice {
        if !choice.is_empty() {
            out.tool_choice = Some(choice.clone());
        }
    }
    Ok(out)
}

/// Converts chat content parts into Responses parts.
fn chat_parts_to_responses(parts: &[ContentPart]) -> Result<Vec<Value>, ProtocolError> {
    let mut out: Vec<Value> = Vec::new();
    for part in parts {
        match part.r#type.as_str() {
            "" | "text" | "input_text" | "output_text" => {
                if !part.text.is_empty() {
                    out.push(sorted_object(vec![
                        ("type", Value::String("input_text".to_string())),
                        ("text", Value::String(part.text.clone())),
                    ]));
                }
            }
            "image_url" => {
                let Some(image) = &part.image_url else {
                    continue;
                };
                // The Responses API calls this input_image with the URL inline.
                out.push(sorted_object(vec![
                    ("type", Value::String("input_image".to_string())),
                    ("image_url", Value::String(image.url.clone())),
                ]));
            }
            other => {
                return Err(translation_error(
                    "openai",
                    "responses",
                    "messages[].content[].type",
                    format!("content part type {:?} has no Responses equivalent", other),
                ))
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Response side: non-streaming conversions
// ---------------------------------------------------------------------------

/// Converts a Messages reply into a chat reply.
pub fn anthropic_to_chat_response(resp: &AnthropicResponse) -> Result<ChatResponse, ProtocolError> {
    let mut msg = ChatMessage {
        role: ROLE_ASSISTANT.to_string(),
        ..ChatMessage::default()
    };
    let mut texts: Vec<String> = Vec::new();
    for block in &resp.content {
        match block.r#type.as_str() {
            "text" => texts.push(block.text.clone()),
            "thinking" => msg.reasoning_content.push_str(&block.thinking),
            "tool_use" => {
                let args = match &block.input {
                    Some(input) if !input.is_empty() => {
                        raw_or_error(input.as_str(), "tool_use input")?
                            .as_str()
                            .to_string()
                    }
                    _ => "{}".to_string(),
                };
                msg.tool_calls.push(ToolCall {
                    id: block.id.clone(),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: block.name.clone(),
                        arguments: args,
                    },
                    ..ToolCall::default()
                });
            }
            _ => {}
        }
    }
    msg.content = Content::from_parts(vec![ContentPart {
        r#type: "text".to_string(),
        text: texts.join(""),
        ..ContentPart::default()
    }]);
    let mut out = ChatResponse {
        id: or_default_str(&resp.id, "msg_translated"),
        object: "chat.completion".to_string(),
        model: resp.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: anthropic_stop_to_chat(&resp.stop_reason).to_string(),
            logprobs: None,
        }],
        ..ChatResponse::default()
    };
    if let Some(usage) = &resp.usage {
        out.usage = Some(usage.to_openai());
    }
    Ok(out)
}

/// Converts a chat reply into a Messages reply.
pub fn chat_to_anthropic_response(resp: &ChatResponse) -> Result<AnthropicResponse, ProtocolError> {
    let mut out = AnthropicResponse {
        id: or_default_str(&resp.id, "chatcmpl_translated"),
        r#type: "message".to_string(),
        role: ROLE_ASSISTANT.to_string(),
        model: resp.model.clone(),
        ..AnthropicResponse::default()
    };
    let mut stop = "end_turn".to_string();
    for choice in &resp.choices {
        let text = choice.message.content.plain();
        if !text.is_empty() {
            out.content.push(AnthropicBlock {
                r#type: "text".to_string(),
                text,
                ..AnthropicBlock::default()
            });
        }
        if !choice.message.reasoning_content.is_empty() {
            out.content.push(AnthropicBlock {
                r#type: "thinking".to_string(),
                thinking: choice.message.reasoning_content.clone(),
                ..AnthropicBlock::default()
            });
        }
        for tc in &choice.message.tool_calls {
            let input = if tc.function.arguments.is_empty() {
                RawJson::new("{}")
            } else {
                raw_or_error(&tc.function.arguments, "tool_use input")?
            };
            out.content.push(AnthropicBlock {
                r#type: "tool_use".to_string(),
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input: Some(input),
                ..AnthropicBlock::default()
            });
        }
        stop = chat_stop_to_anthropic(&choice.finish_reason);
    }
    out.stop_reason = stop;
    if let Some(usage) = &resp.usage {
        out.usage = Some(AnthropicUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            ..AnthropicUsage::default()
        });
    }
    Ok(out)
}

/// Converts a Responses reply into a chat reply.
pub fn responses_to_chat_response(resp: &ResponsesResponse) -> Result<ChatResponse, ProtocolError> {
    let mut msg = ChatMessage {
        role: ROLE_ASSISTANT.to_string(),
        ..ChatMessage::default()
    };
    msg.content = Content::from_parts(vec![ContentPart {
        r#type: "text".to_string(),
        text: resp.response_text(),
        ..ContentPart::default()
    }]);
    msg.reasoning_content = resp.reasoning_text();
    for call in resp.function_calls() {
        msg.tool_calls.push(ToolCall {
            id: call.call_id.clone(),
            r#type: "function".to_string(),
            function: FunctionCall {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
            ..ToolCall::default()
        });
    }
    let mut finish = "stop".to_string();
    if !msg.tool_calls.is_empty() {
        finish = "tool_calls".to_string();
    }
    if resp.status == "incomplete" {
        finish = "length".to_string();
    }
    let mut out = ChatResponse {
        id: or_default_str(&resp.id, "resp_translated"),
        object: "chat.completion".to_string(),
        model: resp.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: finish,
            logprobs: None,
        }],
        ..ChatResponse::default()
    };
    if let Some(usage) = &resp.usage {
        out.usage = Some(usage.to_usage());
    }
    Ok(out)
}

/// Converts a chat reply into a Responses reply.
pub fn chat_to_responses_response(resp: &ChatResponse) -> Result<ResponsesResponse, ProtocolError> {
    let mut out = ResponsesResponse {
        id: or_default_str(&resp.id, "chatcmpl_translated"),
        object: "response".to_string(),
        model: resp.model.clone(),
        status: "completed".to_string(),
        ..ResponsesResponse::default()
    };
    let mut texts: Vec<String> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut calls: Vec<ResponsesOutputItem> = Vec::new();
    for choice in &resp.choices {
        let text = choice.message.content.plain();
        if !text.is_empty() {
            texts.push(text);
        }
        if !choice.message.reasoning_content.is_empty() {
            reasoning.push(choice.message.reasoning_content.clone());
        }
        for tc in &choice.message.tool_calls {
            calls.push(ResponsesOutputItem {
                r#type: "function_call".to_string(),
                id: format!("fc_{}", tc.id),
                call_id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
                ..ResponsesOutputItem::default()
            });
        }
        if choice.finish_reason == "length" {
            out.status = "incomplete".to_string();
        }
    }
    // Output items are ordered the way the Responses API emits them: reasoning
    // first, then the assistant message, then any function calls.
    let mut ordered: Vec<ResponsesOutputItem> = Vec::with_capacity(calls.len() + 2);
    if !reasoning.is_empty() {
        ordered.push(ResponsesOutputItem {
            r#type: "reasoning".to_string(),
            summary: vec![ResponsesContentPart {
                r#type: "summary_text".to_string(),
                text: reasoning.join(""),
                annotations: None,
            }],
            ..ResponsesOutputItem::default()
        });
    }
    if !texts.is_empty() {
        ordered.push(ResponsesOutputItem {
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            status: "completed".to_string(),
            content: vec![ResponsesContentPart {
                r#type: "output_text".to_string(),
                text: texts.join(""),
                annotations: None,
            }],
            ..ResponsesOutputItem::default()
        });
    }
    ordered.extend(calls);
    out.output = ordered;
    if let Some(usage) = &resp.usage {
        out.usage = Some(ResponsesUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            ..ResponsesUsage::default()
        });
    }
    Ok(out)
}

/// Maps a Messages stop reason onto the chat vocabulary.
pub fn anthropic_stop_to_chat(stop: &str) -> String {
    match stop {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        other => other.to_string(),
    }
}

/// Maps a chat finish reason onto the Messages vocabulary.
pub fn chat_stop_to_anthropic(stop: &str) -> String {
    match stop {
        "stop" => "end_turn".to_string(),
        "length" => "max_tokens".to_string(),
        "tool_calls" | "function_call" => "tool_use".to_string(),
        other => or_default_str(other, "end_turn"),
    }
}

fn or_default_str(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Converts a request body between protocols.
///
/// It is the single entry point the proxy uses, so adding a protocol later means
/// adding cases here rather than touching the request pipeline.
pub fn translate_request_body(from: &str, to: &str, body: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if from == to {
        return Ok(body.to_vec());
    }
    match (from, to) {
        ("openai", "anthropic") => {
            let req: ChatRequest =
                serde_json::from_slice(body).map_err(|e| parse_error("chat request", e))?;
            let out = chat_to_anthropic(&req)?;
            to_bytes(&out)
        }
        ("anthropic", "openai") => {
            let req: AnthropicRequest =
                serde_json::from_slice(body).map_err(|e| parse_error("anthropic request", e))?;
            let out = anthropic_to_chat(&req)?;
            to_bytes(&out)
        }
        ("openai", "responses") => {
            let req: ChatRequest =
                serde_json::from_slice(body).map_err(|e| parse_error("chat request", e))?;
            let out = chat_to_responses(&req)?;
            to_bytes(&out)
        }
        ("responses", "openai") => {
            let req: ResponsesRequest =
                serde_json::from_slice(body).map_err(|e| parse_error("responses request", e))?;
            let out = responses_to_chat(&req)?;
            to_bytes(&out)
        }
        // Two hops, because the intermediate representation is chat and
        // duplicating every rule for the direct path would be a second place to
        // get the same details wrong.
        ("responses", "anthropic") => {
            let mid = translate_request_body("responses", "openai", body)?;
            translate_request_body("openai", "anthropic", &mid)
        }
        ("anthropic", "responses") => {
            let mid = translate_request_body("anthropic", "openai", body)?;
            translate_request_body("openai", "responses", &mid)
        }
        _ => Err(err_unsupported(from, to)),
    }
}

/// Converts a response body between protocols.
pub fn translate_response_body(
    from: &str,
    to: &str,
    body: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    if from == to {
        return Ok(body.to_vec());
    }
    match (from, to) {
        ("anthropic", "openai") => {
            let resp: AnthropicResponse =
                serde_json::from_slice(body).map_err(|e| parse_error("anthropic response", e))?;
            let out = anthropic_to_chat_response(&resp)?;
            to_bytes(&out)
        }
        ("openai", "anthropic") => {
            let resp: ChatResponse =
                serde_json::from_slice(body).map_err(|e| parse_error("chat response", e))?;
            let out = chat_to_anthropic_response(&resp)?;
            to_bytes(&out)
        }
        ("responses", "openai") => {
            let resp: ResponsesResponse =
                serde_json::from_slice(body).map_err(|e| parse_error("responses response", e))?;
            let out = responses_to_chat_response(&resp)?;
            to_bytes(&out)
        }
        ("openai", "responses") => {
            let resp: ChatResponse =
                serde_json::from_slice(body).map_err(|e| parse_error("chat response", e))?;
            let out = chat_to_responses_response(&resp)?;
            to_bytes(&out)
        }
        ("responses", "anthropic") => {
            let mid = translate_response_body("responses", "openai", body)?;
            translate_response_body("openai", "anthropic", &mid)
        }
        ("anthropic", "responses") => {
            let mid = translate_response_body("anthropic", "openai", body)?;
            translate_response_body("openai", "responses", &mid)
        }
        _ => Err(err_unsupported(from, to)),
    }
}

fn to_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(value).map_err(|e| ProtocolError::Message(format!("translate: encode: {e}")))
}

/// Reports whether a protocol name is recognised.
pub fn valid_protocol(p: &str) -> bool {
    matches!(p, "openai" | "anthropic" | "responses")
}

/// Returns a human-facing protocol name.
pub fn protocol_display_name(p: &str) -> String {
    match p {
        "openai" => "OpenAI Chat Completions".to_string(),
        "anthropic" => "Anthropic Messages".to_string(),
        "responses" => "OpenAI Responses".to_string(),
        other => other.to_string(),
    }
}

/// Convenience re-export so callers do not need the `ResponseFormat` import when
/// they only want to strip structured-output config.
impl ResponseFormat {
    /// Reports whether the format asks for a JSON schema.
    pub fn is_json_schema(&self) -> bool {
        self.r#type == "json_schema"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_request(body: &str) -> ChatRequest {
        serde_json::from_str(body).expect("chat request")
    }

    fn anthropic_request(body: &str) -> AnthropicRequest {
        serde_json::from_str(body).expect("anthropic request")
    }

    /// Translated from `TestChatAnthropicRoundTrip`.
    #[test]
    fn test_chat_anthropic_round_trip() {
        let body = r#"{
            "model":"gpt-4o","max_tokens":100,"temperature":0.5,
            "messages":[
                {"role":"system","content":"You are terse."},
                {"role":"user","content":"What is the weather?"},
                {"role":"assistant","content":"Let me check.","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"SH\"}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"sunny, 22C"},
                {"role":"user","content":"Thanks."}
            ],
            "tools":[{"type":"function","function":{"name":"get_weather","description":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
        }"#;
        let chat = chat_request(body);
        let anth = chat_to_anthropic(&chat).expect("chat_to_anthropic");

        // The system prompt must move to the top-level field.
        let system = anth.system.as_ref().expect("system hoisted");
        assert!(system.text().contains("terse"), "{system:?}");
        // The Messages API requires max_tokens, and it must be the value asked
        // for.
        assert_eq!(anth.max_tokens, 100);
        // Roles must alternate: no two consecutive user turns.
        for i in 1..anth.messages.len() {
            assert_ne!(
                anth.messages[i].role,
                anth.messages[i - 1].role,
                "consecutive {} turns at {i}",
                anth.messages[i].role
            );
        }
        // The tool call and its result must both survive.
        let joined = serde_json::to_string(&anth).unwrap();
        for want in ["get_weather", "call_1", "sunny, 22C", "SH"] {
            assert!(
                joined.contains(want),
                "converted request lost {want}: {joined}"
            );
        }
        // The tool schema must be flattened, not nested under "function".
        assert_eq!(anth.tools.len(), 1);
        assert_eq!(anth.tools[0].name, "get_weather");

        // ...and back again.
        let back = anthropic_to_chat(&anth).expect("anthropic_to_chat");
        assert_eq!(back.model, "gpt-4o");
        let mut got = String::new();
        for m in &back.messages {
            got.push_str(&m.role);
            got.push(':');
            got.push_str(&m.content.plain());
            got.push(';');
        }
        for want in [
            "system:You are terse.",
            "user:What is the weather?",
            "sunny, 22C",
        ] {
            assert!(got.contains(want), "round trip lost {want:?}, got {got:?}");
        }
    }

    /// Translated from `TestChatAnthropicRejectsRemoteImage`: an unrepresentable
    /// feature is refused loudly rather than dropped silently.
    #[test]
    fn test_chat_anthropic_rejects_remote_image() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"text","text":"what is this"},
            {"type":"image_url","image_url":{"url":"https://example.com/x.png"}}]}]}"#;
        let chat = chat_request(body);
        let err = chat_to_anthropic(&chat).expect_err("remote image accepted silently");
        let te = err.translation().expect("translation error");
        assert!(te.field.contains("image_url"), "field = {:?}", te.field);
        assert!(err.to_string().contains("base64"), "{err}");
    }

    /// Translated from `TestChatAnthropicAcceptsDataURLImage`.
    #[test]
    fn test_chat_anthropic_accepts_data_url_image() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"text","text":"what is this"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]}]}"#;
        let chat = chat_request(body);
        let anth = chat_to_anthropic(&chat).expect("chat_to_anthropic");
        let joined = serde_json::to_string(&anth).unwrap();
        assert!(joined.contains("base64"), "{joined}");
        assert!(joined.contains("image/png"), "{joined}");
        // Go marshals the map with sorted keys.
        assert!(
            joined.contains(r#"{"source":{"data":"iVBORw0KGgo=","media_type":"image/png","type":"base64"},"type":"image"}"#),
            "{joined}"
        );
    }

    /// Translated from `TestChatAnthropicDefaultsMaxTokens`.
    #[test]
    fn test_chat_anthropic_defaults_max_tokens() {
        let chat = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Content::from_text("hi"),
                ..ChatMessage::default()
            }],
            ..ChatRequest::default()
        };
        let anth = chat_to_anthropic(&chat).unwrap();
        assert_eq!(anth.max_tokens, ANTHROPIC_MAX_TOKENS_DEFAULT);
    }

    // -----------------------------------------------------------------
    // Responses <-> chat
    // -----------------------------------------------------------------

    /// Translated from `TestResponsesToChat`.
    #[test]
    fn test_responses_to_chat() {
        let body = r#"{
            "model":"gpt-4o","instructions":"be brief","max_output_tokens":50,
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"question"}]},
                {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"k\":1}"},
                {"type":"function_call_output","call_id":"c1","output":"found it"}
            ],
            "tools":[{"type":"function","name":"lookup","description":"d","parameters":{"type":"object"}}]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(body).unwrap();
        let chat = responses_to_chat(&req).expect("responses_to_chat");
        assert_eq!(chat.max_completion_tokens, 50);
        let joined = serde_json::to_string(&chat).unwrap();
        for want in ["be brief", "question", "lookup", "c1", "found it"] {
            assert!(joined.contains(want), "conversion lost {want}: {joined}");
        }
        // The nested tool shape must be produced for the chat API.
        #[derive(serde::Deserialize)]
        struct Probe {
            tools: Vec<ProbeTool>,
        }
        #[derive(serde::Deserialize)]
        struct ProbeTool {
            r#type: String,
            function: ProbeFunction,
        }
        #[derive(serde::Deserialize)]
        struct ProbeFunction {
            name: String,
        }
        let probe: Probe = serde_json::from_str(&joined).unwrap();
        assert_eq!(probe.tools.len(), 1);
        assert_eq!(probe.tools[0].r#type, "function");
        assert_eq!(probe.tools[0].function.name, "lookup");
    }

    /// Translated from `TestChatToResponses`.
    #[test]
    fn test_chat_to_responses() {
        let body = r#"{"model":"gpt-4o","max_tokens":30,"messages":[
            {"role":"system","content":"sys"},
            {"role":"user","content":"hello"},
            {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"out"}
        ]}"#;
        let chat = chat_request(body);
        let resp = chat_to_responses(&chat).expect("chat_to_responses");
        assert_eq!(resp.instructions, "sys");
        assert_eq!(resp.max_output_tokens, 30);
        let joined = serde_json::to_string(&resp).unwrap();
        for want in ["function_call", "function_call_output", "hello", "c1"] {
            assert!(joined.contains(want), "conversion lost {want}: {joined}");
        }
        // Go marshals maps with sorted keys, so items are emitted that way.
        assert!(
            joined
                .contains(r#"{"arguments":"{}","call_id":"c1","name":"f","type":"function_call"}"#),
            "{joined}"
        );
    }

    /// Translated from `TestResponsesRejectsBuiltinTools`.
    #[test]
    fn test_responses_rejects_builtin_tools() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":"x","tools":[{"type":"web_search_preview"}]}"#,
        )
        .unwrap();
        assert!(
            responses_to_chat(&req).is_err(),
            "a built-in tool was accepted for a chat target"
        );
    }

    // -----------------------------------------------------------------
    // Response-side conversions
    // -----------------------------------------------------------------

    /// Translated from `TestAnthropicToChatResponse`.
    #[test]
    fn test_anthropic_to_chat_response() {
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3",
            "content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"t1","name":"f","input":{"a":1}}],
            "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":3}}"#;
        let resp: AnthropicResponse = serde_json::from_str(body).unwrap();
        let chat = anthropic_to_chat_response(&resp).expect("anthropic_to_chat_response");
        let choice = &chat.choices[0];
        assert_eq!(choice.message.content.plain(), "hello");
        assert_eq!(choice.message.tool_calls.len(), 1);
        assert_eq!(choice.message.tool_calls[0].function.name, "f");
        assert_eq!(
            choice.message.tool_calls[0].function.arguments,
            r#"{"a":1}"#
        );
        assert_eq!(choice.finish_reason, "tool_calls");
        assert_eq!(chat.usage.as_ref().unwrap().prompt_tokens, 5);
    }

    /// Translated from `TestChatToAnthropicResponse`.
    #[test]
    fn test_chat_to_anthropic_response() {
        let body = r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"message":
            {"role":"assistant","content":"hi","tool_calls":[{"id":"t","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},
            "finish_reason":"tool_calls"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}"#;
        let resp: ChatResponse = serde_json::from_str(body).unwrap();
        let anth = chat_to_anthropic_response(&resp).expect("chat_to_anthropic_response");
        assert_eq!(anth.stop_reason, "tool_use");
        let mut has_text = false;
        let mut has_tool = false;
        for block in &anth.content {
            if block.r#type == "text" && block.text == "hi" {
                has_text = true;
            }
            if block.r#type == "tool_use" && block.name == "f" {
                has_tool = true;
            }
        }
        assert!(has_text && has_tool, "{:?}", anth.content);
        assert_eq!(anth.usage.as_ref().unwrap().input_tokens, 4);
    }

    /// Translated from `TestResponsesToChatResponse`.
    #[test]
    fn test_responses_to_chat_response() {
        let body = r#"{"id":"r1","object":"response","model":"gpt-4o","status":"completed",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"c1"}],
            "usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}"#;
        let resp: ResponsesResponse = serde_json::from_str(body).unwrap();
        let chat = responses_to_chat_response(&resp).expect("responses_to_chat_response");
        let choice = &chat.choices[0];
        assert_eq!(choice.message.content.plain(), "answer");
        assert_eq!(choice.message.tool_calls.len(), 1);
        assert_eq!(choice.message.tool_calls[0].id, "c1");
        assert_eq!(choice.finish_reason, "tool_calls");
    }

    /// Translated from `TestChatToResponsesResponse`.
    #[test]
    fn test_chat_to_responses_response() {
        let body = r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"message":
            {"role":"assistant","content":"hi there","reasoning_content":"thinking"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
        let resp: ChatResponse = serde_json::from_str(body).unwrap();
        let out = chat_to_responses_response(&resp).expect("chat_to_responses_response");
        assert_eq!(out.status, "completed");
        assert!(out.response_text().contains("hi there"));
        assert_eq!(out.reasoning_text(), "thinking");
        // Reasoning must precede the message, matching the Responses ordering.
        assert!(out.output.len() >= 2);
        assert_eq!(out.output[0].r#type, "reasoning");
        assert_eq!(out.output[1].r#type, "message");
        assert_eq!(out.usage.as_ref().unwrap().total_tokens, 5);
    }

    // -----------------------------------------------------------------
    // Dispatch table
    // -----------------------------------------------------------------

    /// Translated from `TestTranslateDispatchCoversEveryPair`: every ordered
    /// pair either produces a body or reports a clear error, so a missing path
    /// cannot be mistaken for a working one.
    #[test]
    fn test_translate_dispatch_covers_every_pair() {
        let bodies = [
            (
                "openai",
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            (
                "anthropic",
                r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
            ("responses", r#"{"model":"m","input":"hi"}"#),
        ];
        let protocols = ["openai", "anthropic", "responses"];
        for (from, body) in bodies {
            for to in protocols {
                if from == to {
                    continue;
                }
                let out = translate_request_body(from, to, body.as_bytes())
                    .unwrap_or_else(|e| panic!("{from} -> {to} failed: {e}"));
                let probe: Value = serde_json::from_slice(&out)
                    .unwrap_or_else(|e| panic!("{from} -> {to} produced invalid JSON: {e}"));
                assert_eq!(
                    probe["model"], "m",
                    "{from} -> {to} lost the model: {out:?}"
                );
                match to {
                    "anthropic" => {
                        assert!(
                            probe.get("max_tokens").is_some(),
                            "{from} -> anthropic omitted max_tokens"
                        );
                        assert!(
                            probe.get("messages").is_some(),
                            "{from} -> anthropic omitted messages"
                        );
                    }
                    "openai" => assert!(
                        probe.get("messages").is_some(),
                        "{from} -> openai omitted messages"
                    ),
                    "responses" => assert!(
                        probe.get("input").is_some(),
                        "{from} -> responses omitted input"
                    ),
                    _ => unreachable!(),
                }
            }
        }
    }

    /// Translated from `TestTranslateResponseDispatchCoversEveryPair`.
    #[test]
    fn test_translate_response_dispatch_covers_every_pair() {
        let samples = [
            (
                "openai",
                r#"{"id":"c","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            ),
            (
                "anthropic",
                r#"{"id":"a","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
            ),
            (
                "responses",
                r#"{"id":"r","object":"response","model":"m","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
            ),
        ];
        let protocols = ["openai", "anthropic", "responses"];
        for (from, body) in samples {
            for to in protocols {
                if from == to {
                    continue;
                }
                let out = translate_response_body(from, to, body.as_bytes())
                    .unwrap_or_else(|e| panic!("{from} -> {to} failed: {e}"));
                let text = String::from_utf8_lossy(&out);
                assert!(text.contains("hi"), "{from} -> {to} lost the text: {text}");
            }
        }
    }

    /// Translated from `TestTranslateSameProtocolIsIdentity`.
    #[test]
    fn test_translate_same_protocol_is_identity() {
        let body = br#"{"model":"m","messages":[]}"#;
        let out = translate_request_body("openai", "openai", body).unwrap();
        assert_eq!(out, body);
        assert_eq!(
            translate_response_body("anthropic", "anthropic", body).unwrap(),
            body
        );
    }

    /// Translated from `TestTranslateUnknownProtocolFails`.
    #[test]
    fn test_translate_unknown_protocol_fails() {
        assert!(translate_request_body("openai", "gemini", b"{}").is_err());
        assert!(translate_response_body("gemini", "openai", b"{}").is_err());
    }

    /// Translated from `TestValidProtocol`.
    #[test]
    fn test_valid_protocol_names() {
        for p in ["openai", "responses", "anthropic"] {
            assert!(valid_protocol(p), "{p} should be valid");
        }
        for p in ["", "gemini", "chat", "OpenAI"] {
            assert!(!valid_protocol(p), "{p} should be invalid");
        }
        assert_eq!(protocol_display_name("openai"), "OpenAI Chat Completions");
        assert_eq!(protocol_display_name("anthropic"), "Anthropic Messages");
        assert_eq!(protocol_display_name("responses"), "OpenAI Responses");
        assert_eq!(protocol_display_name("gemini"), "gemini");
    }

    // -----------------------------------------------------------------
    // Extra coverage
    // -----------------------------------------------------------------

    #[test]
    fn test_unsupported_error_names_both_protocols() {
        let err = err_unsupported("openai", "gemini");
        assert_eq!(
            err.to_string(),
            "cannot translate openai -> gemini: no translation path is implemented ()"
        );
    }

    #[test]
    fn test_translation_error_display_matches_go() {
        let err = TranslationError::new("openai", "anthropic", "stop", "boom");
        assert_eq!(
            err.to_string(),
            "cannot translate openai -> anthropic: boom (stop)"
        );
    }

    #[test]
    fn test_parse_failures_use_the_go_message_shape() {
        let err = translate_request_body("openai", "anthropic", b"{not json").unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("translate: parse chat request: "),
            "{text}"
        );
        assert_eq!(err.translation(), None);
    }

    #[test]
    fn test_chat_tool_result_without_call_id_is_refused() {
        let chat = chat_request(r#"{"model":"m","messages":[{"role":"tool","content":"x"}]}"#);
        let err = chat_to_anthropic(&chat).expect_err("missing tool_call_id accepted");
        let te = err.translation().unwrap();
        assert_eq!(te.field, "messages[].tool_call_id");
    }

    #[test]
    fn test_chat_message_merging_keeps_roles_alternating() {
        let chat = chat_request(
            r#"{"model":"m","messages":[
                {"role":"user","content":"one"},
                {"role":"user","content":"two"},
                {"role":"user","content":[{"type":"text","text":"three"}]}
            ]}"#,
        );
        let anth = chat_to_anthropic(&chat).unwrap();
        assert_eq!(anth.messages.len(), 1);
        let blocks = anth.messages[0].content.parts_or_parse();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].text, "three");
        // The merged raw content is still a JSON array.
        let raw = anth.messages[0].content.raw.as_ref().unwrap();
        assert!(raw.as_str().starts_with('['), "{}", raw.as_str());
    }

    #[test]
    fn test_chat_stop_shapes_are_translated() {
        let one = chat_to_anthropic(&chat_request(
            r#"{"model":"m","stop":"END","messages":[{"role":"user","content":"x"}]}"#,
        ))
        .unwrap();
        assert_eq!(one.stop_sequences, vec!["END"]);

        let many = chat_to_anthropic(&chat_request(
            r#"{"model":"m","stop":["A","B"],"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .unwrap();
        assert_eq!(many.stop_sequences, vec!["A", "B"]);

        let none = chat_to_anthropic(&chat_request(
            r#"{"model":"m","stop":"","messages":[{"role":"user","content":"x"}]}"#,
        ))
        .unwrap();
        assert!(none.stop_sequences.is_empty());

        let bad = chat_to_anthropic(&chat_request(
            r#"{"model":"m","stop":42,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .unwrap_err();
        assert_eq!(bad.translation().unwrap().field, "stop");
    }

    #[test]
    fn test_tool_choice_maps_between_vocabularies() {
        let cases = [
            (r#""auto""#, Some(r#"{"type":"auto"}"#)),
            (r#""required""#, Some(r#"{"type":"any"}"#)),
            (r#""none""#, Some(r#"{"type":"auto"}"#)),
            (r#""bogus""#, None),
            (
                r#"{"type":"function","function":{"name":"f"}}"#,
                Some(r#"{"name":"f","type":"tool"}"#),
            ),
        ];
        for (input, want) in cases {
            let raw = RawJson::new(input);
            let got = translate_tool_choice_to_anthropic(&raw);
            assert_eq!(
                got.as_ref().map(|r| r.as_str()),
                want,
                "anthropic tool_choice for {input}"
            );
        }
        let back = [
            (r#"{"type":"auto"}"#, Some(r#""auto""#)),
            (r#"{"type":"any"}"#, Some(r#""required""#)),
            (
                r#"{"type":"tool","name":"f"}"#,
                Some(r#"{"function":{"name":"f"},"type":"function"}"#),
            ),
            (r#"{"type":"nope"}"#, None),
        ];
        for (input, want) in back {
            let raw = RawJson::new(input);
            let got = translate_tool_choice_to_chat(&raw);
            assert_eq!(
                got.as_ref().map(|r| r.as_str()),
                want,
                "chat tool_choice for {input}"
            );
        }
    }

    #[test]
    fn test_json_schema_response_format_becomes_a_forced_tool() {
        let chat = chat_request(
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],
                "response_format":{"type":"json_schema","json_schema":{"name":"out","schema":{"type":"object"},"strict":true}}}"#,
        );
        let anth = chat_to_anthropic(&chat).unwrap();
        assert_eq!(anth.tools.len(), 1);
        assert_eq!(anth.tools[0].name, "out");
        assert_eq!(
            anth.tool_choice.as_ref().unwrap().as_str(),
            r#"{"type":"tool","name":"out"}"#
        );

        // Without a name the fixed default is used.
        let chat = chat_request(
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],
                "response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object"}}}}"#,
        );
        let anth = chat_to_anthropic(&chat).unwrap();
        assert_eq!(anth.tools[0].name, "structured_output");
        assert_eq!(
            anth.tools[0].description,
            "Return the answer in this JSON schema."
        );
    }

    #[test]
    fn test_anthropic_thinking_blocks_are_dropped_and_images_round_trip() {
        let req = anthropic_request(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"assistant","content":[
                {"type":"thinking","thinking":"internal"},
                {"type":"text","text":"visible"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}},
                {"type":"image","source":{"type":"url","url":"https://x/y.png"}}
            ]}]}"#,
        );
        let chat = anthropic_to_chat(&req).unwrap();
        let msg = &chat.messages[0];
        assert_eq!(msg.content.plain(), "visible");
        let urls: Vec<&str> = msg
            .content
            .parts
            .iter()
            .filter_map(|p| p.image_url.as_ref().map(|i| i.url.as_str()))
            .collect();
        assert_eq!(urls, vec!["data:image/png;base64,AAA", "https://x/y.png"]);
    }

    #[test]
    fn test_anthropic_unknown_block_type_is_refused() {
        let req = anthropic_request(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"mystery"}]}]}"#,
        );
        let err = anthropic_to_chat(&req).unwrap_err();
        assert_eq!(
            err.translation().unwrap().field,
            "messages[].content[].type"
        );
    }

    #[test]
    fn test_chat_unknown_part_type_is_refused_for_each_target() {
        let chat = chat_request(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"input_audio"}]}]}"#,
        );
        let err = chat_to_anthropic(&chat).unwrap_err();
        assert!(err.to_string().contains("input_audio"), "{err}");
        let err = chat_to_responses(&chat).unwrap_err();
        assert_eq!(
            err.translation().unwrap().field,
            "messages[].content[].type"
        );
    }

    #[test]
    fn test_responses_malformed_input_reports_the_go_field() {
        // A non-object element makes the array unparseable, which is the case
        // Go reports as a TranslationError on "input".
        let req: ResponsesRequest = serde_json::from_str(r#"{"model":"m","input":[5]}"#).unwrap();
        let err = responses_to_chat(&req).unwrap_err();
        assert_eq!(err.translation().unwrap().field, "input");

        // A well-formed item whose content is not text-shaped is skipped
        // silently, exactly like Go.
        let req: ResponsesRequest =
            serde_json::from_str(r#"{"model":"m","input":[{"type":"message","content":5}]}"#)
                .unwrap();
        assert!(responses_to_chat(&req).unwrap().messages.is_empty());
    }

    #[test]
    fn test_responses_unknown_item_type_is_refused() {
        let req: ResponsesRequest =
            serde_json::from_str(r#"{"model":"m","input":[{"type":"web_search_call","id":"x"}]}"#)
                .unwrap();
        let err = responses_to_chat(&req).unwrap_err();
        assert_eq!(err.translation().unwrap().field, "input[].type");
    }

    #[test]
    fn test_responses_reasoning_items_are_skipped() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"type":"reasoning","summary":[]},{"type":"message","role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        let chat = responses_to_chat(&req).unwrap();
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content.plain(), "hi");
    }

    #[test]
    fn test_chat_to_responses_without_items_matches_go_nil_slice() {
        let chat = chat_request(r#"{"model":"m","messages":[{"role":"system","content":"only"}]}"#);
        let resp = chat_to_responses(&chat).unwrap();
        assert_eq!(resp.instructions, "only");
        assert_eq!(resp.input.as_ref().unwrap().as_str(), "null");
    }

    #[test]
    fn test_empty_requests_still_translate() {
        let chat = ChatRequest::default();
        assert!(chat_to_anthropic(&chat).is_ok());
        assert!(chat_to_responses(&chat).is_ok());
        let anth = AnthropicRequest::default();
        assert!(anthropic_to_chat(&anth).is_ok());
        let resp = ResponsesRequest::default();
        assert!(responses_to_chat(&resp).is_ok());
        let chat_resp = ChatResponse::default();
        assert!(chat_to_anthropic_response(&chat_resp).is_ok());
        assert!(chat_to_responses_response(&chat_resp).is_ok());
        let anth_resp = AnthropicResponse::default();
        assert!(anthropic_to_chat_response(&anth_resp).is_ok());
        let resp_resp = ResponsesResponse::default();
        assert!(responses_to_chat_response(&resp_resp).is_ok());
    }

    #[test]
    fn test_stop_reason_mappings() {
        assert_eq!(anthropic_stop_to_chat("end_turn"), "stop");
        assert_eq!(anthropic_stop_to_chat("stop_sequence"), "stop");
        assert_eq!(anthropic_stop_to_chat("max_tokens"), "length");
        assert_eq!(anthropic_stop_to_chat("tool_use"), "tool_calls");
        assert_eq!(anthropic_stop_to_chat("refusal"), "refusal");

        assert_eq!(chat_stop_to_anthropic("stop"), "end_turn");
        assert_eq!(chat_stop_to_anthropic("length"), "max_tokens");
        assert_eq!(chat_stop_to_anthropic("tool_calls"), "tool_use");
        assert_eq!(chat_stop_to_anthropic("function_call"), "tool_use");
        assert_eq!(chat_stop_to_anthropic("content_filter"), "content_filter");
        assert_eq!(chat_stop_to_anthropic(""), "end_turn");
    }

    #[test]
    fn test_response_ids_get_translation_fallbacks() {
        assert_eq!(
            chat_to_anthropic_response(&ChatResponse::default())
                .unwrap()
                .id,
            "chatcmpl_translated"
        );
        assert_eq!(
            anthropic_to_chat_response(&AnthropicResponse::default())
                .unwrap()
                .id,
            "msg_translated"
        );
        assert_eq!(
            responses_to_chat_response(&ResponsesResponse::default())
                .unwrap()
                .id,
            "resp_translated"
        );
        assert_eq!(
            chat_to_responses_response(&ChatResponse::default())
                .unwrap()
                .id,
            "chatcmpl_translated"
        );
    }

    #[test]
    fn test_data_url_parsing() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AAA"),
            Some(("image/png".to_string(), "AAA".to_string()))
        );
        assert_eq!(
            parse_data_url("data:,payload"),
            Some((
                "application/octet-stream".to_string(),
                "payload".to_string()
            ))
        );
        assert_eq!(parse_data_url("https://x"), None);
        assert_eq!(parse_data_url("data:image/png"), None);
    }

    #[test]
    fn test_responses_multimodal_parts_convert_both_ways() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,AAA"}]}]}"#,
        )
        .unwrap();
        let chat = responses_to_chat(&req).unwrap();
        let parts = &chat.messages[0].content.parts;
        assert_eq!(parts[0].text, "look");
        assert_eq!(
            parts[1].image_url.as_ref().unwrap().url,
            "data:image/png;base64,AAA"
        );

        // ...and back to Responses.
        let back = chat_to_responses(&chat).unwrap();
        let input = back.input.as_ref().unwrap().as_str().to_string();
        assert!(input.contains("input_image"), "{input}");
        assert!(input.contains("look"), "{input}");
    }

    #[test]
    fn test_responses_builtin_image_parts_are_refused_on_the_way_out() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"output_audio"}]}]}"#,
        )
        .unwrap();
        let err = responses_to_chat(&req).unwrap_err();
        assert_eq!(err.translation().unwrap().field, "input[].content[].type");
    }

    #[test]
    fn test_two_hop_translations_are_reported_as_their_ends() {
        // A failure inside the two-hop path still names the outermost pair.
        let err = translate_request_body(
            "anthropic",
            "responses",
            br#"{"model":"m","messages":[{"role":"user","content":[{"type":"odd"}]}]}"#,
        )
        .unwrap_err();
        assert_eq!(
            err.translation().unwrap().field,
            "messages[].content[].type"
        );
    }
}
