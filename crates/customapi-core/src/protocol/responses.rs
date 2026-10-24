//! OpenAI Responses API (`/v1/responses`) types, event handling and stream
//! accumulation.
//!
//! The Responses API is not a rename of chat completions: the request takes
//! `input` instead of `messages`, the response returns a flat `output` array of
//! typed items instead of `choices`, and streaming uses named events
//! (`response.output_text.delta`) rather than anonymous delta chunks terminated
//! by `[DONE]`. All three differ enough that the gateway models them natively
//! rather than pretending they are chat completions.

use std::collections::BTreeMap;

use super::sse::SseEvent;
use super::types::{ApiError, RawJson, Usage};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A request to `POST /v1/responses`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesRequest {
    #[serde(default)]
    pub model: String,
    /// Either a plain string or an array of input items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RawJson>,
    /// Replaces the chat "system" message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    /// Bounds the generation.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub max_output_tokens: i64,
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub stream: bool,
    /// Mirrors the chat parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Tools and ToolChoice follow the Responses shape (flat function objects).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<RawJson>,
    /// Carries the structured-output configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<RawJson>,
    /// Server-side conversation state. The gateway forwards these but cannot
    /// audit a conversation it never sees, so a request using them is flagged
    /// (see [`ResponsesRequest::uses_server_state`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub previous_response_id: String,
    /// Echoed back by the upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RawJson>,
    /// Configures reasoning-effort style models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<RawJson>,
    /// Controls how over-long input is handled.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub truncation: String,
}

impl ResponsesRequest {
    /// Reports whether the request depends on state the gateway cannot see.
    ///
    /// This matters for auditing: with `previous_response_id` the full prompt
    /// lives on the provider, so the gateway's outbound inspection sees only the
    /// new turn and its token recount cannot account for the earlier
    /// conversation. A caller that cares about complete auditing should refuse
    /// these or force `store=false`.
    pub fn uses_server_state(&self) -> bool {
        if !self.previous_response_id.is_empty() {
            return true;
        }
        self.store.unwrap_or(false)
    }

    /// Extracts every piece of text the request sends, for auditing and for the
    /// local token recount.
    ///
    /// The Responses `input` field has three shapes: a bare string, an array of
    /// message objects, or an array of bare content parts. All three are
    /// handled; anything unrecognised is skipped rather than guessed at.
    pub fn input_text(&self) -> Vec<LabelledTextLike> {
        let mut out = Vec::new();
        if !self.instructions.is_empty() {
            out.push(LabelledTextLike {
                field: "instructions".to_string(),
                text: self.instructions.clone(),
            });
        }
        let Some(input) = &self.input else {
            return out;
        };
        let raw = input.as_str().trim();
        if raw.is_empty() || raw == "null" {
            return out;
        }
        // Shape 1: a plain string.
        if raw.starts_with('"') {
            if let Ok(s) = input.parse::<String>() {
                if !s.is_empty() {
                    out.push(LabelledTextLike {
                        field: "input".to_string(),
                        text: s,
                    });
                }
            }
            return out;
        }
        if !raw.starts_with('[') {
            return out;
        }
        // Shape 2/3: an array. Try the message form first. `type` and `role`
        // are parsed because Go's walker models them, but the extraction below
        // only reads the text-bearing fields.
        #[allow(dead_code)]
        #[derive(Default, serde::Deserialize)]
        struct Item {
            #[serde(rename = "type", default)]
            r#type: String,
            #[serde(default)]
            role: String,
            #[serde(default)]
            content: Option<RawJson>,
            // Some items carry text directly (input_text parts).
            #[serde(default)]
            text: String,
            // Function-call items carry arguments that must be audited too, and
            // a function_call_output carries the tool's result, which is
            // user-visible content and part of the next prompt.
            #[serde(default)]
            arguments: String,
            #[serde(default)]
            name: String,
            #[serde(default)]
            output: Option<RawJson>,
        }
        let Ok(items) = input.parse::<Vec<Item>>() else {
            return out;
        };
        for (i, item) in items.into_iter().enumerate() {
            let field = format!("input[{i}]");
            if !item.text.is_empty() {
                out.push(LabelledTextLike {
                    field: field.clone(),
                    text: item.text,
                });
            }
            if !item.name.is_empty() {
                out.push(LabelledTextLike {
                    field: format!("{field}.name"),
                    text: item.name,
                });
            }
            if !item.arguments.is_empty() {
                out.push(LabelledTextLike {
                    field: format!("{field}.arguments"),
                    text: item.arguments,
                });
            }
            if let Some(output) = &item.output {
                // The output may be a string or an array of content parts.
                let ot = output.as_str().trim();
                if ot.starts_with('"') {
                    if let Ok(s) = output.parse::<String>() {
                        if !s.is_empty() {
                            out.push(LabelledTextLike {
                                field: format!("{field}.output"),
                                text: s,
                            });
                        }
                    }
                } else if ot.starts_with('[') {
                    if let Ok(parts) = output.parse::<Vec<ResponsesContentPart>>() {
                        for (j, part) in parts.into_iter().enumerate() {
                            if !part.text.is_empty() {
                                out.push(LabelledTextLike {
                                    field: format!("{field}.output[{j}]"),
                                    text: part.text,
                                });
                            }
                        }
                    }
                }
            }
            let Some(content) = &item.content else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            // Content may be a string or an array of parts.
            let ct = content.as_str().trim();
            if ct.starts_with('"') {
                if let Ok(s) = content.parse::<String>() {
                    if !s.is_empty() {
                        out.push(LabelledTextLike {
                            field: format!("{field}.content"),
                            text: s,
                        });
                    }
                }
                continue;
            }
            let Ok(parts) = content.parse::<Vec<ResponsesContentPart>>() else {
                continue;
            };
            for (j, part) in parts.into_iter().enumerate() {
                if !part.text.is_empty() {
                    out.push(LabelledTextLike {
                        field: format!("{field}.content[{j}]"),
                        text: part.text,
                    });
                }
            }
        }
        out
    }

