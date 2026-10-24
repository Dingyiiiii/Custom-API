//! Wire types the customs gateway inspects.
//!
//! The gateway is a transparent reverse proxy: it must be able to parse the
//! bodies it forwards without altering them. Every type here therefore keeps the
//! raw JSON of the parts the gateway does not understand ([`RawJson`]), so that
//! unknown or future fields survive a parse/encode round trip untouched.
//!
//! Field order, JSON names and `omitempty` semantics mirror the Go `types.go`
//! structs one for one, so that the bytes produced by a translation stay
//! comparable with the Go implementation.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Raw JSON
// ---------------------------------------------------------------------------

/// A `json.RawMessage`: the exact JSON text of a value.
///
/// The text is kept byte for byte so that splicing it back into a body cannot
/// change fields the gateway never looked at (number spellings, key order,
/// unknown members).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RawJson(String);

impl RawJson {
    /// Wraps already-valid JSON text.
    ///
    /// The argument is not validated: callers either obtained it from a parser
    /// or built it themselves. Use [`RawJson::from_slice`] when the bytes come
    /// from an untrusted boundary.
    pub fn new(text: impl Into<String>) -> Self {
        RawJson(text.into())
    }

    /// Validates and wraps JSON text.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| serde::de::Error::custom(format!("invalid utf-8: {e}")))?;
        // Round-tripping through RawValue validates the syntax.
        RawValue::from_string(text.to_string())?;
        Ok(RawJson(text.to_string()))
    }

    /// Returns the raw JSON text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the raw JSON bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Reports whether the value holds no text at all (Go's `len(raw) == 0`).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the text length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Parses the raw text into a typed value.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(&self.0)
    }

    /// Returns `true` when the raw text is exactly `null`.
    pub fn is_null(&self) -> bool {
        self.0.trim() == "null"
    }

    /// Returns `true` when the raw text starts with the given byte after
    /// trimming leading whitespace (used for the string/array shape probes).
    fn trimmed_starts_with(&self, prefix: u8) -> bool {
        self.0.trim_start().as_bytes().first() == Some(&prefix)
    }
}

impl From<&str> for RawJson {
    fn from(value: &str) -> Self {
        RawJson(value.to_string())
    }
}

impl From<String> for RawJson {
    fn from(value: String) -> Self {
        RawJson(value)
    }
}

impl fmt::Display for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for RawJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Emitting through RawValue splices the text verbatim instead of
        // re-encoding it as a JSON string.
        match RawValue::from_string(self.0.clone()) {
            Ok(raw) => raw.serialize(serializer),
            // Unreachable for values built by this crate: every constructor
            // either validated the text or copied it out of a parsed document.
            Err(_) => Value::Null.serialize(serializer),
        }
    }
}

/// Formats a float exactly the way Go's `encoding/json` does.
///
/// Go picks `%f` for magnitudes in `[1e-6, 1e21)` and `%e` otherwise, always
/// with the shortest representation that round-trips, and then shortens `e-07`
/// to `e-7`. Rust's `Display`/`LowerExp` produce the same digits, so only the
/// exponent sign needs restoring; without this, `temperature: 2` would come out
/// as `2.0` instead of Go's `2`.
fn go_format_float(value: f64) -> String {
    if !value.is_finite() {
        // Go refuses to encode NaN/Inf; JSON has no literal for either.
        return "null".to_string();
    }
    let abs = value.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        let text = format!("{value:e}");
        return match text.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => text,
        };
    }
    format!("{value}")
}

/// Serialises an optional float with Go's number formatting.
pub(crate) fn serialize_opt_f64_go<S: Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => {
            let text = go_format_float(*value);
            match RawValue::from_string(text) {
                Ok(raw) => raw.serialize(serializer),
                Err(e) => Err(serde::ser::Error::custom(e)),
            }
        }
        None => serializer.serialize_none(),
    }
}

/// Serialises a slice the way Go's `encoding/json` does for a struct field
/// without `omitempty`: a nil slice becomes `null`, a non-nil one an array.
///
/// The translated bodies are compared byte for byte with the Go implementation,
/// and Go leaves these slices nil whenever nothing was appended, so the empty
/// case has to render as `null` rather than `[]`.
pub(crate) fn serialize_slice_or_null<T, S>(values: &[T], serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    if values.is_empty() {
        serializer.serialize_none()
    } else {
        values.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Ok(RawJson(raw.get().to_string()))
    }
}

