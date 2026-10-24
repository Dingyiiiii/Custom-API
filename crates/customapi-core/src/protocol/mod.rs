//! Wire protocol layer: types, SSE plumbing, in-place rewriting, and
//! translation between the three line protocols the gateway speaks.
//!
//! | file                 | Go source             | contents                                  |
//! |----------------------|-----------------------|-------------------------------------------|
//! | [`types`]            | `types.go`            | request/response wire types               |
//! | [`sse`]              | `sse.go`              | SSE parser/writer, stream accumulator     |
//! | [`rewrite`]          | `rewrite.go`          | detection and byte-preserving field edits |
//! | [`translate`]        | `translate.go`        | cross-protocol conversion                 |
//! | [`responses`]        | `responses.go`        | the `/v1/responses` dialect               |
//! | [`stream_translate`] | `stream_translate.go` | live event rewriting                      |
//!
//! The gateway is a transparent reverse proxy, so the central constraint here is
//! that parsing must never change the bytes being forwarded. Unknown members are
//! kept as [`RawJson`], and every rewrite works on the original bytes.
//!
//! # Parity notes
//!
//! The port is checked against the Go implementation by feeding both sides the
//! same 250+ request, response and stream specimens and diffing the bytes. The
//! remaining known differences are all encoder-level and semantics-preserving:
//!
//! * Go HTML-escapes `<`, `>`, `&`, U+2028 and U+2029 in every string it writes
//!   (including inside a `json.Marshaler` result); serde_json never does. The
//!   decoded values are identical.
//! * JSON syntax errors are reported with serde_json's wording instead of
//!   `encoding/json`'s, after the same `translate: parse <what>: ` prefix.
//! * Go's decoder matches object keys case-insensitively and lets the last of
//!   duplicate keys win; serde is exact and rejects duplicates.
//!
//! Everything else — field names and order, `omitempty`, nil-slice-vs-`[]`, Go
//! float formatting, sorted map keys, event payloads and error text — matches
//! the Go output byte for byte.

pub mod responses;
pub mod rewrite;
pub mod sse;
pub mod stream_translate;
pub mod translate;
pub mod types;

pub use responses::{
    event_name, is_responses_terminal_event, LabelledTextLike, ResponsesContentPart,
    ResponsesInputTokensDetails, ResponsesOutputItem, ResponsesOutputTokensDetails,
    ResponsesRequest, ResponsesResponse, ResponsesStreamState, ResponsesTool, ResponsesUsage,
};
pub use rewrite::{
    detect_protocol, detect_stream_model, is_streaming, max_tokens_of, model,
    replace_top_level_string, top_level_string,
};
pub use sse::{SseEvent, SseParser, SseReader, SseWriter, StreamAccumulator};
pub use stream_translate::{
    describe_stream_translation, new_stream_translator, stream_translation_supported,
    ChatToAnthropicStream, ChatToResponsesStream, StreamTranslator,
};
pub use translate::{
    anthropic_stop_to_chat, anthropic_to_chat, anthropic_to_chat_response, chat_stop_to_anthropic,
    chat_to_anthropic, chat_to_anthropic_response, chat_to_responses, chat_to_responses_response,
    err_unsupported, protocol_display_name, responses_to_chat, responses_to_chat_response,
    translate_request_body, translate_response_body, valid_protocol, ProtocolError,
    TranslationError, ANTHROPIC_MAX_TOKENS_DEFAULT, CHAT_MAX_TOKENS_DEFAULT, ROLE_ASSISTANT,
    ROLE_SYSTEM, ROLE_TOOL, ROLE_USER,
};
pub use types::{
    AnthropicBlock, AnthropicError, AnthropicMessage, AnthropicRequest, AnthropicResponse,
    AnthropicTool, AnthropicUsage, ApiError, ChatMessage, ChatRequest, ChatResponse, Choice,
    Content, ContentPart, FunctionCall, ImageUrl, ModelInfo, ModelList, RawJson, ResponseFormat,
    StreamChoice, StreamChunk, TokenDetails, Tool, ToolCall, ToolFunction, Usage,
};

#[cfg(test)]
mod tests {
    //! Coverage of the way the gateway wires these pieces together: an upstream
    //! byte stream goes through the parser, the model sniffer, the accumulator
    //! and the stream translator in one pass, which is what the proxy does.

    use super::*;
    use serde_json::Value;

