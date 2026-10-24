//! Streaming translation.
//!
//! Converting a non-streaming body between protocols is a pure function over a
//! complete object. Converting a *stream* is not: the target protocol's event
//! vocabulary and ordering differ, and a client that asked for streamed output
//! must keep receiving events as they are produced rather than being buffered
//! into one body.
//!
//! The translation is therefore expressed as a stateful event rewriter that
//! mirrors the accumulator: it watches what the upstream sent, tracks the
//! indices and ids the target protocol requires, and emits the target's events
//! as it goes.
//!
//! Which pairs are supported: chat -> Responses and chat -> Messages are,
//! because those cover the case that motivated translation at all, a client
//! speaking a newer or vendor-specific protocol while the provider only offers
//! the chat surface. The reverse (a Responses or Messages upstream feeding a
//! chat client) and Responses <-> Messages are *not* yet rewritten; the gateway
//! detects that and falls back to buffering the stream so the reply is still
//! correct, at the cost of incremental delivery. Saying so plainly is better
//! than emitting a stream in the wrong dialect that a client would misparse.

use std::collections::BTreeMap;

use serde_json::Value;

use super::sse::SseEvent;
use super::translate::sorted_object;

/// Exposes the stop-reason mappings for the streaming path.
///
/// Go re-exports these from `stream_translate.go` because its helpers are
/// unexported there; the Rust mappings already live in [`super::translate`] and
/// are re-exported here so the streaming call sites read like the Go ones.
pub use super::translate::{anthropic_stop_to_chat, chat_stop_to_anthropic};
use super::types::{StreamChunk, ToolCall, Usage};

/// Rewrites a streamed reply from one protocol into another.
pub trait StreamTranslator {
    /// Folds one upstream event in and returns the events to send on.
    fn observe(&mut self, ev: &SseEvent) -> Vec<SseEvent>;
    /// Flushes any terminal events the translation still owes.
    fn finish(&mut self) -> Vec<SseEvent>;
}

/// Builds a rewriter for one direction.
///
/// Returns `None` when the direction is not implemented, so the caller can fall
/// back to buffering instead of guessing.
pub fn new_stream_translator(from: &str, to: &str) -> Option<Box<dyn StreamTranslator>> {
    match (from, to) {
        ("openai", "responses") => Some(Box::new(ChatToResponsesStream::default())),
        ("openai", "anthropic") => Some(Box::new(ChatToAnthropicStream::default())),
        _ => None,
    }
}

/// Reports whether a streamed direction is rewritten.
///
/// It is exported so the proxy can decide between live translation and buffered
/// translation without duplicating the table above.
pub fn stream_translation_supported(from: &str, to: &str) -> bool {
    new_stream_translator(from, to).is_some()
}

/// Renders a short description for logs and headers.
pub fn describe_stream_translation(from: &str, to: &str) -> String {
    if !stream_translation_supported(from, to) {
        return String::new();
    }
    format!("{from}->{to}")
}

/// Parses a chat chunk, returning `None` when the payload is not one.
///
/// Go returns `nil` from `Observe` on a parse failure: a malformed chunk must
/// not take the stream down.
fn parse_chunk(ev: &SseEvent) -> Option<StreamChunk> {
    serde_json::from_slice(&ev.data).ok()
}