// ---------------------------------------------------------------------------
// Flexible content
// ---------------------------------------------------------------------------

/// Accepts either a plain string or an array of typed content parts, which is
/// how OpenAI and Anthropic both model message bodies.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    /// Parts holds the textual/typed pieces in order.
    pub parts: Vec<ContentPart>,
    /// Raw preserves the original JSON so a round trip is lossless.
    pub raw: Option<RawJson>,
}

impl Content {
    /// Builds content from typed parts.
    pub fn from_parts(parts: Vec<ContentPart>) -> Self {
        Content { parts, raw: None }
    }

    /// Builds plain text content (the one-part array shape).
    pub fn from_text(text: impl Into<String>) -> Self {
        Content {
            parts: vec![ContentPart {
                r#type: "text".to_string(),
                text: text.into(),
                ..ContentPart::default()
            }],
            raw: None,
        }
    }

    /// Builds content that marshals as the given raw JSON.
    pub fn from_raw(raw: RawJson) -> Self {
        Content {
            parts: Vec::new(),
            raw: Some(raw),
        }
    }

    /// Builds content from raw JSON bytes that are already known to be valid.
    pub fn from_raw_str(text: impl Into<String>) -> Self {
        Content::from_raw(RawJson::new(text))
    }

    /// Returns the content parts, decoding Raw when Parts is empty.
    ///
    /// Content produced by a parse of an Anthropic body may have Raw set but
    /// Parts empty, because the string shorthand (`"content":"hi"`) is what the
    /// wire form looks like most of the time. Reading `parts` directly would
    /// then silently yield no content - a translation would drop every message
    /// written in the shorthand, which is the single most common way to write
    /// one. Every consumer that needs structure rather than plain text must use
    /// this accessor.
    pub fn parts_or_parse(&self) -> Vec<ContentPart> {
        if !self.parts.is_empty() || self.raw.is_none() {
            return self.parts.clone();
        }
        self.decode_raw()
    }

    /// Parses Raw into parts, tolerating both the string and array forms.
    fn decode_raw(&self) -> Vec<ContentPart> {
        let Some(raw) = &self.raw else {
            return Vec::new();
        };
        if raw.is_empty() || raw.is_null() {
            return Vec::new();
        }
        if raw.trimmed_starts_with(b'"') {
            return match raw.parse::<String>() {
                Ok(s) if !s.is_empty() => vec![ContentPart {
                    r#type: "text".to_string(),
                    text: s,
                    ..ContentPart::default()
                }],
                _ => Vec::new(),
            };
        }
        raw.parse::<Vec<ContentPart>>().unwrap_or_default()
    }