    /// Lists the tools the request declares.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| t.tool_name())
            .filter(|n| !n.is_empty())
            .collect()
    }
}

/// A tool definition in the Responses shape.
///
/// The Responses API flattens the chat "function" wrapper: the type is
/// "function" and the name/parameters sit at the top level. Values produced by
/// clients that still send the chat shape are handled by [`ResponsesTool::tool_name`]
/// and [`ResponsesTool::tool_schema`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<RawJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Carries the nested chat-style form when a client sends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<super::types::ToolFunction>,
}

impl ResponsesTool {
    /// Returns the effective tool name regardless of which shape was sent.
    pub fn tool_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if let Some(function) = &self.function {
            return function.name.clone();
        }
        String::new()
    }

    /// Returns the effective parameter schema.
    pub fn tool_schema(&self) -> Option<RawJson> {
        if let Some(parameters) = &self.parameters {
            if !parameters.is_empty() {
                return Some(parameters.clone());
            }
        }
        self.function.as_ref().and_then(|f| f.parameters.clone())
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A non-streaming `/v1/responses` reply.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ResponsesOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
    /// Carries a failed response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
    /// Explains a truncated response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<RawJson>,
    /// A convenience field some compatible servers add.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_text: String,
}

/// One entry of the output array.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesOutputItem {
    /// "message", "function_call", "reasoning", "web_search_call", ...
    #[serde(rename = "type", default)]
    pub r#type: String,
    /// Identifies the item.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Set for message items.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// Holds typed content parts for message items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ResponsesContentPart>,
    /// "completed" or "incomplete".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    // Below: function_call item fields.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub call_id: String,
    // Below: reasoning item fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<ResponsesContentPart>,
    /// Preserved verbatim (reasoning items are opaque).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub encrypted_content: String,
}

/// One part of a message item.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesContentPart {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Preserved but not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<RawJson>,
}

/// The Responses API usage block.
///
/// Note the field names differ from chat completions:
/// `input_tokens`/`output_tokens` rather than
/// `prompt_tokens`/`completion_tokens`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
    /// Breaks the input down (cached tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponsesInputTokensDetails>,
    /// Carries the reasoning-token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponsesOutputTokensDetails>,
}

/// The input breakdown.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub cached_tokens: i64,
}

/// The output breakdown.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesOutputTokensDetails {
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub reasoning_tokens: i64,
}

impl ResponsesUsage {
    /// Converts to the gateway's canonical usage shape.
    pub fn to_usage(&self) -> Usage {
        let total = if self.total_tokens == 0 {
            self.input_tokens + self.output_tokens
        } else {
            self.total_tokens
        };
        Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: total,
            ..Usage::default()
        }
    }
}