    /// An OpenAI-compatible stream, fed in awkward chunks, is parsed,
    /// accumulated for metering and rewritten into Responses events.
    #[test]
    fn proxy_style_stream_pipeline_translates_and_accrues() {
        let stream = concat!(
            "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\r\n\r\n",
            "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
            "data: [DONE]\n\n",
        );

        let mut parser = SseParser::new();
        let mut accumulator = StreamAccumulator::new("openai");
        let mut translator = new_stream_translator("openai", "responses").expect("supported");
        let mut rewritten: Vec<SseEvent> = Vec::new();
        let mut models: Vec<String> = Vec::new();

        // Deliver the body in 5-byte slices so events straddle the reads.
        for chunk in stream.as_bytes().chunks(5) {
            parser.push(chunk);
            while let Some(ev) = parser.next_event() {
                if !ev.raw.is_empty() {
                    let served = detect_stream_model(&ev.raw);
                    if !served.is_empty() {
                        models.push(served);
                    }
                }
                accumulator.observe(&ev);
                rewritten.extend(translator.observe(&ev));
            }
        }
        parser.finish();
        while let Some(ev) = parser.next_event() {
            accumulator.observe(&ev);
            rewritten.extend(translator.observe(&ev));
        }
        rewritten.extend(translator.finish());

        // The accumulator saw the whole reply, including the terminator.
        assert!(accumulator.saw_done);
        assert_eq!(accumulator.text, "Hello");
        assert_eq!(accumulator.model, "gpt-4o");
        assert_eq!(accumulator.usage.total_tokens, 9);
        assert!(accumulator.has_usage);
        assert!(accumulator.err.is_none(), "{:?}", accumulator.err);
        assert_eq!(models, vec!["gpt-4o", "gpt-4o", "gpt-4o", "gpt-4o"]);

        // The client receives a complete Responses event stream.
        let names: Vec<&str> = rewritten.iter().map(|ev| ev.event.as_str()).collect();
        assert_eq!(names.first(), Some(&"response.created"));
        assert_eq!(names.last(), Some(&"response.completed"));
        assert!(names.contains(&"response.output_text.delta"));
        assert!(names.contains(&"response.output_item.done"));
        assert!(
            !names.contains(&"response.incomplete"),
            "finish_reason stop is not a truncation"
        );
        for ev in &rewritten {
            let value: Value = serde_json::from_slice(&ev.data).expect("event payload");
            assert_eq!(value["type"], ev.event.as_str());
        }
        // Like Go, the finish_reason chunk already closed the stream, so the
        // later usage-only chunk cannot reach the client: the terminal event
        // carries no usage block (the accumulator still metered it).
        let completed: Value = serde_json::from_slice(&rewritten.last().unwrap().data).unwrap();
        assert!(completed["response"].get("usage").is_none(), "{completed}");
    }

    /// The same request body is detected, has its model rewritten in place and
    /// is then translated to the Messages protocol.
    #[test]
    fn proxy_style_request_pipeline_rewrites_then_translates() {
        let body = r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}],"未知字段":{"a":1}}"#;
        assert_eq!(
            detect_protocol("/v1/chat/completions", body.as_bytes()),
            "openai"
        );
        assert!(is_streaming(body.as_bytes()));

        let rewritten = replace_top_level_string(body.as_bytes(), "model", "claude-3-5-sonnet");
        assert_eq!(model(&rewritten), "claude-3-5-sonnet");
        // Untouched members keep their exact bytes, including the non-ASCII key.
        assert!(String::from_utf8(rewritten.clone())
            .unwrap()
            .contains(r#"{"a":1}}"#));

        let translated = translate_request_body("openai", "anthropic", &rewritten).unwrap();
        let value: Value = serde_json::from_slice(&translated).unwrap();
        assert_eq!(value["model"], "claude-3-5-sonnet");
        // Translated messages always carry the array form: Go marshals the
        // part slice rather than collapsing a lone text part back to a string.
        assert_eq!(
            value["messages"][0]["content"],
            serde_json::json!([{"type": "text", "text": "hi"}])
        );
        assert_eq!(value["stream"], true);
    }

    /// A buffered (non-live) translation is what the gateway falls back to for
    /// unsupported stream directions: the accumulated reply is re-rendered in
    /// the client's protocol.
    #[test]
    fn buffered_fallback_renders_the_accumulated_reply() {
        let mut accumulator = StreamAccumulator::new("anthropic");
        for data in [
            r#"{"type":"message_start","message":{"id":"m1","model":"claude-3","usage":{"input_tokens":4,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hey"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ] {
            accumulator.observe(&SseEvent::named("event", data));
        }
        let rendered = serde_json::to_vec(&accumulator.as_anthropic_response()).unwrap();
        let value: Value = serde_json::from_slice(&rendered).unwrap();
        assert_eq!(value["content"][0]["text"], "hey");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["usage"]["input_tokens"], 4);
        assert_eq!(value["usage"]["output_tokens"], 1);

        // ...and the chat view of the same stream, which the audit rules use.
        let chat = accumulator.as_response();
        assert_eq!(chat.response_text(), "hey");
        assert_eq!(chat.usage.as_ref().unwrap().total_tokens, 5);
    }
}