    /// Returns the concatenated text of all text-bearing parts.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for p in self.parts_or_parse() {
            match p.r#type.as_str() {
                "" | "text" | "input_text" | "output_text" => out.push_str(&p.text),
                "tool_result" => {
                    if let Some(content) = &p.content {
                        out.push_str(&content.text());
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Returns a best-effort plain-text rendering used by audit rules.
    pub fn plain(&self) -> String {
        self.text()
    }

    /// Returns all text-bearing strings, including tool inputs, which is what
    /// the policy engine needs to scan.
    pub fn text_and_tool_input(&self) -> Vec<String> {
        let mut out = Vec::new();
        for p in self.parts_or_parse() {
            match p.r#type.as_str() {
                "" | "text" | "input_text" | "output_text" => {
                    if !p.text.is_empty() {
                        out.push(p.text);
                    }
                }
                "tool_use" => {
                    if !p.name.is_empty() {
                        out.push(p.name);
                    }
                    if let Some(input) = &p.input {
                        if !input.is_empty() {
                            out.push(input.as_str().to_string());
                        }
                    }
                }
                "tool_result" => {
                    if let Some(content) = &p.content {
                        out.extend(content.text_and_tool_input());
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Reports whether the content carries nothing.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

impl Serialize for Content {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The original bytes are emitted when available so unknown fields are
        // never lost.
        if let Some(raw) = &self.raw {
            return raw.serialize(serializer);
        }
        if self.parts.len() == 1 && self.parts[0].r#type == "text" {
            return serializer.serialize_str(&self.parts[0].text);
        }
        if self.parts.is_empty() {
            return serializer.serialize_none();
        }
        self.parts.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let raw = RawJson::new(raw.get().to_string());
        let text = raw.as_str().trim();
        if text.is_empty() || text == "null" {
            return Ok(Content {
                parts: Vec::new(),
                raw: Some(raw),
            });
        }
        if text.starts_with('"') {
            let s: String = raw
                .parse()
                .map_err(|e| de::Error::custom(format!("content: string form: {e}")))?;
            return Ok(Content {
                parts: vec![ContentPart {
                    r#type: "text".to_string(),
                    text: s,
                    ..ContentPart::default()
                }],
                raw: Some(raw),
            });
        }
        if text.starts_with('[') {
            let parts: Vec<ContentPart> = raw
                .parse()
                .map_err(|e| de::Error::custom(format!("content: array form: {e}")))?;
            return Ok(Content {
                parts,
                raw: Some(raw),
            });
        }
        // Anything else (object, number) is preserved but not interpreted.
        Ok(Content {
            parts: Vec::new(),
            raw: Some(raw),
        })
    }
}

/// One piece of a message body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Used by tool_result / tool_use blocks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Identifies a tool_use block, and is referenced by the matching
    /// tool_result block. Required for cross-protocol tool round trips.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The raw JSON of a tool_use input object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RawJson>,
    /// The nested content of a tool_result block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Box<Content>>,
    /// Preserved but not inspected as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// Preserved but not inspected as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RawJson>,
    /// Preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<RawJson>,
}

/// The OpenAI image reference shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions
// ---------------------------------------------------------------------------

/// An OpenAI-compatible chat completion request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub stream: bool,
    /// `max_tokens` / `max_completion_tokens` are both accepted.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub max_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub max_completion_tokens: i64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_go"
    )]
    pub temperature: Option<f64>,
    #[serde(
        rename = "top_p",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_go"
    )]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Preserves fields the gateway does not model. Go never populates it (it
    /// has no custom unmarshaller either); kept for API parity.
    #[serde(skip)]
    pub extra: BTreeMap<String, RawJson>,
}

/// One OpenAI chat message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Content,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refusal: String,
    /// Carries DeepSeek/vLLM style chain-of-thought.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning_content: String,
}

/// An OpenAI tool call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
    #[serde(default)]
    pub function: FunctionCall,
}

/// The function payload of a tool call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
}

/// An OpenAI tool definition.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default)]
    pub function: ToolFunction,
}

/// Describes a callable function.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Models structured output requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseFormat {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(
        rename = "json_schema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub schema: Option<RawJson>,
    /// Not part of the wire form; kept for API parity with Go.
    #[serde(skip)]
    pub json_object: Option<RawJson>,
}

/// An OpenAI-compatible chat completion response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub model: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// OpenAI's backend identity, useful for fingerprinting which model
    /// actually served the request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system_fingerprint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// One completion alternative.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finish_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<RawJson>,
}

/// The OpenAI usage block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<TokenDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<TokenDetails>,
}

/// Carries cached/reasoning breakdowns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenDetails {
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub cached_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub audio_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub reasoning_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub accepted_prediction_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub rejected_prediction_tokens: i64,
}

/// One SSE chunk of a streaming chat completion.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub model: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub choices: Vec<StreamChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Mirrors the non-streaming field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// One delta choice.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub delta: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<RawJson>,
}

/// The OpenAI error envelope.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub message: String,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub param: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
}

/// Renders a JSON value the way Go's `%v` prints the `any` field.
fn go_value_string(value: &Option<Value>) -> String {
    match value {
        None => "<nil>".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "<nil>".to_string(),
        Some(other) => other.to_string(),
    }
}

impl fmt::Display for ApiError {
    /// Mirrors Go's `Error()` so audit code can log the envelope directly.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (type={} code={})",
            self.message,
            self.r#type,
            go_value_string(&self.code)
        )
    }
}

impl std::error::Error for ApiError {}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

/// A Messages API request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Content>,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub max_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub stream: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_go"
    )]
    pub temperature: Option<f64>,
    #[serde(
        rename = "top_p",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_go"
    )]
    pub top_p: Option<f64>,
    #[serde(rename = "top_k", default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RawJson>,
}

/// One Messages API message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Content,
}

/// A Messages API tool definition.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTool {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<RawJson>,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
}