fn encode(name: &str, payload: Value) -> SseEvent {
    SseEvent {
        event: name.to_string(),
        data: serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec()),
        raw: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// chat -> Responses
// ---------------------------------------------------------------------------

/// Rewrites chat completion chunks into Responses events.
#[derive(Debug, Default)]
pub struct ChatToResponsesStream {
    response_id: String,
    model: String,
    created: i64,
    /// Reports whether `response.created` has been emitted.
    started: bool,
    /// Tracks whether the message output item is currently open.
    item_open: bool,
    /// Guards against emitting a terminal event twice.
    finished: bool,
    /// Numbers the emitted events.
    seq: i64,
}

// Go also tracks `textSeen` and `usageSent`; neither is ever read, so the Rust
// port leaves them out rather than carrying dead state.

impl ChatToResponsesStream {
    /// Builds a fresh translator.
    pub fn new() -> Self {
        ChatToResponsesStream::default()
    }

    /// Returns the fixed opening events of a well-formed stream.
    fn open(&mut self, created: i64) -> Vec<SseEvent> {
        self.started = true;
        let resp = sorted_object(vec![
            ("id", Value::String(self.response_id.clone())),
            ("object", Value::String("response".to_string())),
            ("created_at", Value::Number(created.into())),
            ("model", Value::String(self.model.clone())),
            ("status", Value::String("in_progress".to_string())),
        ]);
        vec![self.event("response.created", vec![("response", resp)])]
    }

    fn event(&mut self, name: &str, mut payload: Vec<(&str, Value)>) -> SseEvent {
        payload.push(("type", Value::String(name.to_string())));
        payload.push(("sequence_number", Value::Number(self.seq.into())));
        self.seq += 1;
        encode(name, sorted_object(payload))
    }

    /// Emits the closing items and the terminal event.
    fn terminal_events(&mut self, status: &str, usage: Option<&Usage>) -> Vec<SseEvent> {
        self.finished = true;
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.open(self.created));
        }
        if self.item_open {
            out.push(self.event(
                "response.output_text.done",
                vec![
                    ("item_id", Value::String("msg_0".to_string())),
                    ("output_index", Value::Number(0.into())),
                    ("content_index", Value::Number(0.into())),
                    ("text", Value::String(String::new())),
                ],
            ));
            out.push(self.event(
                "response.output_item.done",
                vec![
                    ("output_index", Value::Number(0.into())),
                    (
                        "item",
                        sorted_object(vec![
                            ("id", Value::String("msg_0".to_string())),
                            ("type", Value::String("message".to_string())),
                            ("role", Value::String("assistant".to_string())),
                            ("status", Value::String("completed".to_string())),
                        ]),
                    ),
                ],
            ));
        }
        let mut resp = vec![
            ("id", Value::String(self.response_id.clone())),
            ("object", Value::String("response".to_string())),
            ("created_at", Value::Number(self.created.into())),
            ("model", Value::String(self.model.clone())),
            ("status", Value::String(status.to_string())),
        ];
        if let Some(usage) = usage {
            resp.push((
                "usage",
                sorted_object(vec![
                    ("input_tokens", Value::Number(usage.prompt_tokens.into())),
                    (
                        "output_tokens",
                        Value::Number(usage.completion_tokens.into()),
                    ),
                    ("total_tokens", Value::Number(usage.total_tokens.into())),
                ]),
            ));
        }
        let name = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        out.push(self.event(name, vec![("response", sorted_object(resp))]));
        // A [DONE] sentinel is not part of the Responses protocol, but some
        // clients tolerate it and some proxies rely on it. It is omitted:
        // emitting a token the protocol does not define is exactly the kind of
        // thing that makes a translation subtly wrong.
        out.retain(|ev| !ev.data.is_empty());
        out
    }

    fn open_message_item(&mut self) -> Vec<SseEvent> {
        self.item_open = true;
        vec![self.event(
            "response.output_item.added",
            vec![
                ("output_index", Value::Number(0.into())),
                (
                    "item",
                    sorted_object(vec![
                        ("id", Value::String("msg_0".to_string())),
                        ("type", Value::String("message".to_string())),
                        ("role", Value::String("assistant".to_string())),
                        ("status", Value::String("in_progress".to_string())),
                        ("content", Value::Array(Vec::new())),
                    ]),
                ),
            ],
        )]
    }

    fn tool_call_events(&mut self, index: i64, tc: ToolCall) -> Vec<SseEvent> {
        let mut out = vec![self.event(
            "response.output_item.added",
            vec![
                ("output_index", Value::Number(index.into())),
                (
                    "item",
                    sorted_object(vec![
                        ("id", Value::String(format!("fc_{}", tc.id))),
                        ("type", Value::String("function_call".to_string())),
                        ("call_id", Value::String(tc.id.clone())),
                        ("name", Value::String(tc.function.name.clone())),
                        ("arguments", Value::String(String::new())),
                        ("status", Value::String("in_progress".to_string())),
                    ]),
                ),
            ],
        )];
        if !tc.function.arguments.is_empty() {
            out.push(self.event(
                "response.function_call_arguments.delta",
                vec![
                    ("item_id", Value::String(format!("fc_{}", tc.id))),
                    ("output_index", Value::Number(index.into())),
                    ("delta", Value::String(tc.function.arguments.clone())),
                ],
            ));
        }
        out
    }

    fn finish_reason_events(&mut self, reason: &str) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        let status = if reason == "length" {
            "incomplete"
        } else {
            "completed"
        };
        self.terminal_events(status, None)
    }

    fn complete(&mut self) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.terminal_events("completed", None)
    }

    fn complete_with_usage(&mut self, usage: &Usage) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.terminal_events("completed", Some(usage))
    }
}