impl ResponsesResponse {
    /// Returns the assistant text of a Responses reply.
    pub fn response_text(&self) -> String {
        let mut out = String::new();
        if !self.output_text.is_empty() {
            out.push_str(&self.output_text);
        }
        for item in &self.output {
            match item.r#type.as_str() {
                "message" => {
                    for part in &item.content {
                        out.push_str(&part.text);
                    }
                }
                "reasoning" => {
                    // Reasoning summaries are generated tokens and are billed,
                    // so they count toward the completion side of the recount.
                    for part in &item.summary {
                        out.push_str(&part.text);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Returns any reasoning summary in the reply.
    pub fn reasoning_text(&self) -> String {
        let mut out = String::new();
        for item in &self.output {
            if item.r#type != "reasoning" {
                continue;
            }
            for part in &item.summary {
                out.push_str(&part.text);
            }
        }
        out
    }

    /// Returns the function calls the model asked for.
    pub fn function_calls(&self) -> Vec<ResponsesOutputItem> {
        self.output
            .iter()
            .filter(|item| item.r#type == "function_call")
            .cloned()
            .collect()
    }
}

/// Mirrors `policy.LabelledText` without importing the policy package, which
/// would create a cycle (policy imports protocol).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelledTextLike {
    pub field: String,
    pub text: String,
}

impl LabelledTextLike {
    /// Builds one labelled fragment.
    pub fn new(field: impl Into<String>, text: impl Into<String>) -> Self {
        LabelledTextLike {
            field: field.into(),
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Responses SSE events
// ---------------------------------------------------------------------------

/// Accumulates a streamed Responses reply.
///
/// The event vocabulary is larger than chat completions and is analysed here
/// rather than in the proxy so that the proxy stays protocol-agnostic.
#[derive(Debug)]
pub struct ResponsesStreamState {
    /// The id from `response.created`.
    pub response_id: String,
    /// The model the upstream reports.
    pub model: String,
    /// The terminal status ("completed", "incomplete", "failed").
    pub status: String,
    /// Accumulates `response.output_text.delta` fragments.
    pub text: String,
    /// Accumulates reasoning summary deltas.
    pub reasoning: String,
    /// Accumulates function-call arguments by output index.
    pub calls: BTreeMap<i64, ResponsesOutputItem>,
    /// Preserves first-seen ordering of function-call indices.
    pub call_order: Vec<i64>,
    /// Taken from `response.completed`/`response.incomplete`.
    pub usage: ResponsesUsage,
    /// Reports whether a usage block was seen.
    pub has_usage: bool,
    /// Set when the stream carried a failure event.
    pub error: Option<ApiError>,
    /// Explains a truncated response.
    pub incomplete_reason: String,
    /// Records which event types arrived, which makes an unexpected upstream
    /// dialect visible in the audit record.
    pub seen_event_types: BTreeMap<String, i64>,
    /// Counts consumed events.
    pub events: i64,
}

impl Default for ResponsesStreamState {
    fn default() -> Self {
        ResponsesStreamState::new()
    }
}

impl ResponsesStreamState {
    /// Creates an accumulator.
    pub fn new() -> Self {
        ResponsesStreamState {
            response_id: String::new(),
            model: String::new(),
            status: String::new(),
            text: String::new(),
            reasoning: String::new(),
            calls: BTreeMap::new(),
            call_order: Vec::new(),
            usage: ResponsesUsage::default(),
            has_usage: false,
            error: None,
            incomplete_reason: String::new(),
            seen_event_types: BTreeMap::new(),
            events: 0,
        }
    }

    /// Folds one SSE event into the state.
    pub fn observe(&mut self, ev: &SseEvent) {
        self.events += 1;
        let data = String::from_utf8_lossy(&ev.data).trim().to_string();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let e: ResponsesEvent = match serde_json::from_str(&data) {
            Ok(e) => e,
            Err(_) => {
                // A non-JSON payload is tolerated: some compatible servers emit
                // bare text. It still counts as assistant output.
                self.text.push_str(&data);
                return;
            }
        };
        // The event name is authoritative when present; some servers omit
        // "type".
        let mut name = e.r#type.clone();
        if name.is_empty() {
            name = ev.event.clone();
        }
        if name.is_empty() {
            name = "unknown".to_string();
        }
        *self.seen_event_types.entry(name.clone()).or_insert(0) += 1;

        match name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(resp) = &e.response {
                    if !resp.id.is_empty() {
                        self.response_id = resp.id.clone();
                    }
                    if !resp.model.is_empty() {
                        self.model = resp.model.clone();
                    }
                }
            }
            "response.output_text.delta" => self.text.push_str(&e.delta),
            "response.output_text.done" => {
                // Some servers send the whole text here instead of deltas; only
                // use it when no deltas were seen, or the text would be counted
                // twice.
                if !e.text.is_empty() && self.text.is_empty() {
                    self.text.push_str(&e.text);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.reasoning.push_str(&e.delta)
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                if !e.text.is_empty() && self.reasoning.is_empty() {
                    self.reasoning.push_str(&e.text);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = &e.item {
                    if item.r#type == "function_call" {
                        self.track_call(e.output_index, &item.id, &item.name, &item.call_id);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let call = self.track_call(e.output_index, &e.item_id, &e.name, &e.call_id);
                call.arguments.push_str(&e.delta);
            }
            "response.function_call_arguments.done" => {
                let call = self.track_call(e.output_index, &e.item_id, &e.name, &e.call_id);
                if !e.arguments.is_empty() {
                    call.arguments = e.arguments.clone();
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(resp) = &e.response {
                    if !resp.id.is_empty() {
                        self.response_id = resp.id.clone();
                    }
                    if !resp.model.is_empty() {
                        self.model = resp.model.clone();
                    }
                    if !resp.status.is_empty() {
                        self.status = resp.status.clone();
                    }
                    if let Some(usage) = &resp.usage {
                        self.usage = usage.clone();
                        self.has_usage = true;
                    }
                    // A completed event may carry the full output when the
                    // server did not stream deltas; fold it in without double
                    // counting.
                    if self.text.is_empty() {
                        for item in &resp.output {
                            if item.r#type == "message" {
                                for part in &item.content {
                                    self.text.push_str(&part.text);
                                }
                            }
                            if item.r#type == "function_call" {
                                let call =
                                    self.track_call_by_call_id(&item.call_id, &item.id, &item.name);
                                if call.arguments.is_empty() {
                                    call.arguments = item.arguments.clone();
                                }
                            }
                        }
                    }
                }
                if name == "response.failed" {
                    if let Some(err) = &e.error {
                        self.error = Some(err.clone());
                    } else if !e.message.is_empty() {
                        self.error = Some(ApiError {
                            message: e.message.clone(),
                            r#type: "upstream_error".to_string(),
                            ..ApiError::default()
                        });
                    }
                }
                if let Some(details) = &e.incomplete_details {
                    self.incomplete_reason = details.as_str().to_string();
                }
            }
            "error" => {
                if let Some(err) = &e.error {
                    self.error = Some(err.clone());
                } else if !e.message.is_empty() {
                    self.error = Some(ApiError {
                        message: e.message.clone(),
                        r#type: "upstream_error".to_string(),
                        ..ApiError::default()
                    });
                }
            }
            _ => {}
        }
    }

    /// Returns the accumulator for a function call at an output index.
    fn track_call(
        &mut self,
        index: i64,
        id: &str,
        name: &str,
        call_id: &str,
    ) -> &mut ResponsesOutputItem {
        let entry = self.calls.entry(index).or_insert_with(|| {
            self.call_order.push(index);
            ResponsesOutputItem {
                r#type: "function_call".to_string(),
                id: id.to_string(),
                call_id: call_id.to_string(),
                ..ResponsesOutputItem::default()
            }
        });
        if !name.is_empty() {
            entry.name.push_str(name);
        }
        if !id.is_empty() {
            entry.id = id.to_string();
        }
        if !call_id.is_empty() {
            entry.call_id = call_id.to_string();
        }
        entry
    }

    /// Finds or creates a call identified by `call_id`.
    fn track_call_by_call_id(
        &mut self,
        call_id: &str,
        id: &str,
        name: &str,
    ) -> &mut ResponsesOutputItem {
        for index in self.call_order.clone() {
            if let Some(call) = self.calls.get(&index) {
                if !call_id.is_empty() && call.call_id == call_id {
                    let call = self.calls.get_mut(&index).expect("checked above");
                    return call;
                }
            }
        }
        let index = self.calls.len() as i64;
        let call = self.calls.entry(index).or_insert_with(|| {
            self.call_order.push(index);
            ResponsesOutputItem {
                r#type: "function_call".to_string(),
                id: id.to_string(),
                name: name.to_string(),
                call_id: call_id.to_string(),
                ..ResponsesOutputItem::default()
            }
        });
        call
    }

    /// Returns accumulated function calls ordered by output index.
    pub fn calls_in_order(&self) -> Vec<ResponsesOutputItem> {
        self.call_order
            .iter()
            .filter_map(|index| self.calls.get(index).cloned())
            .collect()
    }

    /// Renders the accumulated stream as a non-streaming response, so the
    /// non-streaming audit rules can be applied unchanged.
    pub fn as_response(&self) -> ResponsesResponse {
        let mut resp = ResponsesResponse {
            id: self.response_id.clone(),
            object: "response".to_string(),
            model: self.model.clone(),
            status: self.status.clone(),
            ..ResponsesResponse::default()
        };
        if !self.text.is_empty() {
            resp.output.push(ResponsesOutputItem {
                r#type: "message".to_string(),
                role: "assistant".to_string(),
                status: "completed".to_string(),
                content: vec![ResponsesContentPart {
                    r#type: "output_text".to_string(),
                    text: self.text.clone(),
                    annotations: None,
                }],
                ..ResponsesOutputItem::default()
            });
        }
        if !self.reasoning.is_empty() {
            resp.output.push(ResponsesOutputItem {
                r#type: "reasoning".to_string(),
                summary: vec![ResponsesContentPart {
                    r#type: "summary_text".to_string(),
                    text: self.reasoning.clone(),
                    annotations: None,
                }],
                ..ResponsesOutputItem::default()
            });
        }
        resp.output.extend(self.calls_in_order());
        if self.has_usage {
            resp.usage = Some(self.usage.clone());
        }
        if let Some(err) = &self.error {
            resp.error = Some(err.clone());
        }
        resp
    }

    /// Reports whether a terminal event has been observed.
    pub fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "incomplete" | "failed")
            || self.error.is_some()
    }
}

/// The envelope shared by every Responses stream event.
///
/// Several fields are parsed for parity with Go's envelope but never read by the
/// accumulator (`content_index`, `summary_index`).
#[allow(dead_code)]
#[derive(Debug, Default, serde::Deserialize)]
struct ResponsesEvent {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    delta: String,
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    output_index: i64,
    #[serde(default)]
    content_index: i64,
    #[serde(default)]
    summary_index: i64,
    #[serde(default)]
    text: String,
    // Name/Arguments appear on response.function_call_arguments.delta and the
    // output_item.added event for a function call.
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    item: Option<ResponsesEventItem>,
    #[serde(default)]
    response: Option<ResponsesResponse>,
    #[serde(default)]
    error: Option<ApiError>,
    /// Accompanies error events.
    #[serde(default)]
    message: String,
    /// Accompanies `response.incomplete`.
    #[serde(default)]
    incomplete_details: Option<RawJson>,
}

/// The `item` payload of an output-item event. Only `type`, `id`, `name` and
/// `call_id` feed the accumulator; the rest is parsed for envelope parity.
#[allow(dead_code)]
#[derive(Debug, Default, serde::Deserialize)]
struct ResponsesEventItem {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Vec<ResponsesContentPart>,
    #[serde(default)]
    status: String,
}

/// Reports whether the event ends the stream.
///
/// The Responses API has no `[DONE]` sentinel; a stream ends with
/// `response.completed`, `response.incomplete` or `response.failed`, or simply
/// with the connection closing. Recognising this is what stops the gateway from
/// waiting for a terminator that will never arrive.
pub fn is_responses_terminal_event(name: &str) -> bool {
    matches!(
        name,
        "response.completed" | "response.incomplete" | "response.failed"
    )
}

/// Extracts the event name from a raw Responses SSE event without parsing the
/// payload, for the streaming fast path.
pub fn event_name(ev: &SseEvent) -> String {
    if !ev.event.is_empty() {
        return ev.event.clone();
    }
    let data = String::from_utf8_lossy(&ev.data).trim().to_string();
    if !data.starts_with('{') {
        return String::new();
    }
    const KEY: &str = r#""type""#;
    let Some(idx) = data.find(KEY) else {
        return String::new();
    };
    let rest = &data[idx + KEY.len()..];
    let Some(colon) = rest.find(':') else {
        return String::new();
    };
    let rest = rest[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return String::new();
    }
    let Some(end) = rest[1..].find('"') else {
        return String::new();
    };
    rest[1..1 + end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::RawJson;

    /// Translated from `TestDetectResponsesProtocol`.
    #[test]
    fn test_detect_responses_protocol() {
        let cases: [(&str, &str, &str, &str); 6] = [
            (
                "responses path",
                "/v1/responses",
                r#"{"model":"gpt-4o","input":"hi"}"#,
                "responses",
            ),
            (
                "chat path",
                "/v1/chat/completions",
                r#"{"model":"x","messages":[]}"#,
                "openai",
            ),
            (
                "messages path",
                "/v1/messages",
                r#"{"model":"x","max_tokens":1}"#,
                "anthropic",
            ),
            (
                "sniff input string",
                "/proxy",
                r#"{"model":"x","input":"hello"}"#,
                "responses",
            ),
            (
                "sniff instructions",
                "/proxy",
                r#"{"model":"x","instructions":"be nice"}"#,
                "responses",
            ),
            (
                "sniff messages wins",
                "/proxy",
                r#"{"model":"x","input":"a","messages":[{"role":"user","content":"b"}]}"#,
                "openai",
            ),
        ];
        for (name, path, body, want) in cases {
            assert_eq!(
                crate::protocol::rewrite::detect_protocol(path, body.as_bytes()),
                want,
                "case {name}"
            );
        }
    }

    fn input_text_for(body: &str) -> String {
        let req: ResponsesRequest = serde_json::from_str(body).expect("parse request");
        let mut all = String::new();
        for item in req.input_text() {
            all.push_str(&item.text);
            all.push('\n');
        }
        all
    }

    /// Translated from `TestResponsesInputText`: all three input shapes must be
    /// audited, because all three reach the model.
    #[test]
    fn test_responses_input_text() {
        let cases: [(&str, &str, &[&str]); 6] = [
            (
                "string input",
                r#"{"model":"m","input":"hello world"}"#,
                &["hello world"],
            ),
            (
                "instructions included",
                r#"{"model":"m","instructions":"be terse","input":"hi"}"#,
                &["be terse", "hi"],
            ),
            (
                "message array with string content",
                r#"{"model":"m","input":[{"type":"message","role":"user","content":"from a message"}]}"#,
                &["from a message"],
            ),
            (
                "message array with content parts",
                r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"part one"},{"type":"input_text","text":"part two"}]}]}"#,
                &["part one", "part two"],
            ),
            (
                "function call and output",
                r#"{"model":"m","input":[{"type":"function_call","name":"search","arguments":"{\"q\":\"x\"}"},{"type":"function_call_output","call_id":"c1","output":"result text"}]}"#,
                &["search", r#""q""#, "result text"],
            ),
            (
                "bare content part array",
                r#"{"model":"m","input":[{"type":"input_text","text":"bare part"}]}"#,
                &["bare part"],
            ),
        ];
        for (name, body, wants) in cases {
            let got = input_text_for(body);
            for want in wants {
                assert!(got.contains(want), "case {name}: {got:?} misses {want:?}");
            }
        }
    }

    #[test]
    fn test_responses_input_text_reports_field_names() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","instructions":"sys","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},{"type":"function_call","name":"f","arguments":"{}"}]}"#,
        )
        .unwrap();
        let items: Vec<(String, String)> = req
            .input_text()
            .into_iter()
            .map(|i| (i.field, i.text))
            .collect();
        assert_eq!(
            items,
            vec![
                ("instructions".to_string(), "sys".to_string()),
                ("input[0].content[0]".to_string(), "a".to_string()),
                ("input[1].name".to_string(), "f".to_string()),
                ("input[1].arguments".to_string(), "{}".to_string()),
            ]
        );
    }

    #[test]
    fn test_responses_input_text_tolerates_unusual_shapes() {
        for body in [
            r#"{"model":"m"}"#,
            r#"{"model":"m","input":null}"#,
            r#"{"model":"m","input":42}"#,
            r#"{"model":"m","input":["bare"]}"#,
            r#"{"model":"m","input":{}}"#,
        ] {
            let req: ResponsesRequest = serde_json::from_str(body).unwrap();
            assert!(req.input_text().is_empty(), "{body}");
        }
    }

    /// Translated from `TestResponsesUsesServerState`.
    #[test]
    fn test_responses_uses_server_state() {
        let with_prev: ResponsesRequest =
            serde_json::from_str(r#"{"model":"m","input":"x","previous_response_id":"resp_1"}"#)
                .unwrap();
        assert!(
            with_prev.uses_server_state(),
            "previous_response_id must count"
        );

        let with_store = ResponsesRequest {
            store: Some(true),
            ..ResponsesRequest::default()
        };
        assert!(with_store.uses_server_state(), "store=true must count");

        let without = ResponsesRequest {
            store: Some(false),
            ..ResponsesRequest::default()
        };
        assert!(!without.uses_server_state(), "store=false must not count");

        let absent = ResponsesRequest::default();
        assert!(!absent.uses_server_state());
    }

    /// Translated from `TestResponsesResponseText`.
    #[test]
    fn test_responses_response_text() {
        let body = r#"{
            "id":"resp_1","object":"response","model":"gpt-4o","status":"completed",
            "output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thinking hard"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]},
                {"type":"function_call","name":"lookup","arguments":"{\"k\":1}","call_id":"call_1"}
            ],
            "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30}
        }"#;
        let resp: ResponsesResponse = serde_json::from_str(body).unwrap();
        let text = resp.response_text();
        assert!(text.contains("the answer"), "{text}");
        assert!(text.contains("thinking hard"), "{text}");
        assert_eq!(resp.reasoning_text(), "thinking hard");
        let calls = resp.function_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "lookup");
        let usage = resp.usage.as_ref().unwrap().to_usage();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
    }

    /// Translated from `TestResponsesStreamAccumulation`.
    #[test]
    fn test_responses_stream_accumulation() {
        let events = [
            r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-4o","status":"in_progress"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hel"}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"lo "}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"world"}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","name":"look","call_id":"call_1"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"a\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"1}"}"#,
            r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-4o","status":"completed","usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#,
        ];
        let mut st = ResponsesStreamState::new();
        for e in events {
            st.observe(&SseEvent::data(e));
        }
        assert_eq!(st.text, "Hello world");
        assert_eq!(st.model, "gpt-4o");
        assert_eq!(st.response_id, "resp_1");
        assert!(st.has_usage);
        assert_eq!(st.usage.output_tokens, 3);
        let calls = st.calls_in_order();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "look");
        assert_eq!(calls[0].arguments, r#"{"a":1}"#);
        assert!(st.terminal());
        assert_eq!(st.as_response().response_text(), "Hello world");
    }

    /// Translated from `TestResponsesStreamFailed`.
    #[test]
    fn test_responses_stream_failed() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"response.created","response":{"id":"r","model":"m"}}"#,
        ));
        st.observe(&SseEvent::data(
            r#"{"type":"response.failed","response":{"id":"r","status":"failed"},"error":{"message":"boom","type":"server_error"}}"#,
        ));
        let err = st.error.as_ref().expect("failure captured");
        assert!(err.message.contains("boom"));
        assert!(st.terminal());
        assert_eq!(st.as_response().error.as_ref().unwrap().message, "boom");
    }

    /// Translated from `TestResponsesStreamWholeTextFallback`.
    #[test]
    fn test_responses_stream_whole_text_fallback() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"response.output_text.done","text":"whole answer"}"#,
        ));
        assert_eq!(st.text, "whole answer");
        // A completed event repeating the same output must not append it again.
        st.observe(&SseEvent::data(
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"whole answer"}]}]}}"#,
        ));
        assert_eq!(st.text, "whole answer");
    }

    /// Translated from `TestResponsesTerminalEvents`.
    #[test]
    fn test_responses_terminal_events() {
        for name in [
            "response.completed",
            "response.incomplete",
            "response.failed",
        ] {
            assert!(
                is_responses_terminal_event(name),
                "{name} should be terminal"
            );
        }
        for name in ["response.created", "response.output_text.delta", ""] {
            assert!(
                !is_responses_terminal_event(name),
                "{name} should not be terminal"
            );
        }
    }

    /// Translated from `TestEventNameFastPath`.
    #[test]
    fn test_event_name_fast_path() {
        let cases = [
            (
                r#"{"type":"response.completed","response":{}}"#,
                "response.completed",
            ),
            (
                r#"{"response":{"x":1},"type":"response.created"}"#,
                "response.created",
            ),
            (r#"{"foo":1}"#, ""),
            ("not json", ""),
            ("", ""),
            (r#"{"type":42}"#, ""),
            (r#"{"type":"unterminated"#, ""),
        ];
        for (body, want) in cases {
            assert_eq!(event_name(&SseEvent::data(body)), want, "body {body}");
        }
        assert_eq!(
            event_name(&SseEvent::named("message_start", "{}")),
            "message_start",
            "explicit event field should win"
        );
    }

    #[test]
    fn test_stream_state_folds_full_output_without_duplicating_calls() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{}","call_id":"c1"}"#,
        ));
        st.observe(&SseEvent::data(
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"c1"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#,
        ));
        let calls = st.calls_in_order();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "c1");
        assert_eq!(calls[0].arguments, "{}");
        assert_eq!(st.text, "done");
    }

    #[test]
    fn test_stream_state_accepts_bare_text_payloads() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data("bare text response"));
        assert_eq!(st.text, "bare text response");
        assert_eq!(st.events, 1);
    }

    #[test]
    fn test_stream_state_tracks_seen_event_types_and_event_field_name() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::named("response.created", "{}"));
        st.observe(&SseEvent::named("response.created", "{}"));
        assert_eq!(st.seen_event_types.get("response.created"), Some(&2));
        // An empty payload is skipped entirely (no "unknown" entry).
        st.observe(&SseEvent::data(""));
        assert!(!st.seen_event_types.contains_key("unknown"));
        // A payload with no type falls back to the unknown bucket.
        st.observe(&SseEvent::data("{}"));
        assert_eq!(st.seen_event_types.get("unknown"), Some(&1));
    }

    #[test]
    fn test_stream_state_error_event_synthesises_an_api_error() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"error","message":"rate limited"}"#,
        ));
        let err = st.error.as_ref().unwrap();
        assert_eq!(err.message, "rate limited");
        assert_eq!(err.r#type, "upstream_error");
    }

    #[test]
    fn test_stream_state_captures_incomplete_reason_and_status() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}},"incomplete_details":{"reason":"max_output_tokens"}}"#,
        ));
        assert_eq!(st.status, "incomplete");
        assert!(st.incomplete_reason.contains("max_output_tokens"));
        assert!(st.terminal());
    }

    #[test]
    fn test_as_response_orders_message_then_reasoning_then_calls() {
        let mut st = ResponsesStreamState::new();
        st.observe(&SseEvent::data(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#,
        ));
        st.observe(&SseEvent::data(
            r#"{"type":"response.output_text.delta","delta":"answer"}"#,
        ));
        st.observe(&SseEvent::data(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","name":"f","call_id":"c"}}"#,
        ));
        let resp = st.as_response();
        assert_eq!(resp.output.len(), 3);
        assert_eq!(resp.output[0].r#type, "message");
        assert_eq!(resp.output[1].r#type, "reasoning");
        assert_eq!(resp.output[2].r#type, "function_call");
        assert_eq!(resp.object, "response");
    }

    #[test]
    fn test_responses_usage_falls_back_to_component_sum() {
        let usage = ResponsesUsage {
            input_tokens: 4,
            output_tokens: 6,
            total_tokens: 0,
            ..ResponsesUsage::default()
        };
        assert_eq!(usage.to_usage().total_tokens, 10);

        let req = ResponsesRequest {
            tools: vec![
                ResponsesTool {
                    r#type: "function".into(),
                    name: "flat".into(),
                    ..ResponsesTool::default()
                },
                ResponsesTool {
                    r#type: "function".into(),
                    function: Some(crate::protocol::types::ToolFunction {
                        name: "nested".into(),
                        parameters: Some(RawJson::new(r#"{"type":"object"}"#)),
                        ..Default::default()
                    }),
                    ..ResponsesTool::default()
                },
            ],
            ..ResponsesRequest::default()
        };
        assert_eq!(req.tool_names(), vec!["flat", "nested"]);
        assert_eq!(
            req.tools[1].tool_schema().unwrap().as_str(),
            r#"{"type":"object"}"#
        );
    }
}