/// A Messages API response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicResponse {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub content: Vec<AnthropicBlock>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stop_sequence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AnthropicError>,
}

/// One content block of a response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicBlock {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RawJson>,
}

/// The Messages API usage block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub cache_creation_input_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub cache_read_input_tokens: i64,
}

impl AnthropicUsage {
    /// Converts into the OpenAI shape so that a single metering path can handle
    /// both protocols.
    pub fn to_openai(&self) -> Usage {
        let input =
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
        Usage {
            prompt_tokens: input,
            completion_tokens: self.output_tokens,
            total_tokens: input + self.output_tokens,
            ..Usage::default()
        }
    }
}

/// The Messages API error envelope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicError {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default)]
    pub message: String,
}

impl fmt::Display for AnthropicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.r#type, self.message)
    }
}

impl std::error::Error for AnthropicError {}

// ---------------------------------------------------------------------------
// Models / pricing
// ---------------------------------------------------------------------------

/// The OpenAI `/v1/models` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub object: String,
    #[serde(default, serialize_with = "serialize_slice_or_null")]
    pub data: Vec<ModelInfo>,
}

/// One entry of the model list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub created: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owned_by: String,
}

// ---------------------------------------------------------------------------
// Extraction helpers
// ---------------------------------------------------------------------------

impl ChatRequest {
    /// Extracts every piece of request text worth auditing or counting.
    pub fn prompt_text(&self) -> Vec<String> {
        let mut out = Vec::new();
        for m in &self.messages {
            out.extend(m.content.text_and_tool_input());
            if !m.reasoning_content.is_empty() {
                out.push(m.reasoning_content.clone());
            }
        }
        out
    }

    /// Returns the tool names declared by the request (both declared tools and
    /// the calls the conversation already contains).
    pub fn all_tool_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.tools {
            out.push(t.function.name.clone());
        }
        for m in &self.messages {
            for tc in &m.tool_calls {
                out.push(tc.function.name.clone());
            }
        }
        out
    }
}

impl ChatResponse {
    /// Extracts all generated text from a chat response.
    pub fn response_text(&self) -> String {
        let mut out = String::new();
        for ch in &self.choices {
            out.push_str(&ch.message.content.plain());
        }
        out
    }

    /// Extracts any chain-of-thought the upstream returned.
    pub fn reasoning_text(&self) -> String {
        let mut out = String::new();
        for ch in &self.choices {
            out.push_str(&ch.message.reasoning_content);
        }
        out
    }
}