impl StreamTranslator for ChatToResponsesStream {
    fn observe(&mut self, ev: &SseEvent) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        if ev.is_done() {
            return self.complete();
        }
        let Some(chunk) = parse_chunk(ev) else {
            return Vec::new();
        };
        if !chunk.id.is_empty() {
            self.response_id = chunk.id.clone();
        }
        if !chunk.model.is_empty() {
            self.model = chunk.model.clone();
        }
        if chunk.created != 0 {
            self.created = chunk.created;
        }
        if self.response_id.is_empty() {
            self.response_id = "resp_stream".to_string();
        }

        let mut out = Vec::new();
        if !self.started {
            out.extend(self.open(self.created));
        }
        // A stream may carry a failure; forward it as a terminal failure event.
        if let Some(err) = &chunk.error {
            let response = sorted_object(vec![
                ("id", Value::String(self.response_id.clone())),
                ("object", Value::String("response".to_string())),
                ("model", Value::String(self.model.clone())),
                ("status", Value::String("failed".to_string())),
                (
                    "error",
                    sorted_object(vec![
                        ("message", Value::String(err.message.clone())),
                        ("type", Value::String(err.r#type.clone())),
                    ]),
                ),
            ]);
            out.push(self.event("response.failed", vec![("response", response)]));
            self.finished = true;
            return out;
        }

        for choice in &chunk.choices {
            // The message item opens lazily, on the first text or tool delta, so
            // a tool-only reply does not open an empty message item.
            let text = choice.delta.content.plain();
            if !text.is_empty() {
                if !self.item_open {
                    out.extend(self.open_message_item());
                }
                out.push(self.event(
                    "response.output_text.delta",
                    vec![
                        ("item_id", Value::String("msg_0".to_string())),
                        ("output_index", Value::Number(0.into())),
                        ("content_index", Value::Number(0.into())),
                        ("delta", Value::String(text)),
                    ],
                ));
            }
            if !choice.delta.reasoning_content.is_empty() {
                out.push(self.event(
                    "response.reasoning_summary_text.delta",
                    vec![
                        ("item_id", Value::String("rs_0".to_string())),
                        ("output_index", Value::Number(0.into())),
                        ("summary_index", Value::Number(0.into())),
                        (
                            "delta",
                            Value::String(choice.delta.reasoning_content.clone()),
                        ),
                    ],
                ));
            }
            for tc in &choice.delta.tool_calls {
                let index = tc.index.map(|i| i + 1).unwrap_or(1);
                out.extend(self.tool_call_events(index, tc.clone()));
            }
            if let Some(reason) = &choice.finish_reason {
                if !reason.is_empty() {
                    out.extend(self.finish_reason_events(reason));
                }
            }
        }

        // A usage-only chunk (choices empty) still carries the counts the client
        // expects on the terminating event.
        if chunk.choices.is_empty() {
            if let Some(usage) = &chunk.usage {
                out.extend(self.complete_with_usage(usage));
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        // The upstream closed without a terminator; close the stream properly so
        // a client is not left waiting.
        self.terminal_events("completed", None)
    }
}

// ---------------------------------------------------------------------------
// chat -> Messages
// ---------------------------------------------------------------------------

/// Rewrites chat chunks into Messages events.
#[derive(Debug, Default)]
pub struct ChatToAnthropicStream {
    message_id: String,
    model: String,
    started: bool,
    block_open: bool,
    finished: bool,
    /// The next content-block index.
    index: i64,
    /// Maps a chat tool-call index onto a Messages block index.
    ///
    /// Go iterates its map when closing blocks, which makes the order of the
    /// trailing `content_block_stop` events non-deterministic; a `BTreeMap`
    /// fixes the order at ascending block index, which is the order a client
    /// can actually reassemble.
    tool_index: BTreeMap<i64, i64>,
}

impl ChatToAnthropicStream {
    /// Builds a fresh translator.
    pub fn new() -> Self {
        ChatToAnthropicStream::default()
    }

    fn event(&self, name: &str, mut payload: Vec<(&str, Value)>) -> SseEvent {
        payload.push(("type", Value::String(name.to_string())));
        encode(name, sorted_object(payload))
    }

    fn message_start(&mut self) -> Vec<SseEvent> {
        self.started = true;
        vec![self.event(
            "message_start",
            vec![(
                "message",
                sorted_object(vec![
                    ("id", Value::String(self.message_id.clone())),
                    ("type", Value::String("message".to_string())),
                    ("role", Value::String("assistant".to_string())),
                    ("model", Value::String(self.model.clone())),
                    ("content", Value::Array(Vec::new())),
                    (
                        "usage",
                        sorted_object(vec![
                            ("input_tokens", Value::Number(0.into())),
                            ("output_tokens", Value::Number(0.into())),
                        ]),
                    ),
                ]),
            )],
        )]
    }

    fn tool_events(&mut self, key: i64, tc: &ToolCall) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let block_index = match self.tool_index.get(&key) {
            Some(index) => *index,
            None => {
                // Close the text block before opening a tool block: the Messages
                // protocol requires blocks to be strictly sequential.
                if self.block_open {
                    out.push(self.event(
                        "content_block_stop",
                        vec![("index", Value::Number(self.index.into()))],
                    ));
                    self.index += 1;
                    self.block_open = false;
                }
                let block_index = self.index;
                self.tool_index.insert(key, block_index);
                self.index += 1;
                out.push(self.event(
                    "content_block_start",
                    vec![
                        ("index", Value::Number(block_index.into())),
                        (
                            "content_block",
                            sorted_object(vec![
                                ("type", Value::String("tool_use".to_string())),
                                ("id", Value::String(tc.id.clone())),
                                ("name", Value::String(tc.function.name.clone())),
                                ("input", sorted_object(Vec::new())),
                            ]),
                        ),
                    ],
                ));
                block_index
            }
        };
        if !tc.function.arguments.is_empty() {
            out.push(self.event(
                "content_block_delta",
                vec![
                    ("index", Value::Number(block_index.into())),
                    (
                        "delta",
                        sorted_object(vec![
                            ("type", Value::String("input_json_delta".to_string())),
                            ("partial_json", Value::String(tc.function.arguments.clone())),
                        ]),
                    ),
                ],
            ));
        }
        out
    }

    fn terminal(&mut self, stop_reason: &str) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.message_start());
        }
        if self.block_open {
            out.push(self.event(
                "content_block_stop",
                vec![("index", Value::Number(self.index.into()))],
            ));
        }
        let indices: Vec<i64> = self.tool_index.values().copied().collect();
        for block_index in indices {
            out.push(self.event(
                "content_block_stop",
                vec![("index", Value::Number(block_index.into()))],
            ));
        }
        out.push(self.event(
            "message_delta",
            vec![
                (
                    "delta",
                    sorted_object(vec![
                        ("stop_reason", Value::String(stop_reason.to_string())),
                        ("stop_sequence", Value::Null),
                    ]),
                ),
                (
                    "usage",
                    sorted_object(vec![("output_tokens", Value::Number(0.into()))]),
                ),
            ],
        ));
        out.push(self.event("message_stop", Vec::new()));
        out
    }
}

impl StreamTranslator for ChatToAnthropicStream {
    fn observe(&mut self, ev: &SseEvent) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        if ev.is_done() {
            return self.terminal("end_turn");
        }
        let Some(chunk) = parse_chunk(ev) else {
            return Vec::new();
        };
        if !chunk.id.is_empty() {
            self.message_id = chunk.id.clone();
        }
        if !chunk.model.is_empty() {
            self.model = chunk.model.clone();
        }
        if self.message_id.is_empty() {
            self.message_id = "msg_stream".to_string();
        }

        let mut out = Vec::new();
        if !self.started {
            out.extend(self.message_start());
        }
        if let Some(err) = &chunk.error {
            out.push(self.event(
                "error",
                vec![(
                    "error",
                    sorted_object(vec![
                        ("type", Value::String("upstream_error".to_string())),
                        ("message", Value::String(err.message.clone())),
                    ]),
                )],
            ));
            self.finished = true;
            return out;
        }

        for choice in &chunk.choices {
            let text = choice.delta.content.plain();
            if !text.is_empty() {
                if !self.block_open {
                    self.block_open = true;
                    out.push(self.event(
                        "content_block_start",
                        vec![
                            ("index", Value::Number(self.index.into())),
                            (
                                "content_block",
                                sorted_object(vec![
                                    ("type", Value::String("text".to_string())),
                                    ("text", Value::String(String::new())),
                                ]),
                            ),
                        ],
                    ));
                }
                out.push(self.event(
                    "content_block_delta",
                    vec![
                        ("index", Value::Number(self.index.into())),
                        (
                            "delta",
                            sorted_object(vec![
                                ("type", Value::String("text_delta".to_string())),
                                ("text", Value::String(text)),
                            ]),
                        ),
                    ],
                ));
            }
            if !choice.delta.reasoning_content.is_empty() {
                out.push(self.event(
                    "content_block_delta",
                    vec![
                        ("index", Value::Number(self.index.into())),
                        (
                            "delta",
                            sorted_object(vec![
                                ("type", Value::String("thinking_delta".to_string())),
                                (
                                    "thinking",
                                    Value::String(choice.delta.reasoning_content.clone()),
                                ),
                            ]),
                        ),
                    ],
                ));
            }
            for tc in &choice.delta.tool_calls {
                let key = tc.index.unwrap_or(0);
                out.extend(self.tool_events(key, tc));
            }
            if let Some(reason) = &choice.finish_reason {
                if !reason.is_empty() {
                    out.extend(self.terminal(&anthropic_stop_to_chat(reason)));
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.terminal("end_turn")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn json_of(ev: &SseEvent) -> Value {
        serde_json::from_slice(&ev.data).expect("event payload is JSON")
    }

    fn names(events: &[SseEvent]) -> Vec<String> {
        events.iter().map(|ev| ev.event.clone()).collect()
    }

    #[test]
    fn test_translator_table_matches_go() {
        assert!(stream_translation_supported("openai", "responses"));
        assert!(stream_translation_supported("openai", "anthropic"));
        assert!(!stream_translation_supported("anthropic", "openai"));
        assert!(!stream_translation_supported("responses", "openai"));
        assert!(!stream_translation_supported("responses", "anthropic"));
        assert!(!stream_translation_supported("anthropic", "responses"));
        assert_eq!(
            describe_stream_translation("openai", "anthropic"),
            "openai->anthropic"
        );
        assert_eq!(describe_stream_translation("responses", "openai"), "");
    }

    #[test]
    fn test_chat_to_responses_emits_open_deltas_and_terminal() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let mut out = Vec::new();
        out.extend(tr.observe(&SseEvent::data(
            r#"{"id":"c1","model":"gpt-4o","created":1700000000,"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
        )));
        out.extend(tr.observe(&SseEvent::data(
            r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        )));
        out.extend(tr.observe(&SseEvent::data("[DONE]")));

        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let created = json_of(&out[0]);
        assert_eq!(created["type"], "response.created");
        assert_eq!(created["response"]["status"], "in_progress");
        assert_eq!(created["response"]["id"], "c1");
        assert_eq!(created["response"]["created_at"], 1700000000);
        assert_eq!(created["response"]["object"], "response");
        // sequence_number increments per emitted event.
        assert_eq!(created["sequence_number"], 0);
        assert_eq!(json_of(&out[1])["sequence_number"], 1);
        let last = json_of(out.last().unwrap());
        assert_eq!(last["response"]["status"], "completed");
        // Sorted keys, matching Go's map marshalling.
        let raw = String::from_utf8(out[0].data.clone()).unwrap();
        assert!(raw.starts_with(r#"{"response":{"created_at""#), "{raw}");
    }

    #[test]
    fn test_chat_to_responses_marks_length_as_incomplete() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"length"}]}"#,
        ));
        assert_eq!(names(&out).last().unwrap(), "response.incomplete");
        assert_eq!(
            json_of(out.last().unwrap())["response"]["status"],
            "incomplete"
        );
        // Further events are ignored once the terminal event went out.
        assert!(tr.observe(&SseEvent::data("[DONE]")).is_empty());
        assert!(tr.finish().is_empty());
    }

    #[test]
    fn test_chat_to_responses_attaches_usage_from_a_usage_only_chunk() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ));
        let last = out.last().unwrap();
        assert_eq!(last.event, "response.completed");
        let payload = json_of(last);
        assert_eq!(payload["response"]["usage"]["input_tokens"], 10);
        assert_eq!(payload["response"]["usage"]["output_tokens"], 3);
        assert_eq!(payload["response"]["usage"]["total_tokens"], 13);
    }

    #[test]
    fn test_chat_to_responses_forwards_upstream_errors_as_failures() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","error":{"message":"boom","type":"server_error"}}"#,
        ));
        assert_eq!(names(&out), vec!["response.created", "response.failed"]);
        let payload = json_of(out.last().unwrap());
        assert_eq!(payload["response"]["status"], "failed");
        assert_eq!(payload["response"]["error"]["message"], "boom");
        assert!(tr.observe(&SseEvent::data("{}")).is_empty());
    }

    #[test]
    fn test_chat_to_responses_tool_calls_use_index_plus_one() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"look","arguments":"{\"a\":"}}]}}]}"#,
        ));
        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.output_item.added",
                "response.function_call_arguments.delta",
            ],
            "a tool-only reply must not open a message item"
        );
        let added = json_of(&out[1]);
        assert_eq!(added["output_index"], 1);
        assert_eq!(added["item"]["id"], "fc_call_1");
        assert_eq!(added["item"]["call_id"], "call_1");
        assert_eq!(added["item"]["name"], "look");
        let delta = json_of(&out[2]);
        assert_eq!(delta["output_index"], 1);
        assert_eq!(delta["delta"], r#"{"a":"#);
    }

    #[test]
    fn test_chat_to_responses_finish_closes_an_unterminated_stream() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let first = tr.observe(&SseEvent::data(
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"content":"x"}}]}"#,
        ));
        assert_eq!(names(&first).len(), 3);
        let tail = tr.finish();
        assert_eq!(
            names(&tail),
            vec![
                "response.output_text.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert!(tr.finish().is_empty());
    }

    #[test]
    fn test_chat_to_responses_ignores_malformed_chunks() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        assert!(tr.observe(&SseEvent::data("{not json")).is_empty());
        // The translator is still usable afterwards.
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{"content":"x"}}]}"#,
        ));
        assert!(!out.is_empty());
    }

    #[test]
    fn test_chat_to_responses_fallback_ids() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"choices":[{"index":0,"delta":{"content":"x"}}]}"#,
        ));
        assert_eq!(json_of(&out[0])["response"]["id"], "resp_stream");
    }

    #[test]
    fn test_chat_to_anthropic_emits_a_well_formed_messages_stream() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let mut out = Vec::new();
        out.extend(tr.observe(&SseEvent::data(
            r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}"#,
        )));
        out.extend(tr.observe(&SseEvent::data(
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":"stop"}]}"#,
        )));
        out.extend(tr.observe(&SseEvent::data("[DONE]")));

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let start = json_of(&out[0]);
        assert_eq!(start["message"]["id"], "c1");
        assert_eq!(start["message"]["type"], "message");
        assert_eq!(start["message"]["role"], "assistant");
        assert_eq!(start["message"]["model"], "gpt-4o");
        assert_eq!(start["message"]["content"], json!([]));
        assert_eq!(start["message"]["usage"]["input_tokens"], 0);

        let delta = json_of(&out[2]);
        assert_eq!(delta["index"], 0);
        assert_eq!(delta["delta"]["type"], "text_delta");
        assert_eq!(delta["delta"]["text"], "Hi");

        let message_delta = json_of(&out[5]);
        // Go maps the upstream reason through `anthropicStopToChat`, which
        // leaves a chat ``stop`` unchanged instead of producing ``end_turn``.
        assert_eq!(message_delta["delta"]["stop_reason"], "stop");
        assert_eq!(message_delta["delta"]["stop_sequence"], Value::Null);
        assert_eq!(message_delta["usage"]["output_tokens"], 0);
        assert_eq!(json_of(&out[6])["type"], "message_stop");
    }

    #[test]
    fn test_chat_to_anthropic_closes_the_text_block_before_a_tool_block() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{"content":"thinking","tool_calls":[
                {"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#,
        ));
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
            ]
        );
        // The tool block starts at index 1, after the text block was closed.
        let tool_start = json_of(&out[4]);
        assert_eq!(tool_start["index"], 1);
        assert_eq!(tool_start["content_block"]["type"], "tool_use");
        assert_eq!(tool_start["content_block"]["id"], "call_1");
        assert_eq!(tool_start["content_block"]["name"], "f");
        assert_eq!(tool_start["content_block"]["input"], json!({}));
        let arg_delta = json_of(&out[5]);
        assert_eq!(arg_delta["index"], 1);
        assert_eq!(arg_delta["delta"]["type"], "input_json_delta");
        assert_eq!(arg_delta["delta"]["partial_json"], "{}");
    }

    #[test]
    fn test_chat_to_anthropic_reasoning_becomes_a_thinking_delta() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{"reasoning_content":"hmm"}}]}"#,
        ));
        let delta = out.last().unwrap();
        let payload = json_of(delta);
        assert_eq!(payload["delta"]["type"], "thinking_delta");
        assert_eq!(payload["delta"]["thinking"], "hmm");
    }

    #[test]
    fn test_chat_to_anthropic_stop_reasons_map() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ));
        let message_delta = out
            .iter()
            .find(|ev| ev.event == "message_delta")
            .expect("message_delta present");
        // Same Go quirk as above: the chat reason passes through unchanged.
        assert_eq!(json_of(message_delta)["delta"]["stop_reason"], "tool_calls");
    }

    #[test]
    fn test_chat_to_anthropic_forwards_upstream_errors() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","error":{"message":"boom","type":"server_error"}}"#,
        ));
        assert_eq!(names(&out), vec!["message_start", "error"]);
        let payload = json_of(out.last().unwrap());
        assert_eq!(payload["error"]["type"], "upstream_error");
        assert_eq!(payload["error"]["message"], "boom");
        assert!(tr.finish().is_empty());
    }

    #[test]
    fn test_chat_to_anthropic_finish_without_chunks_still_opens_the_message() {
        let mut tr = new_stream_translator("openai", "anthropic").unwrap();
        let out = tr.finish();
        assert_eq!(
            names(&out),
            vec!["message_start", "message_delta", "message_stop"]
        );
        assert_eq!(json_of(&out[0])["message"]["id"], "");
        assert!(tr.observe(&SseEvent::data("{}")).is_empty());
    }

    #[test]
    fn test_stop_reason_helpers_are_reachable_from_this_module() {
        assert_eq!(anthropic_stop_to_chat("max_tokens"), "length");
        assert_eq!(chat_stop_to_anthropic("length"), "max_tokens");
        assert_eq!(anthropic_stop_to_chat("tool_use"), "tool_calls");
        assert_eq!(chat_stop_to_anthropic("tool_calls"), "tool_use");
    }

    #[test]
    fn test_translated_events_are_always_serialisable_json() {
        let mut tr = new_stream_translator("openai", "responses").unwrap();
        let out = tr.observe(&SseEvent::data(
            r#"{"id":"c","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
        ));
        for ev in &out {
            let value = json_of(ev);
            assert_eq!(value["type"], ev.event.as_str());
            assert!(!ev.data.is_empty());
        }
    }

    #[test]
    fn test_malformed_chunk_parse_returns_none() {
        assert!(super::parse_chunk(&SseEvent::data("<html>")).is_none());
        assert!(super::parse_chunk(&SseEvent::data("{}")).is_some());
    }

    #[test]
    fn test_protocol_error_type_is_usable_for_parse_failures() {
        let err =
            super::super::translate::ProtocolError::Message("stream: chunk parse: x".to_string());
        assert_eq!(err.to_string(), "stream: chunk parse: x");
    }
}