impl AnthropicResponse {
    /// Extracts all generated text from a Messages response.
    pub fn response_text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            match b.r#type.as_str() {
                "text" => out.push_str(&b.text),
                "thinking" => out.push_str(&b.thinking),
                _ => {}
            }
        }
        out
    }

    /// Returns the tool calls an upstream asked for.
    pub fn tool_uses(&self) -> Vec<AnthropicBlock> {
        self.content
            .iter()
            .filter(|b| b.r#type == "tool_use")
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Translated from `TestContentStringForm`: the string shorthand stays a
    /// string on the way back out.
    #[test]
    fn test_content_string_form() {
        let c: Content = serde_json::from_str(r#""hello""#).unwrap();
        assert_eq!(c.text(), "hello");
        let out = serde_json::to_string(&c).unwrap();
        assert_eq!(out, r#""hello""#);
    }

    /// Translated from `TestContentArrayForm`.
    #[test]
    fn test_content_array_form() {
        let input = r#"[{"type":"text","text":"a"},{"type":"text","text":"b"}]"#;
        let c: Content = serde_json::from_str(input).unwrap();
        assert_eq!(c.text(), "ab");
        let out = serde_json::to_string(&c).unwrap();
        assert_eq!(out, input);
    }

    /// Translated from `TestContentNull`.
    #[test]
    fn test_content_null() {
        let c: Content = serde_json::from_str("null").unwrap();
        assert_eq!(c.text(), "");
        assert!(c.is_empty());
    }

    /// Translated from `TestChatRequestRoundTripPreservesUnknownFields`.
    #[test]
    fn test_chat_request_round_trip_preserves_unknown_fields() {
        let input = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"vendor_extension":{"x":1},"logit_bias":{"50256":-100}}"#;
        let req: ChatRequest = serde_json::from_str(input).unwrap();
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.text(), "hi");
    }

    /// The parts accessor must decode the raw shorthand, because a translation
    /// that reads `parts` directly would drop those messages.
    #[test]
    fn test_parts_or_parse_decodes_raw_shorthand() {
        let c = Content::from_raw_str(r#""hi there""#);
        assert!(c.parts.is_empty());
        let parts = c.parts_or_parse();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].r#type, "text");
        assert_eq!(parts[0].text, "hi there");
        assert_eq!(c.text(), "hi there");
    }

    #[test]
    fn test_parts_or_parse_handles_empty_and_broken_raw() {
        for raw in ["", "null", "{}", "42", "[}"] {
            let c = Content::from_raw_str(raw);
            assert!(c.parts_or_parse().is_empty(), "raw {raw:?}");
            assert_eq!(c.text(), "", "raw {raw:?}");
        }
        // An empty string shorthand carries no parts at all.
        let c = Content::from_raw_str(r#""""#);
        assert!(c.parts_or_parse().is_empty());
    }

    /// A part object the gateway does not model must survive verbatim.
    #[test]
    fn test_content_preserves_unknown_part_members() {
        let input =
            r#"[{"type":"text","text":"a","cache_control":{"type":"ephemeral"},"x-future":1}]"#;
        let c: Content = serde_json::from_str(input).unwrap();
        assert_eq!(c.text(), "a");
        assert_eq!(serde_json::to_string(&c).unwrap(), input);
        let parts = c.parts_or_parse();
        assert!(parts[0].cache_control.is_some());
    }

    #[test]
    fn test_content_serializes_parts_when_no_raw_is_set() {
        let c = Content::from_text("hi");
        assert_eq!(serde_json::to_string(&c).unwrap(), r#""hi""#);
        let c = Content::from_parts(vec![
            ContentPart {
                r#type: "text".into(),
                text: "a".into(),
                ..Default::default()
            },
            ContentPart {
                r#type: "image".into(),
                source: Some(RawJson::new(r#"{"type":"base64"}"#)),
                ..Default::default()
            },
        ]);
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"[{"type":"text","text":"a"},{"type":"image","source":{"type":"base64"}}]"#
        );
        // The zero value marshals as null, like Go's nil Parts.
        assert_eq!(serde_json::to_string(&Content::default()).unwrap(), "null");
    }

    #[test]
    fn test_text_and_tool_input_walks_tool_blocks() {
        let input = r#"[
            {"type":"text","text":"look"},
            {"type":"tool_use","name":"get_weather","input":{"city":"SH"}},
            {"type":"tool_result","content":[{"type":"text","text":"sunny"}]}
        ]"#;
        let c: Content = serde_json::from_str(input).unwrap();
        assert_eq!(
            c.text_and_tool_input(),
            vec!["look", "get_weather", r#"{"city":"SH"}"#, "sunny"]
        );
        assert_eq!(c.text(), "looksunny");
    }

    #[test]
    fn test_raw_json_round_trips_numbers_verbatim() {
        let input = r#"{"n":1e-3,"big":123456789012345678901234567890}"#;
        let raw: RawJson = serde_json::from_str(input).unwrap();
        assert_eq!(raw.as_str(), input);
        assert_eq!(serde_json::to_string(&raw).unwrap(), input);
        assert!(RawJson::from_slice(input.as_bytes()).is_ok());
        assert!(RawJson::from_slice(b"{oops").is_err());
    }

    #[test]
    fn test_api_error_display_matches_go_format() {
        let e = ApiError {
            message: "boom".into(),
            r#type: "server_error".into(),
            code: None,
            ..Default::default()
        };
        assert_eq!(e.to_string(), "boom (type=server_error code=<nil>)");
        let e = ApiError {
            code: Some(Value::Number(429.into())),
            ..e
        };
        assert_eq!(e.to_string(), "boom (type=server_error code=429)");
        let e = ApiError {
            code: Some(Value::String("rate_limit".into())),
            ..e
        };
        assert_eq!(e.to_string(), "boom (type=server_error code=rate_limit)");
    }

    #[test]
    fn test_anthropic_request_string_system_round_trips() {
        // "system" is the shorthand form in the Messages API; it must survive a
        // parse/encode round trip as a string, not balloon into an array.
        let body = r#"{"model":"m","max_tokens":10,"system":"be nice","messages":[{"role":"user","content":"hi"}]}"#;
        let req: AnthropicRequest = serde_json::from_str(body).unwrap();
        assert_eq!(req.system.as_ref().unwrap().text(), "be nice");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"system":"be nice","max_tokens":10}"#
        );
    }

    #[test]
    fn test_content_object_form_is_preserved_not_interpreted() {
        let c: Content = serde_json::from_str(r#"{"weird":true}"#).unwrap();
        assert!(c.parts_or_parse().is_empty());
        assert_eq!(c.text(), "");
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"weird":true}"#);
    }

    #[test]
    fn test_anthropic_usage_to_openai_includes_cache_tokens() {
        let u = AnthropicUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        };
        let openai = u.to_openai();
        assert_eq!(openai.prompt_tokens, 15);
        assert_eq!(openai.completion_tokens, 5);
        assert_eq!(openai.total_tokens, 20);
    }

    #[test]
    fn test_chat_message_omits_empty_optional_fields() {
        let msg = ChatMessage {
            role: "user".into(),
            content: Content::from_text("hi"),
            ..Default::default()
        };
        let out = serde_json::to_string(&msg).unwrap();
        assert_eq!(out, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn test_stream_choice_finish_reason_null_is_omitted() {
        let choice = StreamChoice {
            index: 0,
            delta: ChatMessage {
                role: "assistant".into(),
                content: Content::from_text("x"),
                ..Default::default()
            },
            finish_reason: None,
            logprobs: None,
        };
        assert_eq!(
            serde_json::to_string(&choice).unwrap(),
            r#"{"index":0,"delta":{"role":"assistant","content":"x"}}"#
        );
    }

    /// Go marshals a nil slice as `null`; a translated body must match.
    #[test]
    fn test_empty_slices_serialize_as_null_like_go() {
        assert_eq!(
            serde_json::to_string(&ChatRequest::default()).unwrap(),
            r#"{"model":"","messages":null}"#
        );
        assert_eq!(
            serde_json::to_string(&AnthropicRequest::default()).unwrap(),
            r#"{"model":"","messages":null}"#
        );
        assert_eq!(
            serde_json::to_string(&ChatResponse::default()).unwrap(),
            r#"{"id":"","object":"","created":0,"model":"","choices":null}"#
        );
        assert_eq!(
            serde_json::to_string(&AnthropicResponse::default()).unwrap(),
            r#"{"id":"","type":"","role":"","model":"","content":null}"#
        );
        assert_eq!(
            serde_json::to_string(&ModelList::default()).unwrap(),
            r#"{"object":"","data":null}"#
        );
        // A non-empty slice is still an array.
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::default()],
            ..ChatRequest::default()
        };
        assert!(serde_json::to_string(&req)
            .unwrap()
            .contains(r#""messages":[{"#));
    }

    /// Go formats floats with the shortest round-tripping form: `2`, not `2.0`.
    #[test]
    fn test_temperature_serializes_like_go() {
        let cases = [
            (2.0, "2"),
            (0.1, "0.1"),
            (-1.5, "-1.5"),
            (0.0, "0"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (0.30000000000000004, "0.30000000000000004"),
        ];
        for (value, want) in cases {
            let req = ChatRequest {
                temperature: Some(value),
                top_p: Some(value),
                ..ChatRequest::default()
            };
            let out = serde_json::to_string(&req).unwrap();
            assert!(
                out.contains(&format!(r#""temperature":{want},"#)),
                "temperature {value:?}: {out}"
            );
            assert!(
                out.contains(&format!(r#""top_p":{want}"#)),
                "top_p {value:?}: {out}"
            );
        }
        // Absent stays absent.
        assert!(!serde_json::to_string(&ChatRequest::default())
            .unwrap()
            .contains("temperature"));
    }

    #[test]
    fn test_zero_value_usage_keeps_all_three_counters() {
        assert_eq!(
            serde_json::to_string(&Usage::default()).unwrap(),
            r#"{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}"#
        );
        assert_eq!(
            serde_json::to_string(&TokenDetails::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn test_responses_types_serialize_with_declared_order() {
        let req = super::super::responses::ResponsesRequest {
            model: "m".into(),
            input: Some(RawJson::new(r#"{"a":1}"#)),
            max_output_tokens: 7,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"model":"m","input":{"a":1},"max_output_tokens":7}"#
        );
    }
}
